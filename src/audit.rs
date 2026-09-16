//! Self-audit: dogfood central-logs by ingesting its own audit events through
//! the same WAL → ingest → DuckDB pipeline user logs use (architecture §8).
//!
//! Every interesting HTTP action (login, dashboard create, alert approve,
//! api-key revoke, ...) emits one audit record. The record is a normal JSON
//! log envelope with `service = "central-logs"`, so it lands in the same
//! `logs` table and is queryable via the existing filter DSL
//! (`service:central-logs action:auth.login.success` once `action` is
//! promoted to a hot attribute, or just `service:central-logs login` for a
//! free-text substring match).
//!
//! Design points:
//!
//! - **Fire-and-forget** via [`InsertHandle::try_append_unacked`]. Audit must
//!   never block the request path or push back on user inserts; if the WAL
//!   channel is saturated we drop the audit event with a `tracing::warn!`
//!   rather than slow the operator down.
//! - **No recursive feedback.** Audit events are emitted by HTTP handlers
//!   only — the ingest workers that process them do not themselves emit
//!   audit events, so there's no audit-of-audit loop.
//! - **Never log raw credentials.** Only the `key_id` / `key_name` /
//!   `key_prefix` of the actor; never the raw API key value, session id, or
//!   Authorization header.
//! - **Source IP** is resolved by the caller (from `ConnectInfo` /
//!   `X-Forwarded-For`) and passed in — keeps this module free of axum
//!   extractor coupling.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};

use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

/// Well-known service tag for self-emitted audit records. Filter on this to
/// scope a query to audit events only.
pub const AUDIT_SERVICE: &str = "central-logs";

/// Handle held by HTTP handlers, wrapping an [`InsertHandle`] for fire-and-
/// forget audit emission. Cheap to clone; share freely.
///
/// Shares an [`InsertCountersRef`] so dropped events (WAL saturated) are
/// counted and surfaced in `/metrics` as `central_logs_audit_dropped_total`.
#[derive(Clone)]
pub struct AuditHandle {
    inner: InsertHandle,
    counters: InsertCountersRef,
}

impl AuditHandle {
    pub fn new(handle: InsertHandle, counters: InsertCountersRef) -> Self {
        Self {
            inner: handle,
            counters,
        }
    }

    /// Build an [`AuditEvent`] bound to this handle. Call `.emit()` on the
    /// event when ready. The builder is `must_use` so dropped events produce
    /// a compile warning.
    pub fn event(&self, action: impl Into<String>) -> AuditEvent<'_> {
        AuditEvent {
            handle: self,
            ts: Utc::now(),
            action: action.into(),
            level: "info".into(),
            actor_key_id: None,
            actor_key_name: None,
            actor_via: None,
            source_ip: None,
            outcome: "success".into(),
            extra: Vec::new(),
        }
    }
}

/// Fluent builder for one audit event. Call `.emit()` (async) to ship it.
#[must_use = "audit events must be .emit()ted or they do nothing"]
pub struct AuditEvent<'a> {
    handle: &'a AuditHandle,
    ts: chrono::DateTime<chrono::Utc>,
    action: String,
    level: String,
    actor_key_id: Option<i64>,
    actor_key_name: Option<String>,
    actor_via: Option<String>,
    source_ip: Option<String>,
    outcome: String,
    extra: Vec<(&'static str, Value)>,
}

impl<'a> AuditEvent<'a> {
    pub fn level(mut self, lvl: impl Into<String>) -> Self {
        self.level = lvl.into();
        self
    }

    /// Mark this event as a failure (e.g. failed login). Sets `outcome` and
    /// bumps level to `warn` unless you override it after.
    pub fn failed(mut self, reason: impl Into<String>) -> Self {
        self.outcome = "failure".into();
        if self.level == "info" {
            self.level = "warn".into();
        }
        self.extra.push(("reason", Value::String(reason.into())));
        self
    }

    pub fn actor(mut self, info: &crate::web::auth::AuthInfo) -> Self {
        self.actor_key_id = info.key_id;
        self.actor_key_name = Some(info.key_name.clone());
        self.actor_via = Some(format!("{:?}", info.via).to_ascii_lowercase());
        self
    }

    pub fn actor_key(mut self, id: i64, name: impl Into<String>) -> Self {
        self.actor_key_id = Some(id);
        self.actor_key_name = Some(name.into());
        self
    }

    pub fn via(mut self, via: impl Into<String>) -> Self {
        self.actor_via = Some(via.into());
        self
    }

    pub fn source_ip(mut self, ip: impl Into<String>) -> Self {
        let s = ip.into();
        if !s.is_empty() {
            self.source_ip = Some(s);
        }
        self
    }

    /// Attach an extra field to the audit `attributes` JSON.
    pub fn field(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.extra.push((key, value.into()));
        self
    }

    /// Ship the event. Fire-and-forget: returns immediately; the WAL write
    /// uses `try_append_unacked` (synchronous, never blocks). If the WAL
    /// channel is saturated the event is dropped with a `tracing::warn!`
    /// rather than push back on the request path.
    pub fn emit(self) {
        let raw = serialize_event(
            self.ts,
            &self.action,
            &self.level,
            self.actor_key_id,
            self.actor_key_name.as_deref(),
            self.actor_via.as_deref(),
            self.source_ip.as_deref(),
            &self.outcome,
            &self.extra,
        );
        let record = RawRecord {
            receive_ts: self.ts,
            source_addr: self.source_ip.clone().unwrap_or_default(),
            protocol: Protocol::HttpJson,
            raw: bytes::Bytes::from(raw),
        };
        // Audit must never block the request path or push back on user
        // inserts; if the WAL channel is saturated we drop the audit event.
        // Audit is supplementary — the operator's primary access trail lives
        // in any reverse-proxy logs in front. Drops are counted so the loss
        // is observable (central_logs_audit_dropped_total).
        if let Err(e) = self.handle.inner.try_append_unacked(record) {
            self.handle.counters.record_audit_drop();
            tracing::warn!(
                action = %self.action,
                error = %e,
                "audit event dropped (WAL saturated or closed)"
            );
        }
    }
}

/// Render the audit event as a JSON envelope compatible with the ingest
/// parser (see `apply_json_object`).
///
/// The fields are flattened into the top-level JSON object — NOT nested
/// under an `attributes` key — because the parser's schema-inference loop
/// treats any non-reserved top-level key as a structured attribute and
/// stuffs it into `row.attributes`. So `action`, `outcome`, `actor_key_id`,
/// etc. land directly in the queryable JSON column without a level of
/// nesting. The parser's RESERVED set (`ts`, `service`, `level`, `msg`,
/// `host`, ...) is honoured so the standard log columns get populated.
fn serialize_event(
    ts: chrono::DateTime<chrono::Utc>,
    action: &str,
    level: &str,
    actor_key_id: Option<i64>,
    actor_key_name: Option<&str>,
    actor_via: Option<&str>,
    source_ip: Option<&str>,
    outcome: &str,
    extra: &[(&'static str, Value)],
) -> Vec<u8> {
    let mut envelope = serde_json::Map::new();
    envelope.insert("ts".into(), json!(ts.to_rfc3339()));
    envelope.insert("service".into(), json!(AUDIT_SERVICE));
    envelope.insert("level".into(), json!(level));
    envelope.insert("msg".into(), json!(action));
    envelope.insert(
        "host".into(),
        json!(source_ip.unwrap_or("internal")),
    );
    // Audit-specific fields — the parser will collect these into
    // `row.attributes` automatically.
    envelope.insert("action".into(), json!(action));
    envelope.insert("outcome".into(), json!(outcome));
    envelope.insert("category".into(), json!(action_category(action)));
    if let Some(id) = actor_key_id {
        envelope.insert("actor_key_id".into(), json!(id));
    }
    if let Some(name) = actor_key_name {
        envelope.insert("actor_key_name".into(), json!(name));
    }
    if let Some(via) = actor_via {
        envelope.insert("actor_via".into(), json!(via));
    }
    if let Some(ip) = source_ip {
        envelope.insert("source_ip".into(), json!(ip));
    }
    for (k, v) in extra {
        envelope.insert((*k).into(), v.clone());
    }
    serde_json::to_vec(&Value::Object(envelope)).unwrap_or_else(|_| b"{}".to_vec())
}

/// Group an action under a coarse category for cheap filtering
/// (`action_category:auth` etc.). Keeps the attribute set small even when
/// the operator hasn't promoted `action` itself.
fn action_category(action: &str) -> &'static str {
    match action.split('.').next().unwrap_or("") {
        "auth" => "auth",
        "apikey" => "apikey",
        "dashboard" => "dashboard",
        "alert" => "alert",
        "ai" => "ai",
        "view" => "view",
        _ => "other",
    }
}

/// Resolve a client IP from request headers, honouring the standard proxy
/// chains (Cloudflare, nginx/traefik, RFC 7239 `Forwarded`, de facto
/// `X-Forwarded-For`), with a socket fallback.
///
/// **Precedence** (first match wins, top to bottom):
///
/// 1. `CF-Connecting-IP` — single IP, set only by Cloudflare's edge. Trust
///    this unconditionally ONLY if the server is reachable through Cloudflare;
///    otherwise a client could spoof it.
/// 2. `X-Real-IP` — single IP, conventionally set by the closest reverse
///    proxy (nginx `proxy_set_header X-Real-IP $remote_addr;`).
/// 3. `Forwarded` — RFC 7239. Parses the first `for=` parameter, stripping
///    the optional `[` `]` brackets and port (`for=192.0.2.60:47028`).
/// 4. `X-Forwarded-For` — comma-separated chain appended by each proxy hop.
///    We take the **leftmost** entry, which is the original client — but note
///    this is **spoofable** unless the front-most proxy strips/overwrites it.
///    For chains where every hop is trusted, consider this good enough; for
///    untrusted networks, future work should add a CIDR allowlist
///    (`--trusted-proxy-cidr`) and walk backwards from the right.
/// 5. Socket address from `ConnectInfo<SocketAddr>` — the literal TCP peer.
///    This is the direct connection (your reverse proxy's address when one
///    is in front) and the only value an attacker cannot fake.
///
/// Empty / malformed values are skipped, not returned. If nothing resolves,
/// returns an empty string.
pub fn resolve_peer(headers: &axum::http::HeaderMap, fallback: Option<std::net::SocketAddr>) -> String {
    // 1. CF-Connecting-IP (Cloudflare).
    if let Some(ip) = first_header(headers, "cf-connecting-ip") {
        return ip;
    }
    // 2. X-Real-IP (closest trusted proxy).
    if let Some(ip) = first_header(headers, "x-real-ip") {
        return ip;
    }
    // 3. Forwarded (RFC 7239): parse the first `for=...` parameter.
    if let Some(ip) = parse_forwarded(headers) {
        return ip;
    }
    // 4. X-Forwarded-For: leftmost entry of the chain.
    if let Some(ip) = first_xff_entry(headers) {
        return ip;
    }
    // 5. Socket fallback.
    fallback
        .map(|sa| sa.ip().to_string())
        .unwrap_or_default()
}

/// Look up a single-value header by name (case-insensitive) and return its
/// trimmed value if non-empty. HeaderMap lookups are case-insensitive by
/// design, so we don't need to canonicalize.
fn first_header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?;
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Parse the leftmost `for=` parameter out of an RFC 7239 `Forwarded`
/// header. Handles obfuscated identifiers (`_hidden`, `h1.example.org`) by
/// returning None for them. Strips bracket `[...]` and `:port` suffixes.
fn parse_forwarded(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get("forwarded")?.to_str().ok()?;
    // Each Forwarded value is a semicolon-separated list of forwarded-pairs.
    // We want the first `for=...` we can resolve to an IP.
    for pair in raw.split(';') {
        for kv in pair.split(',').map(str::trim) {
            if let Some(rest) = kv.strip_prefix("for=").or_else(|| kv.strip_prefix("For=")) {
                let val = rest.trim().trim_matches('"');
                let ip = strip_brackets_and_port(val);
                if is_plausible_ip(&ip) {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// Strip `[...]` brackets (IPv6 syntax in Forwarded) and a trailing `:port`.
/// Recognises both `[ip]:port` and `[ip]` forms per RFC 7239.
fn strip_brackets_and_port(s: &str) -> String {
    let s = s.trim();
    // Bracketed form: `[ip]` or `[ip]:port`.
    if let Some(inner) = s.strip_prefix('[') {
        if let Some(rest) = inner.strip_suffix(']') {
            // Pure `[ip]` form.
            return rest.to_string();
        }
        // `[ip]:port` form — find the closing bracket.
        if let Some(end) = inner.find(']') {
            return inner[..end].to_string();
        }
    }
    // Unbracketed IPv4 with optional :port (single colon → port separator).
    if let Some(idx) = s.rfind(':') {
        let after = &s[idx + 1..];
        if after.chars().all(|c| c.is_ascii_digit()) && s.matches(':').count() == 1 {
            return s[..idx].to_string();
        }
    }
    s.to_string()
}

/// Cheap sanity check: is this string shaped like an IPv4 or IPv6 literal?
/// Used to skip obfuscated Forwarded identifiers (`_hidden`, hostnames).
fn is_plausible_ip(s: &str) -> bool {
    s.parse::<std::net::IpAddr>().is_ok()
}

/// First non-empty entry from `X-Forwarded-For` (comma-separated chain).
fn first_xff_entry(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get("x-forwarded-for")?.to_str().ok()?;
    for entry in raw.split(',').map(str::trim) {
        if !entry.is_empty() {
            return Some(entry.to_string());
        }
    }
    None
}

/// Axum extractor: pulls the resolved client IP from request headers +
/// `ConnectInfo` socket fallback. Handlers add `ResolvedPeer(String)` as
/// a parameter and use it for audit logging instead of calling
/// `resolve_peer` directly.
///
/// Order of extractors in the handler signature: any time after `State<_>`
/// and other `FromRequestParts` extractors, before any body-consuming
/// extractor. ConnectInfo must be present (i.e. the server must be started
/// with `into_make_service_with_connect_info::<SocketAddr>()`); if it isn't,
/// we fall back to an empty string gracefully.
#[derive(Debug, Clone)]
pub struct ResolvedPeer(pub String);

impl<S> axum::extract::FromRequestParts<S> for ResolvedPeer
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::extract::ConnectInfo;
        let fallback = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0);
        Ok(Self(resolve_peer(&parts.headers, fallback)))
    }
}

/// Re-export so the module is usable from anywhere with one path.
pub use crate::web::auth::AuthInfo;

#[allow(dead_code)]
fn _touch_arc<T>(_v: Arc<T>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse_event_json(raw: &[u8]) -> Value {
        serde_json::from_slice(raw).expect("audit envelope should be valid JSON")
    }

    #[test]
    fn envelope_shape_matches_ingest_parser() {
        let raw = serialize_event(
            chrono::Utc::now(),
            "auth.login.success",
            "info",
            Some(7),
            Some("ci-reader"),
            Some("bearer"),
            Some("10.0.0.5"),
            "success",
            &[("filter", json!("service:api"))],
        );
        let v = parse_event_json(&raw);
        // Top-level standard log fields — the parser's RESERVED set picks
        // these out into typed DuckDB columns.
        assert_eq!(v["service"], "central-logs");
        assert_eq!(v["level"], "info");
        assert_eq!(v["msg"], "auth.login.success");
        assert_eq!(v["host"], "10.0.0.5");
        // Audit-specific fields are flattened into the top level — the
        // parser's schema-inference loop collects them into `row.attributes`
        // (the JSON column) without a level of nesting.
        assert_eq!(v["action"], "auth.login.success");
        assert_eq!(v["outcome"], "success");
        assert_eq!(v["category"], "auth");
        assert_eq!(v["actor_key_id"], 7);
        assert_eq!(v["actor_key_name"], "ci-reader");
        assert_eq!(v["actor_via"], "bearer");
        assert_eq!(v["source_ip"], "10.0.0.5");
        assert_eq!(v["filter"], "service:api");
    }

    #[test]
    fn failed_event_flips_outcome_and_warn_level() {
        let raw = serialize_event(
            chrono::Utc::now(),
            "auth.login.failed",
            "warn",
            None,
            None,
            None,
            Some("10.0.0.5"),
            "failure",
            &[("reason", json!("invalid api key"))],
        );
        let v = parse_event_json(&raw);
        assert_eq!(v["level"], "warn");
        assert_eq!(v["outcome"], "failure");
        assert_eq!(v["reason"], "invalid api key");
    }

    #[test]
    fn raw_credentials_never_leak_into_envelope() {
        // Sanity: the actor_* fields carry identity only, never the raw key.
        let raw = serialize_event(
            chrono::Utc::now(),
            "apikey.create",
            "info",
            Some(1),
            Some("bootstrap-admin"),
            Some("cookie"),
            None,
            "success",
            &[("new_key_id", json!(42)), ("new_key_prefix", json!("abc12345"))],
        );
        let s = std::str::from_utf8(&raw).unwrap();
        // The raw key prefix in this test ("abc12345") is intentionally
        // present (it's a non-secret display prefix), but raw key MATERIAL
        // would never be added by the builder.
        assert!(!s.contains("clk_"), "raw key material must not appear");
        assert!(!s.contains("Bearer "), "Authorization header must not appear");
        assert!(!s.contains("cl_session="), "session cookie value must not appear");
    }

    #[test]
    fn categories_partition_actions() {
        assert_eq!(action_category("auth.login.success"), "auth");
        assert_eq!(action_category("apikey.create"), "apikey");
        assert_eq!(action_category("dashboard.update"), "dashboard");
        assert_eq!(action_category("alert.approve"), "alert");
        assert_eq!(action_category("ai.query"), "ai");
        assert_eq!(action_category("view.logs"), "view");
        assert_eq!(action_category("unknown.thing"), "other");
    }

    #[test]
    fn peer_resolution_prefers_cf_connecting_ip() {
        // Cloudflare in front of nginx: all three headers present, but
        // CF-Connecting-IP is the canonical source.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cf-connecting-ip", "203.0.113.5".parse().unwrap());
        headers.insert("x-real-ip", "10.0.0.99".parse().unwrap());
        headers.insert(
            "x-forwarded-for",
            "203.0.113.5, 10.0.0.99".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "203.0.113.5");
    }

    #[test]
    fn peer_resolution_uses_x_real_ip_when_no_cf() {
        // Plain nginx reverse proxy without Cloudflare.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-real-ip", "198.51.100.7".parse().unwrap());
        headers.insert("x-forwarded-for", "198.51.100.7".parse().unwrap());
        assert_eq!(resolve_peer(&headers, None), "198.51.100.7");
    }

    #[test]
    fn peer_resolution_uses_rfc7239_forwarded() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "forwarded",
            "for=192.0.2.60;proto=http;by=203.0.113.43".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "192.0.2.60");
    }

    #[test]
    fn peer_resolution_forwarded_strips_port_and_brackets() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "forwarded",
            "for=192.0.2.60:47028".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "192.0.2.60");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "forwarded",
            "for=\"[2001:db8::1]:3960\"".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "2001:db8::1");
    }

    #[test]
    fn peer_resolution_forwarded_skips_obfuscated() {
        // RFC 7239 allows obfuscated identifiers — we must not return them
        // as IPs; fall through to the next header instead.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("forwarded", "for=_hidden".parse().unwrap());
        headers.insert("x-forwarded-for", "192.0.2.99".parse().unwrap());
        assert_eq!(resolve_peer(&headers, None), "192.0.2.99");
    }

    #[test]
    fn peer_resolution_xff_takes_leftmost() {
        // Multi-hop XFF chain — leftmost is the original client.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.5, 10.0.0.1, 10.0.0.2".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "203.0.113.5");
    }

    #[test]
    fn peer_resolution_xff_skips_empty_entries() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "  , 203.0.113.5 ".parse().unwrap(),
        );
        assert_eq!(resolve_peer(&headers, None), "203.0.113.5");
    }

    #[test]
    fn peer_resolution_falls_back_to_socket_addr() {
        let headers = axum::http::HeaderMap::new();
        let fallback: Option<std::net::SocketAddr> = "127.0.0.1:5555".parse().ok();
        assert_eq!(resolve_peer(&headers, fallback), "127.0.0.1");
    }

    #[test]
    fn peer_resolution_empty_when_nothing_available() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(resolve_peer(&headers, None), "");
    }

    #[test]
    fn peer_resolution_precedence_cf_beats_real_ip_beats_forwarded_beats_xff() {
        // All four headers present — CF-Connecting-IP wins.
        let mut h = axum::http::HeaderMap::new();
        h.insert("cf-connecting-ip", "1.1.1.1".parse().unwrap());
        h.insert("x-real-ip", "2.2.2.2".parse().unwrap());
        h.insert("forwarded", "for=3.3.3.3".parse().unwrap());
        h.insert("x-forwarded-for", "4.4.4.4".parse().unwrap());
        assert_eq!(resolve_peer(&h, None), "1.1.1.1");

        // CF removed — X-Real-IP wins.
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-real-ip", "2.2.2.2".parse().unwrap());
        h.insert("forwarded", "for=3.3.3.3".parse().unwrap());
        h.insert("x-forwarded-for", "4.4.4.4".parse().unwrap());
        assert_eq!(resolve_peer(&h, None), "2.2.2.2");

        // CF + X-Real-IP removed — Forwarded wins.
        let mut h = axum::http::HeaderMap::new();
        h.insert("forwarded", "for=3.3.3.3".parse().unwrap());
        h.insert("x-forwarded-for", "4.4.4.4".parse().unwrap());
        assert_eq!(resolve_peer(&h, None), "3.3.3.3");

        // Only XFF left.
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", "4.4.4.4".parse().unwrap());
        assert_eq!(resolve_peer(&h, None), "4.4.4.4");
    }
}
