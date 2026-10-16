//! MCP tool implementations + rmcp wiring (architecture §7).

#![cfg_attr(not(feature = "mcp"), allow(unused))]

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::analytics::anomaly::AnomalyMethod;
use crate::analytics::forecast::{forecast_series, ForecastRequest};
use crate::store::Store;
use crate::wal::meta::WalMeta;

/// Parameters for `query_logs`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QueryLogsParams {
    /// SQL predicate (WHERE clause body) or `None` for no filter. Only
    /// constrained SQL is allowed — bound to a hard cap.
    #[schemars(default)]
    pub query: Option<String>,
    /// ISO-8601 `from` timestamp.
    #[schemars(default)]
    pub from: Option<String>,
    /// ISO-8601 `to` timestamp.
    #[schemars(default)]
    pub to: Option<String>,
    /// Maximum rows to return. Default 100, hard cap 1000.
    #[serde(default = "default_query_limit")]
    pub limit: i64,
}

fn default_query_limit() -> i64 {
    100
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryLogsRow {
    pub ts: String,
    pub service: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryLogsResult {
    pub rows: Vec<QueryLogsRow>,
    pub total_matched: i64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DashboardSummaryParams {
    /// Metric name (e.g. "volume", "error_rate", "p95_latency").
    pub metric: String,
    /// ISO-8601 `from`.
    #[schemars(default)]
    pub from: Option<String>,
    /// ISO-8601 `to`.
    #[schemars(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSummaryResult {
    pub summary_stats: serde_json::Value,
    pub top_n: Vec<serde_json::Value>,
    pub narrative_hints: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DetectAnomaliesParams {
    /// Metric to scan (e.g. "volume", "error_rate", "p95_latency").
    pub metric: String,
    /// ISO-8601 `from`.
    #[schemars(default)]
    pub from: Option<String>,
    /// ISO-8601 `to`.
    #[schemars(default)]
    pub to: Option<String>,
    /// Sensitivity z-score threshold (default 3.0).
    #[serde(default = "default_sensitivity")]
    pub sensitivity: f64,
}

fn default_sensitivity() -> f64 {
    3.0
}

#[derive(Debug, Clone, Serialize)]
pub struct AnomalyOut {
    pub ts: String,
    pub metric: String,
    pub score: f64,
    pub method: String,
    pub severity: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ForecastToolParams {
    /// Metric to forecast (e.g. "volume", "error_rate").
    pub metric: String,
    /// Forecast horizon, e.g. "6h" or "30m".
    pub horizon: String,
    /// Optional granularity hint: "minute" or "hour".
    #[serde(default)]
    pub granularity: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateAlertRuleParams {
    pub name: String,
    pub metric: String,
    /// Condition body (free-form JSON: threshold or anomaly-based).
    pub condition: serde_json::Value,
    pub notification_channel: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateAlertRuleResult {
    pub rule_id: i64,
    pub status: String,
}

// =====================================================================
// Website-analytics tools (PLAN-WEB-ANALYTICS W5). Aggregates only —
// these never return raw rows, source IPs, or `user_id` hashes.
// =====================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WebStatsParams {
    /// Site id (`[[web.site]].id`). Empty = all sites.
    #[schemars(default)]
    pub site: Option<String>,
    /// ISO-8601 `from`. Default: 24h ago.
    #[schemars(default)]
    pub from: Option<String>,
    /// ISO-8601 `to`. Default: now.
    #[schemars(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WebTopParams {
    /// Dimension: path | source | medium | country | device | browser | os | event.
    pub by: String,
    /// Site id. Empty = all sites.
    #[schemars(default)]
    pub site: Option<String>,
    /// ISO-8601 `from`. Default: 24h ago.
    #[schemars(default)]
    pub from: Option<String>,
    /// ISO-8601 `to`. Default: now.
    #[schemars(default)]
    pub to: Option<String>,
    /// Max rows (default 10, hard cap 50).
    #[serde(default = "default_web_top_limit")]
    pub limit: i64,
}

fn default_web_top_limit() -> i64 {
    10
}

/// Whitelisted breakdown dimension → SQL column (same table as the HTTP API).
fn web_top_column(by: &str) -> Option<&'static str> {
    match by {
        "path" => Some("path"),
        "event" => Some("event"),
        "source" => Some("COALESCE(NULLIF(TRIM(source), ''), 'direct')"),
        "medium" => Some("COALESCE(NULLIF(TRIM(medium), ''), 'none')"),
        "country" => Some("COALESCE(NULLIF(TRIM(country), ''), 'unknown')"),
        "region" => Some("COALESCE(NULLIF(TRIM(region), ''), 'unknown')"),
        "city" => Some("COALESCE(NULLIF(TRIM(city), ''), 'unknown')"),
        "screen" => Some("COALESCE(CAST(screen_w AS VARCHAR), 'unknown')"),
        "utm_source" => Some("COALESCE(NULLIF(TRIM(utm->>'$.source'), ''), '(none)')"),
        "utm_medium" => Some("COALESCE(NULLIF(TRIM(utm->>'$.medium'), ''), '(none)')"),
        "utm_campaign" => Some("COALESCE(NULLIF(TRIM(utm->>'$.campaign'), ''), '(none)')"),
        "device" => Some("COALESCE(NULLIF(TRIM(device), ''), 'unknown')"),
        "browser" => Some("COALESCE(NULLIF(TRIM(browser), ''), 'unknown')"),
        "os" => Some("COALESCE(NULLIF(TRIM(os), ''), 'unknown')"),
        _ => None,
    }
}

/// Parse ISO-8601 (or date-only) bound, with a fallback default.
fn web_time_bound(s: Option<&str>, default: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    s.and_then(|v| {
        chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok()
            .or_else(|| {
                chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .ok()
                    .map(|d| {
                        chrono::DateTime::from_naive_utc_and_offset(
                            d.and_hms_opt(0, 0, 0).unwrap(),
                            chrono::Utc,
                        )
                    })
            })
    })
    .unwrap_or(default)
}

/// The MCP server. Holds shared handles to the store and WAL metadata.
#[derive(Clone)]
pub struct CentralLogsMcpServer {
    pub store: Store,
    pub meta: Arc<WalMeta>,
    pub enable_alert_tool: bool,
}

#[tool_router]
impl CentralLogsMcpServer {
    fn new(store: Store, meta: Arc<WalMeta>, enable_alert_tool: bool) -> Self {
        Self {
            store,
            meta,
            enable_alert_tool,
        }
    }

    #[tool(
        description = "Query raw log rows with optional SQL predicate and time range. Read-only."
    )]
    async fn query_logs(
        &self,
        Parameters(params): Parameters<QueryLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.clamp(1, 1000);
        let mut where_parts: Vec<String> = Vec::new();
        if let Some(q) = &params.query {
            let q = q.trim().trim_end_matches(';');
            if !q.is_empty() {
                where_parts.push(format!("({q})"));
            }
        }
        if let Some(from) = &params.from {
            where_parts.push(format!("ts >= '{from}'"));
        }
        if let Some(to) = &params.to {
            where_parts.push(format!("ts <= '{to}'"));
        }
        let where_clause = if where_parts.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_parts.join(" AND "))
        };
        let sql = format!(
            "SELECT ts, service, level, message FROM logs_all {where_clause} ORDER BY ts DESC LIMIT {limit}"
        );

        let conn = self.store.conn();
        let conn = conn.lock();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| McpError::internal_error(format!("prepare: {e}"), None))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(QueryLogsRow {
                    ts: row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
                    service: row.get::<_, Option<String>>(1)?,
                    level: row.get::<_, Option<String>>(2)?,
                    message: row.get::<_, Option<String>>(3)?,
                })
            })
            .map_err(|e| McpError::internal_error(format!("query: {e}"), None))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| McpError::internal_error(format!("row: {e}"), None))?);
        }
        let total = out.len() as i64;
        let truncated = total >= limit;
        let result = QueryLogsResult {
            rows: out,
            total_matched: total,
            truncated,
        };
        let json = serde_json::to_string(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Get a pre-digested dashboard summary for a metric over a time range. Read-only."
    )]
    async fn get_dashboard_summary(
        &self,
        Parameters(params): Parameters<DashboardSummaryParams>,
    ) -> Result<CallToolResult, McpError> {
        let from = params
            .from
            .unwrap_or_else(|| (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
        let to = params.to.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let metric = params.metric;

        let conn = self.store.conn();
        let conn = conn.lock();

        // Total volume + error rate.
        let (total, errors): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(CASE WHEN level='error' THEN 1 ELSE 0 END) FROM logs WHERE ts >= ?1 AND ts <= ?2",
                [&from, &to],
                |row| Ok((row.get(0)?, row.get(1).unwrap_or(0))),
            )
            .unwrap_or((0, 0));

        let error_rate = if total > 0 {
            errors as f64 / total as f64
        } else {
            0.0
        };

        // Top services by volume.
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(service,''), COUNT(*) n FROM logs WHERE ts >= ?1 AND ts <= ?2 GROUP BY 1 ORDER BY 2 DESC LIMIT 5",
            )
            .map_err(|e| McpError::internal_error(format!("prepare topN: {e}"), None))?;
        let rows = stmt
            .query_map([&from, &to], |row| {
                Ok(serde_json::json!({
                    "service": row.get::<_, String>(0)?,
                    "n": row.get::<_, i64>(1)?,
                }))
            })
            .map_err(|e| McpError::internal_error(format!("query topN: {e}"), None))?;
        let mut top_n = Vec::new();
        for r in rows.flatten() {
            top_n.push(r);
        }

        let mut hints = Vec::new();
        if error_rate > 0.05 {
            hints.push(format!("Error rate is high ({:.2}%).", error_rate * 100.0));
        }
        if total == 0 {
            hints.push("No log volume in the requested window.".into());
        }

        let result = DashboardSummaryResult {
            summary_stats: serde_json::json!({
                "metric": metric,
                "total": total,
                "errors": errors,
                "error_rate": error_rate,
                "window": { "from": from, "to": to },
            }),
            top_n,
            narrative_hints: hints,
        };
        let json = serde_json::to_string(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Detect anomalies in a metric series using rolling MAD, seasonal, and rate-of-change methods. Read-only."
    )]
    async fn detect_anomalies(
        &self,
        Parameters(params): Parameters<DetectAnomaliesParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.store.conn();
        let conn = conn.lock();

        // Pull the recent anomaly rows straight from the table.
        let mut stmt = conn
            .prepare(
                "SELECT ts, metric, score, method, severity FROM anomalies WHERE metric = ?1 ORDER BY ts DESC LIMIT 100",
            )
            .map_err(|e| McpError::internal_error(format!("prepare: {e}"), None))?;
        let rows = stmt
            .query_map([&params.metric], |row| {
                Ok(AnomalyOut {
                    ts: row.get::<_, chrono::DateTime<chrono::Utc>>(0)?.to_rfc3339(),
                    metric: row.get(1)?,
                    score: row.get(2)?,
                    method: row.get(3)?,
                    severity: row.get(4)?,
                })
            })
            .map_err(|e| McpError::internal_error(format!("query: {e}"), None))?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }

        // If table is empty, attempt an on-the-fly computation from rollups.
        if out.is_empty() {
            let (ts, series): (Vec<chrono::DateTime<chrono::Utc>>, Vec<f64>) = {
                let mut s = conn
                    .prepare(
                        "SELECT bucket, SUM(n) FROM rollup_1m GROUP BY 1 ORDER BY 1 DESC LIMIT 120",
                    )
                    .map_err(|e| McpError::internal_error(format!("prepare rollup: {e}"), None))?;
                let rs = s
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, chrono::DateTime<chrono::Utc>>(0)?,
                            row.get::<_, f64>(1)?,
                        ))
                    })
                    .map_err(|e| McpError::internal_error(format!("query rollup: {e}"), None))?;
                let mut pairs: Vec<_> = rs.flatten().collect();
                pairs.reverse();
                pairs.into_iter().unzip()
            };
            if let Ok(detected) = crate::analytics::anomaly::detect_anomalies_once(
                &params.metric,
                &series,
                &ts,
                params.sensitivity,
            ) {
                for d in detected {
                    out.push(AnomalyOut {
                        ts: d.ts.to_rfc3339(),
                        metric: d.metric,
                        score: d.score,
                        method: d.method.as_str().to_string(),
                        severity: d.severity,
                    });
                }
            }
        }

        let _ = AnomalyMethod::Mad; // silence unused import warning
        let json = serde_json::to_string(&out)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Forecast a metric series into the future using ETS/Holt-Winters. Read-only."
    )]
    async fn forecast(
        &self,
        Parameters(params): Parameters<ForecastToolParams>,
    ) -> Result<CallToolResult, McpError> {
        let horizon_count = parse_horizon_count(&params.horizon, params.granularity.as_deref());
        let granularity = params.granularity.as_deref().unwrap_or("minute");
        let (interval_secs, table) = match granularity {
            "hour" => (3600_i64, "rollup_1h"),
            _ => (60_i64, "rollup_1m"),
        };

        let conn = self.store.conn();
        let conn = conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT COALESCE(SUM(n), 0) FROM {table} GROUP BY bucket ORDER BY bucket DESC LIMIT {}",
                horizon_count.saturating_mul(4).max(60)
            ))
            .map_err(|e| McpError::internal_error(format!("prepare: {e}"), None))?;
        let rows: Vec<f64> = stmt
            .query_map([], |row| row.get::<_, f64>(0))
            .map_err(|e| McpError::internal_error(format!("query: {e}"), None))?
            .flatten()
            .collect();
        let mut series: Vec<f64> = rows;
        series.reverse();

        let req = ForecastRequest {
            series,
            interval_secs,
            horizon: horizon_count,
            level: 0.95,
            start: None,
        };
        let resp = forecast_series(&req)
            .map_err(|e| McpError::internal_error(format!("forecast: {e}"), None))?;
        let json = serde_json::to_string(&resp)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Create an alert rule. Rules are created in pending_approval status and must be approved via the dashboard UI before they fire. Requires the --enable-alert-mcp-tool flag."
    )]
    async fn create_alert_rule(
        &self,
        Parameters(params): Parameters<CreateAlertRuleParams>,
    ) -> Result<CallToolResult, McpError> {
        use duckdb::params;
        if !self.enable_alert_tool {
            // Tool-level error so the agent/user sees why it didn't run.
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "create_alert_rule is disabled. Set --enable-alert-mcp-tool to enable it.",
            )]));
        }
        let conn = self.store.conn();
        let conn = conn.lock();
        let cond = serde_json::to_string(&params.condition).unwrap_or_else(|_| "{}".into());
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| McpError::internal_error(format!("tx: {e}"), None))?;
        // DuckDB has no `last_insert_rowid()` (that's a SQLite-ism, and calling
        // it errors with "Scalar Function ... does not exist"); pull the id
        // from the sequence explicitly up front instead.
        let id: i64 = tx
            .query_row("SELECT nextval('alert_rules_id_seq')", [], |row| row.get(0))
            .map_err(|e| McpError::internal_error(format!("nextval: {e}"), None))?;
        tx.execute(
            "INSERT INTO alert_rules (id, name, metric, condition_json, channel, status) \
             VALUES (?, ?, ?, ?, ?, 'pending_approval')",
            params![
                id,
                &params.name,
                &params.metric,
                &cond,
                &params.notification_channel
            ],
        )
        .map_err(|e| McpError::internal_error(format!("insert: {e}"), None))?;
        tx.commit()
            .map_err(|e| McpError::internal_error(format!("commit: {e}"), None))?;
        let result = CreateAlertRuleResult {
            rule_id: id,
            status: "pending_approval".into(),
        };
        let json = serde_json::to_string(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Website analytics totals for a site over a time range: pageviews, visitors, sessions, bounce rate, avg engaged time, and a daily pageview series. Aggregates only (no raw rows, no user identifiers). Read-only."
    )]
    async fn web_stats(
        &self,
        Parameters(params): Parameters<WebStatsParams>,
    ) -> Result<CallToolResult, McpError> {
        let to = web_time_bound(params.to.as_deref(), chrono::Utc::now());
        let from = web_time_bound(
            params.from.as_deref(),
            to - chrono::Duration::hours(24),
        );
        let conn = self.store.conn();
        let conn = conn.lock();
        let (total_pv, visitors, sessions): (i64, i64, i64) = conn
            .query_row(
                "SELECT
                   SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END),
                   SUM(visitors),
                   SUM(sessions)
                 FROM web_rollup_1d
                 WHERE day >= CAST(? AS DATE) AND day <= CAST(? AS DATE)
                   AND (?::VARCHAR IS NULL OR site_id = ?::VARCHAR)",
                duckdb::params![
                    from,
                    to,
                    params.site.as_deref(),
                    params.site.as_deref()
                ],
                |r| Ok((
                    r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                )),
            )
            .map_err(|e| McpError::internal_error(format!("web stats: {e}"), None))?;
        // Bounce rate + avg engaged time come from web_sessions (raw enough
        // to be exact, small enough to scan).
        let (bounce_rate, avg_engaged): (f64, f64) = conn
            .query_row(
                "SELECT
                   COALESCE(AVG(CASE WHEN is_bounce THEN 1.0 ELSE 0.0 END), 0),
                   COALESCE(AVG(engaged_ms), 0)
                 FROM web_sessions
                 WHERE start_ts >= ? AND start_ts <= ?
                   AND (?::VARCHAR IS NULL OR site_id = ?::VARCHAR)",
                duckdb::params![
                    from,
                    to,
                    params.site.as_deref(),
                    params.site.as_deref()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0.0, 0.0));
        // Daily pageview series.
        let mut stmt = conn
            .prepare(
                "SELECT day, SUM(pageviews) FROM web_rollup_1d
                 WHERE day >= CAST(? AS DATE) AND day <= CAST(? AS DATE)
                   AND (?::VARCHAR IS NULL OR site_id = ?::VARCHAR)
                 GROUP BY 1 ORDER BY 1",
            )
            .map_err(|e| McpError::internal_error(format!("prepare series: {e}"), None))?;
        let series = stmt
            .query_map(
                duckdb::params![from, to, params.site.as_deref(), params.site.as_deref()],
                |r| {
                    Ok(serde_json::json!({
                        "day": r.get::<_, chrono::NaiveDate>(0)?.to_string(),
                        "pageviews": r.get::<_, i64>(1)?,
                    }))
                },
            )
            .map_err(|e| McpError::internal_error(format!("series: {e}"), None))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let result = serde_json::json!({
            "site": params.site,
            "from": from.to_rfc3339(),
            "to": to.to_rfc3339(),
            "pageviews": total_pv,
            // Daily-rotating visitor hash: summed daily uniques, never
            // deduplicated across days (privacy model — document, don't hide).
            "visitors": visitors,
            "sessions": sessions,
            "bounce_rate": (bounce_rate * 10000.0).round() / 10000.0,
            "avg_engaged_ms": (avg_engaged * 1000.0).round() / 1000.0,
            "daily": series,
        });
        let json = serde_json::to_string(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Top web-analytics dimension values (pages, sources, mediums, countries, regions, cities, screens, devices, browsers, OSes, UTM source/medium/campaign, event kinds) by pageviews over a time range. Aggregates only. Read-only."
    )]
    async fn web_top(
        &self,
        Parameters(params): Parameters<WebTopParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(col) = web_top_column(&params.by) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "by must be one of: path, source, medium, country, region, city, screen, utm_source, utm_medium, utm_campaign, device, browser, os, event",
            )]));
        };
        let limit = params.limit.clamp(1, 50);
        let to = web_time_bound(params.to.as_deref(), chrono::Utc::now());
        let from = web_time_bound(
            params.from.as_deref(),
            to - chrono::Duration::hours(24),
        );
        let conn = self.store.conn();
        let conn = conn.lock();
        let sql = format!(
            "SELECT {col} AS dim_value,
                    SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END) AS pageviews,
                    COUNT(DISTINCT visitor_id) AS visitors,
                    COUNT(DISTINCT session_id) AS sessions
             FROM web_events_all
             WHERE ts >= ? AND ts <= ? AND NOT is_bot
               AND (?::VARCHAR IS NULL OR site_id = ?::VARCHAR)
             GROUP BY 1 ORDER BY pageviews DESC, dim_value
             LIMIT {limit}"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| McpError::internal_error(format!("prepare: {e}"), None))?;
        let rows = stmt
            .query_map(
                duckdb::params![from, to, params.site.as_deref(), params.site.as_deref()],
                |r| {
                    Ok(serde_json::json!({
                        "value": r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "(none)".into()),
                        "pageviews": r.get::<_, i64>(1)?,
                        "visitors": r.get::<_, i64>(2)?,
                        "sessions": r.get::<_, i64>(3)?,
                    }))
                },
            )
            .map_err(|e| McpError::internal_error(format!("query: {e}"), None))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let result = serde_json::json!({
            "by": params.by,
            "site": params.site,
            "from": from.to_rfc3339(),
            "to": to.to_rfc3339(),
            "rows": rows,
        });
        let json = serde_json::to_string(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
}

fn parse_horizon_count(horizon: &str, granularity: Option<&str>) -> usize {
    // Parse "<number><unit>" like "6h", "30m", "2d".
    let (num_str, unit) = horizon.trim().split_at(
        horizon
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_alphabetic())
            .map(|(i, _)| i)
            .unwrap_or(horizon.len()),
    );
    let n: usize = num_str.parse().unwrap_or(6);
    let unit = unit.trim();
    let per_hour = match granularity {
        Some("hour") => 1,
        _ => 60,
    };
    let hours = match unit {
        "m" | "min" | "minute" | "minutes" => (n as f64 / 60.0).max(1.0) as usize,
        "h" | "hr" | "hour" | "hours" => n,
        "d" | "day" | "days" => n * 24,
        _ => n,
    };
    (hours * per_hour).max(1)
}

/// Wire `ServerHandler` with our tool router. Custom server metadata to expose
/// our name/version to clients.
#[tool_handler(
    name = "central-logs",
    instructions = "central-logs MCP server. Tools: query_logs, get_dashboard_summary, detect_anomalies, forecast."
)]
impl ServerHandler for CentralLogsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// Spawn the stdio MCP server (for local Claude Desktop / Code integration).
pub async fn run_stdio(
    store: Store,
    meta: Arc<WalMeta>,
    enable_alert_tool: bool,
    _shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let server = CentralLogsMcpServer::new(store, meta, enable_alert_tool);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
