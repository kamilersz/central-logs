//! v2 API endpoints: filter DSL-backed log search, dashboard configs CRUD,
//! alert rule approval workflow, missing §4 dashboards, hot-attribute schema,
//! and the AI natural-language query.
//!
//! All endpoints are JSON. The SPA calls these; the v1 dashboard HTML routes
//! in `dashboard.rs` continue to work as a fallback.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::ai::{translate, AiQueryRequest, LlmConfig};
use crate::alerts::{compare as alert_compare, AlertCondition};
use crate::audit::{AuditHandle, ResolvedPeer};
use crate::config::ServiceQueryLimit;
use crate::query::{parse_filter, Clause, ColumnWhitelist, CompiledFilter, Op};
use crate::store::compact::glob_match;
use crate::store::Store;
use crate::wal::meta::WalMeta;
use crate::web::auth_api::MaybeAuthInfo;
use duckdb::params;

#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    pub meta: Arc<WalMeta>,
    pub llm: Arc<LlmConfig>,
    pub audit: AuditHandle,
    /// Per-service query-window caps (`service:<glob>` → max seconds).
    pub query_limits: Arc<Vec<ServiceQueryLimit>>,
    /// SMTP relay for alert email channels (delivery disabled when host empty).
    pub smtp: Arc<crate::config::SmtpConfig>,
    /// Shared HTTP client (alert channel test sends).
    pub http: reqwest::Client,
    /// Housekeeping context (Storage page + ops endpoints).
    pub data_dir: PathBuf,
    pub cold_dir: PathBuf,
    pub retention: crate::config::RetentionConfig,
    pub cold_retention_days: u64,
    pub service_retention_count: usize,
    pub backup: crate::config::BackupConfig,
    pub http_port: u16,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        // Schema / discovery
        .route("/api/schema/hot", get(schema_hot))
        // Logs explorer
        .route("/api/logs", get(logs_search))
        .route("/api/logs/export", get(logs_export))
        .route("/api/logs/count", get(logs_count))
        // §4 dashboards
        .route("/api/dashboard/volume", get(dashboard_volume))
        .route("/api/dashboard/error-rate", get(dashboard_error_rate))
        .route("/api/dashboard/latency", get(dashboard_latency))
        .route("/api/dashboard/anomalies", get(dashboard_anomalies))
        .route("/api/dashboard/forecast", get(dashboard_forecast))
        // Dashboard config CRUD
        .route(
            "/api/dashboard/configs",
            get(list_dashboard_configs).post(create_dashboard_config),
        )
        .route(
            "/api/dashboard/configs/{id}",
            get(get_dashboard_config)
                .put(update_dashboard_config)
                .delete(delete_dashboard_config),
        )
        // Alert rules (architecture §7 approval workflow + evaluator)
        .route(
            "/api/alert-rules",
            get(list_alert_rules).post(create_alert_rule),
        )
        .route(
            "/api/alert-rules/{id}",
            put(update_alert_rule).delete(delete_alert_rule),
        )
        .route("/api/alert-rules/{id}/approve", post(approve_alert_rule))
        .route("/api/alert-rules/{id}/reject", post(reject_alert_rule))
        .route("/api/alert-rules/{id}/events", get(list_alert_events))
        // Notification channels (email lists, telegram bots, webhooks)
        .route(
            "/api/alert-channels",
            get(list_alert_channels).post(create_alert_channel),
        )
        .route(
            "/api/alert-channels/{id}",
            put(update_alert_channel).delete(delete_alert_channel),
        )
        .route("/api/alert-channels/{id}/test", post(test_alert_channel))
        // AI natural-language query
        .route("/api/ai/query", post(ai_query))
        // AI dashboard builder (clarify → build)
        .route("/api/ai/dashboard", post(ai_dashboard))
        // OpenTelemetry traces + metric series (OTLP ingest, protocol:otlp_span
        // / protocol:otlp_metric rows projected from the hot logs table).
        .route("/api/traces", get(traces_list))
        .route("/api/traces/{trace_id}", get(trace_detail))
        .route("/api/metrics/series", get(metric_series))
        .with_state(state)
}

// =====================================================================
// Schema / discovery
// =====================================================================

#[derive(Serialize)]
struct HotSchemaAttr {
    name: String,
    duckdb_type: String,
    json_path: String,
}

#[derive(Serialize)]
struct HotSchemaResponse {
    hot: Vec<HotSchemaAttr>,
    /// All columns the filter DSL will accept (built-in + hot).
    filterable_columns: Vec<String>,
}

async fn schema_hot(State(st): State<ApiState>) -> impl IntoResponse {
    let hot: Vec<HotSchemaAttr> = st
        .store
        .hot_attributes()
        .iter()
        .map(|a| HotSchemaAttr {
            name: a.name.clone(),
            duckdb_type: a.duckdb_type.to_string(),
            json_path: a.json_path.clone(),
        })
        .collect();
    let cols = ColumnWhitelist::standard(st.store.hot_attributes());
    let mut filterable: Vec<String> = cols.keys().into_iter().map(String::from).collect();
    filterable.sort();
    Json(HotSchemaResponse {
        hot,
        filterable_columns: filterable,
    })
}

// =====================================================================
// Logs search — replaces /api/search with the filter DSL
// =====================================================================

#[derive(Debug, Deserialize)]
struct LogsParams {
    /// Filter DSL string (e.g. `service:api level:error user_id:42`).
    /// Empty means no filter.
    #[serde(default)]
    filter: String,
    /// ISO-8601 `from` timestamp. Default: now - 1 hour.
    #[serde(default)]
    from: Option<String>,
    /// ISO-8601 `to` timestamp. Default: now.
    #[serde(default)]
    to: Option<String>,
    /// Relative time shorthand: `5m`, `1h`, `24h`, `7d`. Overrides `from`/`to`.
    #[serde(default)]
    window: Option<String>,
    /// Page size. Default 100, max 1000.
    #[serde(default = "default_page_size")]
    limit: i64,
    /// Offset for pagination.
    #[serde(default)]
    offset: i64,
}

fn default_page_size() -> i64 {
    100
}

#[derive(Serialize)]
struct LogsResponse {
    rows: Vec<serde_json::Value>,
    total_matched: i64,
    truncated: bool,
    /// If the filter didn't parse, we report the error here rather than 500.
    filter_error: Option<String>,
}

async fn logs_search(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    ResolvedPeer(peer): ResolvedPeer,
    Query(p): Query<LogsParams>,
) -> impl IntoResponse {
    let limit = p.limit.clamp(1, 1000);
    let offset = p.offset;
    // Audit before the query fires — view events are fire-and-forget, dropped
    // if the WAL is saturated. We don't log the filter contents (could be
    // large / sensitive); just that a search happened with what window/limit.
    if let Some(i) = &info.0 {
        let mut ev = st
            .audit
            .event("view.logs")
            .actor(i)
            .source_ip(&peer)
            .field("limit", limit)
            .field("offset", offset);
        if let Some(w) = &p.window {
            ev = ev.field("window", w.as_str());
        }
        ev.emit();
    }
    let cols = ColumnWhitelist::standard(st.store.hot_attributes());
    let compiled = match parse_filter(&p.filter) {
        Ok(c) => c,
        Err(e) => {
            return Json(LogsResponse {
                rows: vec![],
                total_matched: 0,
                truncated: false,
                filter_error: Some(format!("{e}")),
            })
        }
    };
    let (where_body, params) = match compiled.to_sql(&cols) {
        Ok(v) => v,
        Err(e) => {
            return Json(LogsResponse {
                rows: vec![],
                total_matched: 0,
                truncated: false,
                filter_error: Some(format!("{e}")),
            })
        }
    };

    let (from_ts, to_ts) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    // Per-service query-window caps (LESSON_LEARNED: per-stream
    // max_query_range). Only enforced when the filter pins `service` by
    // equality — unfiltered queries are not capped.
    if let Err(e) = enforce_service_query_limits(
        &compiled,
        (to_ts - from_ts).num_seconds().max(0),
        &st.query_limits,
    ) {
        return Json(LogsResponse {
            rows: vec![],
            total_matched: 0,
            truncated: false,
            filter_error: Some(e),
        });
    }
    let mut where_parts: Vec<String> = Vec::new();
    if !where_body.is_empty() {
        where_parts.push(format!("({where_body})"));
    }
    where_parts.push("ts >= ?".to_string());
    where_parts.push("ts <= ?".to_string());
    let where_clause = where_parts.join(" AND ");

    let conn = st.store.conn();
    let conn = conn.lock();

    // Count first, then fetch. Both use the same WHERE clause.
    let count_sql = format!("SELECT COUNT(*) FROM logs WHERE {where_clause}");
    let mut count_params: Vec<String> = params.clone();
    count_params.push(from_ts.to_rfc3339());
    count_params.push(to_ts.to_rfc3339());
    let count_duck = params_as_duck(&count_params);
    let count_refs: Vec<&dyn duckdb::ToSql> =
        count_duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let total: i64 = match conn.query_row(&count_sql, count_refs.as_slice(), |r| r.get(0)) {
        Ok(v) => v,
        Err(e) => {
            return Json(LogsResponse {
                rows: vec![],
                total_matched: 0,
                truncated: false,
                filter_error: Some(format!("count query: {e}")),
            });
        }
    };

    // Hot attributes are selected as their typed columns and returned in a
    // `hot` object per row so the SPA can display/filter-promoted values
    // (user_id, user_name, ...) alongside the built-in fields.
    let hot_attrs = st.store.hot_attributes().to_vec();
    let hot_select = if hot_attrs.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = hot_attrs.iter().map(|a| a.name.clone()).collect();
        format!(", {}", names.join(", "))
    };
    let fetch_sql = format!(
        "SELECT ts, insert_ts, source_host, service, level, message, fingerprint, \
         trace_id, span_id, attributes, geo_country, raw_len, protocol{hot_select} \
         FROM logs WHERE {where_clause} ORDER BY ts DESC LIMIT ? OFFSET ?"
    );
    let mut fetch_params = params.clone();
    fetch_params.push(from_ts.to_rfc3339());
    fetch_params.push(to_ts.to_rfc3339());
    fetch_params.push(limit.to_string());
    fetch_params.push(offset.to_string());

    let rows_result = (|| -> Result<Vec<serde_json::Value>, duckdb::Error> {
        let mut stmt = conn.prepare(&fetch_sql)?;
        let duck_params = params_as_duck(&fetch_params);
        let refs: Vec<&dyn duckdb::ToSql> = duck_params
            .iter()
            .map(|v| v as &dyn duckdb::ToSql)
            .collect();
        let rows = stmt.query_map(refs.as_slice(), |row| {
            let ts: chrono::DateTime<chrono::Utc> = row.get(0)?;
            let insert_ts: chrono::DateTime<chrono::Utc> = row.get(1)?;
            let source_host: Option<String> = row.get(2)?;
            let service: Option<String> = row.get(3)?;
            let level: Option<String> = row.get(4)?;
            let message: Option<String> = row.get(5)?;
            let fingerprint: Option<String> = row.get(6)?;
            let trace_id: Option<String> = row.get(7)?;
            let span_id: Option<String> = row.get(8)?;
            let attributes: Option<String> = row.get(9)?;
            let geo_country: Option<String> = row.get(10)?;
            let raw_len: Option<i32> = row.get(11)?;
            let protocol: Option<String> = row.get(12)?;
            // Typed hot columns follow the fixed prefix above.
            let mut hot = serde_json::Map::new();
            for (i, attr) in hot_attrs.iter().enumerate() {
                let col = 13 + i;
                let v = match attr.duckdb_type {
                    crate::hot::HotType::Bigint => {
                        row.get::<_, Option<i64>>(col)?.map(serde_json::Value::from)
                    }
                    crate::hot::HotType::Double => row
                        .get::<_, Option<f64>>(col)?
                        .map(|f| serde_json::Value::from(f as f64)),
                    crate::hot::HotType::Boolean => row
                        .get::<_, Option<bool>>(col)?
                        .map(serde_json::Value::from),
                    crate::hot::HotType::Varchar => row
                        .get::<_, Option<String>>(col)?
                        .map(serde_json::Value::from),
                };
                if let Some(v) = v {
                    hot.insert(attr.name.clone(), v);
                }
            }
            Ok(serde_json::json!({
                "ts": ts.to_rfc3339(),
                "insert_ts": insert_ts.to_rfc3339(),
                "source_host": source_host,
                "service": service,
                "level": level,
                "message": message,
                "fingerprint": fingerprint,
                "trace_id": trace_id,
                "span_id": span_id,
                "attributes": attributes
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .unwrap_or(serde_json::Value::Null),
                "geo_country": geo_country,
                "raw_len": raw_len,
                "protocol": protocol,
                "hot": serde_json::Value::Object(hot),
            }))
        })?;
        rows.collect()
    })();

    match rows_result {
        Ok(rows) => {
            let truncated = total > offset + limit;
            Json(LogsResponse {
                rows,
                total_matched: total,
                truncated,
                filter_error: None,
            })
        }
        Err(e) => Json(LogsResponse {
            rows: vec![],
            total_matched: 0,
            truncated: false,
            filter_error: Some(format!("fetch: {e}")),
        }),
    }
}

// =====================================================================
// Excel export — logs with JSON attributes expanded into columns
// =====================================================================

/// Hard cap on exported rows. Exports buffer the full result in memory
/// (column union must be known before the sheet is written), so unlike the
/// paginated `/api/logs` this is clamped higher but still bounded.
const MAX_EXPORT_ROWS: i64 = 10_000;

/// Built-in columns in export order. Hot attributes follow, then one
/// `attr.<key>` column per distinct key found in any row's `attributes` JSON.
const EXPORT_BUILTIN_COLUMNS: [&str; 12] = [
    "ts",
    "insert_ts",
    "service",
    "level",
    "message",
    "source_host",
    "trace_id",
    "span_id",
    "protocol",
    "geo_country",
    "fingerprint",
    "raw_len",
];

/// Recursively flatten a JSON value into dot-path → scalar pairs. Objects
/// expand one column per leaf entry (`attr.request.route`); arrays and any
/// other non-scalar land as serialized JSON strings in a single cell.
fn flatten_json(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_json(&key, val, out);
            }
        }
        other => out.push((prefix.to_string(), other.clone())),
    }
}

fn write_xlsx_cell(
    sheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    v: &serde_json::Value,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    match v {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Bool(b) => sheet.write(row, col, *b).map(|_| ()),
        serde_json::Value::Number(n) => sheet
            .write(row, col, n.as_f64().unwrap_or_default())
            .map(|_| ()),
        serde_json::Value::String(s) => sheet.write(row, col, s.as_str()).map(|_| ()),
        other => sheet
            .write(row, col, other.to_string().as_str())
            .map(|_| ()),
    }
}

/// GET /api/logs/export — same filter/window/limit contract as `/api/logs`
/// (no pagination offset; the newest `limit` matches in range), rendered as
/// an .xlsx workbook. Every log's `attributes` JSON is expanded into
/// `attr.<key>` columns so each JSON entry becomes a spreadsheet column.
async fn logs_export(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    ResolvedPeer(peer): ResolvedPeer,
    Query(p): Query<LogsParams>,
) -> Response {
    let limit = p.limit.clamp(1, MAX_EXPORT_ROWS);
    // Audit like view.logs: that an export happened, not what was exported.
    if let Some(i) = &info.0 {
        let mut ev = st
            .audit
            .event("export.logs")
            .actor(i)
            .source_ip(&peer)
            .field("limit", limit);
        if let Some(w) = &p.window {
            ev = ev.field("window", w.as_str());
        }
        ev.emit();
    }
    let cols = ColumnWhitelist::standard(st.store.hot_attributes());
    let compiled = match parse_filter(&p.filter) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    let (where_body, params) = match compiled.to_sql(&cols) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    let (from_ts, to_ts) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    if let Err(e) = enforce_service_query_limits(
        &compiled,
        (to_ts - from_ts).num_seconds().max(0),
        &st.query_limits,
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response();
    }

    let mut where_parts: Vec<String> = Vec::new();
    if !where_body.is_empty() {
        where_parts.push(format!("({where_body})"));
    }
    where_parts.push("ts >= ?".to_string());
    where_parts.push("ts <= ?".to_string());
    let where_clause = where_parts.join(" AND ");

    let hot_attrs = st.store.hot_attributes().to_vec();
    let hot_select = if hot_attrs.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = hot_attrs.iter().map(|a| a.name.clone()).collect();
        format!(", {}", names.join(", "))
    };
    let fetch_sql = format!(
        "SELECT ts, insert_ts, source_host, service, level, message, fingerprint, \
         trace_id, span_id, attributes, geo_country, raw_len, protocol{hot_select} \
         FROM logs WHERE {where_clause} ORDER BY ts DESC LIMIT ? OFFSET ?"
    );

    let conn = st.store.conn();
    let conn = conn.lock();

    // Same row shape as logs_search (shared json! mapping) so the export
    // stays byte-identical to what the explorer displays.
    let rows_result = (|| -> Result<(Vec<serde_json::Value>, i64), duckdb::Error> {
        let total: i64 = {
            let count_sql = format!("SELECT COUNT(*) FROM logs WHERE {where_clause}");
            let mut count_params = params.clone();
            count_params.push(from_ts.to_rfc3339());
            count_params.push(to_ts.to_rfc3339());
            let duck = params_as_duck(&count_params);
            let refs: Vec<&dyn duckdb::ToSql> =
                duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            conn.query_row(&count_sql, refs.as_slice(), |r| r.get(0))?
        };
        let mut fetch_params = params.clone();
        fetch_params.push(from_ts.to_rfc3339());
        fetch_params.push(to_ts.to_rfc3339());
        fetch_params.push(limit.to_string());
        fetch_params.push(p.offset.to_string());
        let mut stmt = conn.prepare(&fetch_sql)?;
        let duck = params_as_duck(&fetch_params);
        let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
        let mapped = stmt.query_map(refs.as_slice(), |row| {
            let ts: chrono::DateTime<chrono::Utc> = row.get(0)?;
            let insert_ts: chrono::DateTime<chrono::Utc> = row.get(1)?;
            let source_host: Option<String> = row.get(2)?;
            let service: Option<String> = row.get(3)?;
            let level: Option<String> = row.get(4)?;
            let message: Option<String> = row.get(5)?;
            let fingerprint: Option<String> = row.get(6)?;
            let trace_id: Option<String> = row.get(7)?;
            let span_id: Option<String> = row.get(8)?;
            let attributes: Option<String> = row.get(9)?;
            let geo_country: Option<String> = row.get(10)?;
            let raw_len: Option<i32> = row.get(11)?;
            let protocol: Option<String> = row.get(12)?;
            let mut hot = serde_json::Map::new();
            for (i, attr) in hot_attrs.iter().enumerate() {
                let col = 13 + i;
                let v = match attr.duckdb_type {
                    crate::hot::HotType::Bigint => {
                        row.get::<_, Option<i64>>(col)?.map(serde_json::Value::from)
                    }
                    crate::hot::HotType::Double => row
                        .get::<_, Option<f64>>(col)?
                        .map(|f| serde_json::Value::from(f as f64)),
                    crate::hot::HotType::Boolean => row
                        .get::<_, Option<bool>>(col)?
                        .map(serde_json::Value::from),
                    crate::hot::HotType::Varchar => row
                        .get::<_, Option<String>>(col)?
                        .map(serde_json::Value::from),
                };
                if let Some(v) = v {
                    hot.insert(attr.name.clone(), v);
                }
            }
            Ok(serde_json::json!({
                "ts": ts.to_rfc3339(),
                "insert_ts": insert_ts.to_rfc3339(),
                "source_host": source_host,
                "service": service,
                "level": level,
                "message": message,
                "fingerprint": fingerprint,
                "trace_id": trace_id,
                "span_id": span_id,
                "attributes": attributes
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .unwrap_or(serde_json::Value::Null),
                "geo_country": geo_country,
                "raw_len": raw_len,
                "protocol": protocol,
                "hot": serde_json::Value::Object(hot),
            }))
        })?;
        Ok((mapped.collect::<Result<Vec<_>, _>>()?, total))
    })();

    let (rows, total) = match rows_result {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("export query: {e}") })),
            )
                .into_response()
        }
    };

    // Pass 1: flatten every row's attributes and collect the column union in
    // first-appearance order (stable output for identical data).
    let mut attr_keys: Vec<String> = Vec::new();
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut flat_rows: Vec<Vec<(String, serde_json::Value)>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut flat = Vec::new();
        match row.get("attributes") {
            Some(serde_json::Value::Object(_)) => {
                flatten_json("", &row["attributes"], &mut flat);
            }
            Some(serde_json::Value::Null) | None => {}
            // Non-object attributes (shouldn't happen, but be safe): one cell.
            Some(other) => flat.push((String::new(), other.clone())),
        }
        for (k, _) in &flat {
            if seen_keys.insert(k.clone()) {
                attr_keys.push(k.clone());
            }
        }
        flat_rows.push(flat);
    }

    // Column layout: built-ins, hot attributes, then attr.* expansion.
    let mut columns: Vec<String> = EXPORT_BUILTIN_COLUMNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    columns.extend(hot_attrs.iter().map(|a| a.name.clone()));
    columns.extend(attr_keys.iter().map(|k| format!("attr.{k}")));

    // Pass 2: write the workbook. The write is fallible (bad paths, cell
    // limits), so it's a closure — the handler itself answers in JSON.
    let mut workbook = rust_xlsxwriter::Workbook::new();
    let write_result: Result<(), rust_xlsxwriter::XlsxError> = (|| {
        let sheet = workbook.add_worksheet().set_name("logs")?;
        let header_fmt = rust_xlsxwriter::Format::new().set_bold();
        for (ci, name) in columns.iter().enumerate() {
            sheet.write_with_format(0, ci as u16, name.as_str(), &header_fmt)?;
        }
        for (ri, (row, flat)) in rows.iter().zip(&flat_rows).enumerate() {
            let r = (ri + 1) as u32;
            for (ci, name) in columns.iter().enumerate() {
                let name: &str = name.as_str();
                let v = if let Some(rest) = name.strip_prefix("attr.") {
                    flat.iter()
                        .find(|(k, _)| k == rest)
                        .map(|(_, v)| v)
                        .unwrap_or(&serde_json::Value::Null)
                } else if let Some(v) = row.get(name) {
                    v
                } else {
                    &serde_json::Value::Null
                };
                write_xlsx_cell(sheet, r, ci as u16, v)?;
            }
        }
        sheet.set_freeze_panes(1, 0)?;
        sheet.autofit();
        Ok(())
    })();
    if let Err(e) = write_result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("xlsx render: {e}") })),
        )
            .into_response();
    }
    let bytes = match workbook.save_to_buffer() {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("xlsx render: {e}") })),
            )
                .into_response()
        }
    };

    let filename = format!(
        "logs-{}-{}.xlsx",
        from_ts.format("%Y%m%dT%H%M%SZ"),
        to_ts.format("%Y%m%dT%H%M%SZ")
    );
    let mut out_headers = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.openxmlformats-spreadsheetml.sheet"),
        ),
        (
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"logs.xlsx\""),
        ),
        (
            header::HeaderName::from_static("x-total-matched"),
            HeaderValue::from(total),
        ),
    ];
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        out_headers[1].1 = v;
    }
    (out_headers, bytes).into_response()
}

async fn logs_count(State(st): State<ApiState>, Query(p): Query<LogsParams>) -> impl IntoResponse {
    let cols = ColumnWhitelist::standard(st.store.hot_attributes());
    let compiled = match parse_filter(&p.filter) {
        Ok(c) => c,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };
    let (where_body, params) = match compiled.to_sql(&cols) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };
    let (from_ts, to_ts) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    if let Err(e) = enforce_service_query_limits(
        &compiled,
        (to_ts - from_ts).num_seconds().max(0),
        &st.query_limits,
    ) {
        return Json(serde_json::json!({ "error": e }));
    }
    let where_clause = if where_body.is_empty() {
        "ts >= ? AND ts <= ?".to_string()
    } else {
        format!("({where_body}) AND ts >= ? AND ts <= ?")
    };
    let sql = format!("SELECT COUNT(*) FROM logs WHERE {where_clause}");
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut all: Vec<String> = params;
    all.push(from_ts.to_rfc3339());
    all.push(to_ts.to_rfc3339());
    let duck = params_as_duck(&all);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let count: i64 = conn
        .query_row(&sql, refs.as_slice(), |r| r.get(0))
        .unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

/// Build owned DuckDB Values for a heterogeneous param list. Tries to parse
/// each value as i64, then f64, then falls back to Text. This makes numeric
/// comparisons (`raw_len>=100`) actually compare as numbers instead of strings.
fn params_as_duck(values: &[String]) -> Vec<duckdb::types::Value> {
    crate::query::params_as_duck(values)
}

/// Enforce per-service query-window caps: if the filter pins `service` by
/// equality and a matching limit rule exists, the requested window must not
/// exceed the rule's max. Patterns glob-match the RAW service value.
fn enforce_service_query_limits(
    compiled: &CompiledFilter,
    window_secs: i64,
    limits: &[ServiceQueryLimit],
) -> Result<(), String> {
    if limits.is_empty() {
        return Ok(());
    }
    for clause in compiled.terms() {
        if let Clause::Comparison {
            key,
            op: Op::Eq,
            value,
            negated: false,
        } = clause
        {
            if key != "service" {
                continue;
            }
            for limit in limits {
                if glob_match(&limit.pattern, value) && window_secs > limit.max_window_secs as i64 {
                    return Err(format!(
                        "query window for service '{value}' is {window_secs}s but the configured \
                         cap is {}s (pattern '{}'); narrow the time range",
                        limit.max_window_secs, limit.pattern
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Resolve a (window, from, to) combo to (from_ts, to_ts). `window` shorthand
/// wins if present.
fn resolve_window(
    window: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    let now = chrono::Utc::now();
    let to_ts = to
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or(now);
    let from_ts = if let Some(w) = window {
        let secs = parse_window_shorthand(w).unwrap_or(3600);
        to_ts - chrono::Duration::seconds(secs)
    } else {
        from.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or(to_ts - chrono::Duration::hours(1))
    };
    (from_ts, to_ts)
}

fn parse_window_shorthand(s: &str) -> Option<i64> {
    parse_window_secs(s)
}

/// Shared (pub(crate)) window-shorthand parser — `5m`/`1h`/`24h`/`7d` → secs.
pub(crate) fn parse_window_secs(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (num, unit) = trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, _)| trimmed.split_at(i))
        .unwrap_or((trimmed, ""));
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        "w" | "wk" | "week" | "weeks" => n * 7 * 86400,
        _ => return None,
    };
    Some(secs)
}

// =====================================================================
// §4 dashboards: volume, error-rate, latency, anomalies, forecast
// =====================================================================

/// Shared query params for every preset dashboard endpoint. All support a
/// relative `window`, an explicit `from`/`to` range (ISO-8601), and an
/// optional filter-DSL string. When a `filter` is present the endpoints
/// aggregate from the hot `logs` table (any DSL column works); without one
/// they use the pre-aggregated rollup tables (cheaper).
#[derive(Debug, Deserialize)]
struct DashboardParams {
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    bucket: Option<String>, // "minute" | "hour" (default: auto by span)
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    /// Forecast-only: number of future points to project.
    #[serde(default)]
    horizon: Option<i64>,
}

/// Compiled optional dashboard filter: a WHERE body plus bound params.
type FilterParts = (String, Vec<String>);

/// Parse + compile the optional filter DSL. `Ok(None)` = no filter.
pub(crate) fn compile_dashboard_filter(
    filter: Option<&str>,
    hot: &[crate::hot::HotAttribute],
) -> Result<Option<FilterParts>, String> {
    let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
        return Ok(None);
    };
    let compiled = parse_filter(f).map_err(|e| e.to_string())?;
    let (body, params) = compiled
        .to_sql(&ColumnWhitelist::standard(hot))
        .map_err(|e| e.to_string())?;
    if body.is_empty() {
        return Ok(None);
    }
    Ok(Some((body, params)))
}

/// Pick a bucket width: explicit `bucket` param wins; otherwise minute
/// buckets up to 6h spans, hour buckets beyond.
fn bucket_interval_secs(bucket: Option<&str>, span_secs: i64) -> i64 {
    if let Some(b) = bucket {
        return match b {
            "hour" => 3600,
            _ => 60,
        };
    }
    if span_secs > 6 * 3600 {
        3600
    } else {
        60
    }
}

fn interval_literal(secs: i64) -> &'static str {
    if secs >= 3600 {
        "INTERVAL '1 hour'"
    } else {
        "INTERVAL '1 minute'"
    }
}

/// Append the from/to timestamps to the bound param list (filter params
/// first — matching the `WHERE (body) AND ts >= ? AND ts <= ?` shape).
fn with_ts_range(
    params: Vec<String>,
    from: &chrono::DateTime<chrono::Utc>,
    to: &chrono::DateTime<chrono::Utc>,
) -> Vec<duckdb::types::Value> {
    let mut all = params;
    all.push(from.to_rfc3339());
    all.push(to.to_rfc3339());
    params_as_duck(&all)
}

/// Resolve the effective (from, to, interval) for a dashboard request.
fn dashboard_range(
    p: &DashboardParams,
) -> (
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    i64,
) {
    let (from, to) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    let span = (to - from).num_seconds().max(1);
    let interval = bucket_interval_secs(p.bucket.as_deref(), span);
    (from, to, interval)
}

fn err_json(e: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "error": e }))
}

/// `GET /api/dashboard/volume` — records per bucket. Rollups when no
/// filter; hot logs with the DSL pushed down when filtered.
async fn dashboard_volume(
    State(st): State<ApiState>,
    Query(p): Query<DashboardParams>,
) -> impl IntoResponse {
    let (from, to, interval) = dashboard_range(&p);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    let (sql, params): (String, Vec<String>) = match &filter {
        Some((body, params)) => (
            format!(
                "SELECT time_bucket({}, ts) AS bucket, COUNT(*) AS n \
                 FROM logs WHERE ({body}) AND ts >= ? AND ts <= ? GROUP BY 1 ORDER BY 1",
                interval_literal(interval)
            ),
            params.clone(),
        ),
        None => {
            let table = if interval >= 3600 {
                "rollup_1h"
            } else {
                "rollup_1m"
            };
            (
                format!(
                    "SELECT bucket, SUM(n)::BIGINT AS n FROM {table} \
                     WHERE bucket >= ? AND bucket <= ? GROUP BY 1 ORDER BY 1"
                ),
                Vec::new(),
            )
        }
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()),
    };
    let duck = with_ts_range(params, &from, &to);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "n":  row.get::<_, i64>(1)?,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => err_json(e.to_string()),
    }
}

async fn dashboard_error_rate(
    State(st): State<ApiState>,
    Query(p): Query<DashboardParams>,
) -> impl IntoResponse {
    let (from, to, interval) = dashboard_range(&p);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    let (sql, params): (String, Vec<String>) = match &filter {
        Some((body, params)) => (
            format!(
                "SELECT time_bucket({}, ts) AS bucket, \
                 COUNT(*) FILTER (WHERE level = 'error') AS errors, \
                 COUNT(*) AS total, \
                 COALESCE(COUNT(*) FILTER (WHERE level = 'error')::DOUBLE / NULLIF(COUNT(*), 0), 0) AS rate \
                 FROM logs WHERE ({body}) AND ts >= ? AND ts <= ? GROUP BY 1 ORDER BY 1",
                interval_literal(interval)
            ),
            params.clone(),
        ),
        None => {
            let table = if interval >= 3600 { "rollup_1h" } else { "rollup_1m" };
            (
                format!(
                    "SELECT bucket, \
                     SUM(CASE WHEN level='error' THEN n ELSE 0 END)::BIGINT AS errors, \
                     SUM(n)::BIGINT AS total, \
                     COALESCE(SUM(CASE WHEN level='error' THEN n ELSE 0 END)::DOUBLE / NULLIF(SUM(n), 0), 0) AS rate \
                     FROM {table} WHERE bucket >= ? AND bucket <= ? GROUP BY 1 ORDER BY 1"
                ),
                Vec::new(),
            )
        }
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()),
    };
    let duck = with_ts_range(params, &from, &to);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "errors": row.get::<_, i64>(1)?,
            "total": row.get::<_, i64>(2)?,
            "rate": row.get::<_, f64>(3)?,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => err_json(e.to_string()),
    }
}

async fn dashboard_latency(
    State(st): State<ApiState>,
    Query(p): Query<DashboardParams>,
) -> impl IntoResponse {
    let (from, to, interval) = dashboard_range(&p);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    // Filtered: compute percentiles straight from hot logs (same
    // duration_ms extraction the rollup job uses). Unfiltered: re-aggregate
    // the per-(service, level) rollups per bucket.
    let (sql, params): (String, Vec<String>) = match &filter {
        Some((body, params)) => (
            format!(
                "SELECT time_bucket({}, ts) AS bucket, COALESCE(service, '') AS service, \
                 COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.5), 0)  AS p50, \
                 COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.95), 0) AS p95, \
                 COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.99), 0) AS p99 \
                 FROM logs WHERE ({body}) AND ts >= ? AND ts <= ? GROUP BY 1, 2 ORDER BY 1",
                interval_literal(interval)
            ),
            params.clone(),
        ),
        None => {
            let table = if interval >= 3600 { "rollup_1h" } else { "rollup_1m" };
            (
                format!(
                    "SELECT bucket, service, \
                     MAX(p50) AS p50, MAX(p95) AS p95, MAX(p99) AS p99 \
                     FROM {table} WHERE bucket >= ? AND bucket <= ? AND p95 > 0 \
                     GROUP BY 1, 2 ORDER BY 1"
                ),
                Vec::new(),
            )
        }
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()),
    };
    let duck = with_ts_range(params, &from, &to);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "service": row.get::<_, String>(1)?,
            "p50": row.get::<_, f64>(2)?,
            "p95": row.get::<_, f64>(3)?,
            "p99": row.get::<_, f64>(4)?,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => err_json(e.to_string()),
    }
}

async fn dashboard_anomalies(
    State(st): State<ApiState>,
    Query(p): Query<DashboardParams>,
) -> impl IntoResponse {
    let (from, to, interval) = dashboard_range(&p);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    match filter {
        Some((body, params)) => {
            // On-demand detection over the filtered series: bucket the hot
            // logs by the requested width, then run the same detectors the
            // background cycle uses. Fresh results for any filter, at the
            // cost of a hot-tier scan.
            let sql = format!(
                "SELECT time_bucket({}, ts) AS bucket, COUNT(*) AS n \
                 FROM logs WHERE ({body}) AND ts >= ? AND ts <= ? GROUP BY 1 ORDER BY 1",
                interval_literal(interval)
            );
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => return err_json(e.to_string()),
            };
            let duck = with_ts_range(params, &from, &to);
            let refs: Vec<&dyn duckdb::ToSql> =
                duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            let pairs: Vec<(chrono::DateTime<chrono::Utc>, f64)> = stmt
                .query_map(refs.as_slice(), |row| {
                    Ok((
                        row.get::<_, chrono::DateTime<chrono::Utc>>(0)?,
                        row.get::<_, i64>(1)? as f64,
                    ))
                })
                .map(|r| r.flatten().collect())
                .unwrap_or_default();
            drop(stmt);
            if pairs.len() < 5 {
                return Json(serde_json::json!([]));
            }
            let (ts, vs): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
            let anomalies =
                match crate::analytics::anomaly::detect_anomalies_once("volume", &vs, &ts, 3.0) {
                    Ok(a) => a,
                    Err(e) => return err_json(e.to_string()),
                };
            let rows: Vec<serde_json::Value> = anomalies
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "ts": a.ts.to_rfc3339(),
                        "metric": a.metric,
                        "score": a.score,
                        "method": a.method.as_str(),
                        "severity": a.severity,
                    })
                })
                .collect();
            Json(serde_json::json!(rows))
        }
        None => {
            let mut stmt = match conn.prepare(
                "SELECT ts, metric, score, method, severity FROM anomalies \
                 WHERE ts >= ? AND ts <= ? ORDER BY ts DESC LIMIT 500",
            ) {
                Ok(s) => s,
                Err(e) => return err_json(e.to_string()),
            };
            let rows = stmt.query_map([from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok(serde_json::json!({
                    "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
                    "metric": row.get::<_, String>(1)?,
                    "score": row.get::<_, f64>(2)?,
                    "method": row.get::<_, String>(3)?,
                    "severity": row.get::<_, String>(4)?,
                }))
            });
            match rows {
                Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
                Err(e) => err_json(e.to_string()),
            }
        }
    }
}

async fn dashboard_forecast(
    State(st): State<ApiState>,
    Query(p): Query<DashboardParams>,
) -> impl IntoResponse {
    let horizon = p.horizon.unwrap_or(60).clamp(1, 200) as usize;
    let (from, to, interval) = dashboard_range(&p);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    // Series source: filtered → hot logs buckets; otherwise the most
    // recent rollup rows (the forecast is always about the recent trend).
    let (sql, params): (String, Vec<String>) = match &filter {
        Some((body, params)) => (
            format!(
                "SELECT time_bucket({}, ts) AS bucket, COUNT(*) AS n \
                 FROM logs WHERE ({body}) AND ts >= ? AND ts <= ? GROUP BY 1 ORDER BY 1 DESC LIMIT 120",
                interval_literal(interval)
            ),
            params.clone(),
        ),
        None => (
            // NB: cast the SUM — DuckDB widens to HUGEINT which can't be
            // read back as f64 and would silently yield zero points.
            "SELECT bucket, SUM(n)::DOUBLE FROM rollup_1m GROUP BY 1 ORDER BY 1 DESC LIMIT 120".into(),
            Vec::new(),
        ),
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()),
    };
    // Only the filtered branch has ts placeholders; the rollup branch takes
    // no params (binding from/to there would fail the param count check and
    // silently yield zero points).
    let duck = match &filter {
        Some(_) => with_ts_range(params, &from, &to),
        None => Vec::new(),
    };
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let pairs: Vec<(chrono::DateTime<chrono::Utc>, f64)> = stmt
        .query_map(refs.as_slice(), |row| {
            Ok((
                row.get::<_, chrono::DateTime<chrono::Utc>>(0)?,
                row.get::<_, f64>(1)?,
            ))
        })
        .ok()
        .map(|r| r.flatten().collect())
        .unwrap_or_default();
    drop(stmt);
    if pairs.len() < 4 {
        return Json(serde_json::json!({
            "error": format!("need at least 4 historical points, got {}", pairs.len()),
            "points": []
        }));
    }
    let (mut ts, mut series): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    ts.reverse();
    series.reverse();
    let req = crate::analytics::forecast::ForecastRequest {
        series,
        interval_secs: interval.max(1),
        horizon,
        level: 0.95,
        // Anchor the projection right after the newest history bucket so the
        // overlay lines up even for historical custom ranges.
        start: ts.last().copied(),
    };
    // Defense in depth: augurs has `todo!()` panics for unimplemented model
    // components (observed with seasonal auto-spec). A panic here would
    // abort the connection ("Failed to fetch" in the SPA) — catch it and
    // surface a JSON error instead.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::analytics::forecast::forecast_series(&req)
    }));
    match res {
        Ok(Ok(resp)) => Json(serde_json::json!({
            "history": ts.iter().map(|t| t.to_rfc3339()).collect::<Vec<_>>(),
            "forecast": resp.points,
            "model": resp.model,
        })),
        Ok(Err(e)) => Json(serde_json::json!({"error": e.to_string()})),
        Err(_) => {
            Json(serde_json::json!({"error": "forecast model panicked (degenerate series?)"}))
        }
    }
}

// =====================================================================
// Dashboard config CRUD
// =====================================================================

#[derive(Debug, Deserialize)]
struct CreateDashboardConfig {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// Array of panel definitions — opaque to the backend.
    panels: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardConfigOut {
    id: i64,
    name: String,
    description: Option<String>,
    panels: serde_json::Value,
    created_at: String,
    updated_at: String,
}

async fn list_dashboard_configs(State(st): State<ApiState>) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(
        "SELECT id, name, description, panels_json, created_at, updated_at FROM dashboard_configs ORDER BY name",
    ) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let rows = stmt.query_map([], |row| {
        let panels_str: String = row.get(3)?;
        let panels = serde_json::from_str(&panels_str).unwrap_or(serde_json::Value::Null);
        Ok(DashboardConfigOut {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            panels,
            created_at: row
                .get::<_, chrono::DateTime<chrono::Utc>>(4)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            updated_at: row
                .get::<_, chrono::DateTime<chrono::Utc>>(5)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
        })
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn get_dashboard_config(
    State(st): State<ApiState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let row = conn.query_row(
        "SELECT id, name, description, panels_json, created_at, updated_at FROM dashboard_configs WHERE id = ?",
        [id],
        |row| {
            let panels_str: String = row.get(3)?;
            let panels = serde_json::from_str(&panels_str).unwrap_or(serde_json::Value::Null);
            Ok(DashboardConfigOut {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                panels,
                created_at: row
                    .get::<_, chrono::DateTime<chrono::Utc>>(4)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default(),
                updated_at: row
                    .get::<_, chrono::DateTime<chrono::Utc>>(5)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default(),
            })
        },
    );
    match row {
        Ok(cfg) => Json(cfg).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn create_dashboard_config(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Json(body): Json<CreateDashboardConfig>,
) -> impl IntoResponse {
    let panels_str = serde_json::to_string(&body.panels).unwrap_or_else(|_| "[]".into());
    let conn = st.store.conn();
    let conn = conn.lock();
    // DuckDB has no `last_insert_rowid()` (SQLite-ism); pull the id from the
    // sequence up front so the response/audit log carry the real row id
    // instead of silently falling back to 0.
    let id: i64 = match conn.query_row("SELECT nextval('dashboard_configs_id_seq')", [], |r| {
        r.get(0)
    }) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let result = conn.execute(
        "INSERT INTO dashboard_configs (id, name, description, panels_json) VALUES (?, ?, ?, ?)",
        duckdb::params![id, &body.name, body.description.as_deref(), &panels_str],
    );
    match result {
        Ok(_) => {
            st.audit
                .event("dashboard.create")
                .actor(info.0.as_ref().expect("middleware enforces write scope"))
                .field("dashboard_id", id)
                .field("dashboard_name", body.name.as_str())
                .emit();
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": id, "status": "created" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateDashboardConfig {
    name: Option<String>,
    description: Option<Option<String>>, // outer Some = update; inner None = clear
    panels: Option<serde_json::Value>,
}

async fn update_dashboard_config(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDashboardConfig>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let mut touched = false;
    if let Some(name) = body.name {
        let _ = tx.execute(
            "UPDATE dashboard_configs SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            duckdb::params![&name, id],
        );
        touched = true;
    }
    if let Some(description) = body.description {
        let _ = tx.execute(
            "UPDATE dashboard_configs SET description = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            duckdb::params![description.as_deref(), id],
        );
        touched = true;
    }
    if let Some(panels) = body.panels {
        let panels_str = serde_json::to_string(&panels).unwrap_or_else(|_| "[]".into());
        let _ = tx.execute(
            "UPDATE dashboard_configs SET panels_json = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            duckdb::params![&panels_str, id],
        );
        touched = true;
    }
    let _ = tx.commit();
    if touched {
        st.audit
            .event("dashboard.update")
            .actor(info.0.as_ref().expect("middleware enforces write scope"))
            .field("dashboard_id", id)
            .emit();
    }
    Json(serde_json::json!({ "id": id, "status": "updated" })).into_response()
}

async fn delete_dashboard_config(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let _ = conn.execute("DELETE FROM dashboard_configs WHERE id = ?", [id]);
    st.audit
        .event("dashboard.delete")
        .actor(info.0.as_ref().expect("middleware enforces write scope"))
        .field("dashboard_id", id)
        .emit();
    Json(serde_json::json!({ "id": id, "status": "deleted" }))
}

// =====================================================================
// Alert rule approval workflow (architecture §7)
// =====================================================================

#[derive(Serialize)]
struct AlertRuleOut {
    id: i64,
    name: String,
    metric: String,
    condition: serde_json::Value,
    channel: Option<String>,
    /// Web-managed notification channel ids (see `alert_channels`).
    channels: Vec<i64>,
    status: String,
    created_at: String,
    activated_at: Option<String>,
    /// Evaluator state (architecture §7 alert evaluation). `None` for rules
    /// the evaluator hasn't reached yet (e.g. still `pending_approval`).
    last_evaluated_at: Option<String>,
    last_fired_at: Option<String>,
    last_notify_error: Option<String>,
}

fn opt_ts(row: &duckdb::Row<'_>, idx: usize) -> Option<String> {
    row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(idx)
        .ok()
        .flatten()
        .map(|d| d.to_rfc3339())
}

async fn list_alert_rules(State(st): State<ApiState>) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(
        "SELECT id, name, metric, condition_json, channel, status, created_at, activated_at, \
         last_evaluated_at, last_fired_at, last_notify_error, channels_json \
         FROM alert_rules ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let rows = stmt.query_map([], |row| {
        let cond_str: String = row.get(3)?;
        let cond = serde_json::from_str(&cond_str).unwrap_or(serde_json::Value::Null);
        let channels: Vec<i64> = row
            .get::<_, Option<String>>(11)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(AlertRuleOut {
            id: row.get(0)?,
            name: row.get(1)?,
            metric: row.get(2)?,
            condition: cond,
            channel: row.get(4)?,
            channels,
            status: row.get(5)?,
            created_at: row
                .get::<_, chrono::DateTime<chrono::Utc>>(6)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            activated_at: opt_ts(row, 7),
            last_evaluated_at: opt_ts(row, 8),
            last_fired_at: opt_ts(row, 9),
            last_notify_error: row.get::<_, Option<String>>(10)?,
        })
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(Serialize)]
struct AlertEventOut {
    id: i64,
    rule_id: i64,
    ts: String,
    metric: String,
    value: Option<f64>,
    message: Option<String>,
    notified: bool,
    error: Option<String>,
    severity: Option<String>,
}

/// Firing history for one rule (architecture §7 alert evaluation), newest
/// first. Populated by the background evaluator in `src/alerts.rs`.
async fn list_alert_events(State(st): State<ApiState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(
        "SELECT id, rule_id, ts, metric, value, message, notified, error, severity \
         FROM alert_events WHERE rule_id = ? ORDER BY ts DESC LIMIT 200",
    ) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let rows = stmt.query_map([id], |row| {
        Ok(AlertEventOut {
            id: row.get(0)?,
            rule_id: row.get(1)?,
            ts: row
                .get::<_, chrono::DateTime<chrono::Utc>>(2)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            metric: row.get(3)?,
            value: row.get(4)?,
            message: row.get(5)?,
            notified: row.get(6)?,
            error: row.get(7)?,
            severity: row.get(8)?,
        })
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn approve_alert_rule(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let n = conn.execute(
        "UPDATE alert_rules SET status = 'active', activated_at = CURRENT_TIMESTAMP WHERE id = ? AND status = 'pending_approval'",
        [id],
    )
    .unwrap_or(0);
    st.audit
        .event("alert.approve")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("alert_id", id)
        .field("updated_rows", n)
        .emit();
    Json(
        serde_json::json!({ "id": id, "updated_rows": n, "status": if n > 0 { "active" } else { "unchanged" } }),
    )
}

async fn reject_alert_rule(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let n = conn.execute(
        "UPDATE alert_rules SET status = 'rejected' WHERE id = ? AND status = 'pending_approval'",
        [id],
    )
    .unwrap_or(0);
    st.audit
        .event("alert.reject")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("alert_id", id)
        .field("updated_rows", n)
        .emit();
    Json(
        serde_json::json!({ "id": id, "updated_rows": n, "status": if n > 0 { "rejected" } else { "unchanged" } }),
    )
}

// =====================================================================
// Web-managed alert rules (create/edit/delete from the UI)
// =====================================================================

#[derive(Deserialize)]
struct AlertRuleBody {
    name: String,
    /// Condition JSON — one of threshold / anomaly / count (see alerts.rs).
    condition: serde_json::Value,
    /// Metric for threshold/anomaly conditions (ignored for count).
    #[serde(default)]
    metric: Option<String>,
    /// Web-managed channel ids to notify.
    #[serde(default)]
    channels: Vec<i64>,
    /// Legacy raw webhook URL (still supported).
    #[serde(default)]
    channel: Option<String>,
}

fn validate_alert_rule(
    store: &Store,
    body: &AlertRuleBody,
) -> Result<(String, serde_json::Value), String> {
    use crate::alerts::AlertCondition;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err("name must be 1..=128 chars".into());
    }
    // Deserialize into the real condition enum (unknown type → error).
    let cond: AlertCondition = serde_json::from_value(body.condition.clone())
        .map_err(|e| format!("invalid condition: {e}"))?;
    // Multi-threshold validation: non-empty entries, known comparators,
    // non-empty severity labels. Array order is the escalation order.
    {
        use crate::alerts::ThresholdEntry;
        let entries: Vec<ThresholdEntry> = match &cond {
            AlertCondition::Threshold { thresholds, .. }
            | AlertCondition::Count { thresholds, .. } => thresholds.clone(),
            _ => Vec::new(),
        };
        for (i, e) in entries.iter().enumerate() {
            if alert_compare(0.0, &e.comparator, 0.0).is_err() {
                return Err(format!(
                    "thresholds[{i}]: unknown comparator '{}' (expected >, >=, <, <=, or ==)",
                    e.comparator
                ));
            }
            if e.severity.trim().is_empty() {
                return Err(format!("thresholds[{i}]: severity label must not be empty"));
            }
        }
    }
    // Count conditions carry their own filter; parse it now so a bad DSL
    // fails at save time instead of silently erroring every eval cycle.
    let metric = match &cond {
        AlertCondition::Count { filter, .. } => {
            let whitelist = ColumnWhitelist::standard(store.hot_attributes());
            let compiled = parse_filter(filter).map_err(|e| format!("invalid filter: {e}"))?;
            compiled
                .to_sql(&whitelist)
                .map_err(|e| format!("invalid filter: {e}"))?;
            "events_matching_filter".to_string()
        }
        AlertCondition::ErrorGroupThreshold { .. } => "error_group_threshold".to_string(),
        _ => {
            let m = body.metric.as_deref().map(str::trim).unwrap_or_default();
            if m.is_empty() {
                return Err("metric is required for threshold/anomaly conditions".into());
            }
            m.to_string()
        }
    };
    // At least one delivery target, and referenced channels must exist.
    if body.channels.is_empty()
        && body
            .channel
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err("at least one notification channel is required".into());
    }
    if !body.channels.is_empty() {
        let conn = store.conn();
        let conn = conn.lock();
        for id in &body.channels {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM alert_channels WHERE id = ?",
                    [id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if exists == 0 {
                return Err(format!("channel {id} does not exist"));
            }
        }
    }
    if let Some(url) = body.channel.as_deref() {
        let url = url.trim();
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("legacy channel must be an http(s) webhook URL".into());
        }
    }
    Ok((metric, body.condition.clone()))
}

async fn create_alert_rule(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Json(body): Json<AlertRuleBody>,
) -> impl IntoResponse {
    let (metric, condition) = match validate_alert_rule(&st.store, &body) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({"error": e})),
    };
    // UI-created rules start active: the human pressing "save" is the
    // approver (architecture §7 human-in-the-loop). MCP-created rules still
    // land in pending_approval.
    let conn = st.store.conn();
    let conn = conn.lock();
    let id: i64 = match conn.query_row("SELECT nextval('alert_rules_id_seq')", [], |r| r.get(0)) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let channels_json = serde_json::to_string(&body.channels).unwrap_or_else(|_| "[]".into());
    let n = conn.execute(
        "INSERT INTO alert_rules (id, name, metric, condition_json, channel, channels_json, status, activated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'active', CURRENT_TIMESTAMP)",
        params![
            id,
            body.name.trim(),
            metric,
            condition.to_string(),
            body.channel.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            channels_json
        ],
    );
    drop(conn);
    match n {
        Ok(_) => {
            st.audit
                .event("alert.create")
                .actor(info.0.as_ref().expect("middleware enforces admin scope"))
                .field("alert_id", id)
                .emit();
            Json(serde_json::json!({ "id": id, "status": "active" }))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn update_alert_rule(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
    Json(body): Json<AlertRuleBody>,
) -> impl IntoResponse {
    let (metric, condition) = match validate_alert_rule(&st.store, &body) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({"error": e})),
    };
    let conn = st.store.conn();
    let conn = conn.lock();
    let channels_json = serde_json::to_string(&body.channels).unwrap_or_else(|_| "[]".into());
    let n = conn.execute(
        "UPDATE alert_rules SET name = ?, metric = ?, condition_json = ?, channel = ?, channels_json = ? \
         WHERE id = ?",
        params![
            body.name.trim(),
            metric,
            condition.to_string(),
            body.channel.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            channels_json,
            id
        ],
    )
    .unwrap_or(0);
    drop(conn);
    st.audit
        .event("alert.update")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("alert_id", id)
        .field("updated_rows", n)
        .emit();
    Json(serde_json::json!({ "id": id, "updated_rows": n }))
}

async fn delete_alert_rule(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    // Firing history is kept (audit trail); only the rule row goes.
    let n = conn
        .execute("DELETE FROM alert_rules WHERE id = ?", [id])
        .unwrap_or(0);
    drop(conn);
    st.audit
        .event("alert.delete")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("alert_id", id)
        .field("deleted_rows", n)
        .emit();
    Json(serde_json::json!({ "id": id, "deleted_rows": n }))
}

// =====================================================================
// Notification channels (email / telegram / webhook)
// =====================================================================

#[derive(Deserialize)]
struct AlertChannelBody {
    name: String,
    /// "email" | "telegram" | "webhook"
    #[serde(rename = "type")]
    channel_type: String,
    config: serde_json::Value,
}

const SECRET_MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// Replace secret fields with a mask so list responses never carry live
/// credentials. `update` restores the previous value when it sees the mask.
fn mask_channel_config(ctype: &str, config: &mut serde_json::Value) {
    let obj = match config.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match ctype {
        "telegram" => {
            if let Some(t) = obj.get("bot_token").and_then(|v| v.as_str()) {
                let masked = if t.len() > 8 {
                    format!("{}{}", SECRET_MASK, &t[t.len() - 4..])
                } else {
                    SECRET_MASK.to_string()
                };
                obj.insert("bot_token_masked".into(), serde_json::Value::String(masked));
            }
            obj.remove("bot_token");
        }
        _ => {}
    }
}

fn validate_channel_config(ctype: &str, config: &serde_json::Value) -> Result<(), String> {
    match ctype {
        "email" => {
            let rcpts = config["recipients"]
                .as_array()
                .ok_or("email config requires \"recipients\": [ ... ]")?;
            let valid = rcpts
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|s| s.contains('@') && !s.trim().is_empty())
                .count();
            if valid == 0 {
                return Err("email recipients must be a non-empty list of addresses".into());
            }
            Ok(())
        }
        "telegram" => {
            let token = config["bot_token"].as_str().unwrap_or_default().trim();
            let chat = config["chat_id"].as_str().unwrap_or_default().trim();
            if token.is_empty() || chat.is_empty() {
                return Err("telegram config requires \"bot_token\" and \"chat_id\"".into());
            }
            Ok(())
        }
        "webhook" => {
            let url = config["url"].as_str().unwrap_or_default().trim();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("webhook config requires \"url\" starting with http(s)://".into());
            }
            Ok(())
        }
        other => Err(format!(
            "unknown channel type '{other}' (expected email, telegram, or webhook)"
        )),
    }
}

/// Live-check a Telegram bot token via `getMe` so wrong-kind secrets (e.g. a
/// `clk_…` API key) or typos are rejected at save time instead of failing on
/// first delivery with a cryptic 404.
async fn verify_telegram_token(http: &reqwest::Client, token: &str) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{}/getMe", token.trim());
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("telegram token check failed: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if body["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("bot_token is not a valid Telegram bot token — get one from @BotFather".into());
    }
    Err(format!("telegram token check returned HTTP {status}"))
}

/// Validate the incoming telegram token only when the caller is actually
/// setting a new one (empty or the masked placeholder means "keep stored").
async fn ensure_telegram_token_valid(
    http: &reqwest::Client,
    ctype: &str,
    config: &serde_json::Value,
) -> Result<(), String> {
    if ctype != "telegram" {
        return Ok(());
    }
    let tok = config
        .get("bot_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if tok.trim().is_empty() || tok.trim() == SECRET_MASK {
        return Ok(());
    }
    verify_telegram_token(http, tok).await
}

#[derive(Serialize)]
struct AlertChannelOut {
    id: i64,
    name: String,
    #[serde(rename = "type")]
    channel_type: String,
    config: serde_json::Value,
    created_at: String,
    updated_at: Option<String>,
}

fn channel_row(row: &duckdb::Row<'_>) -> Result<AlertChannelOut, duckdb::Error> {
    let mut config: serde_json::Value =
        serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(serde_json::Value::Null);
    let ctype: String = row.get(2)?;
    mask_channel_config(&ctype, &mut config);
    Ok(AlertChannelOut {
        id: row.get(0)?,
        name: row.get(1)?,
        channel_type: ctype,
        config,
        created_at: row
            .get::<_, chrono::DateTime<chrono::Utc>>(4)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default(),
        updated_at: opt_ts(row, 5),
    })
}

async fn list_alert_channels(State(st): State<ApiState>) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(
        "SELECT id, name, type, config_json, created_at, updated_at \
         FROM alert_channels ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let rows = stmt.query_map([], channel_row);
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn create_alert_channel(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Json(body): Json<AlertChannelBody>,
) -> impl IntoResponse {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Json(serde_json::json!({"error": "name must be 1..=128 chars"}));
    }
    if let Err(e) = validate_channel_config(&body.channel_type, &body.config) {
        return Json(serde_json::json!({"error": e}));
    }
    if let Err(e) = ensure_telegram_token_valid(&st.http, &body.channel_type, &body.config).await {
        return Json(serde_json::json!({"error": e}));
    }
    let conn = st.store.conn();
    let conn = conn.lock();
    let id: i64 = match conn.query_row("SELECT nextval('alert_channels_id_seq')", [], |r| r.get(0))
    {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };
    let n = conn.execute(
        "INSERT INTO alert_channels (id, name, type, config_json) VALUES (?, ?, ?, ?)",
        params![id, name, body.channel_type, body.config.to_string()],
    );
    drop(conn);
    match n {
        Ok(_) => {
            st.audit
                .event("alert.channel.create")
                .actor(info.0.as_ref().expect("middleware enforces admin scope"))
                .field("channel_id", id)
                .field("channel_type", body.channel_type.as_str())
                .emit();
            Json(serde_json::json!({ "id": id, "name": name }))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

/// Load the stored config, then overlay any non-masked values from `patch`.
fn merge_channel_config(
    stored: &str,
    ctype: &str,
    incoming: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut merged: serde_json::Value =
        serde_json::from_str(stored).map_err(|e| format!("stored config corrupt: {e}"))?;
    let inc = incoming
        .as_object()
        .ok_or_else(|| "config must be an object".to_string())?;
    for (k, v) in inc {
        // Secret fields: skip when the caller sent back the mask unchanged.
        let is_secret = ctype == "telegram" && k == "bot_token";
        if is_secret && v.as_str() == Some(SECRET_MASK) {
            continue;
        }
        if let Some(obj) = merged.as_object_mut() {
            obj.insert(k.clone(), v.clone());
        }
    }
    validate_channel_config(ctype, &merged)?;
    Ok(merged)
}

async fn update_alert_channel(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
    Json(body): Json<AlertChannelBody>,
) -> impl IntoResponse {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Json(serde_json::json!({"error": "name must be 1..=128 chars"}));
    }
    if let Err(e) = ensure_telegram_token_valid(&st.http, &body.channel_type, &body.config).await {
        return Json(serde_json::json!({"error": e}));
    }
    let conn = st.store.conn();
    let conn = conn.lock();
    let stored: Option<String> = conn
        .query_row(
            "SELECT config_json FROM alert_channels WHERE id = ? AND type = ?",
            params![id, body.channel_type],
            |r| r.get(0),
        )
        .ok();
    let Some(stored) = stored else {
        return Json(
            serde_json::json!({"error": format!("channel {id} ({}) not found", body.channel_type)}),
        );
    };
    let merged = match merge_channel_config(&stored, &body.channel_type, &body.config) {
        Ok(m) => m,
        Err(e) => return Json(serde_json::json!({"error": e})),
    };
    let n = conn
        .execute(
            "UPDATE alert_channels SET name = ?, config_json = ?, updated_at = CURRENT_TIMESTAMP \
             WHERE id = ?",
            params![name, merged.to_string(), id],
        )
        .unwrap_or(0);
    drop(conn);
    st.audit
        .event("alert.channel.update")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("channel_id", id)
        .field("updated_rows", n)
        .emit();
    Json(serde_json::json!({ "id": id, "updated_rows": n }))
}

async fn delete_alert_channel(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = st.store.conn();
    let conn = conn.lock();
    let n = conn
        .execute("DELETE FROM alert_channels WHERE id = ?", [id])
        .unwrap_or(0);
    drop(conn);
    st.audit
        .event("alert.channel.delete")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("channel_id", id)
        .field("deleted_rows", n)
        .emit();
    Json(serde_json::json!({ "id": id, "deleted_rows": n }))
}

/// Fire a sample notification through the channel so the operator can
/// verify credentials without waiting for a real alert.
async fn test_alert_channel(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let targets = {
        let conn = st.store.conn();
        let conn = conn.lock();
        crate::alerts::load_channel_targets(&conn, &[id])
    };
    let Some((_name, target)) = targets.first().cloned() else {
        return Json(serde_json::json!({"error": format!("channel {id} not found")}));
    };
    let sample = crate::alerts::FiredAlert {
        rule_id: 0,
        name: "channel test".into(),
        metric: "test".into(),
        value: 0.0,
        message: "This is a test notification from central-logs. If you can read this, the channel works.".into(),
        severity: "test".into(),
        channel: String::new(),
        channel_ids: vec![id],
    };
    let (ok, err) =
        crate::alerts::deliver(&st.http, Some(st.smtp.as_ref()), &target, &sample).await;
    st.audit
        .event("alert.channel.test")
        .actor(info.0.as_ref().expect("middleware enforces admin scope"))
        .field("channel_id", id)
        .field("ok", ok)
        .emit();
    Json(serde_json::json!({ "ok": ok, "error": err }))
}

// =====================================================================
// AI natural-language query
// =====================================================================

async fn ai_query(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    ResolvedPeer(peer): ResolvedPeer,
    Json(req): Json<AiQueryRequest>,
) -> impl IntoResponse {
    let hot = st.store.hot_attributes().to_vec();
    // Audit the AI query before issuing the LLM call. We log the user's
    // input text — it's the operator's data and "who asked what" is exactly
    // the kind of question an audit trail should answer.
    if let Some(i) = &info.0 {
        st.audit
            .event("ai.query")
            .actor(i)
            .source_ip(&peer)
            .field("query_text", req.query.as_str())
            .field("query_len", req.query.len() as i64)
            .emit();
    }
    match translate(&st.llm, &req, &hot).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /api/ai/dashboard` — describe a dashboard in prose; get back
/// either clarifying questions (with choices when enumerable) or a
/// validated dashboard proposal ready to save. Stateless: the SPA holds
/// any clarify-round questions and echoes them with answers.
async fn ai_dashboard(
    State(st): State<ApiState>,
    info: MaybeAuthInfo,
    ResolvedPeer(peer): ResolvedPeer,
    Json(req): Json<crate::ai::AiDashboardRequest>,
) -> Response {
    // Ground the model: which columns exist + which services actually have
    // data (top by rollup volume) so it stops inventing service names.
    let mut columns = ColumnWhitelist::standard(st.store.hot_attributes())
        .keys()
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    columns.sort();
    let mut services: Vec<String> = Vec::new();
    {
        let conn = st.store.conn();
        let conn = conn.lock();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT service, SUM(n) AS n FROM rollup_1m GROUP BY 1 ORDER BY 2 DESC LIMIT 20",
        ) {
            if let Ok(rs) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                services = rs.flatten().collect();
            }
        }
    }
    if let Some(i) = &info.0 {
        st.audit
            .event("ai.dashboard")
            .actor(i)
            .source_ip(&peer)
            .field("description_len", req.description.len() as i64)
            .field("answers", req.answers.len() as i64)
            .emit();
    }
    let ctx = crate::ai::DashboardContext { columns, services };
    match crate::ai::build_dashboard(&st.llm, &req, &ctx, st.store.hot_attributes()).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// =====================================================================
// OpenTelemetry traces + metrics (protocol:otlp_span / protocol:otlp_metric)
// =====================================================================

#[derive(Debug, Deserialize)]
struct TracesParams {
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// SQL for `GET /api/traces`. The bound parameter order is
/// `[filter params…, from, to, limit]` (see `with_ts_range`), so `{filter_body}`
/// is placed BEFORE the `ts >= ?`/`ts <= ?` placeholders — otherwise the
/// filter value binds to the timestamp slot and the result is silently empty.
fn traces_list_sql(filter_body: &str) -> String {
    format!(
        "SELECT trace_id, MIN(ts) AS start_ts, MAX(ts) AS last_ts, \
                COUNT(*)::BIGINT AS span_count, \
                SUM(CASE WHEN level='error' THEN 1 ELSE 0 END)::BIGINT AS error_count, \
                MAX(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE)) AS duration_ms, \
                MIN(service) AS service, MIN(message) AS root_name \
         FROM logs \
         WHERE protocol='otlp_span' AND trace_id IS NOT NULL AND trace_id <> ''{filter_body} \
           AND ts >= ? AND ts <= ? \
         GROUP BY trace_id ORDER BY start_ts DESC LIMIT ?"
    )
}

/// `GET /api/traces` — recent traces, one row per `trace_id`, aggregated from
/// OTLP spans (`protocol:otlp_span`). Newest first.
async fn traces_list(State(st): State<ApiState>, Query(p): Query<TracesParams>) -> Response {
    let (from, to) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let filter = match compile_dashboard_filter(p.filter.as_deref(), st.store.hot_attributes()) {
        Ok(f) => f,
        Err(e) => return err_json(e).into_response(),
    };
    // NOTE: filter params are bound BEFORE the from/to timestamps (matching
    // `with_ts_range`), so the filter body must precede the `ts >= ?` clause.
    let (filter_body, filter_params) = match filter {
        Some((body, params)) => (format!(" AND ({body})"), params),
        None => (String::new(), Vec::new()),
    };
    let sql = traces_list_sql(&filter_body);
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()).into_response(),
    };
    let mut duck = with_ts_range(filter_params, &from, &to);
    duck.push(duckdb::types::Value::BigInt(limit));
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "trace_id": row.get::<_, String>(0)?,
            "start_ts": row.get::<_, chrono::DateTime<chrono::Utc>>(1)?.to_rfc3339(),
            "last_ts": row.get::<_, chrono::DateTime<chrono::Utc>>(2)?.to_rfc3339(),
            "span_count": row.get::<_, i64>(3)?,
            "error_count": row.get::<_, i64>(4)?,
            "duration_ms": row.get::<_, Option<f64>>(5)?,
            "service": row.get::<_, Option<String>>(6)?,
            "root_name": row.get::<_, Option<String>>(7)?,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())).into_response(),
        Err(e) => err_json(e.to_string()).into_response(),
    }
}

/// `GET /api/traces/{trace_id}` — every stored span for one trace, in
/// start-time order, with the queryable span fields promoted to top level.
async fn trace_detail(State(st): State<ApiState>, Path(trace_id): Path<String>) -> Response {
    let conn = st.store.conn();
    let conn = conn.lock();
    let sql = "SELECT ts, service, level, message, span_id, \
                      attributes->>'$.parent_span_id' AS parent_span_id, \
                      attributes->>'$.span_kind' AS span_kind, \
                      attributes->>'$.status_code' AS status_code, \
                      TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE) AS duration_ms, \
                      attributes::VARCHAR AS attributes \
               FROM logs \
               WHERE protocol='otlp_span' AND trace_id = ? \
               ORDER BY ts ASC LIMIT 2000";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()).into_response(),
    };
    let rows = stmt.query_map([trace_id.as_str()], |row| {
        let attrs_str: Option<String> = row.get(9)?;
        let attrs = attrs_str
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "service": row.get::<_, Option<String>>(1)?,
            "level": row.get::<_, Option<String>>(2)?,
            "name": row.get::<_, Option<String>>(3)?,
            "span_id": row.get::<_, Option<String>>(4)?,
            "parent_span_id": row.get::<_, Option<String>>(5)?,
            "span_kind": row.get::<_, Option<String>>(6)?,
            "status_code": row.get::<_, Option<String>>(7)?,
            "duration_ms": row.get::<_, Option<f64>>(8)?,
            "attributes": attrs,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!({
            "trace_id": trace_id,
            "spans": rs.flatten().collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err_json(e.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct MetricsParams {
    /// Metric name (the `message` column of `otlp_metric` rows).
    name: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    bucket: Option<String>,
}

/// `GET /api/metrics/series?name=...` — a numeric time series for one OTLP
/// metric, averaged per bucket. `value` comes from `attributes.value`.
async fn metric_series(State(st): State<ApiState>, Query(p): Query<MetricsParams>) -> Response {
    let (from, to) = resolve_window(p.window.as_deref(), p.from.as_deref(), p.to.as_deref());
    let span = (to - from).num_seconds().max(1);
    let interval = bucket_interval_secs(p.bucket.as_deref(), span);
    let service_clause = if p.service.is_some() {
        " AND service = ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT time_bucket({}, ts) AS bucket, \
                AVG(TRY_CAST(attributes->>'$.value' AS DOUBLE)) AS value, \
                COUNT(*)::BIGINT AS n \
         FROM logs \
         WHERE protocol='otlp_metric' AND message = ? AND ts >= ? AND ts <= ?{service_clause} \
         GROUP BY 1 ORDER BY 1",
        interval_literal(interval)
    );
    let conn = st.store.conn();
    let conn = conn.lock();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err_json(e.to_string()).into_response(),
    };
    let mut params: Vec<String> = vec![p.name.clone()];
    params.push(from.to_rfc3339());
    params.push(to.to_rfc3339());
    if let Some(svc) = &p.service {
        params.push(svc.clone());
    }
    let duck = params_as_duck(&params);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "ts": row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
            "value": row.get::<_, Option<f64>>(1)?,
            "n": row.get::<_, i64>(2)?,
        }))
    });
    match rows {
        Ok(rs) => Json(serde_json::json!(rs.flatten().collect::<Vec<_>>())).into_response(),
        Err(e) => err_json(e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_list_sql_binds_filter_before_timestamps() {
        // Params are bound filter-first; the filter placeholder must therefore
        // appear before `ts >= ?` or the query silently returns no rows.
        let sql = traces_list_sql(" AND (service = ?)");
        let filter_pos = sql.find("service = ?").expect("filter placeholder");
        let ts_pos = sql.find("ts >= ?").expect("ts placeholder");
        assert!(
            filter_pos < ts_pos,
            "filter placeholder must precede timestamp placeholders"
        );
    }

    #[test]
    fn window_shorthand_parses() {
        assert_eq!(parse_window_shorthand("5m"), Some(300));
        assert_eq!(parse_window_shorthand("1h"), Some(3600));
        assert_eq!(parse_window_shorthand("2d"), Some(2 * 86400));
        assert_eq!(parse_window_shorthand("1w"), Some(7 * 86400));
        assert_eq!(parse_window_shorthand("garbage"), None);
    }

    #[test]
    fn flatten_json_expands_nested_objects_into_dot_paths() {
        let v = serde_json::json!({
            "request": { "route": "/x", "retries": 2 },
            "tags": ["a", "b"],
            "ok": true
        });
        let mut out = Vec::new();
        flatten_json("", &v, &mut out);
        let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        // Key order follows serde_json's map (alphabetical here); the export
        // unions keys across rows in first-appearance order on top of this.
        assert_eq!(keys, vec!["ok", "request.retries", "request.route", "tags"]);
        assert_eq!(out[1].1, serde_json::json!(2));
        assert_eq!(out[2].1, serde_json::json!("/x"));
        // Arrays are not expanded — they land as a JSON string in one cell.
        assert_eq!(out[3].1.to_string(), r#"["a","b"]"#);
    }

    #[test]
    fn flatten_json_scalar_root_lands_under_empty_key() {
        let mut out = Vec::new();
        flatten_json("", &serde_json::json!("plain"), &mut out);
        assert_eq!(out, vec![("".to_string(), serde_json::json!("plain"))]);
    }

    #[test]
    fn window_shorthand_defaults_to_one_hour_when_empty() {
        let (from, to) = resolve_window(None, None, None);
        let span = to - from;
        assert_eq!(span.num_seconds(), 3600);
    }

    #[test]
    fn window_5m_shorthand_overrides_default() {
        let (from, _to) = resolve_window(Some("5m"), None, None);
        let span = chrono::Utc::now() - from;
        // ~5 minutes (within a small slack for test timing).
        assert!(
            span.num_seconds() >= 290 && span.num_seconds() <= 310,
            "got {}s",
            span.num_seconds()
        );
    }

    #[test]
    fn query_limit_rejects_oversized_window_for_matching_service() {
        let limits = vec![ServiceQueryLimit {
            pattern: "payment_*".into(),
            max_window_secs: 3600,
        }];
        let compiled = parse_filter("service:payment_api level:error").unwrap();
        let err = enforce_service_query_limits(&compiled, 86400, &limits).unwrap_err();
        assert!(err.contains("payment_api"), "got: {err}");
        // Within the cap → ok.
        assert!(enforce_service_query_limits(&compiled, 600, &limits).is_ok());
    }

    #[test]
    fn query_limit_ignores_non_matching_services_and_other_columns() {
        let limits = vec![ServiceQueryLimit {
            pattern: "payment_*".into(),
            max_window_secs: 3600,
        }];
        // Different service — not capped even with a huge window.
        let compiled = parse_filter("service:audit_log").unwrap();
        assert!(enforce_service_query_limits(&compiled, 86400 * 30, &limits).is_ok());
        // Non-service equality columns are not capped.
        let compiled = parse_filter("level:error").unwrap();
        assert!(enforce_service_query_limits(&compiled, 86400 * 30, &limits).is_ok());
        // Text search (ILIKE) on service isn't an equality pin — not capped.
        let compiled = parse_filter("service~payment").unwrap();
        assert!(enforce_service_query_limits(&compiled, 86400 * 30, &limits).is_ok());
        // No limits configured → never capped.
        let compiled = parse_filter("service:payment_api").unwrap();
        assert!(enforce_service_query_limits(&compiled, i64::MAX, &[]).is_ok());
    }

    #[test]
    fn bucket_interval_auto_and_explicit() {
        // Auto: minute up to 6h, hour beyond.
        assert_eq!(bucket_interval_secs(None, 60), 60);
        assert_eq!(bucket_interval_secs(None, 6 * 3600), 60);
        assert_eq!(bucket_interval_secs(None, 6 * 3600 + 1), 3600);
        assert_eq!(bucket_interval_secs(None, 7 * 86400), 3600);
        // Explicit bucket param always wins.
        assert_eq!(bucket_interval_secs(Some("hour"), 300), 3600);
        assert_eq!(bucket_interval_secs(Some("minute"), 7 * 86400), 60);
        assert_eq!(interval_literal(3600), "INTERVAL '1 hour'");
        assert_eq!(interval_literal(60), "INTERVAL '1 minute'");
    }

    #[test]
    fn dashboard_filter_compiles_or_rejects() {
        assert!(compile_dashboard_filter(None, &[]).unwrap().is_none());
        assert!(compile_dashboard_filter(Some("  "), &[]).unwrap().is_none());
        let (body, params) = compile_dashboard_filter(Some("service:api level:error"), &[])
            .unwrap()
            .unwrap();
        assert!(body.contains("service = ?"));
        assert_eq!(params.len(), 2);
        // Unknown column → friendly error, not a panic.
        assert!(compile_dashboard_filter(Some("bogus:x"), &[]).is_err());
        assert!(compile_dashboard_filter(Some("service:api OR ("), &[]).is_err());
    }

    #[test]
    fn dashboard_range_honors_custom_from_to() {
        let p = DashboardParams {
            window: None,
            bucket: None,
            from: Some("2026-09-01T00:00:00Z".into()),
            to: Some("2026-09-01T02:00:00Z".into()),
            filter: None,
            horizon: None,
        };
        let (from, to, interval) = dashboard_range(&p);
        assert_eq!(from.to_rfc3339(), "2026-09-01T00:00:00+00:00");
        assert_eq!(to.to_rfc3339(), "2026-09-01T02:00:00+00:00");
        assert_eq!(interval, 60, "2h span → minute buckets");
        // window shorthand still wins when both are set (same as /api/logs).
        let p = DashboardParams {
            window: Some("7d".into()),
            bucket: None,
            from: Some("2026-09-01T00:00:00Z".into()),
            to: Some("2026-09-01T02:00:00Z".into()),
            filter: None,
            horizon: None,
        };
        let (_, _, interval) = dashboard_range(&p);
        assert_eq!(interval, 3600, "7d span → hour buckets");
    }
}
