//! First-party website-analytics collector (PLAN-WEB-ANALYTICS.md, W1).
//!
//! - `POST /v1/web/collect` — public same-origin beacon endpoint. One beacon
//!   becomes ONE WAL record (`Protocol::WebEvent`) carrying a JSON envelope
//!   with N validated browser events; ingest expands it into `web_events`.
//! - `GET /v1/web/t.js` — the embedded tracker script.
//!
//! The hot path does cheap work only, in this order:
//! DNT/GPC → rate limit → WAL shed → size/shape validation → site+origin
//! check → IP resolution (trusted-proxy aware) → envelope → `append_acked`.
//! Bad input answers an empty `202` (never leaks which site ids exist);
//! `413`/`429`/`503` are reserved for real limits (size, rate, load).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use crate::config::{WebConfig, WebSiteConfig};
use crate::insert::counters::InsertCountersRef;
use crate::store::Store;
use crate::wal::writer::WalGauges;
use crate::wal::InsertHandle;
use crate::{Protocol, RawRecord};

/// Embedded tracker script (hand-written, no build step).
pub const TRACKER_JS: &str = include_str!("../../tracker/tracker.js");

/// Maximum event-name length accepted from browsers.
const MAX_NAME_LEN: usize = 128;
/// Maximum path+query length accepted (paths beyond this are truncated).
const MAX_PATH_LEN: usize = 2048;
/// Maximum referrer length accepted.
const MAX_REFERRER_LEN: usize = 2048;
/// Maximum serialized `props` size per event.
const MAX_PROPS_BYTES: usize = 4096;
/// Maximum `dt` (client-claimed age of the event) — older than a day is bogus.
const MAX_DT_MS: i64 = 24 * 3600 * 1000;
/// How long an idle rate-limit bucket stays cached before cleanup.
const RATE_BUCKET_TTL_SECS: u64 = 600;
/// Maximum tracked IPs in the rate-limit table (bounded memory).
const RATE_BUCKET_CAP: usize = 65_536;

#[derive(Clone)]
pub struct WebState {
    pub handle: InsertHandle,
    pub counters: InsertCountersRef,
    pub backpressure_timeout: Duration,
    pub cfg: Arc<WebConfig>,
    /// Runtime project registry: config `[[web.site]]` seeds merged with
    /// DB-created `web_sites` rows. The hot path looks up here (memory only,
    /// never the DuckDB mutex).
    pub sites: WebSites,
    /// Pre-parsed trusted-proxy CIDRs.
    pub trusted: Arc<Vec<crate::config::Cidr>>,
    /// WAL channel gauges for the shed check — analytics is shed first.
    pub wal: Arc<WalGauges>,
    /// Set by the WAL-cap monitor; web beacons are refused while paused.
    pub ingest_paused: Arc<std::sync::atomic::AtomicBool>,
    pub rate: Arc<RateLimiter>,
    /// Store handle used ONLY for trust-on-first-use auto-provisioning (first
    /// beacon of an unknown project id). `None` disables persistence (tests):
    /// the project is still registered in memory. The steady-state hot path
    /// never touches this — lookups stay lock-only, and the mutex is taken
    /// solely for unknown ids (already bounded by the per-IP rate limiter).
    pub auto_create_store: Option<Store>,
}

impl WebState {
    pub fn new(
        handle: InsertHandle,
        counters: InsertCountersRef,
        backpressure_timeout: Duration,
        cfg: Arc<WebConfig>,
        sites: WebSites,
        wal: Arc<WalGauges>,
        ingest_paused: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let trusted = Arc::new(cfg.parse_trusted_proxies());
        let rate = Arc::new(RateLimiter::new(cfg.rate_limit_per_ip_per_min));
        Self {
            handle,
            counters,
            backpressure_timeout,
            cfg,
            sites,
            trusted,
            wal,
            ingest_paused,
            rate,
            auto_create_store: None,
        }
    }

    /// Enable first-beacon auto-provisioning persistence (production wiring
    /// in `main.rs`). Chain after [`WebState::new`].
    pub fn with_auto_create_store(mut self, store: Store) -> Self {
        self.auto_create_store = Some(store);
        self
    }
}

/// In-memory web-analytics project registry shared between the collector hot
/// path and the CRUD API (PLAN-WEB-ANALYTICS.md W5).
///
/// Seeded from config `[[web.site]]` at startup, then merged with `web_sites`
/// DB rows (DB wins on id clash so an admin can override origins without a
/// restart). Mutations go through the CRUD API which writes the DB row first
/// and only then updates the map — the hot path stays lock-free-ish (a
/// parking_lot read lock, no I/O).
#[derive(Clone, Default)]
pub struct WebSites {
    inner: Arc<RwLock<std::collections::HashMap<String, WebSiteConfig>>>,
}

impl WebSites {
    /// Seed from config. DB rows are merged later via [`WebSites::load_from_db`].
    pub fn seeded(cfg: &WebConfig) -> Self {
        let map = cfg
            .site
            .iter()
            .map(|s| (s.id.clone(), s.clone()))
            .collect();
        Self {
            inner: Arc::new(RwLock::new(map)),
        }
    }

    /// Hot-path lookup: clone the matching project, if any.
    pub fn lookup(&self, id: &str) -> Option<WebSiteConfig> {
        self.inner.read().get(id).cloned()
    }

    /// Insert or replace a project (CRUD API, after the DB write).
    pub fn upsert(&self, site: WebSiteConfig) {
        self.inner.write().insert(site.id.clone(), site);
    }

    /// Remove a project (CRUD API, after the DB delete). Returns the removed
    /// entry, if it was present.
    pub fn remove(&self, id: &str) -> Option<WebSiteConfig> {
        self.inner.write().remove(id)
    }

    /// Snapshot of all projects, sorted by id (CRUD list endpoint).
    pub fn list(&self) -> Vec<WebSiteConfig> {
        let mut v: Vec<WebSiteConfig> = self.inner.read().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Merge `web_sites` DB rows over the config seeds. Returns the number of
    /// rows loaded. Missing table → 0 (tolerated so fresh DBs boot).
    pub fn load_from_db(&self, conn: &duckdb::Connection) -> usize {
        // `exclude_ips` may be absent on pre-migration tables: probe for
        // the column once and fall back to the 3-column shape.
        let has_exclude: bool = conn
            .prepare("SELECT exclude_ips FROM web_sites LIMIT 0")
            .is_ok();
        let sql = if has_exclude {
            "SELECT id, allowed_origins, user_header, exclude_ips FROM web_sites"
        } else {
            "SELECT id, allowed_origins, user_header FROM web_sites"
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        // Row shape varies (see above); missing exclude_ips → empty list.
        let rows: Vec<(String, String, Option<String>, Vec<String>)> = match stmt.query_map(
            [],
            |row| {
                let exclude: Vec<String> = if has_exclude {
                    row.get::<_, String>(3)
                        .ok()
                        .and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    exclude,
                ))
            },
        ) {
            Ok(r) => r.filter_map(Result::ok).collect(),
            Err(_) => return 0,
        };
        let mut n = 0usize;
        for (id, origins_json, user_header, exclude_ips) in rows {
            if id.trim().is_empty() || id.len() > 64 {
                continue;
            }
            let origins: Vec<String> =
                serde_json::from_str(&origins_json).unwrap_or_default();
            // Skip rows that would fail validation (defensive: the CRUD
            // handler validates on write, but hand-edited DBs happen).
            if crate::config::validate_web_site(&id, &origins, user_header.as_deref(), &exclude_ips)
                .is_err()
            {
                tracing::warn!(id, "web_sites row failed validation; skipping");
                continue;
            }
            self.upsert(WebSiteConfig {
                id,
                allowed_origins: origins,
                user_header,
                exclude_ips,
            });
            n += 1;
        }
        n
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/v1/web/collect", post(handle_collect))
        .route("/v1/web/t.js", get(handle_tracker_js))
        .with_state(state)
}

// =====================================================================
// GET /v1/web/t.js
// =====================================================================

async fn handle_tracker_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        TRACKER_JS,
    )
        .into_response()
}

// =====================================================================
// POST /v1/web/collect
// =====================================================================

async fn handle_collect(
    State(st): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 0) WAL-cap paused → refuse before doing any work.
    if st.ingest_paused.load(std::sync::atomic::Ordering::Relaxed) {
        st.counters.record_web_rejected("paused");
        return empty(StatusCode::SERVICE_UNAVAILABLE);
    }

    // 1) DNT / GPC: accept but do not store (privacy, plan §W1).
    if st.cfg.honor_dnt {
        if let Some(v) = header_str(&headers, "dnt") {
            if v.trim() == "1" {
                st.counters.record_web_rejected("dnt");
                return empty(StatusCode::ACCEPTED);
            }
        }
        if let Some(v) = header_str(&headers, "sec-gpc") {
            if v.trim() == "1" {
                st.counters.record_web_rejected("gpc");
                return empty(StatusCode::ACCEPTED);
            }
        }
    }

    // 2) Resolve the client IP once (trusted-proxy aware, anti-spoof).
    let client_ip = resolve_client_ip(peer, &headers, &st.trusted);

    // 3) Rate limit per client IP.
    if !st.rate.allow(client_ip) {
        st.counters.record_web_rejected("rate_limited");
        return empty(StatusCode::TOO_MANY_REQUESTS);
    }

    // 4) Shed: keep log ingest healthy first (plan §W1, decision D2).
    let (depth, capacity) = (st.wal.depth(), st.wal.capacity());
    if capacity > 0
        && depth >= 0
        && (depth as f64 / capacity as f64) * 100.0
            >= st.cfg.shed_when_wal_fill_pct as f64
    {
        st.counters.record_web_rejected("shed");
        return empty(StatusCode::SERVICE_UNAVAILABLE);
    }

    // 5) Parse + validate the beacon (size was capped by the route layer).
    let beacon = match parse_beacon(&body, &st.cfg) {
        Ok(b) => b,
        Err(reason) => {
            st.counters.record_web_rejected(reason);
            // Empty 202: never confirm or deny which sites/origins are valid.
            return empty(StatusCode::ACCEPTED);
        }
    };

    // 6) Site + origin check (from memory only — never the DuckDB mutex).
    //    Unknown ids are auto-provisioned on first contact when enabled
    //    (TOFU): the beacon's own origin becomes the project's first allowed
    //    origin, so embedding the snippet is sufficient — no config+restart.
    let site = match st.sites.lookup(&beacon.site) {
        Some(s) => s,
        None => match maybe_auto_create(&st, &beacon.site, &headers) {
            Some(s) => s,
            None => {
                st.counters.record_web_rejected("unknown_site");
                return empty(StatusCode::ACCEPTED);
            }
        },
    };
    if !origin_allowed(&headers, &site) {
        st.counters.record_web_rejected("origin_denied");
        return empty(StatusCode::ACCEPTED);
    }

    // 7) Build the envelope (decision D4): context rides inside `raw`; the
    //    already-resolved client IP rides in `source_addr`.
    let envelope = build_envelope(&st, &site, &beacon, &headers, client_ip);
    let raw = match serde_json::to_vec(&envelope) {
        Ok(v) => v,
        Err(_) => {
            st.counters.record_web_rejected("bad_payload");
            return empty(StatusCode::ACCEPTED);
        }
    };

    let rec = RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: client_ip.map(|ip| ip.to_string()).unwrap_or_default(),
        protocol: Protocol::WebEvent,
        raw: bytes::Bytes::from(raw),
    };

    // 8) Durability ack, same backpressure contract as /v1/logs.
    match tokio::time::timeout(
        st.backpressure_timeout,
        st.handle.append_acked(rec),
    )
    .await
    {
        Ok(Ok(())) => {
            st.counters
                .record_n(Protocol::WebEvent, beacon.events.len(), body.len() as u64);
            empty(StatusCode::ACCEPTED)
        }
        _ => {
            st.counters.record_web_rejected("wal_append_failed");
            empty(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

fn empty(status: StatusCode) -> Response {
    (status, "").into_response()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// =====================================================================
// IP resolution (trusted proxies)
// =====================================================================

/// Resolve the real client IP. The direct TCP peer is the default; hops from
/// `X-Forwarded-For` are only believed when the peer itself is a trusted
/// proxy (walking right-to-left, skipping trusted hops — anti-spoof per
/// plan §4.1).
///
/// Cloudflare-fronted deployments: when the peer is trusted and Cloudflare
/// sent `CF-Connecting-IP`, it wins over XFF. Rationale: CF sets that header
/// to the eyeball client IP at the edge, while XFF as seen by us is whatever
/// the last proxy wrote — commonly `$remote_addr`, i.e. a *rotating
/// Cloudflare edge IP* that changes per connection and would otherwise mint
/// a fresh visitor hash per beacon (fragmenting visitors/sessions).
pub fn resolve_client_ip(
    peer: SocketAddr,
    headers: &HeaderMap,
    trusted: &[crate::config::Cidr],
) -> Option<IpAddr> {
    let peer_ip = peer.ip();    let peer_trusted = trusted.iter().any(|c| c.contains(peer_ip));
    if peer_trusted {
        // Most authoritative first: Cloudflare's edge-stamped client IP.
        if let Some(cf) = header_str(headers, "cf-connecting-ip") {
            let cf = cf.trim();
            if !cf.is_empty() {
                if let Ok(ip) = cf.parse::<IpAddr>() {
                    if !trusted.iter().any(|c| c.contains(ip)) {
                        return Some(ip);
                    }
                    // A trusted-proxy IP in CF-Connecting-IP means the header
                    // wasn't stamped by Cloudflare for this hop — fall through
                    // to the XFF walk rather than attributing to a proxy.
                }
                // Unparseable CF-Connecting-IP: fall through to XFF rather
                // than rejecting the beacon outright.
            }
        }
        if let Some(xff) = header_str(headers, "x-forwarded-for") {
            for hop in xff.split(',').rev() {
                let hop = hop.trim();
                if hop.is_empty() {
                    continue;
                }
                if let Ok(ip) = hop.parse::<IpAddr>() {
                    if !trusted.iter().any(|c| c.contains(ip)) {
                        return Some(ip);
                    }
                } else {
                    break; // malformed hop → don't trust the rest either
                }
            }
        }
    }
    if peer_ip.is_unspecified() {
        None
    } else {
        Some(peer_ip)
    }
}

// =====================================================================
// Origin check
// =====================================================================

fn origin_allowed(headers: &HeaderMap, site: &WebSiteConfig) -> bool {
    if site.allowed_origins.is_empty() {
        return false; // a site with no origins accepts nothing (explicit opt-in)
    }
    let Some(candidate) = origin_candidate(headers) else {
        return false;
    };
    // "*" accepts any present origin (see validator). A missing Origin/
    // Referer is still rejected — beacons must come from a browser-ish
    // context, not a bare socket.
    if site.allowed_origins.iter().any(|o| o.trim() == "*") {
        return true;
    }
    site.allowed_origins
        .iter()
        .any(|o| o.trim().trim_end_matches('/') == candidate)
}

/// The beacon's claimed origin: the `Origin` header, falling back to the
/// `Referer`'s scheme://host. `None` when neither is present or usable.
fn origin_candidate(headers: &HeaderMap) -> Option<String> {
    let candidate = header_str(headers, "origin")
        .map(|o| o.trim().trim_end_matches('/').to_string())
        .or_else(|| {
            header_str(headers, "referer").map(|r| {
                let r = r.trim();
                // origin = scheme://host[:port]
                let mut end = 0;
                let mut slashes = 0;
                for (i, c) in r.char_indices() {
                    if c == '/' {
                        slashes += 1;
                        if slashes == 3 {
                            end = i;
                            break;
                        }
                    }
                    end = i + c.len_utf8();
                }
                r[..end.min(r.len())].trim_end_matches('/').to_string()
            })
        })?;
    if candidate.is_empty() {
        return None;
    }
    Some(candidate)
}

/// Trust-on-first-use auto-provisioning for unknown project ids.
///
/// Returns the (possibly just-created) project, or `None` when the beacon
/// must be rejected as `unknown_site`. Creation requires ALL of:
/// - `auto_create_projects` enabled,
/// - a well-formed id (same rules as config/API),
/// - a usable `Origin`/`Referer` (it becomes the first allowed origin —
///   first origin wins, later origins are allowlisted against it),
/// - headroom under `auto_create_max_projects`.
///
/// The DB row is written before the registry update (as in the CRUD API);
/// a duplicate-key race (two first beacons at once) falls back to a
/// re-lookup. Fail-closed: any validation/DB problem rejects the beacon.
fn maybe_auto_create(
    st: &WebState,
    id: &str,
    headers: &HeaderMap,
) -> Option<WebSiteConfig> {
    if !st.cfg.auto_create_projects {
        return None;
    }
    let id = id.trim().to_string();
    let Some(origin) = origin_candidate(headers) else {
        return None;
    };
    if crate::config::validate_web_site(&id, &[origin.clone()], None, &[]).is_err() {
        return None;
    }
    if st.sites.list().len() >= st.cfg.auto_create_max_projects.max(1) {
        tracing::warn!(id, "web auto-create cap reached; rejecting beacon");
        return None;
    }
    let origins_json = serde_json::to_string(&vec![origin.clone()]).unwrap_or_else(|_| "[]".into());
    if let Some(store) = &st.auto_create_store {
        let conn = store.lock();
        match conn.execute(
            "INSERT INTO web_sites (id, allowed_origins, user_header) VALUES (?, ?, NULL)",
            duckdb::params![id, origins_json],
        ) {
            Ok(_) => {}
            Err(_) => {
                // Most likely a duplicate-key race: someone created it
                // concurrently — re-read the registry and proceed if present.
                drop(conn);
                return st.sites.lookup(&id);
            }
        }
    }
    let site = WebSiteConfig {
        id: id.clone(),
        allowed_origins: vec![origin],
        user_header: None,
        exclude_ips: Vec::new(),
    };
    st.sites.upsert(site.clone());
    tracing::info!(id, "web-analytics project auto-created on first beacon (TOFU)");
    Some(site)
}

// =====================================================================
// Beacon parsing / validation
// =====================================================================

/// A validated browser event (wire shape, plan §3.1). Serialized verbatim
/// into the WAL envelope — field names ARE the wire format.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BeaconEvent {
    pub e: String,
    pub n: Option<String>,
    pub u: Option<String>,
    pub r: Option<String>,
    pub v: Option<f64>,
    pub w: Option<u16>,
    pub sd: Option<f64>,
    pub dt: i64,
    pub p: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Beacon {
    pub site: String,
    pub tp: Option<String>,
    pub events: Vec<BeaconEvent>,
}

/// Validate the wire format. Returns `Err(reason)` with the rejection reason
/// so callers can count it. Deliberately strict: length caps, name pattern,
/// numeric ranges — the ingest side re-checks nothing.
pub fn parse_beacon(body: &[u8], cfg: &WebConfig) -> Result<Beacon, &'static str> {
    if body.is_empty() || body.len() > cfg.max_beacon_bytes {
        return Err("bad_payload");
    }
    let text = std::str::from_utf8(body).map_err(|_| "bad_payload")?;
    let v: Value = serde_json::from_str(text).map_err(|_| "bad_payload")?;
    let site = v
        .get("s")
        .and_then(Value::as_str)
        .ok_or("bad_payload")?
        .trim()
        .to_string();
    if site.is_empty() || site.len() > 64 {
        return Err("bad_payload");
    }
    let ev = v.get("ev").and_then(Value::as_array).ok_or("bad_payload")?;
    if ev.is_empty() || ev.len() > cfg.max_events_per_beacon {
        return Err("too_many_events");
    }
    let tp = match v.get("tp").and_then(Value::as_str) {
        Some(tp) if is_traceparent(tp) => Some(tp.to_string()),
        Some(_) => None, // malformed → drop silently rather than reject
        None => None,
    };

    let mut events = Vec::with_capacity(ev.len());
    for e in ev {
        let kind = e
            .get("e")
            .and_then(Value::as_str)
            .ok_or("bad_payload")?
            .trim();
        if !is_valid_event_kind(kind) {
            return Err("bad_payload");
        }
        let n = match e.get("n").and_then(Value::as_str) {
            Some(n) if n.is_empty() => None,
            Some(n) => {
                if n.len() > MAX_NAME_LEN || !is_valid_name(n) {
                    return Err("bad_payload");
                }
                Some(n.to_string())
            }
            None => None,
        };
        let u = match e.get("u").and_then(Value::as_str) {
            Some(u) if u.is_empty() => None,
            Some(u) => {
                if !u.starts_with('/') || u.len() > MAX_PATH_LEN || u.contains('\0') {
                    return Err("bad_payload");
                }
                Some(u.to_string())
            }
            None => None,
        };
        let r = match e.get("r").and_then(Value::as_str) {
            Some(r) if r.is_empty() => None,
            Some(r) => {
                if r.len() > MAX_REFERRER_LEN {
                    return Err("bad_payload");
                }
                Some(r.to_string())
            }
            None => None,
        };
        let v_num = match e.get("v") {
            Some(Value::Number(num)) => Some(
                num.as_f64()
                    .filter(|f| f.is_finite())
                    .ok_or("bad_payload")?,
            ),
            Some(Value::Null) | None => None,
            Some(_) => return Err("bad_payload"),
        };
        let w = match e.get("w") {
            Some(Value::Number(num)) => {
                let raw = num.as_f64().ok_or("bad_payload")?;
                let w = raw.clamp(0.0, 65535.0) as u16;
                Some(w)
            }
            _ => None,
        };
        let sd = match e.get("sd") {
            Some(Value::Number(num)) => {
                let raw = num.as_f64().ok_or("bad_payload")?;
                Some(raw.clamp(0.0, 100.0))
            }
            _ => None,
        };
        let dt = match e.get("dt") {
            Some(Value::Number(num)) => {
                let raw = num.as_f64().ok_or("bad_payload")?;
                (raw.clamp(0.0, MAX_DT_MS as f64)) as i64
            }
            _ => 0,
        };
        let p = match e.get("p") {
            Some(Value::Null) | None => None,
            Some(p @ Value::Object(_)) => {
                if serde_json::to_vec(p).map(|b| b.len()).unwrap_or(usize::MAX)
                    > MAX_PROPS_BYTES
                {
                    return Err("bad_payload");
                }
                Some(p.clone())
            }
            Some(_) => return Err("bad_payload"),
        };
        events.push(BeaconEvent {
            e: kind.to_string(),
            n,
            u,
            r,
            v: v_num,
            w,
            sd,
            dt,
            p,
        });
    }
    Ok(Beacon { site, tp, events })
}

/// Event kinds accepted on the wire (plan §3.1).
fn is_valid_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "pageview" | "custom" | "outbound" | "download" | "engagement" | "vital" | "error"
    )
}

/// Names (custom events, vital metrics) allow only safe characters so the
/// ingest layer can whitelist them into SQL without surprises.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | ' ' | '/')
        })
}

/// W3C traceparent `00-<32 hex>-<16 hex>-<2 hex>` (55 chars).
fn is_traceparent(tp: &str) -> bool {
    let b = tp.as_bytes();
    if b.len() != 55 || &tp[..3] != "00-" || &tp[35..36] != "-" || &tp[52..53] != "-" {
        return false;
    }
    tp.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

// =====================================================================
// Envelope building (collector → ingest, plan §3.2)
// =====================================================================

fn build_envelope(
    st: &WebState,
    site: &WebSiteConfig,
    beacon: &Beacon,
    headers: &HeaderMap,
    client_ip: Option<IpAddr>,
) -> Value {
    let ua = header_str(headers, "user-agent")
        .map(|s| truncate(&s, 512))
        .unwrap_or_default();
    let lang = header_str(headers, "accept-language")
        .and_then(|s| s.split(',').next().map(str::to_string))
        .map(|s| truncate(s.trim(), 64))
        .unwrap_or_default();
    let origin = header_str(headers, "origin")
        .or_else(|| header_str(headers, "referer"))
        .map(|s| truncate(s.trim(), 256))
        .unwrap_or_default();
    // `uid` is only honoured from trusted proxies (anti-spoof, plan §3.2):
    // the resolved client IP must itself be inside a trusted CIDR.
    let uid = match (&site.user_header, client_ip) {
        (Some(h), Some(ip)) if st.trusted.iter().any(|c| c.contains(ip)) => {
            header_str(headers, h).map(|v| truncate(v.trim(), 128))
        }
        _ => None,
    };
    // `internal`: the operator's own visit. Matched here, at the edge,
    // against the project's `exclude_ips` — using the already-resolved
    // client IP, so it inherits the anti-spoof guarantees (direct clients
    // can't forge TCP source; proxied ones resolve only via trusted peers).
    let internal = client_ip.map(|ip| site.excludes(ip)).unwrap_or(false);
    // `CF-IPCountry` (Cloudflare edge-stamped ISO country, e.g. `ID`).
    // Passed through untouched (unlike XFF, which proxies commonly rewrite
    // to `$remote_addr`). Gives per-country analytics with zero setup;
    // region/city still need the GeoIP City DB. Strict 2-letter uppercase.
    let cc = header_str(headers, "cf-ipcountry")
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()));
    json!({
        "v": 1,
        "site": beacon.site,
        "ua": ua,
        "lang": lang,
        "origin": origin,
        "uid": uid,
        "cc": cc,
        "internal": internal,
        "ev": beacon.events,
        "tp": beacon.tp,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

// =====================================================================
// Rate limiter (token bucket, bounded table, no external crate)
// =====================================================================

/// Token-bucket rate limiter keyed by client IP. The table is bounded: when
/// the cap is exceeded the stalest half is dropped (amortized O(n) cleanup).
pub struct RateLimiter {
    per_minute: f64,
    buckets: Mutex<std::collections::HashMap<IpAddr, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute: per_minute.max(1) as f64,
            buckets: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Consume one token; `true` = allowed.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let Some(ip) = ip else { return true };
        let now = std::time::Instant::now();
        let mut buckets = self.buckets.lock();
        if buckets.len() >= RATE_BUCKET_CAP {
            buckets.retain(|_, b| now.duration_since(b.last).as_secs() < RATE_BUCKET_TTL_SECS);
            if buckets.len() >= RATE_BUCKET_CAP {
                buckets.clear(); // extreme load: fail open rather than OOM
            }
        }
        let refill_rate = self.per_minute / 60.0;
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.per_minute,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_rate).min(self.per_minute);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buckets.lock().len()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tower::ServiceExt;

    /// Build a collect request as the real service would deliver it — the
    /// `ConnectInfo` extension is populated by `into_make_service_with_connect_info`.
    fn collect_req(headers: &[(&str, &str)], body: impl Into<axum::body::Body>) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/web/collect");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut req = b.body(body.into()).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            39999,
        )));
        req
    }

    fn cfg() -> WebConfig {
        let mut c = WebConfig::default();
        c.enabled = true;
        c.site.push(WebSiteConfig {
            id: "site_abc".into(),
            allowed_origins: vec!["https://example.com".into()],
            user_header: Some("X-CL-User".into()),
            exclude_ips: Vec::new(),
        });
        c
    }

    fn beacon_body() -> String {
        r#"{"s":"site_abc","ev":[
            {"e":"pageview","u":"/pricing?utm_source=x","r":"https://google.com/","w":1440,"dt":0},
            {"e":"custom","n":"signup","p":{"plan":"pro"},"dt":850}
        ]}"#
        .to_string()
    }

    #[test]
    fn registry_loads_db_rows_over_config_seeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        let web_cfg = cfg();
        let reg = WebSites::seeded(&web_cfg);
        assert!(reg.lookup("site_abc").is_some());
        assert!(reg.lookup("shop").is_none());

        // Simulate a CRUD-created project row.
        {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO web_sites (id, allowed_origins, user_header)
                 VALUES ('shop', '[\"https://shop.example.com\"]', NULL)",
                [],
            )
            .unwrap();
        }
        let conn = store.lock();
        assert_eq!(reg.load_from_db(&conn), 1);
        let shop = reg.lookup("shop").unwrap();
        assert_eq!(shop.allowed_origins, vec!["https://shop.example.com"]);
        // Config seed still intact.
        assert!(reg.lookup("site_abc").is_some());
    }

    #[test]
    fn beacon_parses_valid_payload() {
        let b = parse_beacon(beacon_body().as_bytes(), &cfg()).unwrap();
        assert_eq!(b.site, "site_abc");
        assert_eq!(b.events.len(), 2);
        assert_eq!(b.events[0].e, "pageview");
        assert_eq!(b.events[0].u.as_deref(), Some("/pricing?utm_source=x"));
        assert_eq!(b.events[1].n.as_deref(), Some("signup"));
        assert!(b.tp.is_none());
    }

    #[test]
    fn beacon_rejects_bad_shapes() {
        let c = &cfg();
        assert_eq!(parse_beacon(b"", c), Err("bad_payload"));
        assert_eq!(parse_beacon(b"not json", c), Err("bad_payload"));
        assert_eq!(
            parse_beacon(br#"{"ev":[]}"#, c),
            Err("bad_payload")
        );
        assert_eq!(
            parse_beacon(br#"{"s":"x","ev":[{"e":"nonsense"}]}"#,
            c),
            Err("bad_payload")
        );
        // bad name characters
        assert_eq!(
            parse_beacon(
                br#"{"s":"x","ev":[{"e":"custom","n":"bad;name"}]}"#,
                c
            ),
            Err("bad_payload")
        );
        // path must be relative
        assert_eq!(
            parse_beacon(
                br#"{"s":"x","ev":[{"e":"pageview","u":"https://evil.example/"}]}"#,
                c
            ),
            Err("bad_payload")
        );
        // too many events
        let mut too_many = c.clone();
        too_many.max_events_per_beacon = 2;
        assert_eq!(
            parse_beacon(
                br#"{"s":"x","ev":[{"e":"pageview"},{"e":"pageview"},{"e":"pageview"}]}"#,
                &too_many
            ),
            Err("too_many_events")
        );
        // dt clamped
        let b = parse_beacon(
            br#"{"s":"x","ev":[{"e":"pageview","dt":99999999999}]}"#,
            c,
        )
        .unwrap();
        assert_eq!(b.events[0].dt, MAX_DT_MS);
    }

    #[test]
    fn traceparent_validation() {
        let good = format!("00-{:032x}-{:016x}-01", 1u128, 2u64);
        assert!(is_traceparent(&good));
        assert!(!is_traceparent("00-short-01"));
        let mut evil = good.clone();
        evil.replace_range(40..41, "g");
        assert!(!is_traceparent(&evil));
    }

    #[test]
    fn trusted_proxy_resolution_and_spoof_rejection() {
        let trusted = vec![
            crate::config::Cidr::parse("127.0.0.1/32").unwrap(),
            crate::config::Cidr::parse("10.0.0.0/8").unwrap(),
        ];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 4000);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.3".parse().unwrap());
        let ip = resolve_client_ip(peer, &headers, &trusted).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));

        // Untrusted peer claiming XFF → header ignored (spoof rejected).
        let evil_peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 4000);
        let ip = resolve_client_ip(evil_peer, &headers, &trusted).unwrap();
        assert_eq!(ip, evil_peer.ip());

        // Trusted proxy with no XFF → peer itself.
        let headers2 = HeaderMap::new();
        let ip = resolve_client_ip(peer, &headers2, &trusted).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn cf_connecting_ip_wins_over_rotating_xff() {
        let trusted = vec![crate::config::Cidr::parse("127.0.0.1/32").unwrap()];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000);
        // Simulates Cloudflare → same-host nginx with
        // `proxy_set_header X-Forwarded-For $remote_addr`: XFF carries a
        // rotating Cloudflare edge IP per connection, while CF-Connecting-IP
        // carries the stable eyeball IP. The stable one must win, or every
        // beacon mints a fresh visitor hash (fragmented visitors/sessions).
        for edge in ["104.21.13.11", "172.67.167.224", "104.21.44.10"] {
            let mut headers = HeaderMap::new();
            headers.insert("cf-connecting-ip", "203.0.113.77".parse().unwrap());
            headers.insert("x-forwarded-for", edge.parse().unwrap());
            let ip = resolve_client_ip(peer, &headers, &trusted).unwrap();
            assert_eq!(
                ip,
                IpAddr::V4(Ipv4Addr::new(203, 0, 113, 77)),
                "edge {edge} must not leak into the visitor hash"
            );
        }

        // Spoofed CF-Connecting-IP from an untrusted peer → ignored.
        let evil_peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 4000);
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "203.0.113.77".parse().unwrap());
        let ip = resolve_client_ip(evil_peer, &headers, &trusted).unwrap();
        assert_eq!(ip, evil_peer.ip());

        // Garbage CF header → fall back to the XFF walk, not a rejection.
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
        headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        let ip = resolve_client_ip(peer, &headers, &trusted).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    }

    #[test]
    fn cidr_matching() {
        let c = crate::config::Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains(IpAddr::V4(Ipv4Addr::new(10, 255, 0, 1))));
        assert!(!c.contains(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))));
        let c6 = crate::config::Cidr::parse("::1/128").unwrap();
        assert!(c6.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!c6.contains(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))));
        // bare address = /32
        let bare = crate::config::Cidr::parse("192.0.2.5").unwrap();
        assert!(bare.contains(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5))));
        assert!(!bare.contains(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 6))));
        assert!(crate::config::Cidr::parse("10.0.0.0/33").is_none());
    }

    #[test]
    fn origin_matching() {
        let site = WebSiteConfig {
            id: "s".into(),
            allowed_origins: vec!["https://example.com".into()],
            user_header: None,
            exclude_ips: Vec::new(),
        };
        let mut h = HeaderMap::new();
        h.insert("origin", "https://example.com".parse().unwrap());
        assert!(origin_allowed(&h, &site));
        h.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!origin_allowed(&h, &site));
        // No Origin → Referer fallback.
        h.remove("origin");
        h.insert(
            "referer",
            "https://example.com/page?utm_source=x".parse().unwrap(),
        );
        assert!(origin_allowed(&h, &site));
        // Neither → denied.
        h.remove("referer");
        assert!(!origin_allowed(&h, &site));
    }

    #[test]
    fn origin_wildcard_accepts_any_present_origin() {
        let site = WebSiteConfig {
            id: "s".into(),
            allowed_origins: vec!["*".into()],
            user_header: None,
            exclude_ips: Vec::new(),
        };
        let mut h = HeaderMap::new();
        h.insert("origin", "https://anything.example:8443".parse().unwrap());
        assert!(origin_allowed(&h, &site));
        // Referer fallback works too.
        h.remove("origin");
        h.insert("referer", "https://other.example/page".parse().unwrap());
        assert!(origin_allowed(&h, &site));
        // But a missing Origin/Referer is still rejected.
        h.remove("referer");
        assert!(!origin_allowed(&h, &site));
        // And "*" passes config validation.
        assert!(crate::config::validate_web_site("s", &["*".to_string()], None, &[]).is_ok());
    }

    #[test]
    fn rate_limiter_blocks_after_burst() {
        let rl = RateLimiter::new(3);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 77));
        assert!(rl.allow(Some(ip)));
        assert!(rl.allow(Some(ip)));
        assert!(rl.allow(Some(ip)));
        assert!(!rl.allow(Some(ip)), "4th request within a minute must fail");
        // Other IPs are independent.
        assert!(rl.allow(Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 78)))));
        // None (unresolvable) never limited.
        assert!(rl.allow(None));
    }

    #[test]
    fn envelope_carries_context_and_uid_only_from_trusted() {
        let web_cfg = Arc::new(cfg());
        let counters = InsertCountersRef::new();
        let st = WebState::new(
            crate::wal::InsertHandle::new(tokio::sync::mpsc::channel(1).0),
            counters,
            Duration::from_millis(10),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let site = st.cfg.site("site_abc").unwrap();
        let beacon = parse_beacon(beacon_body().as_bytes(), &st.cfg).unwrap();

        // Spoofed X-CL-User from an untrusted client must NOT yield a uid.
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", "TestUA/1.0".parse().unwrap());
        headers.insert("x-cl-user", "u_spoofed".parse().unwrap());
        let untrusted = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9));
        let env = build_envelope(&st, site, &beacon, &headers, Some(untrusted));
        assert_eq!(env["site"], "site_abc");
        assert_eq!(env["ua"], "TestUA/1.0");
        assert_eq!(env["uid"], Value::Null);

        // From a trusted proxy the header is honoured.
        let trusted_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let env = build_envelope(&st, site, &beacon, &headers, Some(trusted_ip));
        assert_eq!(env["uid"], "u_spoofed");
        assert_eq!(env["ev"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn collect_end_to_end_into_channel() {
        let web_cfg = Arc::new(cfg());
        let counters = InsertCountersRef::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters.clone(),
            Duration::from_millis(500),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        // Fire the WAL ack like the real writer task does, so the handler's
        // `append_acked` await completes.
        let acker = tokio::spawn(async move {
            if let Some(entry) = rx.recv().await {
                if let Some(ack) = entry.ack {
                    let _ = ack.send(());
                }
                return Some(entry.record);
            }
            None
        });
        let app = router(st);

        let res = app
            .oneshot(collect_req(
                &[
                    ("content-type", "text/plain"),
                    ("origin", "https://example.com"),
                    ("x-forwarded-for", "203.0.113.50"),
                ],
                beacon_body(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let rec = tokio::time::timeout(Duration::from_secs(1), acker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(rec.protocol, Protocol::WebEvent);
        assert_eq!(rec.source_addr, "203.0.113.50");
        let env: Value = serde_json::from_slice(&rec.raw).unwrap();
        assert_eq!(env["site"], "site_abc");
    }

    #[tokio::test]
    async fn collect_rejects_unknown_site_with_empty_202() {
        // Auto-create OFF: unknown ids are rejected with an empty 202 that
        // reveals nothing about which projects exist.
        let mut no_tofu = cfg();
        no_tofu.auto_create_projects = false;
        let no_tofu = Arc::new(no_tofu);
        let counters = InsertCountersRef::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters.clone(),
            Duration::from_millis(100),
            no_tofu.clone(),
            WebSites::seeded(&no_tofu),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let app = router(st);
        let body = r#"{"s":"unknown_site","ev":[{"e":"pageview","u":"/"}]}"#;
        let res = app
            .oneshot(collect_req(
                &[("origin", "https://example.com")],
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(counters.snapshot().web_rejected_total["unknown_site"] >= 1);
    }

    #[tokio::test]
    async fn collect_honors_dnt() {
        let web_cfg = Arc::new(cfg());
        let counters = InsertCountersRef::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters.clone(),
            Duration::from_millis(100),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let app = router(st);
        let res = app
            .oneshot(collect_req(
                &[("dnt", "1"), ("origin", "https://example.com")],
                beacon_body(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(rx.try_recv().is_err(), "DNT beacon must not enter the WAL");
        assert!(counters.snapshot().web_rejected_total["dnt"] >= 1);
    }

    #[tokio::test]
    async fn collect_sheds_when_wal_full() {
        let web_cfg = Arc::new(cfg());
        let mut gauges = WalGauges::default();
        gauges.channel_capacity = std::sync::atomic::AtomicU64::new(100);
        gauges.channel_depth = std::sync::atomic::AtomicI64::new(95); // 95% ≥ 80%
        let counters = InsertCountersRef::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters.clone(),
            Duration::from_millis(100),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(gauges),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let app = router(st);
        let res = app
            .oneshot(collect_req(
                &[("origin", "https://example.com")],
                beacon_body(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(counters.snapshot().web_rejected_total["shed"] >= 1);
    }

    #[tokio::test]
    async fn tracker_js_is_served_public() {        let web_cfg = Arc::new(cfg());
        let counters = InsertCountersRef::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters,
            Duration::from_millis(100),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let app = router(st);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/v1/web/t.js")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("javascript"));
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        // Raw-size guard (plan W2 target: < 2 KB gzip → keep raw < 8 KiB).
        assert!(
            body.len() < 8 * 1024,
            "tracker.js raw size {} exceeds 8 KiB budget",
            body.len()
        );
        assert!(core::str::from_utf8(&body).unwrap().contains("data-site"));
    }

    fn auto_create_state(
        web_cfg: Arc<WebConfig>,
        store: Option<crate::store::Store>,
    ) -> (WebState, InsertCountersRef) {
        let counters = InsertCountersRef::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::wal::WalEntry>(8);
        // Ack every entry like the real WAL writer, so `append_acked` in the
        // handler completes and accepted beacons answer 202.
        tokio::spawn(async move {
            while let Some(entry) = rx.recv().await {
                if let Some(ack) = entry.ack {
                    let _ = ack.send(());
                }
            }
        });
        let st = WebState::new(
            crate::wal::InsertHandle::new(tx),
            counters.clone(),
            Duration::from_millis(100),
            web_cfg.clone(),
            WebSites::seeded(&web_cfg),
            Arc::new(WalGauges::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let st = match store {
            Some(s) => st.with_auto_create_store(s),
            None => st,
        };
        (st, counters)
    }

    fn new_project_beacon(site: &str) -> String {
        format!(r#"{{"s":"{site}","ev":[{{"e":"pageview","u":"/","dt":0}}]}}"#)
    }

    #[tokio::test]
    async fn unknown_project_is_auto_created_on_first_beacon() {
        let web_cfg = Arc::new(cfg()); // auto_create_projects defaults true
        let (st, counters) = auto_create_state(web_cfg, None);
        let app = router(st.clone());

        // First contact: unknown id + valid Origin → 202 AND project exists.
        let res = app
            .clone()
            .oneshot(collect_req(
                &[("origin", "https://shop.example.com")],
                new_project_beacon("shop"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let shop = st.sites.lookup("shop").expect("project must be auto-created");
        assert_eq!(shop.allowed_origins, vec!["https://shop.example.com"]);
        assert!(counters.snapshot().web_rejected_total.get("unknown_site").is_none());

        // Same origin again → accepted (no longer unknown).
        let res = app
            .clone()
            .oneshot(collect_req(
                &[("origin", "https://shop.example.com/")],
                new_project_beacon("shop"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        // A DIFFERENT origin is now allowlisted against the project.
        let res = app
            .oneshot(collect_req(
                &[("origin", "https://evil.example")],
                new_project_beacon("shop"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED); // empty 202, not stored
        assert!(counters.snapshot().web_rejected_total["origin_denied"] >= 1);
    }

    #[tokio::test]
    async fn auto_create_needs_an_origin_and_honors_flag_and_cap() {
        // No Origin/Referer → cannot establish trust → rejected.
        let web_cfg = Arc::new(cfg());
        let (st, counters) = auto_create_state(web_cfg, None);
        let app = router(st);
        let res = app
            .oneshot(collect_req(&[], new_project_beacon("ghost")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(counters.snapshot().web_rejected_total["unknown_site"] >= 1);

        // Flag off → rejected.
        let mut off = cfg();
        off.auto_create_projects = false;
        let off = Arc::new(off);
        let (st, counters) = auto_create_state(off, None);
        let app = router(st);
        let res = app
            .oneshot(collect_req(
                &[("origin", "https://a.example")],
                new_project_beacon("nope"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(counters.snapshot().web_rejected_total["unknown_site"] >= 1);

        // Cap reached (seed site_abc already fills a cap of 1) → rejected.
        let mut capped = cfg();
        capped.auto_create_max_projects = 1;
        let capped = Arc::new(capped);
        let (st, counters) = auto_create_state(capped, None);
        let app = router(st);
        let res = app
            .oneshot(collect_req(
                &[("origin", "https://b.example")],
                new_project_beacon("over-cap"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(counters.snapshot().web_rejected_total["unknown_site"] >= 1);
    }

    #[tokio::test]
    async fn auto_create_persists_to_db() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        let web_cfg = Arc::new(cfg());
        let (st, _counters) = auto_create_state(web_cfg, Some(store.clone()));
        let app = router(st.clone());

        let res = app
            .oneshot(collect_req(
                &[("origin", "https://db.example")],
                new_project_beacon("dbproj"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        // Row survived in DuckDB (a restart would reload it via load_from_db).
        let conn = store.lock();
        let origins: String = conn
            .query_row(
                "SELECT allowed_origins FROM web_sites WHERE id = 'dbproj'",
                [],
                |r| r.get(0),
            )
            .expect("auto-created project must be persisted");
        assert_eq!(origins, r#"["https://db.example"]"#);

        // And a fresh registry picks it up.
        let fresh = WebSites::seeded(&cfg());
        assert_eq!(fresh.load_from_db(&conn), 1);
        assert!(fresh.lookup("dbproj").is_some());
    }
}
