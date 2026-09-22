//! Sentry-SDK-compatible ingest (error tracking, `docs/ERROR_TRACKING.md`).
//!
//! Implements enough of the Sentry protocol for unmodified Sentry SDKs to
//! ship errors into the normal WAL → ingest → DuckDB pipeline:
//!
//! - `POST /api/{project}/envelope/` — the modern transport (all current
//!   SDKs). Envelope = JSON headers line + typed items (event / session /
//!   attachment).
//! - `POST /api/{project}/store/` — legacy JSON event endpoint
//!   (`X-Sentry-Auth: Sentry sentry_key=...` or `?sentry_key=`).
//!
//! Auth: the DSN's `sentry_key` IS an existing central-logs API key with
//! `insert` scope — `http://clk_...@host:port/my-service`. `project` maps to
//! the `service` column. Scope enforcement + `sentry_key` credential
//! extraction live in `web::auth` (the outermost middleware), so these
//! handlers assume an authenticated caller.
//!
//! Each event is mapped to a standard central-logs record and appended to
//! the WAL exactly like `/v1/logs` — parsing/grouping happens asynchronously
//! in the ingest worker.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use crate::config::ErrorTrackingConfig;
use crate::errors::{
    client_fingerprint, exception_fingerprint, message_fingerprint, truncate_stack,
};
use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;

#[derive(Clone)]
pub struct SentryState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    pub backpressure_timeout: Duration,
    pub cfg: ErrorTrackingConfig,
    /// WAL-cap pause flag (shared with the HTTP insert path).
    pub ingest_paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn router(state: SentryState) -> Router {
    Router::new()
        .route(
            "/api/{project}/envelope",
            post(envelope_post).options(cors_preflight),
        )
        .route(
            "/api/{project}/envelope/",
            post(envelope_post).options(cors_preflight),
        )
        .route(
            "/api/{project}/store",
            post(store_post).options(cors_preflight),
        )
        .route(
            "/api/{project}/store/",
            post(store_post).options(cors_preflight),
        )
        .with_state(state)
}

// =====================================================================
// Handlers
// =====================================================================

async fn cors_preflight() -> Response {
    with_cors(StatusCode::OK, Json(serde_json::json!({})))
}

async fn envelope_post(
    State(st): State<SentryState>,
    Path(project): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed) {
        return with_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                serde_json::json!({"error": "wal cap reached (retention.wal_max_bytes); ingest paused"}),
            ),
        );
    }
    if !st.cfg.enabled {
        return with_cors(
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "sentry ingest disabled"})),
        );
    }
    let Some(envelope) = parse_envelope(&body) else {
        return with_cors(
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid envelope"})),
        );
    };
    let service = resolve_service(&project, &st.cfg);
    let mut last_event_id = envelope
        .headers
        .get("event_id")
        .and_then(|v| v.as_str().map(String::from));
    let mut accepted = 0usize;
    for item in &envelope.items {
        if item.kind != "event" {
            // session/attachment/etc. are acked but not persisted (sessions
            // fold into nothing yet; large attachments are intentionally
            // dropped — see docs/ERROR_TRACKING.md §2.2).
            continue;
        }
        let Ok(ev) = serde_json::from_slice::<serde_json::Value>(&item.payload) else {
            continue;
        };
        if let Some(id) = ev.get("event_id").and_then(|v| v.as_str()) {
            last_event_id = Some(id.to_string());
        }
        if ingest_event(&st, &service, &ev).await {
            accepted += 1;
        }
    }
    if accepted == 0 && envelope.items.iter().all(|i| i.kind != "event") {
        // Non-event envelopes (sessions) are still a success for the SDK.
        return with_cors(
            StatusCode::OK,
            Json(serde_json::json!({"id": last_event_id})),
        );
    }
    with_cors(
        StatusCode::OK,
        Json(serde_json::json!({"id": last_event_id})),
    )
}

async fn store_post(
    State(st): State<SentryState>,
    Path(project): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed) {
        return with_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                serde_json::json!({"error": "wal cap reached (retention.wal_max_bytes); ingest paused"}),
            ),
        );
    }
    if !st.cfg.enabled {
        return with_cors(
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "sentry ingest disabled"})),
        );
    }
    let Ok(ev) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return with_cors(
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid JSON event"})),
        );
    };
    let service = resolve_service(&project, &st.cfg);
    let event_id = ev
        .get("event_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ok = ingest_event(&st, &service, &ev).await;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    with_cors(status, Json(serde_json::json!({"id": event_id})))
}

// =====================================================================
// Project → service mapping
// =====================================================================

/// Sentry SDKs require an INTEGER project id in the DSN, so map ids to
/// service names via `[error_tracking.projects]`. Unmapped numeric ids
/// become `project-<id>`; a non-numeric path (hand-rolled HTTP clients) is
/// used as the service name verbatim.
pub fn resolve_service(project: &str, cfg: &ErrorTrackingConfig) -> String {
    if let Some(name) = cfg.projects.get(project) {
        return name.clone();
    }
    if project.bytes().all(|b| b.is_ascii_digit()) && !project.is_empty() {
        format!("project-{project}")
    } else {
        project.to_string()
    }
}

// =====================================================================
// Event → central-logs record mapping
// =====================================================================

/// Map one Sentry event to a standard record and push it into the WAL.
async fn ingest_event(st: &SentryState, project: &str, ev: &serde_json::Value) -> bool {
    let record = map_event(project, ev, &st.cfg);
    let raw = serde_json::to_vec(&record).unwrap_or_else(|_| b"{}".to_vec());
    // Protocol::Sentry marks the row: filterable as `protocol:sentry` in the
    // DSL so error-tracked events separate cleanly from regular logs.
    let rec = crate::RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: String::new(),
        protocol: crate::Protocol::Sentry,
        raw: bytes::Bytes::from(raw),
    };
    st.counters.record(crate::Protocol::Sentry, rec.raw_len());
    let push = st.handle.append_acked(rec);
    match tokio::time::timeout(st.backpressure_timeout, push).await {
        Ok(Ok(())) => true,
        _ => {
            st.counters.record_error(crate::Protocol::Sentry);
            tracing::warn!(project, "sentry ingest: WAL append failed/backpressured");
            false
        }
    }
}

/// Build the central-logs record JSON for a Sentry event. Field mapping is
/// documented in `docs/ERROR_TRACKING.md` §2.3.
pub fn map_event(
    project: &str,
    ev: &serde_json::Value,
    cfg: &ErrorTrackingConfig,
) -> serde_json::Value {
    let level = normalize_sentry_level(ev.get("level").and_then(|v| v.as_str()));
    let exception = best_exception(ev);
    let (title, exception_type, stack) = match &exception {
        Some(exc) => {
            let etype = exc
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("UnknownError")
                .to_string();
            let evalue = exc
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let stack = serialize_stacktrace(
                exc.get("stacktrace"),
                cfg.stack_max_frames,
                cfg.stack_max_bytes,
            );
            // Prefix the frames with the exception headline — it's the line
            // every traceback viewer shows first.
            let stack = if stack.is_empty() {
                stack
            } else {
                format!("{etype}: {evalue}\n{stack}")
            };
            let title = if evalue.is_empty() {
                etype.clone()
            } else {
                format!("{etype}: {evalue}")
            };
            (title, Some(etype), stack)
        }
        None => {
            let msg = ev
                .get("message")
                .and_then(|m| match m {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Object(o) => o
                        .get("formatted")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    _ => None,
                })
                .unwrap_or_default();
            let stack = serialize_stacktrace(
                ev.get("stacktrace").or_else(|| {
                    ev.get("threads")
                        .and_then(|t| t.get("values"))
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|t| t.get("stacktrace"))
                }),
                cfg.stack_max_frames,
                cfg.stack_max_bytes,
            );
            let title = if msg.is_empty() {
                "Unknown error".to_string()
            } else {
                msg.chars().take(300).collect()
            };
            (title, None, stack)
        }
    };

    // Grouping: client fingerprint array wins; else exception+frames; else
    // message; else the service name alone (keeps one catch-all group).
    let fingerprint = ev
        .get("fingerprint")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
        .map(|a| {
            let vals: Vec<String> = a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if vals.is_empty() {
                None
            } else {
                Some(client_fingerprint(&vals))
            }
        })
        .flatten()
        .unwrap_or_else(|| match (&exception, &stack) {
            (Some(exc), _) => {
                let frames = collect_fingerprint_frames(exc.get("stacktrace"));
                let etype = exc
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UnknownError");
                let evalue = exc
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                exception_fingerprint(etype, evalue, &frames)
            }
            (None, stack) if !stack.is_empty() => crate::errors::variant_hash(stack),
            _ => message_fingerprint(project, &title),
        });

    let ts = sentry_timestamp(ev)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();

    // Everything we don't lift into columns rides in `attributes.sentry`
    // (user, request, tags, environment, release, breadcrumbs, ...).
    let mut sentry_meta = serde_json::Map::new();
    const META_KEYS: &[&str] = &[
        "event_id",
        "user",
        "request",
        "tags",
        "extra",
        "environment",
        "release",
        "dist",
        "server_name",
        "transaction",
        "logger",
        "platform",
        "sdk",
        "contexts",
        "breadcrumbs",
        "modules",
    ];
    if let Some(obj) = ev.as_object() {
        for k in META_KEYS {
            if let Some(v) = obj.get(*k) {
                if !v.is_null() {
                    sentry_meta.insert((*k).to_string(), v.clone());
                }
            }
        }
    }

    serde_json::json!({
        "ts": ts,
        "service": project,
        "level": level,
        "msg": title,
        "fingerprint": fingerprint,
        "exception_type": exception_type,
        "stack": stack,
        "sentry": sentry_meta,
    })
}

/// Pick the most relevant exception: Sentry lists the *originating* exception
/// last in `values`; prefer the last entry with a stacktrace, else the last.
fn best_exception(ev: &serde_json::Value) -> Option<serde_json::Value> {
    let values = ev
        .get("exception")
        .and_then(|e| e.get("values"))
        .and_then(|v| v.as_array())?;
    let chosen = values
        .iter()
        .rev()
        .find(|v| v.get("stacktrace").is_some())
        .or_else(|| values.last())?;
    Some(chosen.clone())
}

/// Serialize frames newest-first as `{function} @ {file}:{line}` lines with a
/// `#<n>` in_app marker, truncated per config.
fn serialize_stacktrace(
    st: Option<&serde_json::Value>,
    max_frames: usize,
    max_bytes: usize,
) -> String {
    let Some(st) = st else { return String::new() };
    let Some(frames) = st.get("frames").and_then(|f| f.as_array()) else {
        return String::new();
    };
    let mut lines: Vec<String> = Vec::with_capacity(frames.len());
    for frame in frames.iter().rev() {
        let func = frame
            .get("function")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let module = frame.get("module").and_then(|v| v.as_str()).unwrap_or("");
        let what = if module.is_empty() {
            func.to_string()
        } else {
            format!("{module}.{func}")
        };
        let file = frame
            .get("abs_path")
            .or_else(|| frame.get("filename"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let line = frame
            .get("lineno")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        let in_app = frame
            .get("in_app")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let marker = if in_app { " #" } else { "" };
        lines.push(format!("{what} @ {file}:{line}{marker}"));
    }
    let full = lines.join("\n");
    truncate_stack(&full, max_frames, max_bytes).0
}

/// (module, function, filename) triples for fingerprinting — top 5,
/// in-app frames preferred. Line numbers deliberately excluded.
fn collect_fingerprint_frames(st: Option<&serde_json::Value>) -> Vec<(String, String, String)> {
    let Some(frames) = st.and_then(|s| s.get("frames")).and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    // Newest-first; in-app frames carry the real signal, library noise is
    // only used as a fallback.
    let reversed: Vec<&serde_json::Value> = frames.iter().rev().collect();
    let in_app: Vec<&serde_json::Value> = reversed
        .iter()
        .copied()
        .filter(|f| f.get("in_app").and_then(|v| v.as_bool()).unwrap_or(false))
        .take(5)
        .collect();
    let chosen: Vec<&serde_json::Value> = if in_app.is_empty() {
        reversed.into_iter().take(5).collect()
    } else {
        in_app
    };
    chosen
        .iter()
        .map(|f| {
            (
                f.get("module")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                f.get("function")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                f.get("filename")
                    .or_else(|| f.get("abs_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn normalize_sentry_level(raw: Option<&str>) -> String {
    match raw.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("warning") => "warn".into(),
        Some("fatal") | Some("critical") => "fatal".into(),
        Some("error") => "error".into(),
        Some("info") => "info".into(),
        Some("debug") => "debug".into(),
        // Captured exceptions/messages default to error severity.
        _ => "error".into(),
    }
}

/// Sentry `timestamp` is seconds-since-epoch (int/float) or an ISO-8601
/// string depending on SDK/version.
fn sentry_timestamp(ev: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match ev.get("timestamp")? {
        serde_json::Value::Number(n) => {
            if let Some(secs) = n.as_i64() {
                chrono::DateTime::from_timestamp(secs, 0)
            } else {
                let f = n.as_f64()?;
                chrono::DateTime::from_timestamp(f.trunc() as i64, (f.fract().abs() * 1e9) as u32)
            }
        }
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc)),
        _ => None,
    }
}

// =====================================================================
// Envelope parsing (https://develop.sentry.dev/sdk/envelopes/)
// =====================================================================

struct Envelope {
    headers: serde_json::Value,
    items: Vec<EnvelopeItem>,
}

struct EnvelopeItem {
    kind: String,
    payload: Vec<u8>,
}

/// Slice off trailing `\r` (some SDKs send CRLF line endings).
fn trim_cr(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'\r' {
        end -= 1;
    }
    &b[..end]
}

/// Byte-level envelope parse: headers line, then per item a JSON header line
/// (may carry a byte `length`) followed by the payload.
fn parse_envelope(body: &[u8]) -> Option<Envelope> {
    let nl = body.iter().position(|&b| b == b'\n')?;
    let headers: serde_json::Value = serde_json::from_slice(trim_cr(&body[..nl])).ok()?;
    let mut items = Vec::new();
    let mut pos = nl + 1;
    while pos < body.len() {
        // Item header line.
        let rel = body[pos..].iter().position(|&b| b == b'\n')?;
        let header_end = pos + rel;
        let header: serde_json::Value =
            serde_json::from_slice(trim_cr(&body[pos..header_end])).ok()?;
        let kind = header
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("event")
            .to_string();
        let mut payload_start = header_end + 1;
        let payload: Vec<u8> = if let Some(len) = header.get("length").and_then(|v| v.as_u64()) {
            let end = (payload_start + len as usize).min(body.len());
            let p = body[payload_start..end].to_vec();
            payload_start = end;
            // Skip the newline after a length-delimited payload.
            if payload_start < body.len() && body[payload_start] == b'\n' {
                payload_start += 1;
            }
            p
        } else {
            // Length-less item: payload is the next line (or trailing bytes).
            let rest = &body[payload_start..];
            match rest.iter().position(|&b| b == b'\n') {
                Some(rel) => {
                    let p = rest[..rel].to_vec();
                    payload_start = payload_start + rel + 1;
                    p
                }
                None => {
                    let p = rest.to_vec();
                    payload_start = body.len();
                    p
                }
            }
        };
        pos = payload_start;
        items.push(EnvelopeItem { kind, payload });
    }
    Some(Envelope { headers, items })
}

// =====================================================================
// CORS (browser SDKs are cross-origin by definition)
// =====================================================================

fn with_cors(status: StatusCode, body: Json<serde_json::Value>) -> Response {
    let mut resp = (status, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("X-Sentry-Auth, X-Sentry-Last-Event-Id, Content-Type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, OPTIONS"),
    );
    resp
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ErrorTrackingConfig;

    fn cfg() -> ErrorTrackingConfig {
        ErrorTrackingConfig::default()
    }

    #[test]
    fn envelope_roundtrip_with_length_delimited_items() {
        let payload = r#"{"id":"x","message":"boom"}"#;
        let body = format!(
            "{{\"event_id\":\"abc-1\",\"sent_at\":\"2026-01-01T00:00:00Z\"}}\n\
             {{\"type\":\"event\",\"length\":{}}}\n{}\n\
             {{\"type\":\"session\"}}\n{{\"sid\":\"s1\",\"status\":\"exited\"}}\n",
            payload.len(),
            payload
        );
        let env = parse_envelope(body.as_bytes()).expect("envelope parses");
        assert_eq!(env.headers["event_id"], "abc-1");
        assert_eq!(env.items.len(), 2);
        assert_eq!(env.items[0].kind, "event");
        let ev: serde_json::Value = serde_json::from_slice(&env.items[0].payload).unwrap();
        assert_eq!(ev["message"], "boom");
        assert_eq!(env.items[1].kind, "session");
    }

    #[test]
    fn envelope_without_item_lengths() {
        let body = b"{\"event_id\":\"e2\"}\n{\"type\":\"event\"}\n{\"message\":\"m\"}\n";
        let env = parse_envelope(body).expect("envelope parses");
        assert_eq!(env.items.len(), 1);
        let ev: serde_json::Value = serde_json::from_slice(&env.items[0].payload).unwrap();
        assert_eq!(ev["message"], "m");
    }

    #[test]
    fn envelope_garbage_rejected() {
        assert!(parse_envelope(b"not json").is_none());
        assert!(parse_envelope(b"").is_none());
    }

    #[test]
    fn maps_exception_event_with_client_fingerprint() {
        let event = serde_json::json!({
            "event_id": "d6f2b1f0-1",
            "timestamp": 1_760_000_000.5,
            "platform": "python",
            "level": "error",
            "environment": "prod",
            "release": "app@1.2.3",
            "fingerprint": ["payment", "timeout"],
            "exception": {"values": [{
                "type": "TimeoutError",
                "value": "gateway did not respond in 42ms",
                "stacktrace": {"frames": [
                    {"filename": "app.py", "function": "outer", "in_app": false, "lineno": 10},
                    {"filename": "pay.py", "function": "charge", "module": "pay", "in_app": true, "lineno": 99}
                ]}
            }]},
            "user": {"id": 42, "email": "u@example.com"},
            "tags": {"region": "eu"}
        });
        let rec = map_event("checkout", &event, &cfg());
        assert_eq!(rec["service"], "checkout");
        assert_eq!(rec["level"], "error");
        assert_eq!(rec["msg"], "TimeoutError: gateway did not respond in 42ms");
        assert_eq!(rec["exception_type"], "TimeoutError");
        // Client fingerprint wins.
        assert_eq!(
            rec["fingerprint"],
            client_fingerprint(&["payment".into(), "timeout".into()])
        );
        // Stack serialized newest-first with in_app marker.
        let stack = rec["stack"].as_str().unwrap();
        assert!(stack.contains("pay.charge @ pay.py:99 #"), "got: {stack}");
        assert!(stack.contains("outer @ app.py:10"));
        // Passthrough metadata rides in attributes.sentry.
        assert_eq!(rec["sentry"]["user"]["id"], 42);
        assert_eq!(rec["sentry"]["tags"]["region"], "eu");
        assert_eq!(rec["sentry"]["release"], "app@1.2.3");
        // Float timestamps parse.
        let ts = chrono::DateTime::parse_from_rfc3339(rec["ts"].as_str().unwrap()).unwrap();
        assert_eq!(ts.timestamp_millis(), 1_760_000_000_500);
    }

    #[test]
    fn maps_message_event_and_normalizes_level() {
        let event = serde_json::json!({
            "message": {"formatted": "disk almost full"},
            "level": "warning",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let rec = map_event("svc", &event, &cfg());
        assert_eq!(
            rec["level"], "warn",
            "sentry 'warning' normalizes to 'warn'"
        );
        assert_eq!(rec["msg"], "disk almost full");
        // message events get a message-based fingerprint.
        assert_eq!(
            rec["fingerprint"],
            message_fingerprint("svc", "disk almost full")
        );
    }

    #[test]
    fn server_fingerprint_ignores_volatile_exception_values() {
        let base = serde_json::json!({
            "level": "error",
            "exception": {"values": [{
                "type": "ValueError",
                "value": "bad input 12345",
                "stacktrace": {"frames": [
                    {"filename": "a.py", "function": "run", "module": "a", "in_app": true}
                ]}
            }]}
        });
        let noisy = serde_json::json!({
            "level": "error",
            "exception": {"values": [{
                "type": "ValueError",
                "value": "bad input 98765",
                "stacktrace": {"frames": [
                    {"filename": "a.py", "function": "run", "module": "a", "in_app": true}
                ]}
            }]}
        });
        let r1 = map_event("svc", &base, &cfg());
        let r2 = map_event("svc", &noisy, &cfg());
        assert_eq!(
            r1["fingerprint"], r2["fingerprint"],
            "noise must not split groups"
        );
    }

    #[test]
    fn stack_truncation_applies() {
        let mut ev = serde_json::json!({
            "level": "error",
            "exception": {"values": [{"type": "E", "value": "v", "stacktrace": {"frames": []}}]}
        });
        let frames: Vec<serde_json::Value> = (0..50)
            .map(|i| serde_json::json!({"filename": "f.rs", "function": format!("fn{i}"), "lineno": i}))
            .collect();
        ev["exception"]["values"][0]["stacktrace"]["frames"] = serde_json::json!(frames);
        let mut c = cfg();
        c.stack_max_frames = 10;
        let rec = map_event("svc", &ev, &c);
        let stack = rec["stack"].as_str().unwrap();
        // headline + 10 frame lines (the headline does not count against
        // stack_max_frames).
        assert!(stack.starts_with("E: v\n"), "headline first, got: {stack}");
        assert_eq!(stack.lines().count(), 11);
    }

    #[test]
    fn sentry_key_header_parsing_matches_auth_side() {
        // The auth middleware strips the "Sentry " scheme token, then parses
        // comma-separated key=value pairs.
        let h = "Sentry sentry_key=clk_abc, sentry_version=777, sentry_client=sentry.python/2.0";
        let s = h.trim().strip_prefix("Sentry ").unwrap_or(h);
        let got = s.split(',').find_map(|p| {
            let p = p.trim();
            let eq = p.find('=')?;
            (p[..eq].trim() == "sentry_key").then(|| p[eq + 1..].trim().to_string())
        });
        assert_eq!(got, Some("clk_abc".to_string()));
    }

    #[test]
    fn project_mapping_resolves_service_names() {
        let mut c = cfg();
        c.projects.insert("7".into(), "checkout".into());
        assert_eq!(
            resolve_service("7", &c),
            "checkout",
            "explicit mapping wins"
        );
        assert_eq!(
            resolve_service("42", &c),
            "project-42",
            "unmapped numeric id"
        );
        assert_eq!(
            resolve_service("checkout", &c),
            "checkout",
            "non-numeric path used verbatim (hand-rolled clients)"
        );
    }
}
