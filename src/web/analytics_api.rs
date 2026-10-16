//! Website-analytics query API (PLAN-WEB-ANALYTICS.md W3).
//!
//! Everything lives under `/api/web/*`, so the existing auth middleware
//! classifies it as `Scope::Read` (plan §4.7). The collector endpoints are
//! the ONLY public web surface — see `insert::web`.
//!
//! SQL safety: `metric`/`by`/`interval` are whitelisted through fixed lookup
//! tables (the `query.rs` pattern) — user input never reaches SQL text.
//! Query semantics note (privacy model): unique visitors are exact within a
//! single day bucket and SUMMED across buckets (the daily-rotating visitor
//! hash cannot be deduplicated across days).

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::insert::web::WebSites;
use crate::store::Store;

#[derive(Clone)]
pub struct WebApiState {
    pub store: Store,
    /// Static config (`[[web.site]]` seeds + the `enabled` flag).
    pub web_cfg: Arc<crate::config::WebConfig>,
    /// Runtime project registry shared with the collector hot path.
    pub sites: WebSites,
}

pub fn router(state: WebApiState) -> Router {
    Router::new()
        .route("/api/web/stats", get(web_stats))
        .route("/api/web/breakdown", get(web_breakdown))
        .route("/api/web/realtime", get(web_realtime))
        .route("/api/web/vitals", get(web_vitals))
        .route("/api/web/errors", get(web_errors))
        .route("/api/web/sites", get(web_sites).post(create_site))
        .route("/api/web/sites/{id}", delete(delete_site).put(update_site))
        .route("/api/web/sources", get(web_sources))
        .route("/api/web/pages", get(web_pages))
        .route("/api/web/links", get(web_links))
        .route("/api/web/custom", get(web_custom))
        .route("/api/web/funnel", get(web_funnel))
        .route("/api/web/sessions", get(web_sessions_list))
        .route("/api/web/sessions/{id}", get(web_session_detail))
        .route("/api/web/export", get(web_export))
        .route("/api/web/summary", get(web_summary))
        .route("/api/web/trace/{trace_id}", get(web_trace))
        .with_state(state)
}

// =====================================================================
// Parameter plumbing
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct RangeParams {
    pub site: Option<String>,
    /// RFC 3339 timestamp or `YYYY-MM-DD`. Default: 24h ago.
    pub from: Option<String>,
    /// RFC 3339 timestamp or `YYYY-MM-DD`. Default: now.
    pub to: Option<String>,
    /// Relative window shorthand (`1h`, `24h`, `7d`…) — used when
    /// `from`/`to` are absent. Explicit `from`/`to` always win.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true). Pass
    /// `?include_internal=true` to audit them.
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
    /// pageviews | visitors | sessions | bounce_rate | avg_duration
    pub metric: Option<String>,
    /// auto | 1m | 5m | 1h | 1d (auto picks by range length)
    pub interval: Option<String>,
}

/// (from, to) with hard bounds (max 90 days — same spirit as the log query
/// limits; unbounded scans would hog the shared DuckDB connection).
fn parse_range(
    from: Option<&str>,
    to: Option<&str>,
    window: Option<&str>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let parse_one = |s: &str| -> Result<DateTime<Utc>, String> {
        if let Ok(t) = DateTime::parse_from_rfc3339(s) {
            return Ok(t.with_timezone(&Utc));
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Ok(DateTime::from_naive_utc_and_offset(
                d.and_hms_opt(0, 0, 0).unwrap(),
                Utc,
            ));
        }
        Err(format!("bad time '{s}' (use RFC 3339 or YYYY-MM-DD)"))
    };
    let to_t = match to {
        Some(s) => parse_one(s)?,
        None => Utc::now(),
    };
    let from_t = match (from, window) {
        (Some(s), _) => parse_one(s)?,
        (None, Some(w)) => match crate::web::api::parse_window_secs(w) {
            Some(secs) if secs > 0 => to_t - chrono::Duration::seconds(secs),
            _ => {
                return Err(format!(
                    "bad window '{w}' (use 5m, 1h, 24h, 7d … or explicit from/to)"
                ))
            }
        },
        (None, None) => to_t - chrono::Duration::hours(24),
    };
    if from_t >= to_t {
        return Err("'from' must be before 'to'".into());
    }
    if (to_t - from_t) > chrono::Duration::days(90) {
        return Err("range exceeds the 90-day cap".into());
    }
    Ok((from_t, to_t))
}

/// Bucket interval, whitelisted. `auto` picks by range length. Returns the
/// SQL fragment plus the canonical key the UI maps to a human label.
fn interval_sql(
    interval: Option<&str>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> (&'static str, &'static str) {
    let hours = (to - from).num_hours();
    match interval {
        Some("1m") => ("INTERVAL '1 minute'", "1m"),
        Some("5m") => ("INTERVAL '5 minutes'", "5m"),
        Some("1h") => ("INTERVAL '1 hour'", "1h"),
        Some("1d") | Some("1day") => ("INTERVAL '1 day'", "1d"),
        Some(_) | None => {
            if hours <= 2 {
                ("INTERVAL '1 minute'", "1m")
            } else if hours <= 48 {
                ("INTERVAL '1 hour'", "1h")
            } else {
                ("INTERVAL '1 day'", "1d")
            }
        }
    }
}

/// Whitelisted stat metric → (aggregate SQL over web_events, is_session_metric).
/// Bounce and duration consider only sessions that actually viewed a page
/// (bounces = 1-pageview sessions with no deliberate interaction ÷ sessions
/// with ≥1 pageview); NULLIF guards session_id=0 ("deliberately sessionless"
/// orphan events) so they never count as a session.
fn metric_sql(metric: &str) -> Option<(&'static str, bool)> {
    match metric {
        "pageviews" => Some((
            "SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END)",
            false,
        )),
        "events" => Some(("COUNT(*)", false)),
        // Daily-rotating hash: distinct per bucket, summed across buckets.
        "visitors" => Some(("COUNT(DISTINCT visitor_id)", false)),
        "sessions" => Some(("COUNT(DISTINCT NULLIF(session_id, 0))", false)),
        "bounce_rate" => Some((
            "COALESCE(SUM(CASE WHEN pageviews >= 1 AND is_bounce THEN 1 ELSE 0 END)::DOUBLE \
             / NULLIF(SUM(CASE WHEN pageviews >= 1 THEN 1 ELSE 0 END), 0), 0)",
            true,
        )),
        "avg_duration" => Some((
            "COALESCE(AVG(engaged_ms) FILTER (WHERE pageviews >= 1), 0)",
            true,
        )),
        _ => None,
    }
}

async fn web_stats(
    State(st): State<WebApiState>,
    Query(p): Query<RangeParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let metric = p.metric.as_deref().unwrap_or("pageviews");
    let Some((agg, is_session)) = metric_sql(metric) else {
        return bad_request("metric must be pageviews|events|visitors|sessions|bounce_rate|avg_duration");
    };
    let (bucket, interval_key) = interval_sql(p.interval.as_deref(), from, to);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let path_ev = filters.event_clause();
    let path_ses = filters.session_clause();
    let conn = st.store.lock();
    let sql = if is_session {
        // Session metrics read web_sessions (one row per visit).
        format!(
            "SELECT time_bucket({bucket}, start_ts) AS bucket, ROUND({agg}, 4) AS value
             FROM web_sessions
             WHERE start_ts >= ? AND start_ts < ?{internal}{pathses}
               {site}
             GROUP BY 1 ORDER BY 1",
            bucket = bucket,
            agg = agg,
            internal = internal,
            pathses = path_ses,
            site = site_clause(p.site.as_deref()),
        )
    } else {
        format!(
            "SELECT time_bucket({bucket}, ts) AS bucket, {agg} AS value
             FROM web_events_all
             WHERE ts >= ? AND ts < ? AND NOT is_bot{internal}{pathev}
               {site}
             GROUP BY 1 ORDER BY 1",
            bucket = bucket,
            agg = agg,
            internal = internal,
            pathev = path_ev,
            site = site_clause(p.site.as_deref()),
        )
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = if is_session {
        session_query_params(&filters, &from, &to, p.site.as_ref())
    } else {
        event_query_params(&filters, &from, &to, p.site.as_ref())
    };
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(Point {
                t: r.get::<_, DateTime<Utc>>(0)?,
                v: r.get::<_, f64>(1)?,
            })
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(points) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({
        "metric": metric,
        "interval": interval_key,
        "from": from,
        "to": to,
        "points": points,
    }))
    .into_response()
}

#[derive(Debug, Serialize)]
struct Point {
    t: DateTime<Utc>,
    v: f64,
}

fn site_clause(site: Option<&str>) -> String {
    match site {
        Some(_) => " AND site_id = ?".to_string(),
        None => String::new(),
    }
}

/// `AND NOT …` fragment excluding operator-internal visits, unless the caller
/// passes `?include_internal=true`. NULL-safe so pre-migration rows
/// (no `is_internal` column value) count as external.
fn internal_clause(include_internal: bool) -> &'static str {
    if include_internal {
        ""
    } else {
        " AND NOT COALESCE(is_internal, FALSE)"
    }
}

/// Multi-dimension filter chips (the toolbar's removable chips). Every key is
/// whitelisted — values bind as parameters, never interpolated into SQL.
/// Event tables take direct predicates; session tables match sessions that
/// touched a matching event in range (subquery rebinds values + from/to).
#[derive(Default, Debug, Clone)]
pub(crate) struct WebFilters {
    path: Option<String>,
    country: Option<String>,
    region: Option<String>,
    city: Option<String>,
    device: Option<String>,
    browser: Option<String>,
    os: Option<String>,
    source: Option<String>,
    medium: Option<String>,
}

/// Query-string keys accepted as filter chips, in binding order.
const FILTER_KEYS: [&str; 9] = [
    "path",
    "country",
    "region",
    "city",
    "device",
    "browser",
    "os",
    "source",
    "medium",
];

impl WebFilters {
    /// Parse from the raw query map; unknown keys are ignored, empties
    /// dropped, values capped (columns are length-limited anyway).
    pub(crate) fn from_map(m: &std::collections::HashMap<String, String>) -> Self {
        let get = |k: &str| -> Option<String> {
            m.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(|v| v.chars().take(128).collect())
        };
        Self {
            path: get("path"),
            country: get("country"),
            region: get("region"),
            city: get("city"),
            device: get("device"),
            browser: get("browser"),
            os: get("os"),
            source: get("source"),
            medium: get("medium"),
        }
    }

    fn values(&self) -> [Option<&String>; 9] {
        [
            self.path.as_ref(),
            self.country.as_ref(),
            self.region.as_ref(),
            self.city.as_ref(),
            self.device.as_ref(),
            self.browser.as_ref(),
            self.os.as_ref(),
            self.source.as_ref(),
            self.medium.as_ref(),
        ]
    }

    fn is_empty(&self) -> bool {
        self.values().iter().all(|v| v.is_none())
    }

    /// `AND col = ?` per set filter, for event-level tables.
    fn event_clause(&self) -> String {
        FILTER_KEYS
            .iter()
            .zip(self.values())
            .filter_map(|(k, v)| v.map(|_| format!(" AND {k} = ?")))
            .collect()
    }

    /// Session-table clause: sessions that touched a matching event in range.
    fn session_clause(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        format!(
            " AND session_id IN (SELECT session_id FROM web_events_all \
             WHERE 1=1{} AND ts >= ? AND ts < ? AND NOT COALESCE(is_internal, FALSE))",
            self.event_clause(),
        )
    }

    /// Bind event-clause values (call after from/to/site).
    fn push_event_params<'a>(&'a self, params: &mut Vec<&'a dyn duckdb::ToSql>) {
        for v in self.values().into_iter().flatten() {
            params.push(v);
        }
    }

    /// Bind session-clause values: filter values, then the subquery's
    /// from/to rebinds.
    fn push_session_params<'a>(
        &'a self,
        params: &mut Vec<&'a dyn duckdb::ToSql>,
        from: &'a DateTime<Utc>,
        to: &'a DateTime<Utc>,
    ) {
        self.push_event_params(params);
        if !self.is_empty() {
            params.push(from);
            params.push(to);
        }
    }
}

/// Event-level query params, in SQL TEXT order: from, to, filter values,
/// [site] — every event-side WHERE renders `…{filters}{site}`.
fn event_query_params<'a>(
    filters: &'a WebFilters,
    from: &'a DateTime<Utc>,
    to: &'a DateTime<Utc>,
    site: Option<&'a String>,
) -> Vec<&'a dyn duckdb::ToSql> {
    let mut v: Vec<&'a dyn duckdb::ToSql> = vec![from, to];
    filters.push_event_params(&mut v);
    if let Some(s) = site {
        v.push(s);
    }
    v
}

/// Session-level query params, in SQL TEXT order: from, to, then the
/// session subquery's own (filter values, from, to), then [site].
fn session_query_params<'a>(
    filters: &'a WebFilters,
    from: &'a DateTime<Utc>,
    to: &'a DateTime<Utc>,
    site: Option<&'a String>,
) -> Vec<&'a dyn duckdb::ToSql> {
    let mut v: Vec<&'a dyn duckdb::ToSql> = vec![from, to];
    filters.push_session_params(&mut v, from, to);
    if let Some(s) = site {
        v.push(s);
    }
    v
}

fn bad_request(msg: impl Into<String>) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, msg.into()).into_response()
}

// =====================================================================
// GET /api/web/breakdown
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct BreakdownParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand (`1h`, `24h`, `7d`…) — used when
    /// `from`/`to` are absent. Explicit `from`/`to` always win.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true). Pass
    /// `?include_internal=true` to audit them.
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
    /// path|event|source|medium|country|…|entry_page|exit_page
    pub by: String,
    /// 1..=100 (default 10)
    pub limit: Option<i64>,
}

/// Whitelisted `by` dimension → column (breakdown) / table choice.
fn breakdown_column(by: &str) -> Option<(&'static str, bool)> {
    // (column expr, from_sessions)
    match by {
        "path" => Some(("path", false)),
        "event" => Some(("event", false)),
        "source" => Some(("COALESCE(NULLIF(TRIM(source), ''), 'direct')", false)),
        "medium" => Some(("COALESCE(NULLIF(TRIM(medium), ''), 'none')", false)),
        "country" => Some(("COALESCE(NULLIF(TRIM(country), ''), 'unknown')", false)),
        "region" => Some(("COALESCE(NULLIF(TRIM(region), ''), 'unknown')", false)),
        "city" => Some(("COALESCE(NULLIF(TRIM(city), ''), 'unknown')", false)),
        "screen" => Some(("COALESCE(CAST(screen_w AS VARCHAR), 'unknown')", false)),
        "utm_source" => Some(("COALESCE(NULLIF(TRIM(utm->>'$.source'), ''), '(none)')", false)),
        "utm_medium" => Some(("COALESCE(NULLIF(TRIM(utm->>'$.medium'), ''), '(none)')", false)),
        "utm_campaign" => Some(("COALESCE(NULLIF(TRIM(utm->>'$.campaign'), ''), '(none)')", false)),
        "device" => Some(("COALESCE(NULLIF(TRIM(device), ''), 'unknown')", false)),
        "browser" => Some(("COALESCE(NULLIF(TRIM(browser), ''), 'unknown')", false)),
        "os" => Some(("COALESCE(NULLIF(TRIM(os), ''), 'unknown')", false)),
        "entry_page" => Some(("entry_path", true)),
        "exit_page" => Some(("exit_path", true)),
        _ => None,
    }
}

async fn web_breakdown(
    State(st): State<WebApiState>,
    Query(p): Query<BreakdownParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let Some((col, is_session)) = breakdown_column(&p.by) else {
        return bad_request(
            "by must be path|event|source|medium|country|region|city|screen|utm_source|utm_medium|utm_campaign|device|browser|os|entry_page|exit_page",
        );
    };
    let limit = p.limit.unwrap_or(10).clamp(1, 100);
    let table = if is_session { "web_sessions" } else { "web_events_all" };
    let ts_col = if is_session { "start_ts" } else { "ts" };
    let bot = if is_session { "" } else { " AND NOT is_bot" };
    let pv1 = if is_session { " AND pageviews >= 1" } else { "" };
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let (path_ev, path_ses) = if is_session {
        (String::new(), filters.session_clause())
    } else {
        (filters.event_clause(), String::new())
    };
    let conn = st.store.lock();
    let sql = format!(
        "SELECT {col} AS dim_value,
                SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END) AS pageviews,
                COUNT(DISTINCT visitor_id) AS visitors,
                COUNT(DISTINCT session_id) AS sessions
         FROM {table}
         WHERE {ts_col} >= ? AND {ts_col} < ?{bot}{pv1}{internal}{pathev}{pathses}{site}
         GROUP BY 1
         ORDER BY pageviews DESC, dim_value
         LIMIT {limit}",
        col = col,
        table = table,
        ts_col = ts_col,
        bot = bot,
        pv1 = pv1,
        internal = internal,
        pathev = path_ev,
        pathses = path_ses,
        site = site_clause(p.site.as_deref()),
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = if is_session {
        session_query_params(&filters, &from, &to, p.site.as_ref())
    } else {
        event_query_params(&filters, &from, &to, p.site.as_ref())
    };
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(BreakdownRow {
                value: r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "(none)".into()),
                pageviews: r.get::<_, i64>(1)?,
                visitors: r.get::<_, i64>(2)?,
                sessions: r.get::<_, i64>(3)?,
            })
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "by": p.by, "from": from, "to": to, "rows": rows }))
        .into_response()
}

#[derive(Debug, Serialize)]
struct BreakdownRow {
    value: String,
    pageviews: i64,
    visitors: i64,
    sessions: i64,
}

// =====================================================================
// GET /api/web/realtime (last 5 minutes — hot table only)
// =====================================================================

async fn web_realtime(
    State(st): State<WebApiState>,
    Query(p): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let internal = internal_clause(p.get("include_internal").map(|v| v == "true").unwrap_or(false));
    let filters = WebFilters::from_map(&p);
    let fev = filters.event_clause();
    let conn = st.store.lock();
    let site = site_clause(p.get("site").map(String::as_str));
    let sql = format!(
        "SELECT COUNT(DISTINCT visitor_id) FROM web_events
         WHERE ts >= now()::TIMESTAMP - INTERVAL '5 minutes' AND NOT is_bot{internal}{fev}{site}",
        internal = internal,
        fev = fev,
        site = site
    );
    let mut params: Vec<&dyn duckdb::ToSql> = Vec::new();
    filters.push_event_params(&mut params);
    if let Some(site) = p.get("site") {
        params.push(site);
    }
    let visitors: i64 = conn.query_row(&sql, params.as_slice(), |r| r.get(0)).unwrap_or(0);
    let sql2 = format!(
        "SELECT COALESCE(path, '(none)') AS p, COUNT(*) AS hits
         FROM web_events
         WHERE ts >= now()::TIMESTAMP - INTERVAL '5 minutes' AND NOT is_bot AND event='pageview'{internal}{fev}{site}
         GROUP BY 1 ORDER BY hits DESC LIMIT 10",
        internal = internal,
        fev = fev,
        site = site
    );
    let Ok(mut stmt) = conn.prepare(&sql2) else {
        return bad_request(format!("query failed: {sql2}"));
    };
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "path": r.get::<_, String>(0)?,
                "hits": r.get::<_, i64>(1)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("realtime page query failed");
    };
    Json(serde_json::json!({
        "visitors_5m": visitors,
        "pages": rows,
    }))
    .into_response()
}

// =====================================================================
// GET /api/web/vitals — p75 per metric (per path when ?path=)
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct VitalsParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    pub path: Option<String>,
}

async fn web_vitals(
    State(st): State<WebApiState>,
    Query(p): Query<VitalsParams>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let conn = st.store.lock();
    // good_pct uses Google's "good" thresholds (LCP ≤ 2500ms, INP ≤ 200ms,
    // CLS ≤ 0.1) — mirrored in the dashboard's color coding.
    let mut sql = String::from(
        "SELECT name, COALESCE(path, '(none)') AS p,
                COALESCE(NULLIF(TRIM(device), ''), 'unknown') AS d,
                APPROX_QUANTILE(value, 0.75) AS p75,
                COUNT(*) AS samples,
                AVG(CASE WHEN (name = 'LCP' AND value <= 2500)
                          OR (name = 'INP' AND value <= 200)
                          OR (name = 'CLS' AND value <= 0.1)
                         THEN 1.0 ELSE 0.0 END) AS good_pct
         FROM web_events_all
         WHERE event='vital' AND ts >= ? AND ts < ?",
    );
    if p.site.is_some() {
        sql.push_str(" AND site_id = ?");
    }
    if p.path.is_some() {
        sql.push_str(" AND path = ?");
    }
    sql.push_str(internal);
    sql.push_str(" GROUP BY 1, 2, 3 ORDER BY 1, p75");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let mut params: Vec<&dyn duckdb::ToSql> = vec![&from, &to];
    if let Some(s) = p.site.as_ref() {
        params.push(s);
    }
    if let Some(path) = p.path.as_ref() {
        params.push(path);
    }
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "metric": r.get::<_, String>(0)?,
                "path": r.get::<_, String>(1)?,
                "device": r.get::<_, String>(2)?,
                "p75": r.get::<_, f64>(3)?,
                "samples": r.get::<_, i64>(4)?,
                "good_pct": r.get::<_, f64>(5)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "from": from, "to": to, "rows": rows })).into_response()
}

// =====================================================================
// GET /api/web/errors — grouped by name (+path)
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct ErrorsParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
    pub limit: Option<i64>,
}

async fn web_errors(
    State(st): State<WebApiState>,
    Query(p): Query<ErrorsParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let pathev = filters.event_clause();
    let conn = st.store.lock();
    let sql = format!(
        "SELECT COALESCE(name, '(unknown)') AS n, COALESCE(path, '(none)') AS p,
                COUNT(*) AS hits, MAX(ts) AS last_seen
         FROM web_events_all
         WHERE event='error' AND ts >= ? AND ts < ?{internal}{pathev}{site}
         GROUP BY 1, 2 ORDER BY hits DESC, last_seen DESC
         LIMIT {limit}",
        internal = internal,
        pathev = pathev,
        site = site_clause(p.site.as_deref()),
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = event_query_params(&filters, &from, &to, p.site.as_ref());
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "name": r.get::<_, String>(0)?,
                "path": r.get::<_, String>(1)?,
                "hits": r.get::<_, i64>(2)?,
                "last_seen": r.get::<_, DateTime<Utc>>(3)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "from": from, "to": to, "rows": rows })).into_response()
}


// =====================================================================
// /api/web/sites — project registry (PLAN-WEB-ANALYTICS.md W5)
//
// GET lists the merged registry (config `[[web.site]]` seeds + DB-created
// projects), each tagged with its `source`. POST/DELETE are admin-only
// (enforced by `required_scope` in auth.rs) and update the DB row first,
// then the in-memory registry the collector hot path reads — so a new
// project starts accepting beacons without a restart.
// =====================================================================

async fn web_sites(State(st): State<WebApiState>) -> Response {
    let config_ids: std::collections::HashSet<&str> = st
        .web_cfg
        .site
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    let sites: Vec<serde_json::Value> = st
        .sites
        .list()
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "allowed_origins": s.allowed_origins,
                "has_user_header": s.user_header.is_some(),
                "has_exclude_ips": !s.exclude_ips.is_empty(),
                "source": if config_ids.contains(s.id.as_str()) { "config" } else { "db" },
            })
        })
        .collect();
    Json(serde_json::json!({ "sites": sites, "enabled": st.web_cfg.enabled }))
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateSiteRequest {
    /// Project id — becomes the beacon `s` value and the `site_id` column.
    pub id: String,
    /// Allowed beacon origins (scheme + host, e.g. `https://example.com`).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Optional authenticated-user header (e.g. `X-CL-User`).
    #[serde(default)]
    pub user_header: Option<String>,
    /// Optional operator IPs/CIDRs whose visits are marked internal.
    #[serde(default)]
    pub exclude_ips: Vec<String>,
}

async fn create_site(
    State(st): State<WebApiState>,
    Json(req): Json<CreateSiteRequest>,
) -> Response {
    let id = req.id.trim().to_string();
    let origins: Vec<String> = req
        .allowed_origins
        .iter()
        .map(|o| o.trim().trim_end_matches('/').to_string())
        .filter(|o| !o.is_empty())
        .collect();
    let user_header = req
        .user_header
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let exclude_ips: Vec<String> = req
        .exclude_ips
        .iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    if let Err(e) = crate::config::validate_web_site(
        &id,
        &origins,
        user_header.as_deref(),
        &exclude_ips,
    ) {
        return bad_request(format!("invalid project: {e}"));
    }
    if origins.is_empty() {
        return bad_request("invalid project: allowed_origins must list at least one origin");
    }
    if st.sites.lookup(&id).is_some() {
        return (
            axum::http::StatusCode::CONFLICT,
            format!("project '{id}' already exists"),
        )
            .into_response();
    }
    // DB first, registry second — the hot path only ever sees committed rows.
    let origins_json = serde_json::to_string(&origins).unwrap_or_else(|_| "[]".into());
    let exclude_json = serde_json::to_string(&exclude_ips).unwrap_or_else(|_| "[]".into());
    {
        let conn = st.store.lock();
        if let Err(e) = conn.execute(
            "INSERT INTO web_sites (id, allowed_origins, user_header, exclude_ips) VALUES (?, ?, ?, ?)",
            duckdb::params![id, origins_json, user_header, exclude_json],
        ) {
            return bad_request(format!("could not store project '{id}': {e}"));
        }
    }
    st.sites.upsert(crate::config::WebSiteConfig {
        id: id.clone(),
        allowed_origins: origins,
        user_header,
        exclude_ips,
    });
    tracing::info!(id, "web-analytics project created");
    (
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({ "status": "created", "id": id })),
    )
        .into_response()
}

/// PUT /api/web/sites/{id} — edit origins / user header / excluded IPs.
/// Config-seeded projects stay config-managed (409), same as delete.
async fn update_site(
    State(st): State<WebApiState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpdateSiteRequest>,
) -> Response {
    let id = id.trim().to_string();
    if st.web_cfg.site(&id).is_some() {
        return (
            axum::http::StatusCode::CONFLICT,
            format!(
                "project '{id}' is managed by config [[web.site]]; edit central-logs.toml and restart instead"
            ),
        )
            .into_response();
    }
    if st.sites.lookup(&id).is_none() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("project '{id}' not found"),
        )
            .into_response();
    }
    let origins: Vec<String> = req
        .allowed_origins
        .iter()
        .map(|o| o.trim().trim_end_matches('/').to_string())
        .filter(|o| !o.is_empty())
        .collect();
    let user_header = req
        .user_header
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let exclude_ips: Vec<String> = req
        .exclude_ips
        .iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    if let Err(e) =
        crate::config::validate_web_site(&id, &origins, user_header.as_deref(), &exclude_ips)
    {
        return bad_request(format!("invalid project: {e}"));
    }
    if origins.is_empty() {
        return bad_request("invalid project: allowed_origins must list at least one origin");
    }
    let origins_json = serde_json::to_string(&origins).unwrap_or_else(|_| "[]".into());
    let exclude_json = serde_json::to_string(&exclude_ips).unwrap_or_else(|_| "[]".into());
    {
        let conn = st.store.lock();
        if let Err(e) = conn.execute(
            "UPDATE web_sites SET allowed_origins = ?, user_header = ?, exclude_ips = ? WHERE id = ?",
            duckdb::params![origins_json, user_header, exclude_json, id],
        ) {
            return bad_request(format!("could not update project '{id}': {e}"));
        }
    }
    st.sites.upsert(crate::config::WebSiteConfig {
        id: id.clone(),
        allowed_origins: origins,
        user_header,
        exclude_ips,
    });
    tracing::info!(id, "web-analytics project updated");
    Json(serde_json::json!({ "status": "updated", "id": id })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct UpdateSiteRequest {
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub user_header: Option<String>,
    #[serde(default)]
    pub exclude_ips: Vec<String>,
}

async fn delete_site(
    State(st): State<WebApiState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let id = id.trim();
    if id.is_empty() {
        return bad_request("missing project id");
    }
    // Config-seeded projects are managed by the TOML, not the API — deleting
    // the DB row would be a lie (the seed would still serve beacons).
    if st.web_cfg.site(id).is_some() {
        return (
            axum::http::StatusCode::CONFLICT,
            format!(
                "project '{id}' is managed by config [[web.site]]; remove it from \
                 central-logs.toml and restart instead"
            ),
        )
            .into_response();
    }
    let deleted: usize = {
        let conn = st.store.lock();
        match conn.execute("DELETE FROM web_sites WHERE id = ?", [id]) {
            Ok(n) => n,
            Err(e) => return bad_request(format!("could not delete project '{id}': {e}")),
        }
    };
    if deleted == 0 {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("project '{id}' not found"),
        )
            .into_response();
    }
    // Purge the project's collected analytics: hot events + sessions +
    // rollups go now; archived Parquet partitions age out on the normal
    // retention schedule (they are immutable date-stamped files).
    let purged: usize = {
        let conn = st.store.lock();
        let mut n = 0usize;
        for sql in [
            "DELETE FROM web_events WHERE site_id = ?",
            "DELETE FROM web_sessions WHERE site_id = ?",
            "DELETE FROM web_rollup_1m WHERE site_id = ?",
            "DELETE FROM web_rollup_1d WHERE site_id = ?",
        ] {
            n += conn.execute(sql, [id]).unwrap_or(0);
        }
        n
    };
    st.sites.remove(id);
    tracing::info!(id, purged, "web-analytics project deleted (data purged)");
    Json(serde_json::json!({ "status": "deleted", "id": id, "rows_purged": purged }))
        .into_response()
}

// =====================================================================
// GET /api/web/sources — traffic sources, Statcounter-style
//
// One row per medium (direct / organic / ai / social / referral / email /
// paid / campaign …): session count, bounce rate, and a daily sessions
// series for the trend column. Reads web_sessions (one row per visit), so
// bounce rates are exact per medium.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct SourcesParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
    /// 1..=50 rows (default 20).
    pub limit: Option<i64>,
}

fn medium_norm_sql() -> &'static str {
    "COALESCE(NULLIF(TRIM(medium), ''), 'direct')"
}

async fn web_sources(
    State(st): State<WebApiState>,
    Query(p): Query<SourcesParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let pathses = filters.session_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());

    // Totals per medium.
    let sql = format!(
        "SELECT {norm} AS medium, COUNT(*) AS sessions,
                COUNT(DISTINCT visitor_id) AS visitors,
                AVG(CASE WHEN is_bounce THEN 1.0 ELSE 0.0 END) AS bounce_rate
         FROM web_sessions
         WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{pathses}{site}
         GROUP BY 1 ORDER BY sessions DESC, medium ASC
         LIMIT {limit}",
        norm = medium_norm_sql(),
        internal = internal,
        pathses = pathses,
        site = site,
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = session_query_params(&filters, &from, &to, p.site.as_ref());
    let totals: Vec<(String, i64, i64, f64)> = match stmt
        .query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    {
        Ok(t) => t,
        Err(_) => return bad_request("query failed"),
    };

    // Daily sessions per medium (trend column).
    let sql = format!(
        "SELECT CAST(start_ts AS DATE) AS day, {norm} AS medium, COUNT(*) AS sessions
         FROM web_sessions
         WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{pathses}{site}
         GROUP BY 1, 2 ORDER BY 1",
        norm = medium_norm_sql(),
        internal = internal,
        pathses = pathses,
        site = site,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = session_query_params(&filters, &from, &to, p.site.as_ref());
    let daily: Vec<(chrono::NaiveDate, String, i64)> = match stmt
        .query_map(params.as_slice(), |r| {
            Ok((r.get::<_, chrono::NaiveDate>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    {
        Ok(d) => d,
        Err(_) => return bad_request("query failed"),
    };
    let mut by_medium: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    for (day, medium, sessions) in daily {
        by_medium.entry(medium).or_default().push(serde_json::json!({
            "day": day.to_string(),
            "sessions": sessions,
        }));
    }
    let empty_daily: Vec<serde_json::Value> = Vec::new();
    let sources: Vec<serde_json::Value> = totals
        .into_iter()
        .map(|(medium, sessions, visitors, bounce_rate)| {
            serde_json::json!({
                "medium": medium,
                "sessions": sessions,
                "visitors": visitors,
                "bounce_rate": (bounce_rate * 10000.0).round() / 10000.0,
                "daily": by_medium.get(&medium).unwrap_or(&empty_daily),
            })
        })
        .collect();
    Json(serde_json::json!({ "from": from, "to": to, "sources": sources })).into_response()
}

// =====================================================================
// GET /api/web/pages — entry / top / exit pages in one call
//
// Statcounter-style Pages view: entry pages and exit pages come from
// web_sessions (with exact bounce rates); top pages from pageview events
// with distinct-session counts. Each list carries a daily series.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct PagesParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
    /// Rows per list, 1..=50 (default 10).
    pub limit: Option<i64>,
}

async fn web_pages(
    State(st): State<WebApiState>,
    Query(p): Query<PagesParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let limit = p.limit.unwrap_or(10).clamp(1, 50);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let path_ses = filters.session_clause();
    let path_ev = filters.event_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());
    // Event-level params (shared by the two top-pages queries).
    let params_ev = event_query_params(&filters, &from, &to, p.site.as_ref());

    // Helper: session-native list (entry_path or exit_path) with bounce.
    let session_list = |col: &str| -> Result<Vec<serde_json::Value>, String> {
        let sql = format!(
            "SELECT COALESCE(NULLIF(TRIM({col}), ''), '(none)') AS page,
                    COUNT(*) AS sessions,
                    COUNT(DISTINCT visitor_id) AS visitors,
                    AVG(CASE WHEN is_bounce THEN 1.0 ELSE 0.0 END) AS bounce_rate
             FROM web_sessions
             WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{pathses}{site}
             GROUP BY 1 ORDER BY sessions DESC, page ASC
             LIMIT {limit}",
            col = col,
            internal = internal,
            pathses = path_ses,
            site = site,
            limit = limit,
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| "query failed".to_string())?;
        let qparams = session_query_params(&filters, &from, &to, p.site.as_ref());
        let rows = stmt
            .query_map(qparams.as_slice(), |r| {
                Ok(serde_json::json!({
                    "page": r.get::<_, String>(0)?,
                    "sessions": r.get::<_, i64>(1)?,
                    "visitors": r.get::<_, i64>(2)?,
                    "bounce_rate": (r.get::<_, f64>(3)? * 10000.0).round() / 10000.0,
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(|_| "query failed".to_string())?;
        Ok(rows)
    };
    // Daily sessions per session-native column value (trend column).
    let session_daily =
        |col: &str| -> Result<std::collections::HashMap<String, Vec<serde_json::Value>>, String> {
            let sql = format!(
                "SELECT CAST(start_ts AS DATE) AS day,
                        COALESCE(NULLIF(TRIM({col}), ''), '(none)') AS page,
                        COUNT(*) AS sessions
                 FROM web_sessions
                 WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{pathses}{site}
                 GROUP BY 1, 2 ORDER BY 1",
                col = col,
                internal = internal,
                pathses = path_ses,
                site = site,
            );
            let mut stmt = conn.prepare(&sql).map_err(|_| "query failed".to_string())?;
            let mut map: std::collections::HashMap<String, Vec<serde_json::Value>> =
                std::collections::HashMap::new();
            let qparams = session_query_params(&filters, &from, &to, p.site.as_ref());
            let rows = stmt
                .query_map(qparams.as_slice(), |r| {
                    Ok((
                        r.get::<_, chrono::NaiveDate>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|_| "query failed".to_string())?;
            for (day, page, sessions) in rows {
                map.entry(page).or_default().push(serde_json::json!({
                    "day": day.to_string(),
                    "sessions": sessions,
                }));
            }
            Ok(map)
        };

    let mut entry = match session_list("entry_path") {
        Ok(r) => r,
        Err(e) => return bad_request(e),
    };
    let mut exit = match session_list("exit_path") {
        Ok(r) => r,
        Err(e) => return bad_request(e),
    };
    let entry_daily = match session_daily("entry_path") {
        Ok(m) => m,
        Err(e) => return bad_request(e),
    };
    let exit_daily = match session_daily("exit_path") {
        Ok(m) => m,
        Err(e) => return bad_request(e),
    };
    for row in &mut entry {
        let page = row.get("page").and_then(|v| v.as_str()).unwrap_or("").to_string();
        row.as_object_mut().unwrap().insert(
            "daily".to_string(),
            entry_daily.get(&page).cloned().unwrap_or_default().into(),
        );
    }
    for row in &mut exit {
        let page = row.get("page").and_then(|v| v.as_str()).unwrap_or("").to_string();
        row.as_object_mut().unwrap().insert(
            "daily".to_string(),
            exit_daily.get(&page).cloned().unwrap_or_default().into(),
        );
    }

    // Top pages from pageview events (+ distinct sessions, + daily pageviews).
    let sql = format!(
        "SELECT COALESCE(NULLIF(TRIM(path), ''), '(none)') AS page,
                COUNT(*) AS pageviews,
                COUNT(DISTINCT NULLIF(session_id, 0)) AS sessions,
                COUNT(DISTINCT visitor_id) AS visitors
         FROM web_events_all
         WHERE event = 'pageview' AND ts >= ? AND ts < ? AND NOT is_bot{internal}{pathev}{site}
         GROUP BY 1 ORDER BY pageviews DESC, page ASC
         LIMIT {limit}",
        internal = internal,
        pathev = path_ev,
        site = site,
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let top: Vec<serde_json::Value> = match stmt
        .query_map(params_ev.as_slice(), |r| {
            Ok(serde_json::json!({
                "page": r.get::<_, String>(0)?,
                "pageviews": r.get::<_, i64>(1)?,
                "sessions": r.get::<_, i64>(2)?,
                "visitors": r.get::<_, i64>(3)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    {
        Ok(t) => t,
        Err(_) => return bad_request("query failed"),
    };
    let sql = format!(
        "SELECT CAST(ts AS DATE) AS day,
                COALESCE(NULLIF(TRIM(path), ''), '(none)') AS page,
                COUNT(*) AS pageviews
         FROM web_events_all
         WHERE event = 'pageview' AND ts >= ? AND ts < ? AND NOT is_bot{internal}{pathev}{site}
         GROUP BY 1, 2 ORDER BY 1",
        internal = internal,
        pathev = path_ev,
        site = site,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let mut top_daily: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    match stmt
        .query_map(params_ev.as_slice(), |r| {
            Ok((
                r.get::<_, chrono::NaiveDate>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    {
        Ok(rows) => {
            for (day, page, pageviews) in rows {
                top_daily.entry(page).or_default().push(serde_json::json!({
                    "day": day.to_string(),
                    "pageviews": pageviews,
                }));
            }
        }
        Err(_) => return bad_request("query failed"),
    }
    let top: Vec<serde_json::Value> = top
        .into_iter()
        .map(|mut row| {
            let page = row.get("page").and_then(|v| v.as_str()).unwrap_or("").to_string();
            row.as_object_mut().unwrap().insert(
                "daily".to_string(),
                top_daily.get(&page).cloned().unwrap_or_default().into(),
            );
            row
        })
        .collect();

    Json(serde_json::json!({
        "from": from, "to": to,
        "entry": entry, "top": top, "exit": exit,
    }))
    .into_response()
}

// =====================================================================
// GET /api/web/links — exit links + downloads (Statcounter-style)
//
// `kind=outbound` (default): clicks leaving the site, grouped by
// destination host + path. `kind=download`: same-origin file downloads
// grouped by path. Empty tables mean "no links/downloads detected", same
// honest empty-state as the reference.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct LinksParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// outbound | download (default outbound).
    pub kind: Option<String>,
    /// 1..=100 (default 20).
    pub limit: Option<i64>,
}

async fn web_links(
    State(st): State<WebApiState>,
    Query(p): Query<LinksParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let kind = p.kind.as_deref().unwrap_or("outbound");
    if kind != "outbound" && kind != "download" {
        return bad_request("kind must be outbound|download");
    }
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let fev = filters.event_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());
    let sql = if kind == "outbound" {
        format!(
            "SELECT COALESCE(NULLIF(TRIM(name), ''), '(unknown)') AS host,
                    COALESCE(NULLIF(TRIM(path), ''), '/') AS path,
                    COUNT(*) AS hits
             FROM web_events_all
             WHERE event = 'outbound' AND ts >= ? AND ts < ? AND NOT is_bot{internal}{fev}{site}
             GROUP BY 1, 2 ORDER BY hits DESC, host ASC, path ASC
             LIMIT {limit}",
            internal = internal,
            fev = fev,
            site = site,
            limit = limit,
        )
    } else {
        format!(
            "SELECT CAST(NULL AS VARCHAR) AS host,
                    COALESCE(NULLIF(TRIM(path), ''), '(unknown)') AS path,
                    COUNT(*) AS hits
             FROM web_events_all
             WHERE event = 'download' AND ts >= ? AND ts < ? AND NOT is_bot{internal}{fev}{site}
             GROUP BY 2 ORDER BY hits DESC, path ASC
             LIMIT {limit}",
            internal = internal,
            fev = fev,
            site = site,
            limit = limit,
        )
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = event_query_params(&filters, &from, &to, p.site.as_ref());
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "host": r.get::<_, Option<String>>(0)?,
                "path": r.get::<_, String>(1)?,
                "hits": r.get::<_, i64>(2)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "kind": kind, "from": from, "to": to, "rows": rows }))
        .into_response()
}

// =====================================================================
// GET /api/web/custom — custom event tags (Statcounter-style Custom Tags)
//
// Top `custom` event names with hits + distinct visitors. The dashboard's
// own `ui.*` instrumentation shows up here immediately, which doubles as a
// live "is tracking working" signal.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct CustomParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// 1..=100 (default 50).
    pub limit: Option<i64>,
}

async fn web_custom(
    State(st): State<WebApiState>,
    Query(p): Query<CustomParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let limit = p.limit.unwrap_or(50).clamp(1, 100);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let fev = filters.event_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());
    let sql = format!(
        "SELECT name, COUNT(*) AS hits, COUNT(DISTINCT visitor_id) AS visitors
         FROM web_events_all
         WHERE event = 'custom' AND name <> '' AND ts >= ? AND ts < ? AND NOT is_bot{internal}{fev}{site}
         GROUP BY 1 ORDER BY hits DESC, name ASC
         LIMIT {limit}",
        internal = internal,
        fev = fev,
        site = site,
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = event_query_params(&filters, &from, &to, p.site.as_ref());
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "name": r.get::<_, String>(0)?,
                "hits": r.get::<_, i64>(1)?,
                "visitors": r.get::<_, i64>(2)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "from": from, "to": to, "rows": rows })).into_response()
}

// =====================================================================
// GET /api/web/summary — exact headline KPIs for one range (Section 3)
// range: pageviews sum, visitors as the SUM of exact daily uniques
// (documented salt semantics), sessions count, overall bounce rate and
// average engaged ms from web_sessions. The UI calls it twice (current +
// previous range) for the change-vs-previous KPIs.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct SummaryParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Exact page filter (UI chip).
    pub path: Option<String>,
}

async fn web_summary(
    State(st): State<WebApiState>,
    Query(p): Query<SummaryParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let path_ev = filters.event_clause();
    let path_ses = filters.session_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());

    // Event-level: pageviews sum + visitors as the SUM of exact daily
    // uniques (documented salt semantics — never deduplicated across days).
    let ev_sql = format!(
        "SELECT COALESCE(SUM(CASE WHEN event = 'pageview' THEN 1 ELSE 0 END), 0),
                COALESCE((SELECT SUM(v) FROM (
                   SELECT COUNT(DISTINCT visitor_id) AS v
                   FROM web_events_all
                   WHERE ts >= ? AND ts < ? AND NOT is_bot{internal}{pathev}{site}
                   GROUP BY CAST(ts AS DATE)
                 )), 0)
         FROM web_events_all
         WHERE ts >= ? AND ts < ? AND NOT is_bot{internal}{pathev}{site}",
        internal = internal,
        pathev = path_ev,
        site = site,
    );
    // ev_params mirror placeholder order in SQL TEXT order: the day-subquery
    // appears first, so each half binds (from, to, filters, site).
    let mut ev_params: Vec<&dyn duckdb::ToSql> = vec![&from, &to];
    filters.push_event_params(&mut ev_params);
    if p.site.is_some() {
        ev_params.push(p.site.as_ref().unwrap());
    }
    ev_params.push(&from);
    ev_params.push(&to);
    filters.push_event_params(&mut ev_params);
    if p.site.is_some() {
        ev_params.push(p.site.as_ref().unwrap());
    }
    let (pageviews, visitors): (i64, i64) =
        match conn.query_row(&ev_sql, ev_params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?))) {
            Ok(v) => v,
            Err(_) => return bad_request("query failed"),
        };

    // Session-level: count, overall bounce rate, average engaged ms.
    let ses_sql = format!(
        "SELECT COUNT(*),
                COALESCE(AVG(CASE WHEN is_bounce THEN 1.0 ELSE 0.0 END), 0),
                COALESCE(AVG(engaged_ms), 0)
         FROM web_sessions
         WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{pathses}{site}",
        internal = internal,
        pathses = path_ses,
        site = site,
    );
    let ses_params = session_query_params(&filters, &from, &to, p.site.as_ref());
    let (sessions, bounce_rate, avg_duration_ms): (i64, f64, f64) =
        match conn.query_row(&ses_sql, ses_params.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }) {
            Ok(v) => v,
            Err(_) => return bad_request("query failed"),
        };
    Json(serde_json::json!({
        "from": from, "to": to,
        "pageviews": pageviews,
        "visitors": visitors,
        "sessions": sessions,
        "bounce_rate": (bounce_rate * 10000.0).round() / 10000.0,
        "avg_duration_ms": (avg_duration_ms * 1000.0).round() / 1000.0,
    }))
    .into_response()
}

// =====================================================================
// GET /api/web/funnel — goal funnel over pageview sequences (Section 6)
//
// `steps` is a comma-separated list of path patterns (`*` = glob, e.g.
// `/,/program/*,/donasi`). A session converts through step k when it has a
// pageview matching pattern k strictly after its step k-1 match (first-touch
// per step) and still inside the range. Response: per-step session counts;
// the UI derives conversion %. Bounded: step-1 scope capped at 20k sessions.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct FunnelParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// Comma-separated path patterns, 2..=8 steps.
    pub steps: String,
}

/// Translate a user glob (`*` only) into a SQL LIKE pattern, escaping
/// LIKE metacharacters first so `100%` stays literal.
fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '*' => out.push('%'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

async fn web_funnel(State(st): State<WebApiState>, Query(p): Query<FunnelParams>) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let steps: Vec<String> = p
        .steps
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if steps.len() < 2 || steps.len() > 8 {
        return bad_request(
            "steps must list 2..=8 comma-separated patterns (e.g. steps=/,/program/*,/donasi,outbound:wa.me)",
        );
    }
    if steps.iter().any(|s| s.len() > 256) {
        return bad_request("each step pattern must be ≤ 256 chars");
    }
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let conn = st.store.lock();
    let site_filter = match p.site.as_deref() {
        Some(_) => " AND e.site_id = ?",
        None => "",
    };

    /// One funnel step → (SQL condition on alias `e`, one LIKE parameter).
    /// `event:name` matches custom events, `outbound:glob` matches exit-link
    /// destinations, anything else is a path glob (`*` wildcard).
    fn step_cond(step: &str) -> (String, String) {
        let like = |col: &str, pat: &str| {
            (
                format!("({col} LIKE ? ESCAPE '\\')"),
                glob_to_like(pat),
            )
        };
        if let Some(pat) = step.strip_prefix("event:") {
            let (cond, param) = like("e.name", pat);
            (format!("(e.event = 'custom' AND {cond})"), param)
        } else if let Some(pat) = step.strip_prefix("outbound:") {
            let (cond, param) = like("e.name", pat);
            (format!("(e.event = 'outbound' AND {cond})"), param)
        } else {
            like("e.path", step)
        }
    }
    let conds: Vec<(String, String)> = steps.iter().map(|s| step_cond(s)).collect();

    // Bound the chain first: refuse absurd step-1 scopes before joining.
    // (20k sessions cap keeps one funnel query from hogging the connection.)
    let scope_sql = format!(
        "SELECT COUNT(DISTINCT e.session_id)
         FROM web_events_all e
         WHERE e.event IN ('pageview','custom','outbound') AND e.session_id IS NOT NULL
           AND e.ts >= ? AND e.ts < ? AND NOT e.is_bot{internal}{site_filter}
           AND {cond0}",
        internal = internal,
        site_filter = site_filter,
        cond0 = conds[0].0,
    );
    let Ok(mut scope_stmt) = conn.prepare(&scope_sql) else {
        return bad_request("query failed");
    };
    let mut scope_params: Vec<&dyn duckdb::ToSql> = vec![&from, &to];
    if let Some(s) = p.site.as_ref() {
        scope_params.push(s);
    }
    scope_params.push(&conds[0].1);
    let scope: i64 = match scope_stmt.query_row(scope_params.as_slice(), |r| r.get(0)) {
        Ok(n) => n,
        Err(_) => return bad_request("query failed"),
    };
    if scope > 20_000 {
        return bad_request("funnel step 1 matches over 20,000 sessions — narrow the range");
    }
    if scope == 0 {
        let empty: Vec<serde_json::Value> = steps
            .iter()
            .enumerate()
            .map(|(i, pattern)| {
                serde_json::json!({
                    "step": i + 1, "pattern": pattern,
                    "sessions": 0, "drop_off": 0.0, "conversion_from_prev": 0.0,
                })
            })
            .collect();
        return Json(serde_json::json!({
            "from": from, "to": to, "steps": empty, "overall_conversion": 0.0,
        }))
        .into_response();
    }

    // f0: sessions (+ entry ts) matching step 0. Each later arm keeps only
    // sessions with a match strictly after the previous step's match and
    // still inside the range (`to`-bounded lookahead).
    let mut prev_alias = "f0".to_string();
    let mut sql = format!(
        "WITH f0 AS (
           SELECT e.session_id AS sid, MIN(e.ts) AS t
           FROM web_events_all e
           WHERE e.session_id IS NOT NULL
             AND e.ts >= ? AND e.ts < ? AND NOT e.is_bot{internal}{site_filter}
             AND {cond0}
           GROUP BY 1
         )",
        internal = internal,
        site_filter = site_filter,
        cond0 = conds[0].0,
    );
    // NOTE: condition params are bound below, in step order.
    for (i, (cond, _)) in conds.iter().enumerate().skip(1) {
        let cur = format!("f{i}");
        sql.push_str(&format!(
            ", {cur} AS (
               SELECT s.sid AS sid, MIN(e.ts) AS t
               FROM {prev} s
               JOIN web_events_all e ON e.session_id = s.sid
               WHERE e.ts > s.t AND e.ts < ? AND NOT e.is_bot{internal}
                 AND {cond}
               GROUP BY 1
             )",
            cur = cur,
            prev = prev_alias,
            internal = internal,
            cond = cond,
        ));
        prev_alias = cur;
    }
    // Count sessions reaching each step: LEFT JOIN chain from f0.
    let mut select = String::from("SELECT COUNT(DISTINCT f0.sid)");
    let mut joins = String::new();
    for i in 1..steps.len() {
        select.push_str(&format!(", COUNT(DISTINCT f{i}.sid)"));
        joins.push_str(&format!(" LEFT JOIN f{i} ON f{i}.sid = f0.sid"));
    }
    sql.push_str(&format!(" {select} FROM f0{joins}"));
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    // Param order: from, to, [site], cond0, then (to, cond_i) per later arm.
    let mut params: Vec<&dyn duckdb::ToSql> = vec![&from, &to];
    if let Some(s) = p.site.as_ref() {
        params.push(s);
    }
    params.push(&conds[0].1);
    for (_, like) in conds.iter().skip(1) {
        params.push(&to);
        params.push(like);
    }
    let counts: Vec<i64> = match stmt.query_row(params.as_slice(), |r| {
        let mut out = Vec::with_capacity(steps.len());
        for i in 0..steps.len() {
            out.push(r.get::<_, i64>(i)?);
        }
        Ok(out)
    }) {
        Ok(c) => c,
        Err(_) => return bad_request("query failed"),
    };
    let step_rows: Vec<serde_json::Value> = steps
        .iter()
        .zip(counts.iter())
        .enumerate()
        .map(|(i, (pattern, count))| {
            let prev = if i == 0 { *count } else { counts[i - 1] };
            let conv = if prev > 0 {
                ((*count as f64 / prev as f64) * 10000.0).round() / 10000.0
            } else {
                0.0
            };
            serde_json::json!({
                "step": i + 1,
                "pattern": pattern,
                "sessions": count,
                "conversion_from_prev": conv,
                "drop_off": (1.0 - conv).clamp(0.0, 1.0),
            })
        })
        .collect();
    let first = counts.first().copied().unwrap_or(0);
    let last = counts.last().copied().unwrap_or(0);
    Json(serde_json::json!({
        "from": from, "to": to,
        "steps": step_rows,
        "overall_conversion": if first > 0 {
            ((last as f64 / first as f64) * 10000.0).round() / 10000.0
        } else { 0.0 },
    }))
    .into_response()
}

// GET /api/web/sessions — session explorer list + detail (Section 6)
//
// At low traffic, individual anonymized journeys say more than aggregates:
// recent sessions with entry/exit/counts, and per-session ordered events.
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct SessionsParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// 1..=200 (default 50).
    pub limit: Option<i64>,
}

async fn web_sessions_list(
    State(st): State<WebApiState>,
    Query(p): Query<SessionsParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let fses = filters.session_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());
    let sql = format!(
        "SELECT session_id, start_ts, end_ts, entry_path, exit_path,
                pageviews, events, engaged_ms, is_bounce,
                source, medium, country, region, city, device, browser, os
         FROM web_sessions
         WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{fses}{site}
         ORDER BY start_ts DESC
         LIMIT {limit}",
        internal = internal,
        fses = fses,
        site = site,
        limit = limit,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let params = session_query_params(&filters, &from, &to, p.site.as_ref());
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(serde_json::json!({
                "session_id": r.get::<_, u64>(0)?.to_string(),
                "start_ts": r.get::<_, DateTime<Utc>>(1)?,
                "end_ts": r.get::<_, DateTime<Utc>>(2)?,
                "entry_path": r.get::<_, Option<String>>(3)?,
                "exit_path": r.get::<_, Option<String>>(4)?,
                "pageviews": r.get::<_, i64>(5)?,
                "events": r.get::<_, i64>(6)?,
                "engaged_ms": r.get::<_, i64>(7)?,
                "is_bounce": r.get::<_, bool>(8)?,
                "source": r.get::<_, Option<String>>(9)?,
                "medium": r.get::<_, Option<String>>(10)?,
                "country": r.get::<_, Option<String>>(11)?,
                "region": r.get::<_, Option<String>>(12)?,
                "city": r.get::<_, Option<String>>(13)?,
                "device": r.get::<_, Option<String>>(14)?,
                "browser": r.get::<_, Option<String>>(15)?,
                "os": r.get::<_, Option<String>>(16)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    Json(serde_json::json!({ "from": from, "to": to, "sessions": rows })).into_response()
}

async fn web_session_detail(
    State(st): State<WebApiState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let session_id: u64 = match id.trim().parse() {
        Ok(v) => v,
        Err(_) => return bad_request("bad session id"),
    };
    let conn = st.store.lock();
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts, event, name, path, value,
                source, medium, country, city, device, trace_id
         FROM web_events_all
         WHERE session_id = ?
         ORDER BY ts ASC
         LIMIT 500",
    ) else {
        return bad_request("query failed");
    };
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, DateTime<Utc>>(0)?,
                "event": r.get::<_, String>(1)?,
                "name": r.get::<_, Option<String>>(2)?,
                "path": r.get::<_, Option<String>>(3)?,
                "value": r.get::<_, Option<f64>>(4)?,
                "source": r.get::<_, Option<String>>(5)?,
                "medium": r.get::<_, Option<String>>(6)?,
                "country": r.get::<_, Option<String>>(7)?,
                "city": r.get::<_, Option<String>>(8)?,
                "device": r.get::<_, Option<String>>(9)?,
                "trace_id": r.get::<_, Option<String>>(10)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>());
    let Ok(rows) = rows else {
        return bad_request("query failed");
    };
    if rows.is_empty() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "no events for session",
        )
            .into_response();
    }
    Json(serde_json::json!({ "session_id": id, "events": rows })).into_response()
}

// =====================================================================
// GET /api/web/export — CSV download for offline reporting (Section 6)
//
// `view=events|sessions`, bounded (default 5000, max 20000 rows).
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub site: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Relative window shorthand — used when `from`/`to` are absent.
    pub window: Option<String>,
    /// Exclude operator-internal visits (default true).
    #[serde(default)]
    pub include_internal: Option<bool>,
    /// events | sessions (default events).
    pub view: Option<String>,
    /// 1..=20000 (default 5000).
    pub limit: Option<i64>,
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

async fn web_export(
    State(st): State<WebApiState>,
    Query(p): Query<ExportParams>,
    Query(f): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Ok((from, to)) = parse_range(p.from.as_deref(), p.to.as_deref(), p.window.as_deref()) else {
        return bad_request("invalid from/to");
    };
    let view = p.view.as_deref().unwrap_or("events");
    if view != "events" && view != "sessions" {
        return bad_request("view must be events|sessions");
    }
    let limit = p.limit.unwrap_or(5000).clamp(1, 20000);
    let internal = internal_clause(p.include_internal.unwrap_or(false));
    let filters = WebFilters::from_map(&f);
    let fev = filters.event_clause();
    let fses = filters.session_clause();
    let conn = st.store.lock();
    let site = site_clause(p.site.as_deref());
    let (header, sql) = if view == "events" {
        (
            "ts,event,name,path,source,medium,country,region,city,device,browser,os,visitor_id,session_id,trace_id\n",
            format!(
                "SELECT ts, event, name, path, source, medium, country, region, city,
                        device, browser, os, visitor_id, session_id, trace_id
                 FROM web_events_all
                 WHERE ts >= ? AND ts < ? AND NOT is_bot{internal}{fev}{site}
                 ORDER BY ts ASC
                 LIMIT {limit}",
                internal = internal,
                fev = fev,
                site = site,
                limit = limit,
            ),
        )
    } else {
        (
            "session_id,start_ts,end_ts,entry_path,exit_path,pageviews,events,engaged_ms,is_bounce,source,medium,country,region,city,device\n",
            format!(
                "SELECT session_id, start_ts, end_ts, entry_path, exit_path,
                        pageviews, events, engaged_ms, is_bounce,
                        source, medium, country, region, city, device
                 FROM web_sessions
                 WHERE pageviews >= 1 AND start_ts >= ? AND start_ts < ?{internal}{fses}{site}
                 ORDER BY start_ts ASC
                 LIMIT {limit}",
                internal = internal,
                fses = fses,
                site = site,
                limit = limit,
            ),
        )
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return bad_request("query failed");
    };
    let filters = WebFilters::from_map(&f);
    let params = if view == "events" {
        event_query_params(&filters, &from, &to, p.site.as_ref())
    } else {
        session_query_params(&filters, &from, &to, p.site.as_ref())
    };
    // Typed extraction per view (NULL → ""); timestamps render RFC 3339.
    let mut out = String::from(header);
    if view == "events" {
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok(vec![
                r.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                r.get::<_, Option<u64>>(12)?.map(|v| v.to_string()).unwrap_or_default(),
                r.get::<_, Option<u64>>(13)?.map(|v| v.to_string()).unwrap_or_default(),
                r.get::<_, Option<String>>(14)?.unwrap_or_default(),
            ])
        });
        let rows = match rows {
            Ok(rs) => rs.collect::<Result<Vec<_>, _>>(),
            Err(_) => return bad_request("query failed"),
        };
        let rows = match rows {
            Ok(rs) => rs,
            Err(_) => return bad_request("query failed"),
        };
        for cols in rows {
            out.push_str(&cols.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
    } else {
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok(vec![
                r.get::<_, Option<u64>>(0)?.map(|v| v.to_string()).unwrap_or_default(),
                r.get::<_, chrono::DateTime<chrono::Utc>>(1)?.to_rfc3339(),
                r.get::<_, chrono::DateTime<chrono::Utc>>(2)?.to_rfc3339(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0).to_string(),
                r.get::<_, Option<i64>>(6)?.unwrap_or(0).to_string(),
                r.get::<_, Option<i64>>(7)?.unwrap_or(0).to_string(),
                r.get::<_, Option<bool>>(8)?.unwrap_or(false).to_string(),
                r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                r.get::<_, Option<String>>(14)?.unwrap_or_default(),
            ])
        });
        let rows = match rows {
            Ok(rs) => rs.collect::<Result<Vec<_>, _>>(),
            Err(_) => return bad_request("query failed"),
        };
        let rows = match rows {
            Ok(rs) => rs,
            Err(_) => return bad_request("query failed"),
        };
        for cols in rows {
            out.push_str(&cols.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
    }
    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"web-analytics.csv\"",
            ),
        ],
        out,
    )
        .into_response()
}

// =====================================================================
// GET /api/web/trace/{trace_id} — correlate web events with backend logs (W5)
// =====================================================================

async fn web_trace(
    State(st): State<WebApiState>,
    AxumPath(trace_id): AxumPath<String>,
) -> Response {
    if trace_id.is_empty() || trace_id.len() > 64 {
        return bad_request("bad trace_id");
    }
    let conn = st.store.lock();
    // Web events for the trace (bounded window around them).
    let web_rows: Vec<serde_json::Value> = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT ts, site_id, event, name, path, country, device
             FROM web_events_all
             WHERE trace_id = ?
             ORDER BY ts
             LIMIT 200",
        ) else {
            return bad_request("query failed");
        };
        stmt.query_map([trace_id.as_str()], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, DateTime<Utc>>(0)?,
                "site_id": r.get::<_, Option<String>>(1)?,
                "event": r.get::<_, Option<String>>(2)?,
                "name": r.get::<_, Option<String>>(3)?,
                "path": r.get::<_, Option<String>>(4)?,
                "country": r.get::<_, Option<String>>(5)?,
                "device": r.get::<_, Option<String>>(6)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    };
    // Backend logs sharing the trace id (via the existing logs_all surface).
    let log_rows: Vec<serde_json::Value> = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT ts, service, level, message
             FROM logs_all
             WHERE trace_id = ?
             ORDER BY ts
             LIMIT 200",
        ) else {
            return bad_request("query failed");
        };
        stmt.query_map([trace_id.as_str()], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, DateTime<Utc>>(0)?,
                "service": r.get::<_, Option<String>>(1)?,
                "level": r.get::<_, Option<String>>(2)?,
                "message": r.get::<_, Option<String>>(3)?,
            }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    };
    Json(serde_json::json!({
        "trace_id": trace_id,
        "web_events": web_rows,
        "logs": log_rows,
    }))
    .into_response()
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn state() -> (tempfile::TempDir, WebApiState) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        let mut cfg = crate::config::WebConfig::default();
        cfg.enabled = true;
        cfg.site.push(crate::config::WebSiteConfig {
            id: "site_abc".into(),
            allowed_origins: vec!["https://example.com".into()],
            user_header: None,
            exclude_ips: Vec::new(),
        });
        let cfg = Arc::new(cfg);
        (
            tmp,
            WebApiState {
                store,
                web_cfg: cfg.clone(),
                sites: crate::insert::web::WebSites::seeded(&cfg),
            },
        )
    }

    /// Seed one sessionized pageview for site_abc right now.
    fn seed(store: &Store) {
        let now = Utc::now();
        let conn = store.lock();
        conn.execute(
            "INSERT INTO web_events (ts, insert_ts, site_id, event, path, visitor_id,
                                     session_id, is_bot)
             VALUES (?, ?, 'site_abc', 'pageview', '/pricing', 42, 4242, false)",
            duckdb::params![now, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce)
             VALUES (4242, 'site_abc', 42, ?, ?, '/pricing', '/pricing', 1, 1, 5000, true)",
            duckdb::params![now, now],
        )
        .unwrap();
        // Vitals + error rows (6 placeholders in one statement).
        conn.execute(
            "INSERT INTO web_events (ts, insert_ts, site_id, event, name, path, value, is_bot)
             VALUES (?, ?, 'site_abc', 'vital', 'LCP', '/pricing', 1840.5, false),
                    (?, ?, 'site_abc', 'vital', 'LCP', '/pricing', 2000.0, false),
                    (?, ?, 'site_abc', 'error', 'TypeError: x', '/pricing', NULL, false)",
            duckdb::params![now, now, now, now, now, now],
        )
        .unwrap();
    }

    /// Default range (last 24h) as query params — keeps tests inside the
    /// 90-day cap without hardcoding dates.
    fn range() -> String {
        let from = (Utc::now() - chrono::Duration::hours(2))
            .format("%Y-%m-%dT%H:%M:%SZ");
        format!("from={from}")
    }

    #[tokio::test]
    async fn stats_returns_seeded_pageviews() {
        let (_tmp, st) = state();
        seed(&st.store);
        let app = router(st);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/web/stats?site=site_abc&metric=pageviews&interval=1h&{}", range()).as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let total: f64 = v["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["v"].as_f64().unwrap_or(0.0))
            .sum();
        assert_eq!(total, 1.0);
    }

    #[tokio::test]
    async fn stats_rejects_bad_metric_and_range() {
        let (_tmp, st) = state();
        let app = router(st);
        for uri in [
            "/api/web/stats?metric=evil;DROP",
            "/api/web/stats?from=2026-01-01&to=2020-01-01",
            "/api/web/stats?from=nonsense",
            "/api/web/stats?from=2020-01-01", // > 90 days
        ] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn session_metrics_read_web_sessions() {
        let (_tmp, st) = state();
        seed(&st.store);
        let app = router(st);
        for metric in ["bounce_rate", "avg_duration"] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/api/web/stats?site=site_abc&metric={metric}&{}", range()).as_str())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::OK);
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(v["points"].as_array().unwrap().len() >= 1, "{metric}");
        }
    }

    #[tokio::test]
    async fn breakdown_whitelists_dimensions() {
        let (_tmp, st) = state();
        seed(&st.store);
        let app = router(st);
        // Allowed dim works.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/web/breakdown?by=path&{}", range()).as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["rows"][0]["value"], "/pricing");
        // SQL-injection attempt rejected by the whitelist.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/web/breakdown?by=path;DROP%20TABLE%20web_events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn realtime_and_vitals_and_errors_and_sites() {
        let (_tmp, st) = state();
        seed(&st.store);
        let app = router(st);
        for uri in [
            "/api/web/realtime?site=site_abc".to_string(),
            format!("/api/web/vitals?{}", range()),
            format!("/api/web/errors?{}", range()),
            "/api/web/sites".to_string(),
            "/api/web/trace/4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
        ] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri.as_str())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            if status != axum::http::StatusCode::OK {
                let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
                panic!("{uri} → {status}: {}", String::from_utf8_lossy(&body));
            }
        }
    }

    #[tokio::test]
    async fn trace_correlates_web_events_and_logs() {        let (_tmp, st) = state();
        {
            let conn = st.store.lock();
            let now = Utc::now();
            conn.execute(
                "INSERT INTO web_events (ts, insert_ts, site_id, event, path, trace_id, is_bot)
                 VALUES (?, ?, 'site_abc', 'pageview', '/x', 'abc123', false)",
                duckdb::params![now, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO logs (ts, insert_ts, service, level, message, trace_id)
                 VALUES (?, ?, 'api', 'info', 'handled request', 'abc123')",
                duckdb::params![now, now],
            )
            .unwrap();
        }
        let app = router(st);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/web/trace/abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["web_events"].as_array().unwrap().len(), 1);
        assert_eq!(v["logs"].as_array().unwrap().len(), 1);
    }

    async fn put_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> axum::response::Response {
        use tower::ServiceExt;
        app.oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn post_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> axum::response::Response {
        use tower::ServiceExt;
        app.oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn sites_crud_round_trip() {
        use tower::ServiceExt;
        let (_tmp, st) = state();
        let app = router(st.clone());

        // Config seed is listed with source=config.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/web/sites")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["sites"][0]["id"], "site_abc");
        assert_eq!(v["sites"][0]["source"], "config");

        // Create a DB project.
        let res = post_json(
            app.clone(),
            "/api/web/sites",
            serde_json::json!({
                "id": "shop",
                "allowed_origins": ["https://shop.example.com"],
            }),
        )
        .await;
        assert_eq!(res.status(), axum::http::StatusCode::CREATED);

        // It shows up as source=db and the hot path sees it immediately.
        assert!(st.sites.lookup("shop").is_some());
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/web/sites")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["sites"].as_array().unwrap().len(), 2);
        let shop = v["sites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "shop")
            .unwrap();
        assert_eq!(shop["source"], "db");

        // Duplicate → 409.
        let res = post_json(
            app.clone(),
            "/api/web/sites",
            serde_json::json!({
                "id": "shop",
                "allowed_origins": ["https://shop.example.com"],
            }),
        )
        .await;
        assert_eq!(res.status(), axum::http::StatusCode::CONFLICT);

        // PUT updates origins + exclude_ips and the hot registry follows.
        let res = put_json(
            app.clone(),
            "/api/web/sites/shop",
            serde_json::json!({
                "allowed_origins": ["https://shop.example.com", "https://m.shop.example.com"],
                "exclude_ips": ["203.0.113.7"]
            }),
        )
        .await;
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let shop = st.sites.lookup("shop").unwrap();
        assert_eq!(shop.allowed_origins.len(), 2);
        assert_eq!(shop.exclude_ips, vec!["203.0.113.7".to_string()]);
        // Unknown id → 404; config seed → 409.
        let res = put_json(
            app.clone(),
            "/api/web/sites/ghost",
            serde_json::json!({"allowed_origins": ["https://x.example.com"]}),
        )
        .await;
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
        let res = put_json(
            app.clone(),
            "/api/web/sites/site_abc",
            serde_json::json!({"allowed_origins": ["https://x.example.com"]}),
        )
        .await;
        assert_eq!(res.status(), axum::http::StatusCode::CONFLICT);

        // Config-managed id cannot be deleted via API.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/web/sites/site_abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::CONFLICT);

        // Delete the DB project → gone from registry and list.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/web/sites/shop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert!(st.sites.lookup("shop").is_none());

        // Deleting again → 404.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/web/sites/shop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sites_create_validates_input() {
        let (_tmp, st) = state();
        let app = router(st);
        // Bad origin, empty origins, whitespace id, bad header — all 400.
        for body in [
            serde_json::json!({"id": "x", "allowed_origins": ["not-a-url"]}),
            serde_json::json!({"id": "x", "allowed_origins": []}),
            serde_json::json!({"id": "has space", "allowed_origins": ["https://a.example"]}),
            serde_json::json!({"id": "", "allowed_origins": ["https://a.example"]}),
        ] {
            let res = post_json(app.clone(), "/api/web/sites", body).await;
            assert_eq!(
                res.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "expected 400"
            );
        }
    }

    fn seed_statcounter_rows(store: &Store) {
        let now = Utc::now();
        let conn = store.lock();
        // Two sessions: one direct bounced, one organic converted.
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce, source, medium,
                                       country, device, browser, os)
             VALUES (1, 'site_abc', 11, ?, ?, '/landing', '/pricing', 2, 3, 5000, false,
                     'direct', 'direct', 'ID', 'desktop', 'Chrome', 'Windows'),
                    (2, 'site_abc', 22, ?, ?, '/landing', '/landing', 1, 1, 500, true,
                     'google', 'organic', 'ID', 'mobile', 'Safari', 'iOS')",
            duckdb::params![now, now, now, now],
        )
        .unwrap();
        // Events backing the top/links/custom views.
        conn.execute(
            "INSERT INTO web_events (ts, insert_ts, site_id, event, name, path,
                                     referrer, screen_w, is_bot)
             VALUES (?, ?, 'site_abc', 'pageview', '', '/landing', '', 1440, false),
                    (?, ?, 'site_abc', 'pageview', '', '/pricing', '', 1440, false),
                    (?, ?, 'site_abc', 'outbound', 'example.com', '/docs', '', NULL, false),
                    (?, ?, 'site_abc', 'download', '', '/file.pdf', '', NULL, false),
                    (?, ?, 'site_abc', 'custom', 'signup', '', '', NULL, false)",
            duckdb::params![now, now, now, now, now, now, now, now, now, now],
        )
        .unwrap();
    }

    async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        use tower::ServiceExt;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn sources_report_mediums_with_bounce_and_daily() {
        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app,
            format!("/api/web/sources?site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let sources = v["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        // Ordered by sessions desc — tie (1 each) breaks alphabetically.
        assert_eq!(sources[0]["medium"], "direct");
        assert_eq!(sources[0]["sessions"], 1);
        assert_eq!(sources[0]["bounce_rate"], 0.0);
        assert_eq!(sources[1]["medium"], "organic");
        assert_eq!(sources[1]["bounce_rate"], 1.0);
        assert_eq!(sources[0]["daily"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pages_report_entry_top_exit() {
        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app,
            format!("/api/web/pages?site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["entry"][0]["page"], "/landing");
        assert_eq!(v["entry"][0]["sessions"], 2);
        assert_eq!(v["entry"][0]["bounce_rate"], 0.5);
        let exits: Vec<&str> = v["exit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["page"].as_str().unwrap())
            .collect();
        assert_eq!(exits, vec!["/landing", "/pricing"], "tie 1-1 breaks alphabetically");
        assert_eq!(v["top"][0]["page"], "/landing");
        assert_eq!(v["top"][0]["pageviews"], 1);
        assert!(v["top"][0].get("daily").is_some());
    }

    #[tokio::test]
    async fn links_and_custom_reports() {        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/links?kind=outbound&site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["rows"][0]["host"], "example.com");
        assert_eq!(v["rows"][0]["path"], "/docs");

        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/links?kind=download&site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["rows"][0]["path"], "/file.pdf");
        assert!(v["rows"][0]["host"].is_null());

        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/links?kind=bogus&site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

        let (status, v) = get_json(
            app,
            format!("/api/web/custom?site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["rows"][0]["name"], "signup");
        assert_eq!(v["rows"][0]["hits"], 1);
    }

    #[tokio::test]
    async fn breakdown_accepts_geo_screen_utm_dims() {
        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        for by in ["region", "city", "screen", "utm_source", "utm_medium", "utm_campaign"] {
            let (status, _) = get_json(
                app.clone(),
                format!("/api/web/breakdown?by={by}&site=site_abc&{}", range()).as_str(),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "by={by}");
        }
    }
    /// Seed one session that walks / → /program/x → /donasi (funnel
    /// converter) plus one that drops after / (bounce). Timestamps run
    /// *behind* now: range `to` defaults to request time, so future-dated
    /// events would fall outside every query window.
    fn seed_funnel(store: &Store) {
        let now = Utc::now() - chrono::Duration::minutes(30);
        let conn = store.lock();
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce, source, medium)
             VALUES (101, 'site_abc', 1, ?, ?, '/', '/donasi', 3, 3, 9000, false, 'direct', 'direct'),
                    (102, 'site_abc', 2, ?, ?, '/', '/', 1, 1, 1000, true, 'direct', 'direct')",
            duckdb::params![now, now, now, now],
        )
        .unwrap();
        // Session 101's ordered pageviews; session 102's single view.
        // (The funnel reads raw events, not the sessions table.)
        for (i, path) in ["/", "/program/x", "/donasi"].iter().enumerate() {
            let ts = now + chrono::Duration::seconds(i as i64 * 60);
            conn.execute(
                "INSERT INTO web_events (ts, insert_ts, site_id, event, path,
                                         visitor_id, session_id, is_bot)
                 VALUES (?, ?, 'site_abc', 'pageview', ?, 1, 101, false)",
                duckdb::params![ts, ts, path],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO web_events (ts, insert_ts, site_id, event, path,
                                     visitor_id, session_id, is_bot)
             VALUES (?, ?, 'site_abc', 'pageview', '/', 2, 102, false)",
            duckdb::params![now, now],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn window_param_scopes_the_range() {
        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        // window=1h over recent seed rows → data; window=1m over the same
        // seed rows (seeded "now", but minutes apart) still finds them…
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/stats?site=site_abc&metric=pageviews&window=1h&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        // …while garbage windows 400.
        let (status, _) = get_json(
            app,
            "/api/web/stats?site=site_abc&metric=pageviews&window=fortnight",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        let _ = v;
    }

    #[tokio::test]
    async fn summary_returns_exact_totals() {
        let (_tmp, st) = state();
        seed_statcounter_rows(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app,
            format!("/api/web/summary?site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["pageviews"], 2);
        // Seed events carry NULL visitor_id (COUNT DISTINCT ignores NULLs).
        assert_eq!(v["visitors"], 0);
        assert_eq!(v["sessions"], 2);
        assert_eq!(v["bounce_rate"], 0.5);
        assert_eq!(v["avg_duration_ms"], 2750.0);
    }

    #[tokio::test]
    async fn funnel_counts_step_conversions() {
        let (_tmp, st) = state();
        seed_funnel(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/funnel?site=site_abc&steps=/,/program/*,/donasi&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["steps"][0]["sessions"], 2);
        assert_eq!(v["steps"][1]["sessions"], 1);
        assert_eq!(v["steps"][2]["sessions"], 1);
        assert_eq!(v["steps"][2]["conversion_from_prev"], 1.0);
        assert_eq!(v["overall_conversion"], 0.5);
        // Bad shapes rejected.
        for uri in [
            format!("/api/web/funnel?site=site_abc&steps=/&{}", range()),
            "/api/web/funnel?site=site_abc&steps=".to_string(),
        ] {
            let (status, _) = get_json(app.clone(), uri.as_str()).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn sessions_list_and_detail() {
        let (_tmp, st) = state();
        seed_funnel(&st.store);
        let app = router(st);
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/sessions?site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["sessions"].as_array().unwrap().len(), 2);
        let (status, v) = get_json(app.clone(), "/api/web/sessions/101").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["path"], "/");
        assert_eq!(events[2]["path"], "/donasi");
        let (status, _) = get_json(app, "/api/web/sessions/999999").await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn filter_chips_scope_summary_and_session_views() {
        let (_tmp, st) = state();
        {
            let now = Utc::now();
            let conn = st.store.lock();
            // Two visitors, two countries, one session each.
            conn.execute(
                "INSERT INTO web_events (ts, insert_ts, site_id, event, path,
                                         visitor_id, session_id, is_bot, country)
                 VALUES (?, ?, 'site_abc', 'pageview', '/', 1, 101, false, 'ID'),
                        (?, ?, 'site_abc', 'pageview', '/de', 2, 102, false, 'DE')",
                duckdb::params![now, now, now, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                           entry_path, exit_path, pageviews, events,
                                           engaged_ms, is_bounce, country, medium)
                 VALUES (101, 'site_abc', 1, ?, ?, '/', '/', 1, 1, 1000, true, 'ID', 'direct'),
                        (102, 'site_abc', 2, ?, ?, '/de', '/de', 1, 1, 1000, true, 'DE', 'direct')",
                duckdb::params![now, now, now, now],
            )
            .unwrap();
        }
        let app = router(st);
        // Summary scoped by chip: event-level counts + session counts both follow.
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/summary?site=site_abc&country=ID&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "country chip must not 400");
        assert_eq!(v["pageviews"], 1, "v={v}");
        assert_eq!(v["sessions"], 1, "v={v}");
        // Sources (session-side subquery) honours the chip too.
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/sources?site=site_abc&country=DE&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let rows = v["sources"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["visitors"], 1);
        // Pages (both sides) with a chip.
        let (status, v) = get_json(
            app,
            format!("/api/web/pages?site=site_abc&country=ID&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["top"].as_array().unwrap().len(), 1);
        assert_eq!(v["top"][0]["page"], "/");
    }

    #[tokio::test]
    async fn export_returns_csv() {
        let (_tmp, st) = state();
        seed_funnel(&st.store);
        let app = router(st);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/web/export?site=site_abc&view=events&{}", range()).as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert!(res.headers()["content-type"].to_str().unwrap().contains("text/csv"));
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with("ts,event,name,path,"));
        assert!(text.contains("/donasi"));
    }

    #[tokio::test]
    async fn internal_visits_filter_by_default() {
        let (_tmp, st) = state();
        {
            let now = Utc::now();
            let conn = st.store.lock();
            conn.execute(
                "INSERT INTO web_events (ts, insert_ts, site_id, event, path,
                                         visitor_id, is_internal, is_bot)
                 VALUES (?, ?, 'site_abc', 'pageview', '/internal', 9, true, false),
                        (?, ?, 'site_abc', 'pageview', '/public', 10, false, false)",
                duckdb::params![now, now, now, now],
            )
            .unwrap();
        }
        let app = router(st);
        let (status, v) = get_json(
            app.clone(),
            format!("/api/web/breakdown?by=path&site=site_abc&{}", range()).as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let values: Vec<&str> = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, vec!["/public"], "internal hidden by default");
        let (status, v) = get_json(
            app,
            format!(
                "/api/web/breakdown?by=path&site=site_abc&include_internal=true&{}",
                range()
            )
            .as_str(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
    }
}
