//! Syslog listeners: UDP (RFC 3164/5424) + TCP framing (octet-counting or newline).
//! Per architecture §1 (v1 protocols).

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

#[derive(Clone)]
pub struct SyslogState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
}

pub struct SyslogListeners {
    pub udp: Option<JoinHandle<()>>,
    pub tcp: Option<JoinHandle<()>>,
}

impl SyslogListeners {
    pub async fn join_all(self) {
        if let Some(h) = self.udp {
            let _ = h.await;
        }
        if let Some(h) = self.tcp {
            let _ = h.await;
        }
    }
}

pub async fn spawn_syslog_listeners(
    cfg_udp: Option<&str>,
    cfg_tcp: Option<&str>,
    state: SyslogState,
    shutdown: CancellationToken,
) -> SyslogListeners {
    let udp = if let Some(addr) = cfg_udp {
        let sock = match UdpSocket::bind(addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(addr, ?e, "syslog UDP bind failed; disabling");
                return SyslogListeners {
                    udp: None,
                    tcp: spawn_tcp(cfg_tcp, state, shutdown).await,
                };
            }
        };
        tracing::info!(addr, "syslog UDP listener bound");
        let st = state.clone();
        let shutdown = shutdown.clone();
        Some(tokio::spawn(async move {
            run_udp(sock, st, shutdown).await;
        }))
    } else {
        None
    };

    let tcp = spawn_tcp(cfg_tcp, state, shutdown).await;

    SyslogListeners { udp, tcp }
}

async fn spawn_tcp(
    cfg: Option<&str>,
    state: SyslogState,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let addr = cfg?;
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr, ?e, "syslog TCP bind failed; disabling");
            return None;
        }
    };
    tracing::info!(addr, "syslog TCP listener bound");
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
                            tracing::warn!(?e, "syslog TCP accept failed");
                            continue;
                        }
                    };
                    let st = st.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_tcp_conn(stream, peer.to_string(), st).await {
                            tracing::warn!(?e, "syslog TCP conn handler exited");
                        }
                    });
                }
            }
        }
    }))
}

async fn run_udp(sock: UdpSocket, state: SyslogState, shutdown: CancellationToken) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            res = sock.recv_from(&mut buf) => {
                match res {
                    Ok((n, peer)) => {
                        let data = &buf[..n];
                        let rec = RawRecord {
                            receive_ts: chrono::Utc::now(),
                            source_addr: peer.to_string(),
                            protocol: Protocol::SyslogUdp,
                            raw: bytes::Bytes::copy_from_slice(data),
                        };
                        state.counters.record(Protocol::SyslogUdp, n);
                        // UDP has no backpressure concept — drop on full channel.
                        match state.handle.try_append_unacked(rec) {
                            Ok(()) => {}
                            Err(crate::Error::InvalidInput(_)) => {
                                state.counters.record_error(Protocol::SyslogUdp);
                            }
                            Err(e) => {
                                tracing::warn!(?e, "udp syslog: wal channel error");
                                state.counters.record_error(Protocol::SyslogUdp);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(?e, "udp syslog recv_from failed");
                    }
                }
            }
        }
    }
}

async fn handle_tcp_conn(
    stream: TcpStream,
    peer: String,
    state: SyslogState,
) -> crate::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        // Try traditional newline-delimited first; this also handles
        // transparent-framing for the common case. (Full octet-counting support
        // can be layered in later.)
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        let bytes_len = trimmed.len();
        let rec = RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: peer.clone(),
            protocol: Protocol::SyslogTcp,
            raw: bytes::Bytes::copy_from_slice(trimmed.as_bytes()),
        };
        state.counters.record(Protocol::SyslogTcp, bytes_len);
        // TCP supports backpressure — wait briefly for room.
        match tokio::time::timeout(Duration::from_millis(250), state.handle.append_unacked(rec)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(?e, "tcp syslog: append failed");
                state.counters.record_error(Protocol::SyslogTcp);
            }
            Err(_) => {
                state.counters.record_error(Protocol::SyslogTcp);
            }
        }
    }
}

/// Silence unused-import warnings for things pulled in only by future code.
#[allow(dead_code)]
fn _unused_imports(_stream: &mut TcpStream) {}
