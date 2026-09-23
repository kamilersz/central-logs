//! Built-in pull collectors: follow container/pod log streams from the
//! Docker Engine API (`docker`) and the Kubernetes API (`kubernetes`) and
//! feed them into the normal WAL pipeline — for hosts where configuring a
//! push log driver per container is impractical.
//!
//! Both collectors share the same shape (architecture §2a, pull side):
//! a discovery loop re-lists workloads every `refresh_secs`, spawning one
//! follow stream per new container/pod; streams end (and get re-discovered)
//! when the workload restarts. Records are normalized into the standard
//! envelope and pushed into the WAL unacked (backpressure = drop-oldest at
//! the channel, matching UDP semantics for volume streams).

pub mod docker;
pub mod kubernetes;

use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

/// Push one normalized envelope record into the WAL (fire-and-forget).
pub(crate) async fn push_record(
    handle: &InsertHandle,
    counters: &crate::insert::counters::InsertCountersRef,
    protocol: Protocol,
    envelope: serde_json::Value,
) {
    let raw = serde_json::to_vec(&envelope).unwrap_or_else(|_| b"{}".to_vec());
    let rec = RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: String::new(),
        protocol,
        raw: bytes::Bytes::from(raw),
    };
    counters.record(protocol, rec.raw_len());
    let ok =
        tokio::time::timeout(std::time::Duration::from_millis(250), handle.append_unacked(rec))
            .await;
    if !matches!(ok, Ok(Ok(()))) {
        counters.record_error(protocol);
    }
}

/// Tiny glob matcher (`*`, `?`) used by include/exclude filters — no
/// wildcard crate; the patterns are short and this is compile-free.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
            (Some(b'?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Parse a leading RFC3339 timestamp from a log line (Docker
/// `timestamps=true` and kubelet log format both prefix RFC3339-ish stamps),
/// returning `(ts, rest_of_line)`.
pub(crate) fn split_timestamp_prefix(line: &str) -> (Option<chrono::DateTime<chrono::Utc>>, &str) {
    if let Some(sp) = line.find(' ') {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&line[..sp]) {
            return (Some(dt.with_timezone(&chrono::Utc)), &line[sp + 1..]);
        }
    }
    (None, line)
}

/// Wrap the per-line fields into the standard envelope JSON.
pub(crate) fn log_envelope(
    message: String,
    level: &str,
    service: String,
    ts: Option<chrono::DateTime<chrono::Utc>>,
    attrs: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("message".into(), serde_json::Value::String(message));
    out.insert("level".into(), serde_json::Value::String(level.into()));
    out.insert("service".into(), serde_json::Value::String(service));
    out.insert(
        "ts".into(),
        serde_json::Value::String(
            ts.unwrap_or_else(chrono::Utc::now).to_rfc3339(),
        ),
    );
    // Extras at top level → parser turns them into the attributes residue.
    for (k, val) in attrs {
        out.insert(k, val);
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_flattens_extras() {
        let mut attrs = serde_json::Map::new();
        attrs.insert("pod".into(), serde_json::json!("web-1"));
        let env = log_envelope("hi".into(), "info", "web-1".into(), None, attrs);
        assert_eq!(env["message"], "hi");
        assert_eq!(env["pod"], "web-1");
        assert!(env.get("attributes").is_none(), "no double nesting");
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match("web-*", "web-1"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("db?", "db1"));
        assert!(!glob_match("web-*", "api-1"));
        assert!(!glob_match("exact", "exactly"));
        assert!(glob_match("exact", "exact"));
    }

    #[test]
    fn timestamp_prefix_parsed() {
        let line = "2026-09-23T12:00:00.123456789Z hello world";
        let (ts, rest) = split_timestamp_prefix(line);
        assert_eq!(ts.unwrap().timestamp_millis(), 1_790_164_800_123);
        assert_eq!(rest, "hello world");
        let (ts, rest) = split_timestamp_prefix("no timestamp here");
        assert!(ts.is_none());
        assert_eq!(rest, "no timestamp here");
    }
}
