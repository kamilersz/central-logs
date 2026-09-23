//! Kubernetes API-server audit ingest — the webhook audit backend
//! (`kube-apiserver --audit-webhook-config-file`, architecture §2a).
//!
//! Wire facts (k8s `audit.k8s.io/v1` API):
//! - The apiserver POSTs batches of
//!   `{"apiVersion":"audit.k8s.io/v1","kind":"EventList","metadata":{},
//!     "items":[Event,…]}` with `Content-Type: application/json`. Batches
//!   hold up to ~400 events (`--audit-webhook-batch-max-size`); failed
//!   POSTs retry with exponential backoff, so a 2xx must only be returned
//!   after the events are durably accepted (the WAL append acks).
//! - One `Event` per (request, stage): `auditID`, `stage`
//!   (RequestReceived | ResponseStarted | ResponseComplete | Panic),
//!   `level` (audit verbosity), `requestURI`, `verb`, `user{username,uid,
//!   groups}`, `sourceIPs`, `userAgent`, `objectRef{resource,namespace,
//!   name,apiGroup,apiVersion,subresource}`, `responseStatus{code,reason,
//!   message}`, `requestObject`/`responseObject` (RequestResponse level),
//!   `annotations`, `stageTimestamp`, `requestReceivedTimestamp`.
//!
//! Mapping: message = `verb objectRef → code`, service = `kube-apiserver`,
//! `auditID` → `trace_id`, Panic stage → fatal, 5xx → error, 4xx → warn.
//! Full event detail rides in `attributes`; queryable as
//! `protocol:k8s_audit`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use crate::config::K8sAuditIngestConfig;
use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct K8sAuditState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    pub cfg: Arc<parking_lot::Mutex<K8sAuditIngestConfig>>,
    pub backpressure_timeout: Duration,
    pub ingest_paused: Arc<std::sync::atomic::AtomicBool>,
}

pub fn router(state: K8sAuditState) -> Router {
    Router::new()
        .route("/ingest/kubernetes/audit", post(audit_post))
        .route("/ingest/kubernetes/audit/", post(audit_post))
        .with_state(state)
}

async fn audit_post(State(st): State<K8sAuditState>, body: Bytes) -> Response {
    if st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed) {
        return status_response(StatusCode::SERVICE_UNAVAILABLE, "ingest paused");
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return status_response(StatusCode::BAD_REQUEST, "invalid audit JSON");
    };
    let omit_stages = st.cfg.lock().omit_stages.clone();
    let events = extract_events(&v);
    if events.is_empty() {
        // An EventList with zero items is a valid (empty) batch.
        return status_response(StatusCode::OK, "Success");
    }
    let records: Vec<RawRecord> = events
        .iter()
        .filter(|ev| {
            ev.get("stage")
        .and_then(|s| s.as_str())
                .map(|s| !omit_stages.iter().any(|o| o == s))
                .unwrap_or(true)
        })
        .filter_map(|ev| audit_event_to_record(ev))
        .collect();
    if records.is_empty() {
        return status_response(StatusCode::OK, "Success");
    }
    let total = records.len();
    let bytes: u64 = records.iter().map(|r| r.raw_len() as u64).sum();
    st.counters.record_n(Protocol::K8sAudit, total, bytes);
    match tokio::time::timeout(st.backpressure_timeout, st.handle.append_batch_acked(records)).await
    {
        Ok(Ok(_)) => status_response(StatusCode::OK, "Success"),
        Ok(Err(e)) => {
            st.counters.record_error_n(Protocol::K8sAudit, total);
            tracing::warn!(?e, "k8s audit: WAL append failed");
            status_response(StatusCode::SERVICE_UNAVAILABLE, "ingest failed")
        }
        Err(_) => {
            st.counters.record_error_n(Protocol::K8sAudit, total);
            status_response(StatusCode::SERVICE_UNAVAILABLE, "ingest backpressure")
        }
    }
}

fn status_response(status: StatusCode, reason: &str) -> Response {
    // k8s Status object shape — the apiserver only checks the HTTP code.
    (
        status,
        Json(serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": reason,
        })),
    )
        .into_response()
}

/// Accept EventList / single Event / bare array — webhook implementations in
/// the wild (and managed offerings) vary slightly.
pub fn extract_events(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
        return items.iter().collect();
    }
    if let Some(arr) = v.as_array() {
        return arr.iter().collect();
    }
    if v.get("auditID").is_some() || v.get("verb").is_some() {
        return vec![v];
    }
    Vec::new()
}

/// Map one audit Event to a WAL record.
pub fn audit_event_to_record(ev: &serde_json::Value) -> Option<RawRecord> {
    let verb = ev.get("verb").and_then(|s| s.as_str()).unwrap_or("unknown");
    let stage = ev.get("stage").and_then(|s| s.as_str()).unwrap_or("");
    let (code, code_reason) = match ev.get("responseStatus") {
        Some(st) => (
            st.get("code").and_then(|c| c.as_i64()),
            st.get("reason").and_then(|r| r.as_str()).unwrap_or(""),
        ),
        None => (None, ""),
    };
    let target = ev
        .get("objectRef")
        .filter(|o| o.is_object())
        .map(|o| {
            let group = o.get("apiGroup").and_then(|g| g.as_str()).unwrap_or("");
            let resource = o.get("resource").and_then(|r| r.as_str()).unwrap_or("");
            let ns = o.get("namespace").and_then(|n| n.as_str()).unwrap_or("");
            let name = o.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let mut t = String::new();
            if !group.is_empty() {
                t.push_str(group);
                t.push('/');
            }
            t.push_str(resource);
            if !ns.is_empty() {
                t.push_str(&format!(" {ns}/"));
            } else if !name.is_empty() {
                t.push(' ');
            }
            t.push_str(name);
            t
        })
        .unwrap_or_else(|| {
            ev.get("requestURI")
                .and_then(|u| u.as_str())
                .unwrap_or("?")
                .to_string()
        });
    let message = match code {
        Some(c) => format!("{verb} {target} → {c}"),
        None => format!("{verb} {target}"),
    };
    let message = if stage == "ResponseStarted" {
        format!("{message} (streaming response started)")
    } else {
        message
    };

    let level = if stage == "Panic" {
        "fatal"
    } else {
        match code {
            Some(c) if c >= 500 => "error",
            Some(c) if (400..500).contains(&c) => "warn",
            _ => "info",
        }
    };

    // Everything not lifted above rides in attributes.
    let mut attrs = serde_json::Map::new();
    const ATTR_KEYS: &[&str] = &[
        "stage",
        "requestURI",
        "userAgent",
        "user",
        "impersonatedUser",
        "sourceIPs",
        "objectRef",
        "responseStatus",
        "requestObject",
        "responseObject",
        "annotations",
        "authenticationMetadata",
    ];
    for key in ATTR_KEYS {
        if let Some(v) = ev.get(*key) {
            if !v.is_null() {
                attrs.insert((*key).to_string(), v.clone());
            }
        }
    }
    // The audit POLICY level (verbosity) — renamed so the parser can't
    // mistake it for the row's severity column.
    if let Some(v) = ev.get("level") {
        if !v.is_null() {
            attrs.insert("audit_level".into(), v.clone());
        }
    }

    let mut out = serde_json::Map::new();
    out.insert("message".into(), serde_json::Value::String(message));
    out.insert("level".into(), serde_json::Value::String(level.into()));
    out.insert(
        "service".into(),
        serde_json::Value::String("kube-apiserver".into()),
    );
    // stageTimestamp = when this stage completed; else the receive time.
    let ts = ev
        .get("stageTimestamp")
        .and_then(|t| t.as_str())
        .or_else(|| ev.get("requestReceivedTimestamp").and_then(|t| t.as_str()))
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    out.insert("ts".into(), serde_json::Value::String(ts));
    // auditID is a per-request correlation id → trace_id.
    if let Some(id) = ev.get("auditID").and_then(|i| i.as_str()) {
        out.insert("trace_id".into(), serde_json::Value::String(id.into()));
    }
    if let Some(ips) = ev.get("sourceIPs").and_then(|i| i.as_array()) {
        if let Some(last) = ips.last().and_then(|i| i.as_str()) {
            out.insert("host".into(), serde_json::Value::String(last.into()));
        }
    }
    if !code_reason.is_empty() {
        attrs.insert("response_reason".into(), serde_json::Value::String(code_reason.into()));
    }
    // Extras at top level → parser turns them into the attributes residue.
    for (k, val) in attrs {
        out.insert(k, val);
    }

    let raw = serde_json::to_vec(&out).ok()?;
    Some(RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: String::new(),
        protocol: Protocol::K8sAudit,
        raw: bytes::Bytes::from(raw),
    })
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> serde_json::Value {
        serde_json::json!({
            "auditID": "d6f2b1f0-1111-2222-3333-444455556666",
            "stage": "ResponseComplete",
            "level": "Metadata",
            "timestamp": "2026-09-23T12:00:00.000000Z",
            "verb": "create",
            "requestURI": "/api/v1/namespaces/default/pods",
            "user": {"username": "kubernetes-admin", "uid": "u-1", "groups": ["system:masters"]},
            "sourceIPs": ["10.0.0.1", "192.168.1.5"],
            "userAgent": "kubectl/v1.30",
            "objectRef": {"resource": "pods", "namespace": "default", "name": "nginx",
                          "apiVersion": "v1"},
            "responseStatus": {"metadata": {}, "code": 201, "status": "Success"},
            "stageTimestamp": "2026-09-23T12:00:00.123456Z",
            "requestReceivedTimestamp": "2026-09-23T12:00:00.100000Z"
        })
    }

    #[test]
    fn extracts_eventlist_and_single_event() {
        let list = serde_json::json!({
            "apiVersion": "audit.k8s.io/v1",
            "kind": "EventList",
            "metadata": {"resourceVersion": "1"},
            "items": [sample_event(), sample_event()]
        });
        assert_eq!(extract_events(&list).len(), 2);
        let single = serde_json::json!({"auditID": "x", "verb": "get"});
        assert_eq!(extract_events(&single).len(), 1);
        assert!(extract_events(&serde_json::json!({"foo": 1})).is_empty());
    }

    #[test]
    fn event_maps_to_row() {
        let rec = audit_event_to_record(&sample_event()).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["message"], "create pods default/nginx → 201");
        assert_eq!(env["level"], "info");
        assert_eq!(env["service"], "kube-apiserver");
        assert_eq!(env["trace_id"], "d6f2b1f0-1111-2222-3333-444455556666");
        assert_eq!(env["host"], "192.168.1.5", "last sourceIP becomes source host");
        let ts = chrono::DateTime::parse_from_rfc3339(env["ts"].as_str().unwrap()).unwrap();
        assert_eq!(ts.timestamp_micros(), 1_790_164_800_123_456);
        assert_eq!(env["stage"], "ResponseComplete");
        assert_eq!(env["user"]["username"], "kubernetes-admin");
        assert_eq!(env["objectRef"]["resource"], "pods");
        assert_eq!(env["responseStatus"]["code"], 201);
        assert_eq!(env["audit_level"], "Metadata");
    }

    #[test]
    fn severity_by_status_code_and_stage() {
        let mut ev = sample_event();
        ev["stage"] = "Panic".into();
        ev["responseStatus"] = serde_json::json!({"code": 500, "reason": "InternalError"});
        let rec = audit_event_to_record(&ev).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["level"], "fatal", "Panic stage wins");

        let mut ev = sample_event();
        ev["responseStatus"] = serde_json::json!({"code": 403, "reason": "Forbidden"});
        let env: serde_json::Value =
            serde_json::from_slice(&audit_event_to_record(&ev).unwrap().raw).unwrap();
        assert_eq!(env["level"], "warn");

        let mut ev = sample_event();
        ev["responseStatus"] = serde_json::json!({"code": 503});
        let env: serde_json::Value =
            serde_json::from_slice(&audit_event_to_record(&ev).unwrap().raw).unwrap();
        assert_eq!(env["level"], "error");
        assert!(env["message"].as_str().unwrap().contains("→ 503"));
    }

    #[test]
    fn api_group_namespaced_target_rendering() {
        let mut ev = sample_event();
        ev["verb"] = "list".into();
        ev["objectRef"] = serde_json::json!({
            "resource": "deployments", "namespace": "prod",
            "apiGroup": "apps", "apiVersion": "v1"
        });
        ev["responseStatus"] = serde_json::json!({"code": 200});
        let env: serde_json::Value =
            serde_json::from_slice(&audit_event_to_record(&ev).unwrap().raw).unwrap();
        assert_eq!(env["message"], "list apps/deployments prod/ → 200");

        let mut ev = sample_event();
        ev["objectRef"] = serde_json::Value::Null;
        ev["verb"] = serde_json::json!("get");
        ev["requestURI"] = serde_json::json!("/healthz");
        ev["responseStatus"] = serde_json::json!({"code": 200});
        let env: serde_json::Value =
            serde_json::from_slice(&audit_event_to_record(&ev).unwrap().raw).unwrap();
        assert_eq!(env["message"], "get /healthz → 200");
    }
}
