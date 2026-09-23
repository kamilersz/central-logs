//! Splunk HEC-compatible HTTP ingest — the wire protocol of Docker Engine's
//! `splunk` log driver (`--log-driver=splunk --log-opt splunk-url=https://…`)
//! and any HEC client (architecture §2a).
//!
//! Wire facts (moby `daemon/logger/splunk` + Splunk HEC docs):
//! - `POST /services/collector/event/1.0` (also accepted: `/services/collector/event`).
//!   Body = event JSON objects concatenated back-to-back (batching: the
//!   driver flushes up to 1000 events per request; no delimiters/newlines).
//!   Event shape: `{"time":"1695123456.123456","host":…,"source":…,
//!   "sourcetype":…,"index":…,"event":{"line":…,"source":"stdout",
//!   "tag":…,"attrs":{…}}}` — `time` is a *string* of a float epoch; the
//!   driver's `splunk-format=json` embeds the parsed log line as an object.
//! - `Authorization: Splunk <token>` — validated through the normal API-key
//!   machinery (web::auth accepts the `Splunk` scheme) plus the optional
//!   static `[ingest.splunk_hec].token`.
//! - `OPTIONS` on the event endpoint = driver connection verification
//!   (`splunk-verify-connection`, default true) — must answer 200.
//! - Success = HTTP 200 with `{"text":"Success","code":0}`; the driver only
//!   checks the status. Busy/paused = 503; malformed = 400.
//!
//! Each event becomes one standard envelope record; queryable as
//! `protocol:splunk_hec`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;

use crate::config::{Config, SplunkHecIngestConfig};
use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct SplunkHecState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    pub cfg: Arc<Mutex<Config>>,
    pub backpressure_timeout: std::time::Duration,
    pub ingest_paused: Arc<std::sync::atomic::AtomicBool>,
}

pub fn router(state: SplunkHecState) -> Router {
    Router::new()
        .route("/services/collector/event", post(event_post).options(verify_options))
        .route("/services/collector/event/", post(event_post).options(verify_options))
        .route("/services/collector/event/1.0", post(event_post).options(verify_options))
        .route("/services/collector/raw", post(event_post).options(verify_options))
        .route("/services/collector/raw/1.0", post(event_post).options(verify_options))
        .route("/services/collector/health", get(health))
        .route("/services/collector/health/1.0", get(health))
        .route("/services/collector/ping", get(ping))
        .with_state(state)
}

// =====================================================================
// Handlers
// =====================================================================

async fn verify_options() -> Response {
    // splunk-verify-connection: any 2xx satisfies the driver.
    StatusCode::OK.into_response()
}

async fn ping() -> Response {
    hec_response(StatusCode::OK, "Success", 0)
}

async fn health() -> Response {
    hec_response(StatusCode::OK, "Health check passed", 17)
}

async fn event_post(State(st): State<SplunkHecState>, body: Bytes) -> Response {
    if st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed) {
        return hec_response(StatusCode::SERVICE_UNAVAILABLE, "Server is busy, please try again", 9);
    }
    let Some(events) = parse_events(&body) else {
        return hec_response(StatusCode::BAD_REQUEST, "Invalid data format", 6);
    };
    if events.is_empty() {
        return hec_response(StatusCode::BAD_REQUEST, "No data", 5);
    }
    let cfg = st.cfg.lock().ingest.splunk_hec.clone();
    let records: Vec<RawRecord> = events
        .iter()
        .filter_map(|ev| hec_to_record(ev, &cfg))
        .collect();
    let total = records.len();
    let bytes: usize = records.iter().map(|r| r.raw_len()).sum();
    st.counters.record_n(Protocol::SplunkHec, total, bytes as u64);
    // Batch ack: the FIFO WAL group-commit covers every record in the batch.
    match tokio::time::timeout(
        st.backpressure_timeout,
        st.handle.append_batch_acked(records),
    )
    .await
    {
        Ok(Ok(_)) => hec_response(StatusCode::OK, "Success", 0),
        Ok(Err(e)) => {
            st.counters.record_error_n(Protocol::SplunkHec, total);
            tracing::warn!(?e, "splunk HEC: WAL append failed");
            hec_response(StatusCode::SERVICE_UNAVAILABLE, "Server is busy, please try again", 9)
        }
        Err(_) => {
            st.counters.record_error_n(Protocol::SplunkHec, total);
            hec_response(StatusCode::SERVICE_UNAVAILABLE, "Server is busy, please try again", 9)
        }
    }
}

// =====================================================================
// HEC parsing + mapping
// =====================================================================

/// Split a concatenated JSON body into event objects (HEC batches have no
/// delimiters). `raw` endpoint bodies (plain lines) are handled by the same
/// tolerant path: a body that isn't JSON becomes a single raw event.
fn parse_events(body: &[u8]) -> Option<Vec<serde_json::Value>> {
    if body.is_empty() {
        return None;
    }
    // Trim whitespace around the stream so a trailing newline doesn't
    // surface as a phantom trailing value error.
    let trimmed: &[u8] = {
        let mut t = body;
        while let Some((first, rest)) = t.split_first() {
            if first.is_ascii_whitespace() {
                t = rest;
            } else {
                break;
            }
        }
        let mut end = t.len();
        while end > 0 && t[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        &t[..end]
    };
    if trimmed.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut de = serde_json::Deserializer::from_slice(trimmed).into_iter::<serde_json::Value>();
    loop {
        match de.next() {
            Some(Ok(v)) => out.push(v),
            Some(Err(_)) if !out.is_empty() => {
                // Concatenated-object batches tolerate a trailing garbage
                // byte, but a hard mid-stream parse error means bad data.
                return None;
            }
            Some(Err(_)) => {
                // Whole body failed to parse as JSON → single raw event
                // (HEC `raw` endpoint semantics; the Docker driver's
                // `splunk-format=raw` never posts through this endpoint, but
                // hand-rolled shippers sometimes do).
                return Some(vec![serde_json::Value::String(
                    String::from_utf8_lossy(trimmed).into_owned(),
                )]);
            }
            None => break,
        }
    }
    Some(out)
}

/// Map one HEC event to a WAL record.
pub fn hec_to_record(
    ev: &serde_json::Value,
    _cfg: &SplunkHecIngestConfig,
) -> Option<RawRecord> {
    // `event` may be a string, an object (Docker: line/source/tag/attrs), or
    // any JSON value.
    let (message, mut attrs) = match ev.get("event") {
        Some(serde_json::Value::String(s)) => (s.clone(), serde_json::Map::new()),
        Some(serde_json::Value::Object(o)) => {
            let line = o.get("line").cloned().unwrap_or(serde_json::Value::Null);
            let msg = match &line {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            let mut attrs = serde_json::Map::new();
            for key in ["source", "tag"] {
                if let Some(v) = o.get(key).and_then(|v| v.as_str()) {
                    attrs.insert(key.to_string(), serde_json::Value::String(v.into()));
                }
            }
            if let Some(serde_json::Value::Object(a)) = o.get("attrs") {
                for (k, v) in a {
                    attrs.insert(k.clone(), v.clone());
                }
            }
            (msg, attrs)
        }
        Some(other) if !other.is_null() => (other.to_string(), serde_json::Map::new()),
        _ => (String::new(), serde_json::Map::new()),
    };
    // HEC metadata → attributes (and `host` doubles as source_host).
    for key in ["source", "sourcetype", "index"] {
        if let Some(v) = ev.get(key).and_then(|v| v.as_str()) {
            attrs.insert(format!("hec_{key}"), serde_json::Value::String(v.into()));
        }
    }
    if let Some(serde_json::Value::Object(fields)) = ev.get("fields") {
        for (k, v) in fields {
            attrs.insert(k.clone(), v.clone());
        }
    }
    // Docker's event.source == "stderr" is the only severity signal HEC
    // carries (mirrors the GELF driver's LOG_ERR mapping).
    let level = match attrs.get("source").and_then(|v| v.as_str()) {
        Some("stderr") => "error",
        _ => "info",
    };
    let service = ev
        .get("sourcetype")
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ev.get("fields")
                .and_then(|f| f.get("container_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim_start_matches('/').to_string())
        });

    let mut out = serde_json::Map::new();
    out.insert("message".into(), serde_json::Value::String(message));
    out.insert("level".into(), serde_json::Value::String(level.into()));
    if let Some(svc) = service {
        out.insert("service".into(), serde_json::Value::String(svc));
    }
    match hec_time(ev) {
        Some(ts) => {
            out.insert("ts".into(), serde_json::Value::String(ts.to_rfc3339()));
        }
        None => {
            out.insert("ts".into(), serde_json::Value::String(chrono::Utc::now().to_rfc3339()));
        }
    }
    if let Some(host) = ev.get("host").and_then(|v| v.as_str()) {
        out.insert("host".into(), serde_json::Value::String(host.into()));
    }
    // Extras at top level → parser turns them into the attributes residue.
    for (k, val) in attrs {
        out.insert(k, val);
    }

    let raw = serde_json::to_vec(&out).ok()?;
    Some(RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: String::new(),
        protocol: Protocol::SplunkHec,
        raw: bytes::Bytes::from(raw),
    })
}

/// HEC `time`: a string of a float epoch (Docker driver always sends 6
/// decimals), or a bare number. Defaults to now.
pub fn hec_time(ev: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = match ev.get("time") {
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok()?,
        Some(serde_json::Value::Number(n)) => n.as_f64()?,
        _ => return None,
    };
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let s = secs.trunc() as i64;
    let n = ((secs.fract().abs()) * 1e9) as u32;
    chrono::DateTime::from_timestamp(s, n)
}

fn hec_response(status: StatusCode, text: &str, code: u16) -> Response {
    let body = serde_json::json!({"text": text, "code": code});
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        Json(body),
    )
        .into_response()
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_concatenated_events() {
        let body = br#"{"time":"100.5","event":"a"}{"time":"200","event":"b"}
{"event":"c"}"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0]["event"], "a");
        assert_eq!(evs[2]["event"], "c");
    }

    #[test]
    fn non_json_body_becomes_single_raw_event() {
        let evs = parse_events(b"plain line\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0], serde_json::json!("plain line"));
        assert!(parse_events(b"").is_none());
    }

    #[test]
    fn docker_hec_event_maps() {
        let ev = serde_json::json!({
            "time": "1760000000.123456",
            "host": "docker-host",
            "source": "stdout",
            "sourcetype": "docker:web-1",
            "index": "logs",
            "event": {"line": "hello\n", "source": "stdout", "tag": "abc123def456",
                      "attrs": {"env": "prod"}}
        });
        let rec = hec_to_record(&ev, &SplunkHecIngestConfig::default()).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["message"], "hello\n");
        assert_eq!(env["level"], "info");
        assert_eq!(env["service"], "docker:web-1");
        assert_eq!(env["host"], "docker-host");
        let ts = chrono::DateTime::parse_from_rfc3339(env["ts"].as_str().unwrap()).unwrap();
        assert_eq!(ts.timestamp_millis(), 1_760_000_000_123);
        assert_eq!(env["tag"], "abc123def456");
        assert_eq!(env["env"], "prod");
        assert_eq!(env["hec_index"], "logs");
    }

    #[test]
    fn stderr_event_marks_error_level() {
        let ev = serde_json::json!({
            "time": 1760000000,
            "event": {"line": "boom", "source": "stderr"}
        });
        let rec = hec_to_record(&ev, &SplunkHecIngestConfig::default()).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["level"], "error");
    }

    #[test]
    fn string_event_and_missing_time_default() {
        let ev = serde_json::json!({"event": "just a string"});
        let rec = hec_to_record(&ev, &SplunkHecIngestConfig::default()).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["message"], "just a string");
        let ts = chrono::DateTime::parse_from_rfc3339(env["ts"].as_str().unwrap()).unwrap();
        let now = chrono::Utc::now();
        assert!((now - ts.with_timezone(&chrono::Utc)).num_seconds().abs() < 5);
    }

    #[test]
    fn time_string_and_number_forms() {
        let s = hec_time(&serde_json::json!({"time": "10.25"})).unwrap();
        assert_eq!(s.timestamp_millis(), 10_250);
        let n = hec_time(&serde_json::json!({"time": 5})).unwrap();
        assert_eq!(n.timestamp(), 5);
        assert!(hec_time(&serde_json::json!({})).is_none());
        assert!(hec_time(&serde_json::json!({"time": "x"})).is_none());
    }
}
