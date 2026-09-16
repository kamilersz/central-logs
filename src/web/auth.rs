//! API-key + session authentication (OWASP A01 + A07).
//!
//! Two complementary surfaces:
//!
//! 1. **CRUD-able API keys** (per source / client). Stored as SHA-256 hashes
//!    in the `api_keys` DuckDB table; the raw token is shown to the operator
//!    exactly once at creation time. Each key carries a comma-separated
//!    `scopes` string drawn from `insert`, `read`, `write`, `admin`.
//! 2. **Browser sessions** for the dashboard UI. `POST /api/auth/login`
//!    exchanges an API key for a random `cl_session` cookie, so the SPA
//!    never has to hold the long-lived raw token in JavaScript.
//!
//! Three credential forms are accepted on every request, in order:
//!   - `Cookie: cl_session=<sid>` (browser flow)
//!   - `Authorization: Bearer <api-key>` (API client flow)
//!   - `X-API-Key: <api-key>` (client convenience)
//!
//! A configured-but-not-CRUD `http_api_key` (the legacy single-static-key
//! mode) is honoured as an implicit `admin`-scoped key, so existing
//! deployments keep working. New deployments should use CRUD keys instead.
//!
//! Key hashing uses SHA-256; comparison happens against the precomputed hash
//! in the in-memory cache (constant-time at the byte level via `subtle`),
//! so the DuckDB file is never consulted on the hot path.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::Engine;
use parking_lot::RwLock;
use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Cookie name carrying the browser session id.
pub const SESSION_COOKIE: &str = "cl_session";
/// Prefix tag on raw API-key strings, so a leaked-looking string is easy to
/// grep for and so future token formats can coexist (`clp_` for personal, etc.)
pub const KEY_PREFIX_TAG: &str = "clk_";

// =====================================================================
// Scopes
// =====================================================================

/// Permission bits carried by an authenticated identity. Ordering goes
/// weakest → strongest; `Admin` implies all the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Insert = 1,
    Read = 2,
    Write = 4,
    Admin = 8,
}

impl Scope {
    /// Parse a comma-separated scope string (as stored in the DB). Unknown
    /// tokens are dropped — the `admin` grants all lower scopes implicitly.
    pub fn parse_list(s: &str) -> u8 {
        let mut bits: u8 = 0;
        for tok in s.split(',').map(str::trim).map(str::to_ascii_lowercase) {
            match tok.as_str() {
                "insert" => bits |= Scope::Insert as u8,
                "read" => bits |= Scope::Read as u8,
                "write" => bits |= Scope::Write as u8,
                "admin" => bits |= Scope::Admin as u8,
                _ => {}
            }
        }
        bits
    }

    /// Render a scope bitmask back to a stable comma-separated string.
    pub fn format_bits(bits: u8) -> String {
        let mut out: Vec<&str> = Vec::new();
        if bits & Scope::Admin as u8 != 0 {
            out.push("admin");
        }
        if bits & Scope::Write as u8 != 0 {
            out.push("write");
        }
        if bits & Scope::Read as u8 != 0 {
            out.push("read");
        }
        if bits & Scope::Insert as u8 != 0 {
            out.push("insert");
        }
        out.join(",")
    }

    /// Admin implies read/write/insert. We expand the mask so simple
    /// `contains` checks work.
    pub fn expand(bits: u8) -> u8 {
        let mut b = bits;
        if b & Scope::Admin as u8 != 0 {
            b |= Scope::Write as u8 | Scope::Read as u8 | Scope::Insert as u8;
        }
        if b & Scope::Write as u8 != 0 {
            b |= Scope::Read as u8;
        }
        b
    }
}

// =====================================================================
// ApiKey record (in-memory cache mirror of the DB row)
// =====================================================================

/// Public projection of an API key — never carries the raw key, only its hash.
/// This is what lives in the auth cache and what the list endpoint returns.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    /// First ~8 chars of the public part, e.g. `aB3xK9pQ`. For UI display so
    /// operators can tell keys apart without seeing the secret.
    pub key_prefix: String,
    /// Bitmask of `Scope` bits.
    pub scopes: u8,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ApiKey {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// A live browser session, keyed by random cookie value.
#[derive(Debug, Clone)]
pub struct Session {
    pub key_id: i64,
    pub key_name: String,
    pub scopes: u8,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Identity attached to a request that survived the auth middleware.
/// Handlers pull this from request extensions to learn who's calling.
#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub key_id: Option<i64>,
    pub key_name: String,
    pub scopes: u8,
    /// Which credential form was used — useful for audit logging.
    pub via: AuthVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVia {
    Cookie,
    Bearer,
    StaticAdminKey,
}

impl AuthInfo {
    pub fn has_scope(&self, s: Scope) -> bool {
        Scope::expand(self.scopes) & s as u8 != 0
    }
}

// =====================================================================
// AuthState — threaded through the router as middleware state
// =====================================================================

#[derive(Clone)]
pub struct AuthState {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    /// key_hash → ApiKey. Mutated by CRUD endpoints, read by the middleware.
    keys: RwLock<HashMap<String, ApiKey>>,
    /// session_id (random hex) → Session.
    sessions: RwLock<HashMap<String, Session>>,
    /// Legacy single static admin key (from `Config::http_api_key`). Empty
    /// when unset. Accepted as an implicit admin-scoped credential.
    static_admin_key: Option<String>,
    /// When true, NO auth is enforced (back-compat for loopback dev). False
    /// when at least one credential source is configured.
    auth_enabled: bool,
    /// Optional DuckDB handle used to persist per-key `last_used_at` touches.
    /// `None` in tests and auth-less builds.
    store: Option<crate::store::Store>,
    /// key id → last-recorded touch, throttling `last_used_at` writes to one
    /// UPDATE per key per minute.
    key_touch: parking_lot::Mutex<HashMap<i64, chrono::DateTime<chrono::Utc>>>,
}

impl AuthState {
    /// Build an empty state. Used in tests.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(AuthInner {
                keys: RwLock::new(HashMap::new()),
                sessions: RwLock::new(HashMap::new()),
                static_admin_key: None,
                auth_enabled: false,
                store: None,
                key_touch: parking_lot::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Build with a preloaded key set + optional static admin key.
    pub fn new(keys: Vec<ApiKey>, static_admin_key: Option<String>) -> Self {
        Self::with_store(keys, static_admin_key, None)
    }

    /// Build with a preloaded key set, optional static admin key, and an
    /// optional store handle for persisting `last_used_at` touches.
    pub fn with_store(
        keys: Vec<ApiKey>,
        static_admin_key: Option<String>,
        store: Option<crate::store::Store>,
    ) -> Self {
        let auth_enabled = !keys.is_empty() || static_admin_key.is_some();
        let map: HashMap<String, ApiKey> =
            keys.into_iter().filter(|k| k.is_active()).map(|k| (k.key_hash.clone(), k)).collect();
        Self {
            inner: Arc::new(AuthInner {
                keys: RwLock::new(map),
                sessions: RwLock::new(HashMap::new()),
                static_admin_key,
                auth_enabled,
                store,
                key_touch: parking_lot::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn auth_enabled(&self) -> bool {
        self.inner.auth_enabled
    }

    /// Upsert a key into the cache (called after the DB write).
    pub fn put_key(&self, k: ApiKey) {
        if k.is_active() {
            self.inner.keys.write().insert(k.key_hash.clone(), k);
        }
    }

    /// Mark a key revoked in the cache (called after the DB UPDATE).
    pub fn revoke_key(&self, key_hash: &str) {
        if let Some(k) = self.inner.keys.write().get_mut(key_hash) {
            k.revoked_at = Some(chrono::Utc::now());
        }
        self.inner.keys.write().remove(key_hash);
    }

    /// Snapshot of all keys (admin "list" endpoint).
    pub fn list_keys(&self) -> Vec<ApiKey> {
        let mut v: Vec<ApiKey> = self.inner.keys.read().values().cloned().collect();
        v.sort_by_key(|k| k.id);
        v
    }

    /// Resolve a raw token (cookie session id, bearer, or X-API-Key value)
    /// to an identity. Returns the resolved identity, or None if no
    /// credential matched.
    fn resolve(&self, req: &Request) -> Option<AuthInfo> {
        // 1) Cookie session.
        if let Some(sid) = cookie_session(req) {
            if let Some(sess) = self.inner.sessions.read().get(&sid).cloned() {
                return Some(AuthInfo {
                    key_id: Some(sess.key_id),
                    key_name: sess.key_name,
                    scopes: Scope::expand(sess.scopes),
                    via: AuthVia::Cookie,
                });
            }
        }
        // 2) Bearer / X-API-Key.
        if let Some(raw) = bearer_or_x_api_key(req) {
            if let Some(info) = self.resolve_raw_token(&raw, AuthVia::Bearer) {
                return Some(info);
            }
        }
        // 3) Sentry-SDK credential forms: `X-Sentry-Auth: Sentry
        //    sentry_key=...` header or `?sentry_key=` query param (browser
        //    SDKs can't set custom headers — error tracking ingest).
        if let Some(raw) = sentry_key_from_request(req) {
            if let Some(info) = self.resolve_raw_token(&raw, AuthVia::Bearer) {
                return Some(info);
            }
        }
        None
    }

    /// Resolve a raw token (static admin key or hashed CRUD key) to an
    /// identity. Shared by the bearer and sentry-key extraction paths.
    fn resolve_raw_token(&self, raw: &str, via: AuthVia) -> Option<AuthInfo> {
        // Static admin key short-circuit (don't hash unknown tokens
        // against it — direct constant-time compare).
        if let Some(static_key) = &self.inner.static_admin_key {
            if raw.as_bytes().ct_eq(static_key.as_bytes()).into() {
                return Some(AuthInfo {
                    key_id: None,
                    key_name: "static-admin".into(),
                    scopes: Scope::expand(Scope::Admin as u8),
                    via: AuthVia::StaticAdminKey,
                });
            }
        }
        // CRUD key — hash and look up. Clone out of the cache and drop the read
        // guard before `touch_key`, which takes the write lock to refresh the
        // cached `last_used_at` (holding the read guard here would deadlock).
        let h = hash_token(raw);
        let hit = self.inner.keys.read().get(&h).cloned();
        if let Some(k) = hit {
            self.touch_key(k.id);
            return Some(AuthInfo {
                key_id: Some(k.id),
                key_name: k.name,
                scopes: Scope::expand(k.scopes),
                via,
            });
        }
        None
    }

    /// Record that a CRUD key was just used, persisting `last_used_at`.
    /// Throttled to one UPDATE per key per minute so high-rate ingest paths
    /// don't hammer DuckDB; the write runs on a blocking thread so the auth
    /// hot path never waits on the store lock.
    fn touch_key(&self, key_id: i64) {
        let Some(store) = self.inner.store.clone() else { return };
        let now = chrono::Utc::now();
        {
            let mut t = self.inner.key_touch.lock();
            if let Some(prev) = t.get(&key_id) {
                if (now - *prev).num_seconds() < 60 {
                    return;
                }
            }
            t.insert(key_id, now);
        }
        // Refresh the in-memory cache too, so `GET /v1/api-keys` (and the SPA
        // "Last used" column) show the touch immediately rather than only after
        // the next restart. Runs at the same 1/min cadence as the DB write, so
        // the first use in a window appears within ~1s.
        if let Some(k) = self.inner.keys.write().values_mut().find(|k| k.id == key_id) {
            k.last_used_at = Some(now);
        }
        tracing::info!(key_id, "recording api key last_used_at touch");
        let _ = tokio::runtime::Handle::try_current().map(|rt| {
            rt.spawn_blocking(move || {
                let conn = store.conn();
                let c = conn.lock();
                if let Err(e) = c.execute(
                    "UPDATE api_keys SET last_used_at = ? WHERE id = ?",
                    duckdb::params![chrono::Utc::now(), key_id],
                ) {
                    tracing::warn!(key_id, error = %e, "api_keys last_used_at update failed");
                }
            });
        });
    }

    /// Issue a new browser session bound to the given key. Returns the random
    /// session id (to be set as a cookie).
    pub fn create_session(&self, key: &ApiKey) -> String {
        let sid = random_hex(32);
        let sess = Session {
            key_id: key.id,
            key_name: key.name.clone(),
            scopes: key.scopes,
            created_at: chrono::Utc::now(),
        };
        self.inner.sessions.write().insert(sid.clone(), sess);
        sid
    }

    /// Drop the session with this id (no-op if absent).
    pub fn drop_session(&self, sid: &str) {
        self.inner.sessions.write().remove(sid);
    }

    /// Look up an active key by its raw token. Used by `/api/auth/login`.
    pub fn lookup_raw(&self, raw: &str) -> Option<ApiKey> {
        if let Some(static_key) = &self.inner.static_admin_key {
            if raw.as_bytes().ct_eq(static_key.as_bytes()).into() {
                return Some(ApiKey {
                    id: 0,
                    name: "static-admin".into(),
                    key_hash: String::new(),
                    key_prefix: "static".into(),
                    scopes: Scope::Admin as u8,
                    created_at: chrono::Utc::now(),
                    last_used_at: None,
                    revoked_at: None,
                });
            }
        }
        let h = hash_token(raw);
        self.inner.keys.read().get(&h).cloned()
    }
}

// =====================================================================
// Middleware
// =====================================================================

/// axum middleware: enforce auth + required scope on non-public paths.
///
/// Public paths (`/login`, `/api/auth/login`, `/health`) bypass entirely.
/// Browser requests to `/` (HTML) without auth are redirected to `/login`;
/// everything else gets a 401 JSON so API clients have a clear signal.
pub async fn require_auth(State(st): State<AuthState>, req: Request, next: Next) -> Response {
    if !st.auth_enabled() {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    if is_public_path(&path) {
        return next.run(req).await;
    }
    // CORS preflight for the Sentry-SDK ingest endpoints: browsers don't
    // send credentials on preflights, so these must answer without auth.
    if req.method() == axum::http::Method::OPTIONS && is_sentry_ingest_path(&path) {
        return next.run(req).await;
    }
    let Some(info) = st.resolve(&req) else {
        return auth_challenge(&req);
    };
    // Scope enforcement by (method, path).
    if let Some(required) = required_scope(req.method(), &path) {
        if !info.has_scope(required) {
            return forbidden(required);
        }
    }
    // Attach identity for downstream handlers (whoami, audit).
    let mut req = req;
    req.extensions_mut().insert(info);
    next.run(req).await
}

/// True for the Sentry-SDK ingest routes: `/api/{project}/envelope[/]` and
/// `/api/{project}/store[/]`. `project` maps to the `service` column
/// (error tracking, docs/ERROR_TRACKING.md).
pub fn is_sentry_ingest_path(path: &str) -> bool {
    let Some(rest) = path.trim_end_matches('/').strip_prefix("/api/") else {
        return false;
    };
    let mut segs = rest.split('/');
    let project = segs.next().unwrap_or("");
    let endpoint = segs.next().unwrap_or("");
    !project.is_empty()
        && project != "error-groups"
        && (endpoint == "envelope" || endpoint == "store")
        && segs.next().is_none()
}

/// Decide which scope a (method, path) requires. `None` = any authenticated
/// identity is acceptable. This is the single source of truth for the
/// authorization policy — audit it when changing routes.
pub fn required_scope(method: &axum::http::Method, path: &str) -> Option<Scope> {
    // Insert path (write a key, push logs).
    if path == "/v1/logs" || path == "/v1/logs/bulk" {
        return Some(Scope::Insert);
    }
    // OTLP/HTTP ingest (OpenTelemetry). Same trust level as /v1/logs: an
    // insert-scoped key must be supplied by the SDK, e.g.
    // `OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer clk_..."`.
    if path == "/v1/traces" || path == "/v1/metrics" {
        return Some(Scope::Insert);
    }
    // Sentry-SDK ingest: writing errors, same trust level as /v1/logs.
    if is_sentry_ingest_path(path) {
        return Some(Scope::Insert);
    }
    // Housekeeping: backup + restore move real bytes / replace state.
    if path == "/api/ops/backup" || path == "/api/ops/restore" {
        return Some(Scope::Admin);
    }
    // Error-group lifecycle mutations (resolve/unresolve/ignore).
    if path.starts_with("/api/error-groups/")
        && (path.ends_with("/resolve")
            || path.ends_with("/unresolve")
            || path.ends_with("/ignore"))
    {
        return Some(Scope::Write);
    }
    // API-key management.
    if path == "/v1/api-keys" || path.starts_with("/v1/api-keys/") {
        return Some(Scope::Admin);
    }
    // Alert approval / rejection — high blast radius, admin only.
    if path.starts_with("/api/alert-rules/") && (path.ends_with("/approve") || path.ends_with("/reject")) {
        return Some(Scope::Admin);
    }
    // Alert rule + notification-channel mutations (create/update/delete/test).
    // Channels carry secrets (SMTP creds context, bot tokens) — admin only.
    if path == "/api/alert-rules" || path == "/api/alert-channels" {
        if method != axum::http::Method::GET {
            return Some(Scope::Admin);
        }
    }
    if (path.starts_with("/api/alert-rules/") || path.starts_with("/api/alert-channels/"))
        && method != axum::http::Method::GET
    {
        return Some(Scope::Admin);
    }
    // Mutating dashboard config writes.
    if path.starts_with("/api/dashboard/configs") && method != axum::http::Method::GET {
        return Some(Scope::Write);
    }
    // Everything else under /api or /metrics is read.
    if path.starts_with("/api/") || path == "/metrics" {
        return Some(Scope::Read);
    }
    // SPA / static / unknown — any identity is fine (or rejected upstream).
    None
}

fn auth_challenge(req: &Request) -> Response {
    // Browser nav to a page → redirect to login. Heuristic: accepts text/html
    // OR no Accept header at all (curl defaults to */*, but a browser nav
    // always advertises text/html).
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/html"))
        .unwrap_or(false);
    if wants_html {
        return Redirect::to("/login").into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "invalid or missing API key"})),
    )
        .into_response()
}

fn forbidden(required: Scope) -> Response {
    let need = match required {
        Scope::Insert => "insert",
        Scope::Read => "read",
        Scope::Write => "write",
        Scope::Admin => "admin",
    };
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": format!("insufficient scope: requires '{need}'")})),
    )
        .into_response()
}

// =====================================================================
// Public paths — bypass auth entirely
// =====================================================================

pub fn is_public_path(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    // NOTE: `/` is intentionally NOT here — that's the SPA shell, which is
    // behind the auth wall so unauthenticated browsers get redirected to
    // `/login`. The login page itself + the login-submit endpoint + the
    // health check are the only unauthenticated surfaces.
    matches!(p, "/login" | "/health" | "/api/auth/login")
}

// =====================================================================
// Credential extraction
// =====================================================================

/// Pull the `cl_session` cookie value out of `Cookie:` header, if any.
fn cookie_session(req: &Request) -> Option<String> {
    let h = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in h.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Pull the candidate token out of `Authorization: Bearer <k>` (case-insensitive
/// scheme) or `X-API-Key: <k>`. Returns the raw key string.
fn bearer_or_x_api_key(req: &Request) -> Option<String> {
    const BEARER_PREFIX: &str = "bearer "; // 7 ASCII bytes
    if let Some(h) = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if h.to_ascii_lowercase().starts_with(BEARER_PREFIX) {
            // SAFE: prefix is 7 ASCII bytes, all valid UTF-8 char boundaries.
            let token = h[BEARER_PREFIX.len()..].trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    if let Some(h) = req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) {
        let token = h.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

/// Pull the Sentry-SDK credential: `X-Sentry-Auth: Sentry sentry_key=<k>, ...`
/// header or `?sentry_key=<k>` query parameter (the form browser SDKs use,
/// since they cannot set custom headers cross-origin).
fn sentry_key_from_request(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().get("x-sentry-auth").and_then(|v| v.to_str().ok()) {
        // Format: `Sentry sentry_key=<k>, sentry_timestamp=..., sentry_version=7`
        // — strip the leading auth-scheme token, then parse comma-separated
        // key=value pairs.
        let mut s = h.trim();
        if s.len() > 7 && s[..7].eq_ignore_ascii_case("sentry ") {
            s = s[7..].trim_start();
        }
        for part in s.split(',') {
            let part = part.trim();
            let Some(eq) = part.find('=') else { continue };
            if part[..eq].trim() == "sentry_key" {
                let v = part[eq + 1..].trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("sentry_key=") {
                let decoded = percent_decode(v);
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Minimal percent-decoding (SDKs may URL-encode DSN key characters).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build the `Set-Cookie` header value for a session id. `max_age_secs=None`
/// makes it a session cookie (cleared when the browser closes); pass a value
/// for persistent cookies.
pub fn session_cookie_header(sid: &str, max_age_secs: Option<i64>) -> String {
    let base = format!(
        "{SESSION_COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/"
    );
    match max_age_secs {
        Some(s) => format!("{base}; Max-Age={s}"),
        None => base,
    }
}

/// `Set-Cookie: cl_session=; Max-Age=0` — used by /api/auth/logout.
pub fn clear_session_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

// =====================================================================
// Token hashing + generation
// =====================================================================

/// SHA-256 hex of a raw token. Used as the cache key and the persisted value.
pub fn hash_token(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    hex_encode(&h.finalize())
}

/// Generate a new raw token: `clk_` + base64url(32 random bytes).
pub fn generate_token() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    format!(
        "{KEY_PREFIX_TAG}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    )
}

/// The user-facing prefix of a raw token (first 8 chars after the tag), so the
/// list endpoint can show `aB3xK9pQ…` without leaking the secret.
pub fn token_display_prefix(raw: &str) -> String {
    let after_tag = raw.strip_prefix(KEY_PREFIX_TAG).unwrap_or(raw);
    after_tag.chars().take(8).collect()
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    OsRng.fill_bytes(&mut buf);
    hex_encode(&buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// =====================================================================
// DuckDB loaders (called once at startup; take a &Connection so they don't
// introduce a layering dep on `Store`)
// =====================================================================

/// Load every non-revoked API key into the in-memory auth cache. Called once
/// at startup; subsequent mutations go through the CRUD endpoints which
/// update both DB and cache. Returns an empty vec if the table doesn't exist
/// yet (which shouldn't happen — `apply_schema` creates it — but we tolerate
/// it so the server can boot on a fresh DB without race conditions).
pub fn load_api_keys(conn: &duckdb::Connection) -> Vec<ApiKey> {
    let sql = "SELECT id, name, key_hash, key_prefix, scopes, created_at, last_used_at, revoked_at \
               FROM api_keys WHERE revoked_at IS NULL";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |row| {
        let scopes_str: String = row.get(4)?;
        Ok(ApiKey {
            id: row.get(0)?,
            name: row.get(1)?,
            key_hash: row.get(2)?,
            key_prefix: row.get(3)?,
            scopes: Scope::parse_list(&scopes_str),
            created_at: row.get(5)?,
            last_used_at: row.get(6)?,
            revoked_at: row.get(7)?,
        })
    });
    match rows {
        Ok(rs) => rs.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// True if the `api_keys` table has zero rows (active or revoked). Used by
/// the bootstrap-admin-token logic at startup.
pub fn api_keys_table_is_empty(conn: &duckdb::Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get::<_, i64>(0))
        .map(|n| n == 0)
        .unwrap_or(true)
}

/// Insert a bootstrap admin key and return its raw form (so the caller can
/// print it once). Used only on first run when no keys exist yet.
pub fn insert_bootstrap_admin_key(conn: &duckdb::Connection, name: &str) -> crate::Result<String> {
    let raw = generate_token();
    let hash = hash_token(&raw);
    let prefix = token_display_prefix(&raw);
    let scopes = Scope::format_bits(Scope::Admin as u8);
    conn.execute(
        "INSERT INTO api_keys (id, name, key_hash, key_prefix, scopes) \
         VALUES (nextval('api_keys_id_seq'), ?, ?, ?, ?)",
        duckdb::params![name, &hash, &prefix, &scopes],
    )?;
    Ok(raw)
}

#[allow(dead_code)]
fn _ensure_headermaps_used(_: HeaderMap) {}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};
    use axum::middleware::from_fn_with_state;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    fn sample_key(name: &str, scopes: u8) -> (String, ApiKey) {
        let raw = generate_token();
        let h = hash_token(&raw);
        let k = ApiKey {
            id: 1,
            name: name.into(),
            key_hash: h.clone(),
            key_prefix: token_display_prefix(&raw),
            scopes,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            revoked_at: None,
        };
        (raw, k)
    }

    fn router_with(state: AuthState) -> Router {
        Router::new()
            .route("/", get(|| async { "spa" }))
            .route("/api/logs", get(|| async { "ok" }))
            .route("/v1/logs", post(|| async { "inserted" }))
            .route("/v1/api-keys", get(|| async { "keys-list" }))
            .route("/login", get(|| async { "login-page" }))
            .layer(from_fn_with_state(state, require_auth))
    }

    #[test]
    fn scope_parse_round_trip() {
        let bits = Scope::parse_list("read, insert ,admin, bogus");
        assert!(bits & Scope::Read as u8 != 0);
        assert!(bits & Scope::Insert as u8 != 0);
        assert!(bits & Scope::Admin as u8 != 0);
        // Admin dominates.
        let expanded = Scope::expand(bits);
        assert!(expanded & Scope::Write as u8 != 0);
        let s = Scope::format_bits(bits);
        assert!(s.contains("admin"));
        assert!(s.contains("read"));
    }

    #[test]
    fn token_hash_is_deterministic_and_distinct() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "tokens should not collide");
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert!(a.starts_with(KEY_PREFIX_TAG));
    }

    #[test]
    fn display_prefix_does_not_leak_full_token() {
        let raw = generate_token();
        let p = token_display_prefix(&raw);
        assert!(p.len() <= 8);
        // Prefix should be a substring of the raw token, but the raw token
        // should be much longer than the prefix.
        assert!(raw.contains(&p));
        assert!(raw.len() > p.len() + 10);
    }

    #[tokio::test]
    async fn middleware_rejects_unauthenticated_when_enabled() {
        let (raw, k) = sample_key("admin", Scope::Admin as u8);
        let _ = raw;
        let app = router_with(AuthState::new(vec![k], None));

        // No auth → 401 (Accept: json keeps it from redirecting).
        let res = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .header("accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_redirects_browser_to_login() {
        let (_, k) = sample_key("admin", Scope::Admin as u8);
        let app = router_with(AuthState::new(vec![k], None));

        // Browser nav (accepts text/html) → 302 to /login.
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/")
                    .header("accept", "text/html,application/xhtml+xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(loc, "/login");
    }

    #[tokio::test]
    async fn middleware_accepts_bearer_with_sufficient_scope() {
        let (raw, k) = sample_key("reader", Scope::Read as u8);
        let app = router_with(AuthState::new(vec![k], None));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .header("authorization", format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_persists_last_used_at_for_crud_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &dir.path().join("test.duckdb"),
            dir.path().join("parquet"),
            vec![],
        )
        .unwrap();
        // Seed the CRUD key row the cached ApiKey points at (sample_key id=1).
        {
            let conn = store.conn();
            let c = conn.lock();
            c.execute(
                "INSERT INTO api_keys (id, name, key_hash, key_prefix, scopes) \
                 VALUES (1, 'reader', 'deadbeef', 'deadbeef', 1)",
                duckdb::params![],
            )
            .unwrap();
        }

        let (raw, k) = sample_key("reader", Scope::Read as u8);
        let app = router_with(AuthState::with_store(vec![k], None, Some(store.clone())));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .header("authorization", format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The touch is async (spawn_blocking); poll briefly for the write.
        let mut last_used = None;
        for _ in 0..50 {
            {
                let conn = store.conn();
                let c = conn.lock();
                let mut stmt = c.prepare("SELECT last_used_at FROM api_keys WHERE id = 1").unwrap();
                last_used = stmt.query_row([], |row| row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(0)).unwrap();
            }
            if last_used.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(last_used.is_some(), "last_used_at was never written");
    }

    #[tokio::test]
    async fn middleware_refreshes_cached_last_used_at_for_listing() {
        // Regression: the touch used to update only DuckDB, while
        // `GET /v1/api-keys` serves the startup in-memory cache, so "Last used"
        // stayed empty until a restart.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &dir.path().join("test.duckdb"),
            dir.path().join("parquet"),
            vec![],
        )
        .unwrap();
        {
            let conn = store.conn();
            let c = conn.lock();
            c.execute(
                "INSERT INTO api_keys (id, name, key_hash, key_prefix, scopes) \
                 VALUES (1, 'reader', 'deadbeef', 'deadbeef', 1)",
                duckdb::params![],
            )
            .unwrap();
        }

        let (raw, k) = sample_key("reader", Scope::Read as u8);
        let auth = AuthState::with_store(vec![k], None, Some(store.clone()));
        let app = router_with(auth.clone());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .header("authorization", format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The cache refresh is synchronous in `touch_key`, so the listing must
        // reflect it without waiting for the spawn_blocking DB write.
        let listed = auth.list_keys();
        assert!(
            listed.iter().any(|k| k.last_used_at.is_some()),
            "cached last_used_at was not refreshed for the listing"
        );
    }

    #[tokio::test]
    async fn middleware_rejects_insufficient_scope_on_insert() {
        // Read-only key cannot insert.
        let (raw, k) = sample_key("reader", Scope::Read as u8);
        let app = router_with(AuthState::new(vec![k], None));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/v1/logs")
                    .header("authorization", format!("Bearer {raw}"))
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn middleware_accepts_cookie_session() {
        let (raw, k) = sample_key("admin", Scope::Admin as u8);
        let st = AuthState::new(vec![k], None);
        let sid = st.create_session(&st.lookup_raw(&raw).unwrap());
        let app = router_with(st);

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_passes_through_when_disabled() {
        let app = router_with(AuthState::empty());

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_paths_bypass_auth() {
        let (_, k) = sample_key("admin", Scope::Admin as u8);
        let app = router_with(AuthState::new(vec![k], None));

        for path in ["/login"] {
            let res = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "public path {path} gated");
        }
    }

    #[tokio::test]
    async fn static_admin_key_accepted_as_admin() {
        let st = AuthState::new(vec![], Some("legacy-static-key".into()));
        let app = router_with(st);

        // Static key works on admin route.
        let res = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/api-keys")
                    .header("authorization", "Bearer legacy-static-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Wrong key rejected.
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/api-keys")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn constant_time_compare_through_subtle() {
        // ct_eq on equal-length slices returns 1 (true).
        let a = b"abcdef";
        let b = b"abcdef";
        let eq: bool = a.ct_eq(b).into();
        assert!(eq);
        let c = b"abcdefX";
        let ne: bool = a.ct_eq(c).into();
        assert!(!ne);
    }
}
