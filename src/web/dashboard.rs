//! Dashboard routes: HTML pages + JSON APIs that the charts poll.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::store::schema::store_state;
use crate::web::templates::{DashboardTemplate, MetricsApiTemplate};
use crate::web::WebState;

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/volume", get(api_volume))
        .route("/api/levels", get(api_levels))
        .route("/api/topn", get(api_topn))
        .route("/api/counters", get(api_counters))
        .route("/api/store", get(api_store))
        .route("/api/search", get(api_search))
        .route("/api/pipeline", get(api_pipeline))
        .route("/metrics", get(metrics_text))
        .route("/health", get(health))
        .with_state(state)
}

async fn dashboard_index(State(_st): State<WebState>) -> impl IntoResponse {
    let tmpl = DashboardTemplate {
        refresh_secs: 5,
        generated_at: chrono::Utc::now(),
    };
    let body = tmpl.render().unwrap_or_else(|e| format!("template error: {e}"));
    Html(body)
}

#[derive(Debug, Deserialize)]
pub struct TimeRange {
    pub hours: Option<i64>,
}

async fn api_volume(
    State(st): State<WebState>,
    Query(q): Query<TimeRange>,
) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(1).max(1);
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);
    let conn = st.store.conn();
    let conn = conn.lock();
    // NOTE: the cutoff is bound as a parameter instead of computing
    // `NOW() - INTERVAL` in SQL — DuckDB's binder rejects timestamp/interval
    // arithmetic in some prepared-statement shapes, and a SQL-side error here
    // used to return `{"error": ...}` which crashed SPA chart pages.
    let mut stmt = match conn.prepare(
        "SELECT bucket, SUM(n)::BIGINT AS n FROM rollup_1m WHERE bucket >= ? GROUP BY 1 ORDER BY 1",
    ) {
        Ok(s) => s,
        Err(e) => return axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    };
    let rows = stmt.query_map([cutoff], |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "n":  row.get::<_, i64>(1)?,
        }))
    });
    match rows {
        Ok(rs) => {
            let v: Vec<_> = rs.flatten().collect();
            axum::Json(serde_json::json!(v)).into_response()
        }
        Err(e) => axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    }
}

async fn api_levels(State(st): State<WebState>, Query(q): Query<TimeRange>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(1).max(1);
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(
        "SELECT level, SUM(n)::BIGINT AS n FROM rollup_1m WHERE bucket >= ? GROUP BY 1",
    ) {
        Ok(s) => s,
        Err(e) => return axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    };
    let rows = stmt.query_map([cutoff], |row| {
        Ok(serde_json::json!({
            "level": row.get::<_, String>(0)?,
            "n": row.get::<_, i64>(1)?,
        }))
    });
    match rows {
        Ok(rs) => {
            let v: Vec<_> = rs.flatten().collect();
            axum::Json(serde_json::json!(v)).into_response()
        }
        Err(e) => axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct TopnParams {
    pub hours: Option<i64>,
    /// Optional filter DSL (e.g. `service:api level:error`). Compiled to a
    /// parameterized WHERE on the hot logs table.
    #[serde(default)]
    pub filter: Option<String>,
}

async fn api_topn(State(st): State<WebState>, Query(q): Query<TopnParams>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(1).max(1);
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);
    let filter =
        match crate::web::api::compile_dashboard_filter(q.filter.as_deref(), st.store.hot_attributes()) {
            Ok(f) => f,
            Err(e) => return axum::Json(serde_json::json!({"error": e})).into_response(),
        };
    let conn = st.store.conn();
    let conn = conn.lock();
    let (sql, params): (String, Vec<String>) = match filter {
        Some((body, params)) => (
            format!(
                "SELECT COALESCE(service, '') AS service, COUNT(*)::BIGINT n, \
                 SUM(CASE WHEN level='error' THEN 1 ELSE 0 END)::BIGINT AS errors \
                 FROM logs WHERE ({body}) AND ts >= ? GROUP BY 1 ORDER BY 2 DESC LIMIT 10"
            ),
            params,
        ),
        None => (
            "SELECT service, COUNT(*)::BIGINT n, SUM(CASE WHEN level='error' THEN 1 ELSE 0 END)::BIGINT AS errors
             FROM logs WHERE ts >= ?
             GROUP BY 1 ORDER BY 2 DESC LIMIT 10"
                .to_string(),
            Vec::new(),
        ),
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    };
    let mut all = params;
    all.push(cutoff.to_rfc3339());
    let duck: Vec<duckdb::types::Value> = all
        .iter()
        .map(|s| {
            if let Ok(i) = s.parse::<i64>() {
                duckdb::types::Value::BigInt(i)
            } else {
                duckdb::types::Value::Text(s.clone())
            }
        })
        .collect();
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "service": row.get::<_, String>(0)?,
            "n":       row.get::<_, i64>(1)?,
            "errors":  row.get::<_, i64>(2)?,
        }))
    });
    match rows {
        Ok(rs) => {
            let v: Vec<_> = rs.flatten().collect();
            axum::Json(serde_json::json!(v)).into_response()
        }
        Err(e) => axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    }
}

async fn api_counters(State(st): State<WebState>) -> impl IntoResponse {
    axum::Json(st.counters.snapshot())
}

/// Pipeline health snapshot (LESSON_LEARNED: backpressure must be visible
/// before data starts dropping). One call powers the SPA "Pipeline" page.
#[derive(Debug, serde::Serialize)]
pub struct PipelineStatus {
    pub channel_depth: i64,
    pub channel_capacity: u64,
    pub channel_fill_ratio: f64,
    pub wal_bytes_written_total: u64,
    pub ingest_lag_bytes: u64,
    pub audit_dropped_total: u64,
    pub records_total: u64,
    pub errors_total: u64,
}

async fn api_pipeline(State(st): State<WebState>) -> impl IntoResponse {
    let counters = st.counters.snapshot();
    let wal_dir = st.cfg.lock().data_dir.join("wal");
    let lag = crate::wal::ingest_lag_bytes(&wal_dir, &st.meta);
    let depth = st.wal.depth();
    let capacity = st.wal.capacity();
    let fill = if capacity > 0 {
        (depth.max(0) as f64) / (capacity as f64)
    } else {
        0.0
    };
    axum::Json(PipelineStatus {
        channel_depth: depth,
        channel_capacity: capacity,
        channel_fill_ratio: fill,
        wal_bytes_written_total: st.wal.bytes_written(),
        ingest_lag_bytes: lag,
        audit_dropped_total: counters.audit_dropped_total,
        records_total: counters.records_total,
        errors_total: counters.errors_total,
    })
}

async fn api_store(State(st): State<WebState>) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let state = store_state(&conn);
    axum::Json(state)
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<i64>,
}

async fn api_search(
    State(st): State<WebState>,
    Query(p): Query<SearchParams>,
) -> impl IntoResponse {
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let q = p.q.unwrap_or_default().trim().to_string();
    let conn = st.store.conn();
    let conn = conn.lock();
    let sql = if q.is_empty() {
        format!("SELECT ts, service, level, message FROM logs ORDER BY ts DESC LIMIT {limit}")
    } else {
        let qe = q.replace('\'', "''");
        format!(
            "SELECT ts, service, level, message FROM logs WHERE message ILIKE '%{qe}%' ORDER BY ts DESC LIMIT {limit}"
        )
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    };
    let rows = stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "service": row.get::<_, Option<String>>(1)?,
            "level": row.get::<_, Option<String>>(2)?,
            "message": row.get::<_, Option<String>>(3)?,
        }))
    });
    match rows {
        Ok(rs) => {
            let v: Vec<_> = rs.flatten().collect();
            axum::Json(serde_json::json!(v)).into_response()
        }
        Err(e) => axum::Json(serde_json::json!({"error": e.to_string()})).into_response(),
    }
}

async fn metrics_text(State(st): State<WebState>) -> impl IntoResponse {
    let c = st.counters.snapshot();
    let mut out = String::new();
    out.push_str(&format!("# HELP central_logs_records_total Total records received.\n"));
    out.push_str(&format!("# TYPE central_logs_records_total counter\n"));
    out.push_str(&format!(
        "central_logs_records_total {}\n",
        c.records_total
    ));
    out.push_str(&format!(
        "central_logs_bytes_total {}\n",
        c.bytes_total
    ));
    out.push_str(&format!(
        "central_logs_errors_total {}\n",
        c.errors_total
    ));
    out.push_str("# HELP central_logs_audit_dropped_total Self-audit events dropped (WAL channel saturated).\n");
    out.push_str("# TYPE central_logs_audit_dropped_total counter\n");
    out.push_str(&format!(
        "central_logs_audit_dropped_total {}\n",
        c.audit_dropped_total
    ));
    // WAL / pipeline health gauges.
    let depth = st.wal.depth();
    let capacity = st.wal.capacity();
    let wal_dir = st.cfg.lock().data_dir.join("wal");
    let lag = crate::wal::ingest_lag_bytes(&wal_dir, &st.meta);
    out.push_str("# HELP central_logs_wal_channel_depth Entries waiting in the bounded insert->WAL channel.\n");
    out.push_str("# TYPE central_logs_wal_channel_depth gauge\n");
    out.push_str(&format!("central_logs_wal_channel_depth {depth}\n"));
    out.push_str("# HELP central_logs_wal_channel_capacity Configured bound of the insert->WAL channel.\n");
    out.push_str("# TYPE central_logs_wal_channel_capacity gauge\n");
    out.push_str(&format!("central_logs_wal_channel_capacity {capacity}\n"));
    out.push_str("# HELP central_logs_wal_bytes_written_total WAL bytes durably written.\n");
    out.push_str("# TYPE central_logs_wal_bytes_written_total counter\n");
    out.push_str(&format!(
        "central_logs_wal_bytes_written_total {}\n",
        st.wal.bytes_written()
    ));
    out.push_str("# HELP central_logs_ingest_lag_bytes WAL bytes not yet consumed by ingest workers.\n");
    out.push_str("# TYPE central_logs_ingest_lag_bytes gauge\n");
    out.push_str(&format!("central_logs_ingest_lag_bytes {lag}\n"));
    for (proto, pc) in c.per_protocol {
        out.push_str(&format!(
            "central_logs_protocol_records{{protocol=\"{proto}\"}} {}\n",
            pc.records
        ));
        out.push_str(&format!(
            "central_logs_protocol_bytes{{protocol=\"{proto}\"}} {}\n",
            pc.bytes
        ));
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"))],
        out,
    )
        .into_response()
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// Tiny server-rendered Prometheus-ish text view (handy when curl'ing).
pub async fn _render_metrics(_state: &WebState) -> String {
    MetricsApiTemplate { generated_at: chrono::Utc::now() }
        .render()
        .unwrap_or_default()
}

#[allow(dead_code)]
fn _unused_arc<T>(_v: Arc<T>) {}
