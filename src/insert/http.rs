//! HTTP insert endpoints (architecture §2a).
//!
//! - `POST /v1/logs` (alias `/v1/logs/bulk`) — the original JSON array /
//!   single-object / NDJSON contract, **plus** OTLP logs when the request is
//!   protobuf or an OTLP/JSON body (`{"resourceLogs":[...]}`).
//! - `POST /v1/traces` — OTLP spans (protobuf or JSON).
//! - `POST /v1/metrics` — OTLP metric points (protobuf or JSON).
//!
//! Every record is wrapped into a [`RawRecord`] and pushed into the WAL
//! channel; the HTTP response is returned only after the WAL has fsynced.
//! OTLP responses use the request's own wire encoding (a protobuf client must
//! not be handed a JSON/HTML body, or its parser fails with "unexpected wire
//! type").

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Serialize;
use tracing::warn;

use crate::insert::counters::InsertCountersRef;
use crate::insert::otlp::{self, Wire};
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct InsertState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    pub backpressure_timeout: Duration,
    /// Peer identifier resolved from request headers / socket (best-effort).
    /// For v1 we keep this empty unless a proxy header is provided.
    pub peer_header: Option<String>,
    /// Set by the WAL-cap monitor (`retention.wal_max_bytes`): inserts are
    /// refused with 503 until the WAL shrinks below the cap.
    pub ingest_paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn router(state: InsertState) -> Router {
    Router::new()
        .route("/v1/logs", post(handle_post_logs))
        .route("/v1/logs/bulk", post(handle_post_logs))
        .route("/v1/traces", post(handle_post_traces))
        .route("/v1/metrics", post(handle_post_metrics))
        .with_state(state)
}

#[derive(Debug, Serialize)]
pub struct InsertResponse {
    pub accepted: usize,
    pub rejected: usize,
    pub errors: Vec<String>,
}

impl IntoResponse for InsertResponse {
    fn into_response(self) -> Response {
        let code = if self.accepted > 0 {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        };
        (code, axum::Json(self)).into_response()
    }
}

struct AppendOutcome {
    accepted: usize,
    rejected: usize,
    errors: Vec<String>,
}

fn ingest_paused(st: &InsertState) -> bool {
    st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed)
}

fn paused_generic_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "accepted": 0, "rejected": 0,
            "errors": ["wal cap reached (retention.wal_max_bytes); ingest paused"]
        })),
    )
        .into_response()
}

/// OTLP-shaped response: content type mirrors the request wire encoding, and
/// the body is a valid (possibly empty) `Export*ServiceResponse`.
fn otlp_response(wire: Wire, status: StatusCode) -> Response {
    let (ct, body) = otlp::success_body(wire);
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(ct))],
        body.to_vec(),
    )
        .into_response()
}

/// Push records into the WAL with the configured backpressure timeout.
async fn append_all(st: &InsertState, records: Vec<RawRecord>) -> AppendOutcome {
    let mut out = AppendOutcome {
        accepted: 0,
        rejected: 0,
        errors: Vec::new(),
    };
    for rec in records {
        let proto = rec.protocol;
        let bytes = rec.raw_len();
        st.counters.record(proto, bytes);
        match tokio::time::timeout(st.backpressure_timeout, st.handle.append_acked(rec)).await {
            Ok(Ok(())) => out.accepted += 1,
            Ok(Err(crate::Error::ChannelClosed)) => {
                out.rejected += 1;
                out.errors.push("wal channel closed".into());
                st.counters.record_error(proto);
                break;
            }
            Ok(Err(e)) => {
                out.rejected += 1;
                out.errors.push(format!("{e}"));
                st.counters.record_error(proto);
            }
            Err(_) => {
                out.rejected += 1;
                out.errors.push("backpressure timeout".into());
                st.counters.record_error(proto);
            }
        }
    }
    out
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn is_otlp_protobuf(ct: &str) -> bool {
    ct.to_ascii_lowercase().contains("protobuf")
}

// =====================================================================
// /v1/logs — generic JSON/NDJSON, or OTLP logs
// =====================================================================

async fn handle_post_logs(
    State(st): State<InsertState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if ingest_paused(&st) {
        return paused_generic_response();
    }
    let ct = content_type(&headers);
    // OTLP detection: explicit protobuf content type, or OTLP/JSON shape.
    let otlp_wire = if is_otlp_protobuf(&ct) {
        Some(Wire::Protobuf)
    } else if ct.to_ascii_lowercase().contains("json")
        && otlp::json_body_looks_like_otlp(&body)
    {
        Some(Wire::Json)
    } else {
        None
    };
    if let Some(wire) = otlp_wire {
        return handle_otlp_logs(&st, wire, &body).await;
    }
    handle_generic_logs(&st, &body).await.into_response()
}

async fn handle_generic_logs(st: &InsertState, body: &[u8]) -> InsertResponse {
    let now = chrono::Utc::now();
    let peer = st.peer_header.clone().unwrap_or_default();
    let records = parse_body(body, now, &peer);
    let outcome = append_all(st, records).await;
    if outcome.rejected > 0 && outcome.accepted == 0 {
        warn!(
            accepted = outcome.accepted,
            rejected = outcome.rejected,
            "all /v1/logs inserts rejected"
        );
    }
    InsertResponse {
        accepted: outcome.accepted,
        rejected: outcome.rejected,
        errors: outcome.errors,
    }
}

async fn handle_otlp_logs(st: &InsertState, wire: Wire, body: &[u8]) -> Response {
    let req = match otlp::decode_logs(wire, body) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, "otlp/logs decode failed");
            return otlp_response(wire, StatusCode::BAD_REQUEST);
        }
    };
    let records = otlp::logs_to_records(req, chrono::Utc::now(), &st.peer_header.clone().unwrap_or_default());
    finish_otlp(st, wire, records).await
}

// =====================================================================
// /v1/traces
// =====================================================================

async fn handle_post_traces(
    State(st): State<InsertState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let wire = otlp::detect_wire(&content_type(&headers));
    if ingest_paused(&st) {
        return otlp_response(wire, StatusCode::SERVICE_UNAVAILABLE);
    }
    let req = match otlp::decode_traces(wire, &body) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, "otlp/traces decode failed");
            return otlp_response(wire, StatusCode::BAD_REQUEST);
        }
    };
    let records = otlp::traces_to_records(req, chrono::Utc::now(), &st.peer_header.clone().unwrap_or_default());
    finish_otlp(&st, wire, records).await
}

// =====================================================================
// /v1/metrics
// =====================================================================

async fn handle_post_metrics(
    State(st): State<InsertState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let wire = otlp::detect_wire(&content_type(&headers));
    if ingest_paused(&st) {
        return otlp_response(wire, StatusCode::SERVICE_UNAVAILABLE);
    }
    let req = match otlp::decode_metrics(wire, &body) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, "otlp/metrics decode failed");
            return otlp_response(wire, StatusCode::BAD_REQUEST);
        }
    };
    let records = otlp::metrics_to_records(req, chrono::Utc::now(), &st.peer_header.clone().unwrap_or_default());
    finish_otlp(&st, wire, records).await
}

async fn finish_otlp(st: &InsertState, wire: Wire, records: Vec<RawRecord>) -> Response {
    let outcome = append_all(st, records).await;
    if outcome.rejected > 0 {
        warn!(
            accepted = outcome.accepted,
            rejected = outcome.rejected,
            errors = ?outcome.errors,
            "otlp insert partially rejected"
        );
    }
    if outcome.accepted == 0 && outcome.rejected > 0 {
        // All rejected — signal a retryable failure (SDKs back off and retry).
        otlp_response(wire, StatusCode::SERVICE_UNAVAILABLE)
    } else {
        otlp_response(wire, StatusCode::OK)
    }
}

/// Parse the incoming request body into one or more [`RawRecord`]s.
///
/// Supported formats:
/// - JSON array of objects
/// - NDJSON (newline-delimited objects)
/// - A single JSON object
/// - Otherwise: each non-empty line becomes one opaque record
fn parse_body(body: &[u8], now: chrono::DateTime<chrono::Utc>, peer: &str) -> Vec<RawRecord> {
    if let Ok(slice) = std::str::from_utf8(body) {
        let trimmed = slice.trim_start();
        if trimmed.starts_with('[') {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
                return arr
                    .into_iter()
                    .filter_map(|v| record_from_json(v, now, peer))
                    .collect();
            }
        } else if trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(r) = record_from_json(v, now, peer) {
                    return vec![r];
                }
            }
        }
        return slice
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    record_from_json(v, now, peer)
                } else {
                    Some(RawRecord {
                        receive_ts: now,
                        source_addr: peer.to_string(),
                        protocol: Protocol::HttpJson,
                        raw: Bytes::copy_from_slice(line.as_bytes()),
                    })
                }
            })
            .collect();
    }
    // Non-UTF-8: treat the whole thing as one opaque record.
    vec![RawRecord {
        receive_ts: now,
        source_addr: peer.to_string(),
        protocol: Protocol::HttpJson,
        raw: Bytes::copy_from_slice(body),
    }]
}

fn record_from_json(
    v: serde_json::Value,
    now: chrono::DateTime<chrono::Utc>,
    peer: &str,
) -> Option<RawRecord> {
    if v.is_null() {
        return None;
    }
    Some(RawRecord {
        receive_ts: now,
        source_addr: peer.to_string(),
        protocol: Protocol::HttpJson,
        raw: Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_protobuf_content_type() {
        assert!(is_otlp_protobuf("application/x-protobuf"));
        assert!(is_otlp_protobuf("application/protobuf"));
        assert!(!is_otlp_protobuf("application/json"));
        assert!(!is_otlp_protobuf("application/x-ndjson"));
    }
}
