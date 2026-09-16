//! Auth + API-key management routes (OWASP A01/A07).
//!
//! Two surface areas, both mounted at the root of the merged router:
//!
//! - **Browser flow** — `GET /login` (server-rendered HTML form, public),
//!   `POST /api/auth/login` (exchange API key for `cl_session` cookie,
//!   public), `POST /api/auth/logout`, `GET /api/auth/whoami`.
//! - **API-key CRUD** — `GET /v1/api-keys`, `POST /v1/api-keys`,
//!   `DELETE /v1/api-keys/{id}`. Admin-scoped only (enforced by the auth
//!   middleware via [`crate::web::auth::required_scope`]).
//!
//! Raw keys are persisted as SHA-256 hashes; the plaintext surfaces exactly
//! once, in the JSON body of `POST /v1/api-keys`, and is never retrievable
//! after that response.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use askama::Template;
use serde::{Deserialize, Serialize};

use crate::audit::{AuditHandle, ResolvedPeer};
use crate::store::Store;
use crate::web::auth::{
    clear_session_cookie_header, generate_token, hash_token, session_cookie_header,
    token_display_prefix, AuthInfo, AuthState, ApiKey, Scope, SESSION_COOKIE,
};
use crate::web::templates::LoginTemplate;

/// State threaded through every auth + api-key handler. Carries the auth
/// cache, the DuckDB connection for write-through, and the audit handle for
/// self-logging.
#[derive(Clone)]
pub struct AuthApiState {
    pub auth: AuthState,
    pub store: Store,
    pub audit: AuditHandle,
}

pub fn router(state: AuthApiState) -> Router {
    Router::new()
        // Browser login flow.
        .route("/login", get(login_page).post(login_submit_redirect))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/whoami", get(auth_whoami))
        // API-key CRUD (admin-only — enforced by middleware scope check).
        .route("/v1/api-keys", get(list_api_keys).post(create_api_key))
        .route("/v1/api-keys/{id}", axum::routing::delete(revoke_api_key))
        .with_state(state)
}

// =====================================================================
// Login page (HTML) + form-post handler
// =====================================================================

/// `GET /login` — render the sign-in page (no error banner).
async fn login_page() -> Html<String> {
    let tmpl = LoginTemplate { error: String::new() };
    Html(tmpl.render().unwrap_or_default())
}

/// `POST /login` (form-encoded fallback for browsers with JS disabled).
/// Accepts the same form fields as the JSON endpoint, sets the session
/// cookie on success, and redirects to `/`. On failure re-renders the
/// page with an error.
async fn login_submit_redirect(
    State(st): State<AuthApiState>,
    headers: HeaderMap,
    ResolvedPeer(peer): ResolvedPeer,
    body: String,
) -> Response {
    // Parse application/x-www-form-urlencoded body minimally.
    let api_key = body
        .split('&')
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == "api_key" {
                Some(percent_decode(v))
            } else {
                None
            }
        })
        .unwrap_or_default();
    let _ = &headers; // peer already resolved via ResolvedPeer extractor
    match st.auth.lookup_raw(&api_key) {
        Some(k) if k.is_active() => {
            let sid = st.auth.create_session(&k);
            let mut h = HeaderMap::new();
            h.insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&session_cookie_header(&sid, None)).unwrap(),
            );
            st.audit
                .event("auth.login.success")
                .actor_key(k.id, &k.name)
                .via("cookie")
                .source_ip(&peer)
                .emit();
            (StatusCode::SEE_OTHER, [(header::LOCATION, "/")], h).into_response()
        }
        other => {
            let reason = match &other {
                Some(k) if !k.is_active() => "revoked key",
                _ => "unknown key",
            };
            st.audit
                .event("auth.login.failed")
                .failed(reason)
                .source_ip(&peer)
                .emit();
            let tmpl = LoginTemplate { error: "Invalid or revoked API key.".into() };
            (StatusCode::UNAUTHORIZED, Html(tmpl.render().unwrap_or_default())).into_response()
        }
    }
}

// =====================================================================
// JSON auth endpoints
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub session_id: String,
    pub key_name: String,
    pub scopes: Vec<String>,
}

/// `POST /api/auth/login` — JSON. Returns the session id in the body AND
/// sets the `cl_session` cookie, so the same handler serves both
/// browser-driven fetches (which auto-send cookies) and clients that
/// prefer to read the session id from the JSON body.
async fn auth_login(
    State(st): State<AuthApiState>,
    ResolvedPeer(peer): ResolvedPeer,
    Json(req): Json<LoginRequest>,
) -> Response {
    match st.auth.lookup_raw(req.api_key.trim()) {
        Some(k) if k.is_active() => {
            let sid = st.auth.create_session(&k);
            let mut out_headers = HeaderMap::new();
            out_headers.insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&session_cookie_header(&sid, None)).unwrap(),
            );
            st.audit
                .event("auth.login.success")
                .actor_key(k.id, &k.name)
                .via("cookie")
                .source_ip(&peer)
                .emit();
            (
                StatusCode::OK,
                out_headers,
                Json(LoginResponse {
                    session_id: sid,
                    key_name: k.name,
                    scopes: scope_names(k.scopes),
                }),
            )
                .into_response()
        }
        other => {
            let reason = match &other {
                Some(k) if !k.is_active() => "revoked key",
                _ => "unknown key",
            };
            st.audit
                .event("auth.login.failed")
                .failed(reason)
                .source_ip(&peer)
                .emit();
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid or revoked API key"})),
            )
                .into_response()
        }
    }
}

/// `POST /api/auth/logout` — drop the calling session from the cache and
/// clear the cookie.
async fn auth_logout(
    State(st): State<AuthApiState>,
    headers: HeaderMap,
    ResolvedPeer(peer): ResolvedPeer,
    info: MaybeAuthInfo,
) -> Response {
    if let Some(sid) = session_id_from_headers(&headers) {
        st.auth.drop_session(&sid);
    }
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_session_cookie_header()).unwrap(),
    );
    let mut ev = st.audit.event("auth.logout").source_ip(&peer);
    if let Some(i) = &info.0 {
        ev = ev.actor(i);
    }
    ev.emit();
    (StatusCode::OK, h, Json(serde_json::json!({"status": "logged out"}))).into_response()
}

/// `GET /api/auth/whoami` — returns the calling identity. Used by the SPA
/// on boot to decide whether to render the app or bounce to /login.
async fn auth_whoami(info: MaybeAuthInfo) -> Response {
    match &info.0 {
        Some(i) => Json(serde_json::json!({
            "key_id": i.key_id,
            "name":   i.key_name,
            "scopes": scope_names(i.scopes),
            "via":    format!("{:?}", i.via).to_ascii_lowercase(),
        }))
        .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "not authenticated"})),
        )
            .into_response(),
    }
}

// =====================================================================
// API-key CRUD (admin only — enforced by middleware scope rule)
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    /// Comma-separated scope list: `insert,read,write,admin`.
    /// Unknown tokens are dropped; default is `read`.
    #[serde(default)]
    pub scopes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateApiKeyResponse {
    /// The raw token. ONLY returned here; never retrievable later.
    pub key: String,
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiKeyOut {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl From<&ApiKey> for ApiKeyOut {
    fn from(k: &ApiKey) -> Self {
        ApiKeyOut {
            id: k.id,
            name: k.name.clone(),
            key_prefix: k.key_prefix.clone(),
            scopes: scope_names(k.scopes),
            created_at: k.created_at.to_rfc3339(),
            last_used_at: k.last_used_at.map(|t| t.to_rfc3339()),
            revoked_at: k.revoked_at.map(|t| t.to_rfc3339()),
        }
    }
}

async fn list_api_keys(State(st): State<AuthApiState>) -> Response {
    let out: Vec<ApiKeyOut> = st.auth.list_keys().iter().map(ApiKeyOut::from).collect();
    Json(out).into_response()
}

/// Insert a new key into DuckDB + the in-memory cache. Returns the raw key
/// exactly once.
async fn create_api_key(
    State(st): State<AuthApiState>,
    info: MaybeAuthInfo,
    Json(body): Json<CreateApiKeyRequest>,
) -> Response {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name must be 1..=128 chars"})),
        )
            .into_response();
    }
    let scopes_str = body.scopes.as_deref().unwrap_or("read");
    let bits = Scope::parse_list(scopes_str);
    if bits == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "no valid scopes; pick from insert,read,write,admin"})),
        )
            .into_response();
    }
    let raw = generate_token();
    let hash = hash_token(&raw);
    let prefix = token_display_prefix(&raw);
    let created_at = chrono::Utc::now();
    let conn = st.store.conn();
    let conn = conn.lock();
    let sql = "INSERT INTO api_keys (id, name, key_hash, key_prefix, scopes) \
               VALUES (nextval('api_keys_id_seq'), ?, ?, ?, ?) RETURNING id";
    let id: i64 = match conn.query_row(
        sql,
        duckdb::params![name, &hash, &prefix, Scope::format_bits(bits)],
        |r| r.get(0),
    ) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("db: {e}")})),
            )
                .into_response()
        }
    };
    drop(conn);
    let k = ApiKey {
        id,
        name: name.to_string(),
        key_hash: hash,
        key_prefix: prefix.clone(),
        scopes: bits,
        created_at,
        last_used_at: None,
        revoked_at: None,
    };
    st.auth.put_key(k);
    st.audit
        .event("apikey.create")
        .actor(info.0.as_ref().expect("admin scope enforced by middleware"))
        .field("new_key_id", id)
        .field("new_key_name", name)
        .field("new_key_prefix", prefix.as_str())
        .field("new_key_scopes", Scope::format_bits(bits))
        .emit();
    Json(CreateApiKeyResponse {
        key: raw,
        id,
        name: name.to_string(),
        key_prefix: prefix,
        scopes: scope_names(bits),
    })
    .into_response()
}

/// Soft-revoke: set `revoked_at` and drop from cache. The row is retained
/// for audit.
async fn revoke_api_key(
    State(st): State<AuthApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> Response {
    let conn = st.store.conn();
    let conn = conn.lock();
    // Find the key_hash before UPDATE so we can drop it from the cache.
    let hash: Option<String> = conn
        .query_row(
            "SELECT key_hash FROM api_keys WHERE id = ?",
            [id],
            |r| r.get(0),
        )
        .ok();
    // Also grab the name for the audit record (best-effort).
    let target_name: Option<String> = conn
        .query_row("SELECT name FROM api_keys WHERE id = ?", [id], |r| r.get(0))
        .ok();
    let _ = conn.execute(
        "UPDATE api_keys SET revoked_at = CURRENT_TIMESTAMP WHERE id = ? AND revoked_at IS NULL",
        [id],
    );
    drop(conn);
    if let Some(h) = hash {
        st.auth.revoke_key(&h);
    }
    st.audit
        .event("apikey.revoke")
        .actor(info.0.as_ref().expect("admin scope enforced by middleware"))
        .field("target_key_id", id)
        .field("target_key_name", target_name.unwrap_or_default())
        .emit();
    Json(serde_json::json!({"id": id, "status": "revoked"})).into_response()
}

// =====================================================================
// Helpers
// =====================================================================

/// Extractor that pulls the `AuthInfo` extension out of request extensions.
/// Handlers using this MUST be behind the auth middleware (otherwise the
/// extension is absent — surfaced as `None`, NOT an error, so handlers can
/// decide whether to require auth or tolerate anonymous calls).
///
/// We use `Infallible` as the rejection so `Option<MaybeAuthInfo>` is never
/// needed — the extractor itself carries the missing-auth case as `None`.
#[derive(Debug, Clone, Default)]
pub struct MaybeAuthInfo(pub Option<AuthInfo>);

impl<S> axum::extract::FromRequestParts<S> for MaybeAuthInfo
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<AuthInfo>().cloned()))
    }
}

fn scope_names(bits: u8) -> Vec<String> {
    let mut v = Vec::new();
    let expanded = Scope::expand(bits);
    if expanded & Scope::Admin as u8 != 0 { v.push("admin".into()); }
    if expanded & Scope::Write as u8 != 0 { v.push("write".into()); }
    if expanded & Scope::Read as u8 != 0 { v.push("read".into()); }
    if expanded & Scope::Insert as u8 != 0 { v.push("insert".into()); }
    v
}

fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                &std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_spaces_and_hex() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("clk_abc+def"), "clk_abc def");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn scope_names_expands_admin() {
        let names = scope_names(Scope::Admin as u8);
        assert!(names.contains(&"admin".to_string()));
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"write".to_string()));
    }
}

// Keep the unused Redirect import warning silenced — Redirect is used in
// other modules that pull from `auth`, and `auth` re-exports nothing here.
#[allow(dead_code)]
fn _touch_unused() {
    let _ = SESSION_COOKIE;
}
