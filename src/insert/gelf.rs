//! GELF 1.1 ingest — the wire protocol of Docker Engine's `gelf` log driver
//! (`--log-driver=gelf --log-opt gelf-address=udp://host:port`) and any
//! GELF-capable shipper (architecture §2a).
//!
//! Wire facts (moby `daemon/logger/gelf` + Graylog2/go-gelf):
//! - **UDP**: one datagram per message. Payload is JSON, gzip-compressed by
//!   default (`gelf-compression-type` = gzip|zlib|none). Messages larger than
//!   one MTU are split into GELF chunks: 2-byte magic `\x1e\x0f`, 8-byte
//!   message id, 1-byte sequence, 1-byte total, ≤1408 payload bytes each,
//!   ≤128 chunks. Chunk order is arbitrary; this side reassembles.
//! - **TCP**: JSON + a single trailing NUL byte per message (no framing).
//! - Message fields: `version`, `host`, `short_message`, `timestamp` (float
//!   epoch seconds), `level` (syslog int 0-7), plus `_`-prefixed extras.
//!   Docker adds `_container_id`, `_container_name`, `_image_id`,
//!   `_image_name`, `_command`, `_tag`, `_created`, and label/env extras.
//!
//! Each message is normalized into the standard central-logs envelope
//! (message/service/level/ts + attributes) and pushed into the WAL, exactly
//! like `/v1/logs`; parsing/enrichment happens asynchronously in the ingest
//! workers. Queryable as `protocol:gelf`.

use std::collections::HashMap;
use std::io::Read as _;
use std::time::{Duration, Instant};

use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct GelfState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
}

pub struct GelfListeners {
    pub udp: Option<JoinHandle<()>>,
    pub tcp: Option<JoinHandle<()>>,
}

impl GelfListeners {
    pub async fn join_all(self) {
        if let Some(h) = self.udp {
            let _ = h.await;
        }
        if let Some(h) = self.tcp {
            let _ = h.await;
        }
    }
}

pub async fn spawn_gelf_listeners(
    udp_bind: Option<&str>,
    tcp_bind: Option<&str>,
    state: GelfState,
    shutdown: CancellationToken,
    chunk_timeout: Duration,
) -> GelfListeners {
    let udp = spawn_udp(udp_bind, state.clone(), shutdown.clone(), chunk_timeout).await;
    let tcp = spawn_tcp(tcp_bind, state, shutdown).await;
    GelfListeners { udp, tcp }
}

// =====================================================================
// UDP + chunk reassembly
// =====================================================================

/// GELF chunk header: magic(2) + message id(8) + seq(1) + total(1).
const CHUNK_HEADER: usize = 12;
const CHUNK_MAGIC: [u8; 2] = [0x1e, 0x0f];
const CHUNK_MAX_TOTAL: usize = 128;
/// Combined-size guard so a flood of random "chunk" datagrams can't pin
/// unbounded memory (Graylog's own default reassembly budget is 8 MiB).
const CHUNK_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

struct ChunkBuf {
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    first_seen: Instant,
}

async fn spawn_udp(
    bind: Option<&str>,
    state: GelfState,
    shutdown: CancellationToken,
    chunk_timeout: Duration,
) -> Option<JoinHandle<()>> {
    let bind = bind?;
    let sock = match UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(bind, ?e, "gelf UDP bind failed; disabling");
            return None;
        }
    };
    tracing::info!(bind, "gelf UDP listener bound");
    Some(tokio::spawn(async move {
        let mut chunks: HashMap<[u8; 8], ChunkBuf> = HashMap::new();
        let mut since_sweep = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                res = sock.recv_from(&mut buf) => {
                    let (n, peer) = match res {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(?e, "gelf UDP recv_from failed");
                            continue;
                        }
                    };
                    let data = &buf[..n];
                    let payload: Option<Vec<u8>> = if data.len() >= CHUNK_HEADER
                        && data[..2] == CHUNK_MAGIC
                    {
                        handle_chunk(&mut chunks, data, chunk_timeout, &mut since_sweep)
                    } else {
                        Some(data.to_vec())
                    };
                    since_sweep = since_sweep.wrapping_add(1);
                    if since_sweep >= 1024 || chunks.len() > 4096 {
                        sweep_expired(&mut chunks, chunk_timeout);
                        since_sweep = 0;
                    }
                    let Some(payload) = payload else { continue };
                    ingest_payload(&state, &payload, &peer.to_string(), false).await;
                }
            }
        }
    }))
}

/// Handle one chunked datagram. Returns the complete payload when this chunk
/// was the final one, `None` while chunks are still missing (or on error).
fn handle_chunk(
    chunks: &mut HashMap<[u8; 8], ChunkBuf>,
    data: &[u8],
    chunk_timeout: Duration,
    since_sweep: &mut usize,
) -> Option<Vec<u8>> {
    let msg_id: [u8; 8] = data[2..10].try_into().ok()?;
    let seq = data[10] as usize;
    let total = data[11] as usize;
    if total == 0 || total > CHUNK_MAX_TOTAL || seq >= total {
        return None;
    }
    sweep_expired(chunks, chunk_timeout);
    *since_sweep = 0;

    let entry = chunks.entry(msg_id).or_insert_with(|| ChunkBuf {
        parts: vec![None; total],
        received: 0,
        bytes: 0,
        first_seen: Instant::now(),
    });
    // A late duplicate with a different declared shape → discard the old.
    if entry.parts.len() != total {
        chunks.remove(&msg_id);
        return None;
    }
    let payload = data[CHUNK_HEADER..].to_vec();
    if entry.parts[seq].is_none() {
        entry.received += 1;
        entry.bytes += payload.len();
        if entry.bytes > CHUNK_MAX_MESSAGE_BYTES {
            chunks.remove(&msg_id);
            return None;
        }
    }
    entry.parts[seq] = Some(payload);
    if entry.received < total {
        return None;
    }
    let entry = chunks.remove(&msg_id)?;
    let mut full = Vec::with_capacity(entry.bytes);
    for part in entry.parts {
        full.extend_from_slice(&part?);
    }
    Some(full)
}

fn sweep_expired(chunks: &mut HashMap<[u8; 8], ChunkBuf>, chunk_timeout: Duration) {
    let now = Instant::now();
    chunks.retain(|_, c| now.duration_since(c.first_seen) < chunk_timeout);
}

// =====================================================================
// TCP (JSON + NUL framing)
// =====================================================================

async fn spawn_tcp(
    bind: Option<&str>,
    state: GelfState,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let bind = bind?;
    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(bind, ?e, "gelf TCP bind failed; disabling");
            return None;
        }
    };
    tracing::info!(bind, "gelf TCP listener bound");
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
                            tracing::warn!(?e, "gelf TCP accept failed");
                            continue;
                        }
                    };
                    let st = st.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_tcp_conn(stream, peer.to_string(), st).await {
                            tracing::warn!(?e, "gelf TCP conn handler exited");
                        }
                    });
                }
            }
        }
    }))
}

async fn handle_tcp_conn(stream: TcpStream, peer: String, state: GelfState) -> crate::Result<()> {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut frame = Vec::with_capacity(1024);
    loop {
        frame.clear();
        let n = reader.read_until(0u8, &mut frame).await?;
        if n == 0 {
            return Ok(());
        }
        if frame.last() == Some(&0) {
            frame.pop();
        }
        if frame.is_empty() {
            continue;
        }
        ingest_payload(&state, &frame, &peer, true).await;
    }
}

// =====================================================================
// Payload → WAL
// =====================================================================

/// Decode (gzip/zlib/none), normalize, and append one GELF payload.
/// `wait` = TCP semantics (brief backpressure wait); UDP drops instead.
async fn ingest_payload(state: &GelfState, payload: &[u8], peer: &str, wait: bool) {
    let decoded = match decompress(payload) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(?e, bytes = payload.len(), "gelf payload decompress failed");
            state.counters.record_error(Protocol::Gelf);
            return;
        }
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        tracing::debug!(bytes = decoded.len(), "gelf payload is not JSON; dropped");
        state.counters.record_error(Protocol::Gelf);
        return;
    };
    let envelope = gelf_to_envelope(&v);
    let raw = serde_json::to_vec(&envelope).unwrap_or_else(|_| b"{}".to_vec());
    let rec = RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: peer.to_string(),
        protocol: Protocol::Gelf,
        raw: bytes::Bytes::from(raw),
    };
    state.counters.record(Protocol::Gelf, rec.raw_len());
    let outcome = if wait {
        let h = state.handle.clone();
        match tokio::time::timeout(Duration::from_millis(250), h.append_unacked(rec)).await {
            Ok(Ok(())) => Some(()),
            _ => None,
        }
    } else {
        // UDP: no backpressure concept — drop on a full channel.
        state.handle.try_append_unacked(rec).ok()
    };
    if outcome.is_none() {
        state.counters.record_error(Protocol::Gelf);
    }
}

fn decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
        return Ok(out);
    }
    // zlib (RFC 1950): CMF/FLG header, CM must be 8 (deflate).
    if !data.is_empty() && (data[0] & 0x0f) == 8 {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
        return Ok(out);
    }
    Ok(data.to_vec())
}

/// Normalize one GELF message into the standard envelope.
///
/// - `short_message` → `message`; `level` int (syslog 0-7) → level string;
///   `timestamp` float epoch secs → `ts` (RFC3339, parse.rs reads it back).
/// - `_container_name` (Docker) → `service`; `host` stays for `source_host`.
/// - Everything else (incl. `_`-extras like `_container_id`, `_tag`, label/
///   env extras, and `full_message`) rides in `attributes`.
pub fn gelf_to_envelope(v: &serde_json::Value) -> serde_json::Value {
    let mut attrs = serde_json::Map::new();
    const RESERVED: &[&str] = &[
        "version",
        "host",
        "short_message",
        "full_message",
        "timestamp",
        "level",
    ];
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !RESERVED.contains(&k.as_str()) {
                attrs.insert(k.clone(), val.clone());
            }
        }
    }
    let short = v
        .get("short_message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    let level = match v.get("level") {
        Some(serde_json::Value::Number(n)) => gelf_int_level(n.as_i64().unwrap_or(6)).to_string(),
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        _ => "info".to_string(),
    };
    // Docker's `_container_name` is the natural service; else `facility`;
    // else let the ingest pipeline default it.
    let service = v
        .get("_container_name")
        .and_then(|s| s.as_str())
        .map(|s| s.trim_start_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("facility")
                .and_then(|s| s.as_str())
                .map(String::from)
        });

    let mut out = serde_json::Map::new();
    out.insert("message".into(), serde_json::Value::String(short));
    out.insert("level".into(), serde_json::Value::String(level));
    if let Some(svc) = service {
        out.insert("service".into(), serde_json::Value::String(svc));
    }
    // `host` lifts to `source_host` in the parser (Docker sets it to the
    // daemon host's OS hostname).
    if let Some(host) = v.get("host") {
        if !host.is_null() {
            out.insert("host".into(), host.clone());
        }
    }
    match v.get("timestamp") {
        Some(ts @ serde_json::Value::Number(_)) => {
            out.insert("ts".into(), ts.clone());
        }
        _ => {
            out.insert(
                "ts".into(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
        }
    }
    if let Some(full) = v.get("full_message").and_then(|m| m.as_str()) {
        if !full.is_empty() {
            attrs.insert("full_message".into(), serde_json::Value::String(full.into()));
        }
    }
    // Extras ride at the envelope's top level: the parser lifts reserved
    // keys into columns and turns the REST into the attributes residue, so
    // nesting them under "attributes" would just double-nest.
    for (k, val) in attrs {
        out.insert(k, val);
    }
    serde_json::Value::Object(out)
}

/// GELF `level` is a syslog severity: 0-2 emerg/alert/crit → fatal, 3 err,
/// 4 warning, 5-6 notice/info, 7 debug.
pub fn gelf_int_level(n: i64) -> &'static str {
    match n {
        0..=2 => "fatal",
        3 => "error",
        4 => "warn",
        5..=6 => "info",
        _ => "debug",
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_reassembly_out_of_order() {
        let mut chunks = HashMap::new();
        let mut sweep = 0;
        let payload = b"hello chunked world";
        let mk = |seq: u8, total: u8, part: &[u8]| {
            let mut d = CHUNK_MAGIC.to_vec();
            d.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            d.push(seq);
            d.push(total);
            d.extend_from_slice(part);
            d
        };
        assert!(handle_chunk(&mut chunks, &mk(0, 3, &payload[..7]), Duration::from_secs(5), &mut sweep).is_none());
        assert!(handle_chunk(&mut chunks, &mk(2, 3, &payload[14..]), Duration::from_secs(5), &mut sweep).is_none());
        let done = handle_chunk(&mut chunks, &mk(1, 3, &payload[7..14]), Duration::from_secs(5), &mut sweep);
        assert_eq!(done.as_deref(), Some(payload.as_slice()));
        assert!(chunks.is_empty(), "completed message must be removed");
    }

    #[test]
    fn chunk_validation_rejects_bogus_headers() {
        let mut chunks = HashMap::new();
        let mut sweep = 0;
        // total = 0
        let mut d = CHUNK_MAGIC.to_vec();
        d.extend_from_slice(&[0u8; 8]);
        d.extend_from_slice(&[0, 0, 5, 5, 5]);
        assert!(handle_chunk(&mut chunks, &d, Duration::from_secs(5), &mut sweep).is_none());
        // seq >= total
        let mut d = CHUNK_MAGIC.to_vec();
        d.extend_from_slice(&[0u8; 8]);
        d.push(3);
        d.push(2);
        assert!(handle_chunk(&mut chunks, &d, Duration::from_secs(5), &mut sweep).is_none());
    }

    #[test]
    fn decompress_detects_gzip_and_zlib() {
        let raw = b"plain gelf json";
        // gzip
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, raw).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(decompress(&gz).unwrap(), raw);
        // zlib
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, raw).unwrap();
        let zz = enc.finish().unwrap();
        assert_eq!(decompress(&zz).unwrap(), raw);
        // none
        assert_eq!(decompress(raw).unwrap(), b"plain gelf json".as_slice());
    }

    #[test]
    fn envelope_maps_docker_gelf_message() {
        let msg = serde_json::json!({
            "version": "1.1",
            "host": "docker-host",
            "short_message": "hello\n",
            "timestamp": 1_760_000_000.789,
            "level": 3,
            "_container_id": "abc123",
            "_container_name": "/web-1",
            "_image_name": "nginx:latest",
            "_tag": "abc123def456",
            "_env": "prod"
        });
        let env = gelf_to_envelope(&msg);
        assert_eq!(env["message"], "hello\n");
        assert_eq!(env["level"], "error");
        assert_eq!(env["service"], "web-1");
        assert_eq!(env["ts"], 1_760_000_000.789);
        assert_eq!(env["host"], "docker-host");
        // Extras ride at the envelope's top level (the parser turns them
        // into the attributes residue).
        assert_eq!(env["_container_id"], "abc123");
        assert_eq!(env["_image_name"], "nginx:latest");
        assert_eq!(env["_tag"], "abc123def456");
        assert_eq!(env["_env"], "prod");
        assert!(env.get("short_message").is_none());
    }

    #[test]
    fn envelope_defaults_and_level_strings() {
        let msg = serde_json::json!({
            "version": "1.1",
            "host": "h",
            "short_message": "x",
            "level": "WARNING"
        });
        let env = gelf_to_envelope(&msg);
        assert_eq!(env["level"], "WARNING");
        assert!(env["service"].is_null(), "no service key when no container/facility");
        assert_eq!(env["host"], "h", "host stays for source_host lift");
        assert!(env.get("attributes").is_none());
        // parse path: ts must be readable
        let level = crate::ingest::parse::normalize_level_public("WARNING");
        assert_eq!(level, "warn");
    }

    #[test]
    fn int_level_mapping_matches_syslog() {
        assert_eq!(gelf_int_level(0), "fatal");
        assert_eq!(gelf_int_level(3), "error");
        assert_eq!(gelf_int_level(4), "warn");
        assert_eq!(gelf_int_level(6), "info");
        assert_eq!(gelf_int_level(7), "debug");
        assert_eq!(gelf_int_level(99), "debug");
    }
}
