//! Fluentd forward-protocol ingest — the wire protocol of Docker Engine's
//! `fluentd` log driver (`--log-driver=fluentd --log-opt fluentd-address=…`)
//! and fluentd/fluent-bit `forward` outputs (architecture §2a).
//!
//! Wire facts (moby `daemon/logger/fluentd` + fluent-logger-golang):
//! - MessagePack over TCP, one value per message — the driver always sends
//!   **message mode**: `[tag, time, record, option]` (4 elements; some
//!   shippers send 3-element message mode or packed-forward arrays — both
//!   accepted).
//! - `time` is unix seconds (msgpack int) or ext type 0 ("EventTime",
//!   4-byte BE seconds + 4-byte BE nanoseconds) with
//!   `fluentd-sub-second-precision=true`.
//! - Docker's `record`: `container_id`, `container_name` (with a leading
//!   `/`), `source` ("stdout"/"stderr"), `log` (the line); container label/
//!   env extras are merged as top-level string keys; long-line splits add
//!   `partial_message`/`partial_id`/`partial_ordinal`/`partial_last`.
//! - `option` carries `chunk` (base64 ack id) with `fluentd-request-ack=true`;
//!   the client then expects an ack reply `{"ack":"<id>"}` on the socket.
//!
//! Records are normalized into the standard envelope and pushed into the
//! WAL; queryable as `protocol:fluentd`.

use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::insert::counters::InsertCountersRef;
use crate::insert::msgpack::{try_decode, MsgValue};
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct FluentdState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    /// Reply acks when the client requests them (`fluentd-request-ack`).
    pub ack: bool,
}

pub async fn spawn_fluentd_listener(
    bind: Option<&str>,
    state: FluentdState,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let bind = bind?;
    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(bind, ?e, "fluentd TCP bind failed; disabling");
            return None;
        }
    };
    tracing::info!(bind, "fluentd (forward protocol) TCP listener bound");
    let st = state.clone();
    Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                res = listener.accept() => {
                    let (stream, peer) = match res {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(?e, "fluentd TCP accept failed");
                            continue;
                        }
                    };
                    let st = st.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_conn(stream, peer.to_string(), st).await
                        {
                            tracing::debug!(?e, "fluentd conn handler exited");
                        }
                    });
                }
            }
        }
    }))
}

async fn handle_conn(
    stream: TcpStream,
    peer: String,
    state: FluentdState,
) -> std::io::Result<()> {
    // The forward protocol has no outer framing — msgpack values are
    // self-delimiting — so bytes accumulate in a buffer and complete values
    // are decoded as they become available.
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    loop {
        match try_decode(&buf) {
            Ok(Some((value, used))) => {
                buf.drain(..used);
                process_entry(value, &state, &mut writer).await?;
            }
            Ok(None) => {
                let n = reader.read(&mut tmp).await?;
                if n == 0 {
                    if !buf.is_empty() {
                        tracing::debug!(peer, bytes = buf.len(), "fluentd: truncated tail dropped");
                    }
                    return Ok(()); // clean close
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => {
                tracing::debug!(peer, %e, "fluentd: malformed stream, closing connection");
                state.counters.record_error(Protocol::Fluentd);
                return Ok(());
            }
        }
    }
}

/// Decode + append one forward-protocol entry; reply the ack when the
/// client requested one.
async fn process_entry(
    value: MsgValue,
    state: &FluentdState,
    writer: &mut tokio::io::WriteHalf<TcpStream>,
) -> std::io::Result<()> {
    match forward_to_record(&value) {
        Ok(Some((rec, ack_id))) => {
            state.counters.record(Protocol::Fluentd, rec.raw_len());
            let ok =
                tokio::time::timeout(Duration::from_millis(250), state.handle.append_unacked(rec))
                    .await;
            if !matches!(ok, Ok(Ok(()))) {
                state.counters.record_error(Protocol::Fluentd);
            }
            if let Some(id) = ack_id {
                // fluentd-request-ack: write the JSON ack on the same
                // socket, newline-terminated (fluentd accepts JSON or
                // msgpack; JSON is the documented form).
                let line = format!("{{\"ack\":\"{id}\"}}\n");
                writer.write_all(line.as_bytes()).await?;
                writer.flush().await?;
            }
        }
        Ok(None) => {} // non-log entry (e.g. option-only ping); ignore
        Err(e) => {
            tracing::debug!(%e, "fluentd: malformed entry dropped");
            state.counters.record_error(Protocol::Fluentd);
        }
    }
    Ok(())
}

/// Decode one forward-protocol entry into a WAL record.
///
/// Accepts:
/// - `[tag, time, record]` / `[tag, time, record, option]` (message mode)
/// - `[tag, [[time, record], …]]` / `[…, option]` (forward/packed-forward)
///
/// Returns `(record, ack_id)` — ack id present when the entry's option map
/// carries a `chunk` value.
pub fn forward_to_record(value: &MsgValue) -> Result<Option<(RawRecord, Option<String>)>, String> {
    let MsgValue::Array(parts) = value else {
        return Err("entry is not an array".into());
    };
    match parts.len() {
        // message mode: [tag, time, record, (option)]
        3 | 4 if matches!(parts.get(2), Some(MsgValue::Map(_))) => {
            let tag = parts[0].to_string_lossy().unwrap_or_default();
            let ts = msgpack_time_to_rfc3339(&parts[1]);
            let record = &parts[2];
            let option = if parts.len() == 4 { &parts[3] } else { &MsgValue::Nil };
            let (rec, ack) = build_record(&tag, ts, record, option)?;
            Ok(Some((rec, ack)))
        }
        // forward / packed-forward: [tag, entries, (option)]
        2 | 3 if matches!(parts.get(1), Some(MsgValue::Array(_))) => {
            let tag = parts[0].to_string_lossy().unwrap_or_default();
            let option = if parts.len() == 3 { &parts[2] } else { &MsgValue::Nil };
            let ack = option_ack(option);
            let MsgValue::Array(entries) = &parts[1] else {
                return Err("entries must be an array".into());
            };
            // Forward-mode entries arrive as [time, record] pairs (or
            // [time, record, option] in packed-forward). Each becomes its
            // own WAL record; only the first carries the ack (a real server
            // acks the whole batch — we ack per-record, the first is enough
            // for this connection's single entry-per-call path).
            let mut first: Option<(RawRecord, Option<String>)> = None;
            for entry in entries {
                let MsgValue::Array(pair) = entry else {
                    return Err("forward entry must be [time, record]".into());
                };
                let Some(time_v) = pair.first() else {
                    continue;
                };
                let Some(record) = pair.get(1) else {
                    continue;
                };
                let ts = msgpack_time_to_rfc3339(time_v);
                let (rec, _) = build_record(&tag, ts, record, &MsgValue::Nil)?;
                first.get_or_insert((rec, ack.clone()));
            }
            Ok(first)
        }
        other => Err(format!("unsupported entry shape ({other} elements)")),
    }
}

fn build_record(
    tag: &str,
    ts: Option<String>,
    record: &MsgValue,
    option: &MsgValue,
) -> Result<(RawRecord, Option<String>), String> {
    let MsgValue::Map(map) = record else {
        return Err("record must be a map".into());
    };
    let get = |key: &str| -> Option<&MsgValue> {
        map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    };
    // Docker: `log` carries the line. Generic forward shippers use
    // `message`/`msg` — accept all three.
    let message = ["log", "message", "msg"]
        .iter()
        .find_map(|k| get(k).and_then(|v| v.to_string_lossy()))
        .unwrap_or_default();

    let source = get("source").and_then(MsgValue::to_string_lossy);
    let level = match source.as_deref() {
        Some("stderr") => "error".to_string(),
        _ => "info".to_string(),
    };
    // Docker's container_name carries a leading `/` (names.go stores it raw).
    let service = get("container_name")
        .and_then(MsgValue::to_string_lossy)
        .map(|s| s.trim_start_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if tag.is_empty() {
                "unknown".to_string()
            } else {
                tag.to_string()
            }
        });

    let mut attrs = serde_json::Map::new();
    for (k, v) in map {
        if matches!(k.as_str(), "log" | "message" | "msg") {
            continue;
        }
        attrs.insert(k.clone(), msgpack_to_json(v));
    }
    if !tag.is_empty() {
        attrs.insert("fluentd_tag".into(), serde_json::Value::String(tag.into()));
    }

    let mut out = serde_json::Map::new();
    out.insert("message".into(), serde_json::Value::String(message));
    out.insert("level".into(), serde_json::Value::String(level));
    out.insert("service".into(), serde_json::Value::String(service));
    out.insert(
        "ts".into(),
        serde_json::Value::String(
            ts.unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        ),
    );
    // Extras at top level → parser turns them into the attributes residue.
    for (k, val) in attrs {
        out.insert(k, val);
    }

    let raw = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
    let rec = RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: String::new(),
        protocol: Protocol::Fluentd,
        raw: bytes::Bytes::from(raw),
    };
    Ok((rec, option_ack(option)))
}

fn option_ack(option: &MsgValue) -> Option<String> {
    match option {
        MsgValue::Map(pairs) => pairs
            .iter()
            .find(|(k, _)| k == "chunk")
            .and_then(|(_, v)| v.to_string_lossy())
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// msgpack time → RFC3339. Accepts unix seconds (int/float) and Fluentd's
/// EventTime ext (type 0, 8-byte body: BE u32 seconds + BE u32 nanoseconds).
pub fn msgpack_time_to_rfc3339(v: &MsgValue) -> Option<String> {
    match v {
        MsgValue::Ext(0, body) if body.len() == 8 => {
            let secs = u32::from_be_bytes(body[..4].try_into().ok()?) as i64;
            let nans = u32::from_be_bytes(body[4..].try_into().ok()?);
            chrono::DateTime::from_timestamp(secs, nans).map(|d| d.to_rfc3339())
        }
        _ => {
            let secs = v.as_f64()?;
            let s = secs.trunc() as i64;
            let n = ((secs.fract().abs()) * 1e9) as u32;
            chrono::DateTime::from_timestamp(s, n).map(|d| d.to_rfc3339())
        }
    }
}

fn msgpack_to_json(v: &MsgValue) -> serde_json::Value {
    match v {
        MsgValue::Nil => serde_json::Value::Null,
        MsgValue::Bool(b) => serde_json::Value::Bool(*b),
        MsgValue::Int(i) => serde_json::json!(i),
        MsgValue::Uint(u) => serde_json::json!(u),
        MsgValue::F64(f) => serde_json::json!(f),
        MsgValue::Str(s) => serde_json::Value::String(s.clone()),
        MsgValue::Bin(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
        MsgValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(msgpack_to_json).collect())
        }
        MsgValue::Map(pairs) => {
            let mut m = serde_json::Map::new();
            for (k, v) in pairs {
                m.insert(k.clone(), msgpack_to_json(v));
            }
            serde_json::Value::Object(m)
        }
        MsgValue::Ext(t, body) => serde_json::json!({
            "ext_type": t,
            "ext_body_b64": base64::engine::general_purpose::STANDARD.encode(body),
        }),
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(bytes: &[u8]) -> MsgValue {
        crate::insert::msgpack::try_decode(bytes)
            .expect("decodes")
            .expect("one value")
            .0
    }

    /// Build the msgpack bytes Docker's fluentd driver sends:
    /// ["docker.<id>", <unix secs>, {"container_id":…,…}]
    fn docker_message_bytes() -> Vec<u8> {
        let mut b = Vec::new();
        // fixarray(4)
        b.push(0x94);
        // tag: fixstr "docker.abc123def456" (19 bytes)
        b.push(0xa0 + 19);
        b.extend_from_slice(b"docker.abc123def456");
        // time: uint32 unix secs
        b.push(0xce);
        b.extend_from_slice(&1_760_000_000u32.to_be_bytes());
        // record: fixmap(5)
        b.push(0x85);
        for (k, v) in [
            ("container_id", "abc123def4567890"),
            ("container_name", "/web-1"),
            ("source", "stdout"),
            ("log", "hello fluent\n"),
            ("env", "prod"),
        ] {
            b.push(0xa0 + k.len() as u8);
            b.extend_from_slice(k.as_bytes());
            b.push(0xa0 + v.len() as u8);
            b.extend_from_slice(v.as_bytes());
        }
        // option: fixmap(0)
        b.push(0x80);
        b
    }

    #[test]
    fn docker_message_mode_decodes() {
        let v = msg(&docker_message_bytes());
        let (rec, ack) = forward_to_record(&v).expect("entry").expect("record");
        assert!(ack.is_none());
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["message"], "hello fluent\n");
        assert_eq!(env["service"], "web-1", "leading '/' trimmed");
        assert_eq!(env["level"], "info");
        assert_eq!(env["container_id"], "abc123def4567890");
        assert_eq!(env["fluentd_tag"], "docker.abc123def456");
        assert_eq!(env["env"], "prod");
        assert!(env.get("log").is_none(), "line lifted out");
        let ts = chrono::DateTime::parse_from_rfc3339(env["ts"].as_str().unwrap()).unwrap();
        assert_eq!(ts.timestamp(), 1_760_000_000);
    }

    #[test]
    fn stderr_maps_to_error_level() {
        let mut b = Vec::new();
        b.push(0x93); // [tag, time, record]
        b.push(0xa3);
        b.extend_from_slice(b"tag");
        b.push(0xce);
        b.extend_from_slice(&1_760_000_000u32.to_be_bytes());
        b.push(0x82);
        for (k, v) in [("source", "stderr"), ("log", "panic!")] {
            b.push(0xa0 + k.len() as u8);
            b.extend_from_slice(k.as_bytes());
            b.push(0xa0 + v.len() as u8);
            b.extend_from_slice(v.as_bytes());
        }
        let v = msg(&b);
        let (rec, _) = forward_to_record(&v).expect("entry").expect("record");
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["level"], "error");
    }

    #[test]
    fn ack_option_is_detected_and_carried() {
        let mut b = docker_message_bytes();
        assert_eq!(b.pop(), Some(0x80));
        // replace empty option with {"chunk": "abc="}
        b.push(0x81); // fixmap(1)
        b.push(0xa5);
        b.extend_from_slice(b"chunk");
        b.push(0xa4);
        b.extend_from_slice(b"abc=");
        let v = msg(&b);
        let (_, ack) = forward_to_record(&v).expect("entry").expect("record");
        assert_eq!(ack.as_deref(), Some("abc="));
    }

    #[test]
    fn forward_mode_array_accepted() {
        // ["tag", [[time, record]]]
        let mut b = Vec::new();
        b.push(0x92); // array(2)
        b.push(0xa3);
        b.extend_from_slice(b"tag");
        b.push(0x91); // entries array(1)
        b.push(0x92); // [time, record]
        b.push(0xce);
        b.extend_from_slice(&1_760_000_000u32.to_be_bytes());
        b.push(0x81);
        b.push(0xa3);
        b.extend_from_slice(b"log");
        b.push(0xa2);
        b.extend_from_slice(b"hi");
        let v = msg(&b);
        let (rec, _) = forward_to_record(&v).expect("entry").expect("record");
        let env: serde_json::Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["message"], "hi");
        assert_eq!(env["service"], "tag");
    }

    #[test]
    fn event_time_ext_time() {
        let secs: u32 = 1_758_585_600;
        let mut time_b = vec![0xd7, 0x00];
        time_b.extend_from_slice(&secs.to_be_bytes());
        time_b.extend_from_slice(&0u32.to_be_bytes());
        let v = msg(&time_b);
        let ts = msgpack_time_to_rfc3339(&v).expect("ts");
        let dt = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.timestamp(), secs as i64);
    }

    #[test]
    fn non_array_rejected() {
        assert!(forward_to_record(&MsgValue::Nil).is_err());
        assert!(forward_to_record(&MsgValue::Uint(1)).is_err());
    }
}
