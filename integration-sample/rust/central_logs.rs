//! central-logs integration client — batched NDJSON inserts, std-only.
//!
//! Uses a raw HTTP/1.1 POST over `TcpStream`, so there are zero external
//! crate dependencies. `http://host:port` URLs only (no TLS); for HTTPS use
//! the `ureq` or `reqwest` crate and swap out `post_ndjson` — the rest of
//! this module is transport-agnostic.
//!
//! Setup (see INTEGRATION.md):
//!   1. Admin key pinned in the server's .env (CENTRAL_LOGS_HTTP_API_KEY).
//!   2. Mint an insert-only key (POST /v1/api-keys, scopes: "insert").
//!   3. Export for this app:
//!        CENTRAL_LOGS_URL      (default http://localhost:8080)
//!        CENTRAL_LOGS_API_KEY  (the raw clk_... value — required)
//!        CENTRAL_LOGS_SERVICE  (default "default-app")
//!
//! Usage:
//! ```
//! mod central_logs;
//! use central_logs::{CentralLogs, Json};
//!
//! let cl = CentralLogs::from_env();
//! cl.log("info", "app started");
//! cl.log_kv("warn", "queue lag 12s", &[("queue", Json::str("events")), ("duration_ms", Json::int(12_000))]);
//! cl.close(); // final flush
//! ```
//!
//! The flusher thread runs in the background; `close()` (or `Drop`) does the
//! final flush. On persistent failure the batch goes to stderr — nothing is
//! silently lost — and the queue is bounded so outages can't exhaust memory.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Minimal JSON value for attribute fields (extend as needed).
pub enum Json<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl<'a> Json<'a> {
    pub fn str(s: &'a str) -> Self {
        Json::Str(s)
    }
    pub fn int(i: i64) -> Self {
        Json::Int(i)
    }
    pub fn float(f: f64) -> Self {
        Json::Float(f)
    }
    pub fn boolean(b: bool) -> Self {
        Json::Bool(b)
    }

    fn to_json(&self) -> String {
        match self {
            Json::Str(s) => json_escape(s),
            Json::Int(i) => i.to_string(),
            Json::Float(f) => {
                if f.is_finite() {
                    format!("{f}")
                } else {
                    "null".into()
                }
            }
            Json::Bool(b) => b.to_string(),
        }
    }
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn rfc3339_now() -> String {
    // SystemTime → seconds since epoch; format by hand (no chrono dep).
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days algorithm (Howard Hinnant) — no external time crate.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

struct Shared {
    queue: Mutex<Vec<String>>,
    cv: Condvar,
    closed: AtomicBool,
}

pub struct CentralLogs {
    shared: Arc<Shared>,
    pub base_url: String,
    pub api_key: String,
    pub service: String,
}

impl CentralLogs {
    /// Build from CENTRAL_LOGS_URL / CENTRAL_LOGS_API_KEY / CENTRAL_LOGS_SERVICE.
    pub fn from_env() -> Self {
        Self::new(
            &env::var("CENTRAL_LOGS_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
            &env::var("CENTRAL_LOGS_API_KEY").unwrap_or_default(),
            &env::var("CENTRAL_LOGS_SERVICE").unwrap_or_else(|_| "default-app".into()),
        )
    }

    pub fn new(base_url: &str, api_key: &str, service: &str) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let this = Self {
            shared,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            service: service.to_string(),
        };
        // Background flusher.
        let bg = this.clone();
        std::thread::Builder::new()
            .name("central-logs".into())
            .spawn(move || {
                let mut interval = Duration::from_millis(
                    env::var("CENTRAL_LOGS_FLUSH_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1000),
                );
                loop {
                    let (guard, _) = bg
                        .shared
                        .cv
                        .wait_timeout(bg.shared.queue.lock().unwrap(), interval)
                        .unwrap();
                    if bg.shared.closed.load(Ordering::Relaxed) {
                        return;
                    }
                    if !guard.is_empty() {
                        interval = Duration::ZERO; // flush immediately while busy
                    } else {
                        interval = Duration::from_millis(
                            env::var("CENTRAL_LOGS_FLUSH_MS")
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(1000),
                        );
                    }
                    bg.flush();
                }
            })
            .expect("spawn central-logs flusher");
        this
    }

    /// Queue one record at the given level ("debug"|"info"|"warn"|"error"|"fatal").
    pub fn log(&self, level: &str, msg: &str) {
        self.record(level, msg, &[]);
    }

    /// Queue one record with queryable attribute fields.
    pub fn log_kv(&self, level: &str, msg: &str, fields: &[(&str, Json)]) {
        self.record(level, msg, fields);
    }

    fn record(&self, level: &str, msg: &str, fields: &[(&str, Json)]) {
        let max_queue: usize = env::var("CENTRAL_LOGS_MAX_QUEUE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);
        let mut line = format!(
            "{{\"service\":{},\"level\":\"{}\",\"msg\":{},\"ts\":\"{}\"",
            json_escape(&self.service),
            level,
            json_escape(msg),
            rfc3339_now()
        );
        for (k, v) in fields {
            line.push_str(&format!(",\"{}\":{}", k, v.to_json()));
        }
        line.push('}');

        let mut q = self.shared.queue.lock().unwrap();
        if q.len() >= max_queue {
            q.remove(0); // bounded: drop oldest
        }
        q.push(line);
        let max_batch: usize = env::var("CENTRAL_LOGS_MAX_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        if q.len() >= max_batch {
            drop(q);
            self.flush();
        }
    }

    /// Send the queued batch now.
    pub fn flush(&self) {
        let batch: Vec<String> = {
            let mut q = self.shared.queue.lock().unwrap();
            std::mem::take(&mut *q)
        };
        if batch.is_empty() {
            return;
        }
        if self.api_key.is_empty() {
            drop_to_stderr(&batch, "CENTRAL_LOGS_API_KEY not set");
            return;
        }
        match post_ndjson(&self.base_url, &self.api_key, &batch.join("\n")) {
            Ok(()) => {}
            Err(e) => drop_to_stderr(&batch, &e),
        }
    }

    /// Stop the flusher and send the final batch.
    pub fn close(&self) {
        self.shared
            .closed
            .store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        self.flush();
    }
}

impl Clone for CentralLogs {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            service: self.service.clone(),
        }
    }
}

impl Drop for CentralLogs {
    fn drop(&mut self) {
        // Last handle: final flush. (Arc strong-count check keeps clones safe.)
        if Arc::strong_count(&self.shared) == 1
            && !self.shared.closed.load(Ordering::Relaxed)
        {
            self.flush();
        }
    }
}

fn drop_to_stderr(batch: &[String], reason: &str) {
    eprintln!("[central-logs] flush failed ({reason}); {} records dropped to stderr", batch.len());
    for line in batch {
        eprintln!("{line}");
    }
}

/// Minimal HTTP/1.1 POST — `http://host:port` only, no TLS, no chunking.
fn post_ndjson(base_url: &str, api_key: &str, body: &str) -> Result<(), String> {
    let rest = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported URL (http:// only): {base_url}"))?;
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "bad port")?),
        None => (rest, 80),
    };
    let mut stream =
        TcpStream::connect((host, port)).map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let req = format!(
        "POST /v1/logs HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {api_key}\r\n\
         Content-Type: application/x-ndjson\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut response = String::new();
    stream
        .take(1024 * 64)
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;

    let status = response
        .split_whitespace()
        .nth(1)
        .unwrap_or("000")
        .to_string();
    if status != "200" {
        return Err(format!("HTTP {status}: {}", &response[..response.len().min(200)]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_json_strings() {
        assert_eq!(json_escape("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn formats_timestamp() {
        let ts = rfc3339_now();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }
}
