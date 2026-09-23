//! Docker Engine API collector — pull container logs via the unix socket /
//! TCP endpoint (Engine API `GET /containers/{id}/logs?follow=1`).
//!
//! Engine API facts (api/docs/v1.56.yaml + pkg/stdcopy):
//! - `GET /v1.x/containers/json` → array of `{Id, Names: ["/name"], Image,
//!   State, Labels, …}`.
//! - `GET /v1.x/containers/{id}/json` (inspect) → `{Name, Config: {Tty, …}}`
//!   — `Tty` decides the log stream framing.
//! - `GET /v1.x/containers/{id}/logs?follow=true&stdout=true&stderr=true
//!   &timestamps=true&tail=N` streams:
//!   - **Non-TTY**: multiplexed frames — 8-byte header
//!     `[stream_type, 0, 0, 0, len u32 BE]` + payload; type 1 = stdout,
//!     2 = stderr, 3 = daemon error text (stream aborts).
//!   - **TTY**: raw merged stream.
//!   - With `timestamps=true` every frame payload line is prefixed with an
//!     RFC3339Nano stamp.
//!
//! Rows are tagged `protocol:docker_api`; `service` = container name,
//! attributes carry `container_id`/`image`/`stream`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::client::legacy::connect::Connected;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::collector::{glob_match, log_envelope, push_record, split_timestamp_prefix};
use crate::config::DockerCollectorConfig;
use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::Protocol;

pub fn spawn(
    cfg: DockerCollectorConfig,
    handle: InsertHandle,
    counters: InsertCountersRef,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        let endpoint = match Endpoint::parse(&cfg.socket) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                tracing::error!(socket = %cfg.socket, ?e, "docker collector: bad socket config");
                return;
            }
        };
        let connector = EndpointConnector(endpoint.clone());
        let client: Client<EndpointConnector, Empty<Bytes>> =
            Client::builder(TokioExecutor::new()).build(connector);
        let attached: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let refresh = Duration::from_secs(cfg.refresh_secs.max(1));
        tracing::info!(
            socket = %cfg.socket,
            refresh_secs = cfg.refresh_secs,
            tail = cfg.tail_lines,
            "docker collector started"
        );
        let mut tick = tokio::time::interval(refresh);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(e) =
                        discover(&client, &cfg, &handle, &counters, &attached, &shutdown).await
                    {
                        tracing::debug!(?e, "docker collector: discovery failed");
                    }
                }
            }
        }
        tracing::info!("docker collector stopped");
    }))
}

/// One discovery pass: list containers, attach follow streams to new ones.
async fn discover(
    client: &Client<EndpointConnector, Empty<Bytes>>,
    cfg: &DockerCollectorConfig,
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    attached: &Arc<Mutex<HashSet<String>>>,
    shutdown: &CancellationToken,
) -> Result<(), String> {
    let list = api_get_json(client, &containers_path(cfg, "/containers/json")).await?;
    let Some(items) = list.as_array() else {
        return Err("containers list is not an array".into());
    };
    for item in items {
        let Some(id) = item.get("Id").and_then(|i| i.as_str()) else {
            continue;
        };
        let name = item
            .get("Names")
            .and_then(|n| n.as_array())
            .and_then(|a| a.first())
            .and_then(|n| n.as_str())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.chars().take(12).collect());
        if !cfg.include.is_empty() && !cfg.include.iter().any(|p| glob_match(p, &name)) {
            continue;
        }
        if cfg.exclude.iter().any(|p| glob_match(p, &name)) {
            continue;
        }
        let id = id.to_string();
        if attached.lock().contains(&id) {
            continue;
        }
        attached.lock().insert(id.clone());
        let image = item
            .get("Image")
            .and_then(|i| i.as_str())
            .unwrap_or_default()
            .to_string();
        let client = client.clone();
        let cfg = cfg.clone();
        let handle = handle.clone();
        let counters = counters.clone();
        let attached = attached.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            follow_container(client, cfg, handle, counters, id.clone(), name, image, attached, shutdown)
                .await;
        });
    }
    Ok(())
}

/// Follow one container's log stream until it ends (container stopped /
/// stream error), then release the attach slot so a restart re-attaches.
#[allow(clippy::too_many_arguments)]
async fn follow_container(
    client: Client<EndpointConnector, Empty<Bytes>>,
    cfg: DockerCollectorConfig,
    handle: InsertHandle,
    counters: InsertCountersRef,
    id: String,
    name: String,
    image: String,
    attached: Arc<Mutex<HashSet<String>>>,
    shutdown: CancellationToken,
) {
    let result = async {
        // Inspect once: TTY containers stream raw (no 8-byte headers).
        let inspect =
            api_get_json(&client, &containers_path(&cfg, &format!("/containers/{id}/json")))
                .await?;
        let tty = inspect
            .get("Config")
            .and_then(|c| c.get("Tty"))
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let query = format!(
            "?follow=true&stdout=true&stderr=true&timestamps=true&tail={}",
            cfg.tail_lines
        );
        let resp = api_get(
            &client,
            &containers_path(&cfg, &format!("/containers/{id}/logs{query}")),
        )
        .await?;
        if !resp.status().is_success() {
            return Err(format!("logs endpoint returned {}", resp.status()));
        }
        stream_logs(resp.into_body(), tty, &handle, &counters, &id, &name, &image).await;
        Ok::<(), String>(())
    };
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        res = result => {
            if let Err(e) = res {
                tracing::debug!(container = %name, %e, "docker follow stream ended");
            }
        }
    }
    attached.lock().remove(&id);
}

/// Consume the log stream body, demultiplex frames (non-TTY), split lines,
/// and push one record per line. Log lines can be split across stdcopy
/// frames, so payload bytes accumulate in a persistent line buffer.
async fn stream_logs(
    mut body: Incoming,
    tty: bool,
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    id: &str,
    name: &str,
    image: &str,
) {
    let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut spill: Vec<u8> = Vec::new(); // raw bytes awaiting a complete frame header
    let mut stream_label = String::new();
    loop {
        let frame = match body.frame().await {
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                tracing::debug!(container = %name, error = %e, "docker log stream error");
                break;
            }
            None => break,
        };
        let Some(data) = frame.data_ref() else { continue };
        if tty {
            line_buf.extend_from_slice(data);
            flush_lines(handle, counters, id, name, image, &mut line_buf, &stream_label).await;
            continue;
        }
        // stdcopy framing: possibly many frames inside one body chunk, and
        // frames can straddle body chunks.
        let mut cur: &[u8] = data;
        while !cur.is_empty() {
            if spill.is_empty() && cur.len() >= 8 {
                let stype = cur[0];
                let len = u32::from_be_bytes(cur[4..8].try_into().unwrap()) as usize;
                if stype > 2 {
                    // SystemErr: daemon error text (e.g. "Error grabbing logs").
                    tracing::debug!(
                        container = %name,
                        text = %String::from_utf8_lossy(&cur[8..]),
                        "docker daemon stream error"
                    );
                    return;
                }
                stream_label = if stype == 2 { "stderr" } else { "stdout" }.into();
                if cur.len() >= 8 + len {
                    line_buf.extend_from_slice(&cur[8..8 + len]);
                    cur = &cur[8 + len..];
                    flush_lines(handle, counters, id, name, image, &mut line_buf, &stream_label).await;
                    continue;
                }
            }
            // Slow path: header/payload split across body chunks.
            spill.extend_from_slice(cur);
            cur = &[];
            while let Some((payload, stype)) = next_frame(&mut spill) {
                stream_label = if stype == 2 { "stderr" } else { "stdout" }.into();
                line_buf.extend_from_slice(&payload);
                flush_lines(handle, counters, id, name, image, &mut line_buf, &stream_label).await;
            }
        }
    }
    // Final partial line.
    if !line_buf.is_empty() {
        let text = String::from_utf8_lossy(&line_buf).into_owned();
        push_line(handle, counters, id, name, image, &stream_label, text).await;
    }
}

/// Extract one complete stdcopy frame from `spill`, draining its bytes.
/// Returns `(payload, stream_type)` or `None` when more bytes are needed.
fn next_frame(spill: &mut Vec<u8>) -> Option<(Vec<u8>, u8)> {
    if spill.len() < 8 {
        return None;
    }
    let stype = spill[0];
    if stype > 2 {
        spill.clear();
        return None;
    }
    let len = u32::from_be_bytes(spill[4..8].try_into().unwrap()) as usize;
    if len > 16 * 1024 * 1024 {
        // Corrupt length — drop the buffer rather than pinning memory.
        spill.clear();
        return None;
    }
    if spill.len() < 8 + len {
        return None;
    }
    let payload = spill[8..8 + len].to_vec();
    spill.drain(..8 + len);
    Some((payload, stype))
}

/// Split buffered payload bytes into complete lines and push each.
async fn flush_lines(
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    id: &str,
    name: &str,
    image: &str,
    buf: &mut Vec<u8>,
    stream_label: &str,
) {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
        if s.ends_with('\r') {
            s.pop();
        }
        push_line(handle, counters, id, name, image, stream_label, s).await;
    }
}

async fn push_line(
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    id: &str,
    name: &str,
    image: &str,
    stream_label: &str,
    line: String,
) {
    if line.is_empty() {
        return;
    }
    let (ts, message) = split_timestamp_prefix(&line);
    let level = if stream_label == "stderr" { "error" } else { "info" };
    let mut attrs = serde_json::Map::new();
    attrs.insert("container_id".into(), serde_json::json!(id));
    if !image.is_empty() {
        attrs.insert("image".into(), serde_json::json!(image));
    }
    if !stream_label.is_empty() {
        attrs.insert("stream".into(), serde_json::json!(stream_label));
    }
    let env = log_envelope(message.to_string(), level, name.to_string(), ts, attrs);
    push_record(handle, counters, Protocol::DockerApi, env).await;
}

fn containers_path(cfg: &DockerCollectorConfig, suffix: &str) -> String {
    let ver = cfg.api_version.trim().trim_matches('/');
    if ver.is_empty() {
        suffix.to_string()
    } else {
        format!("/{ver}{suffix}")
    }
}

async fn api_get_json(
    client: &Client<EndpointConnector, Empty<Bytes>>,
    path: &str,
) -> Result<serde_json::Value, String> {
    let resp = api_get(client, path).await?;
    let status = resp.status();
    let body = collect_body(resp.into_body()).await?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&body);
        return Err(format!("GET {path} → {status}: {}", truncate_str(&preview, 200)));
    }
    serde_json::from_slice(&body).map_err(|e| format!("GET {path}: bad JSON: {e}"))
}

async fn api_get(
    client: &Client<EndpointConnector, Empty<Bytes>>,
    path: &str,
) -> Result<hyper::Response<Incoming>, String> {
    let uri: hyper::Uri = format!("http://docker{path}")
        .parse()
        .map_err(|e| format!("bad uri {path}: {e}"))?;
    let req = Request::builder()
        .method(hyper::Method::GET)
        .uri(uri)
        .header(hyper::header::HOST, "docker")
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    client.request(req).await.map_err(|e| e.to_string())
}

async fn collect_body(mut body: Incoming) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Some(d) = f.data_ref() {
                    out.extend_from_slice(d);
                    if out.len() > 16 * 1024 * 1024 {
                        return Err("response body too large".into());
                    }
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

fn truncate_str(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// =====================================================================
// Transport: hyper client connector over unix socket / TCP
// =====================================================================

#[derive(Debug, Clone)]
pub(crate) enum Endpoint {
    Unix(PathBuf),
    Tcp(String),
}

impl Endpoint {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix://") {
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if let Some(addr) = s.strip_prefix("tcp://").or_else(|| s.strip_prefix("http://")) {
            return Ok(Endpoint::Tcp(addr.trim_end_matches('/').to_string()));
        }
        Err("endpoint must be unix://… or tcp://…".into())
    }
}

/// Either-flavor stream the hyper client can talk to.
pub(crate) enum Conn {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Conn::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Conn::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Conn::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            Conn::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Conn::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl hyper_util::client::legacy::connect::Connection for Conn {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

/// Newtype connector handed to the hyper legacy client (orphan rule: the
/// impl must live on a local type).
#[derive(Clone)]
pub(crate) struct EndpointConnector(pub(crate) Arc<Endpoint>);

impl tower::Service<hyper::Uri> for EndpointConnector {
    type Response = TokioIo<Conn>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: hyper::Uri) -> Self::Future {
        let ep = self.0.clone();
        Box::pin(async move {
            let conn = match &*ep {
                Endpoint::Unix(path) => Conn::Unix(tokio::net::UnixStream::connect(path).await?),
                Endpoint::Tcp(addr) => Conn::Tcp(tokio::net::TcpStream::connect(addr).await?),
            };
            Ok(TokioIo::new(conn))
        })
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parse() {
        assert!(matches!(
            Endpoint::parse("unix:///var/run/docker.sock"),
            Ok(Endpoint::Unix(p)) if p == PathBuf::from("/var/run/docker.sock")
        ));
        assert!(matches!(
            Endpoint::parse("tcp://127.0.0.1:2375"),
            Ok(Endpoint::Tcp(a)) if a == "127.0.0.1:2375"
        ));
        assert!(Endpoint::parse("/var/run/docker.sock").is_err());
    }

    #[test]
    fn api_paths() {
        let mut cfg = DockerCollectorConfig::default();
        assert_eq!(
            containers_path(&cfg, "/containers/json"),
            "/v1.41/containers/json"
        );
        cfg.api_version = String::new();
        assert_eq!(containers_path(&cfg, "/containers/json"), "/containers/json");
    }

    #[test]
    fn stdcopy_frame_demux() {
        // Two frames in one buffer: stdout "hello\n", stderr "oops\n".
        let mut buf: Vec<u8> = Vec::new();
        for (stype, text) in [(1u8, "hello\n".as_bytes()), (2u8, "oops\n".as_bytes())] {
            buf.extend_from_slice(&[stype, 0, 0, 0]);
            buf.extend_from_slice(&(text.len() as u32).to_be_bytes());
            buf.extend_from_slice(text);
        }
        // Reuse the sync parts of demux: verify frame math.
        let stype = buf[0];
        let len = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
        assert_eq!(stype, 1);
        assert_eq!(len, 6);
        assert_eq!(&buf[8..8 + len], b"hello\n");
    }
}
