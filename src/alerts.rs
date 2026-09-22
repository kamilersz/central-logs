//! Alert rule evaluation + notification dispatch (architecture §7).
//!
//! Rules are created either via the MCP tool (landing in `pending_approval`)
//! or from the web UI (the creating human *is* the approver, so they start
//! `active`). This module is what makes an `active` rule *do* something: on
//! a fixed interval it re-evaluates every active rule's condition against
//! current data, and for each breach past its cooldown it delivers a
//! notification to every configured channel.
//!
//! **Condition types**
//! - `threshold` — a rollup metric (volume, error_rate, p50/p95/p99_latency)
//!   compared to a value over a trailing window.
//! - `anomaly` — a recent `anomalies` row at/above a severity.
//! - `count` — **n matching rows per period**: a filter-DSL expression
//!   (e.g. `service:api level:error`) evaluated against the hot `logs`
//!   table; fires when the count over the trailing window crosses the
//!   threshold.
//!
//! **Channels** (`alert_channels` table, managed from the web UI):
//! - `webhook`  — POST a JSON payload to any `http(s)` URL (Slack/Discord/
//!   Teams incoming webhooks, PagerDuty Events API, custom endpoints).
//! - `telegram` — bot token + chat id; message via the Bot API.
//! - `email`    — recipient list; delivered through the configured SMTP
//!   relay (`[smtp]` in config / `CENTRAL_LOGS_SMTP__*` env).
//!
//! Legacy rules created before channels existed keep a raw webhook URL in
//! `alert_rules.channel` and still work.
//!
//! Every evaluation is recorded: a breach writes one `alert_events` row per
//! channel attempt (`notified` distinguishes "fired but delivery failed"
//! from a clean delivery) and updates `alert_rules.last_evaluated_at` /
//! `last_fired_at` / `last_notify_error` so the dashboard can show rule
//! health without re-deriving it.

use duckdb::{params, Connection};
use serde::Deserialize;

use crate::query::{parse_filter, ColumnWhitelist};
use crate::store::Store;
use crate::Result;

/// A rule's condition, parsed from `alert_rules.condition_json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertCondition {
    /// Fire when the metric's value over the trailing `window_secs` compares
    /// to `value` per `comparator` (`">"`, `">="`, `"<"`, `"<="`, `"=="`/`"="`).
    Threshold {
        #[serde(default)]
        comparator: String,
        #[serde(default)]
        value: f64,
        #[serde(default = "default_window_secs")]
        window_secs: i64,
        #[serde(default = "default_cooldown_secs")]
        cooldown_secs: i64,
        /// Multiple escalation levels (array order = escalation order), e.g.
        /// [{severity:"warning", comparator:">=", value:50},
        ///  {severity:"critical", comparator:">=", value:200}]. When present,
        /// the single comparator/value pair is ignored.
        #[serde(default)]
        thresholds: Vec<ThresholdEntry>,
    },
    /// Fire when an `anomalies` row for this metric at or above
    /// `min_severity` ("low" | "medium" | "high" | "critical") landed within
    /// `window_secs`.
    Anomaly {
        #[serde(default = "default_min_severity")]
        min_severity: String,
        #[serde(default = "default_window_secs")]
        window_secs: i64,
        #[serde(default = "default_cooldown_secs")]
        cooldown_secs: i64,
    },
    /// Fire when at least `count` rows matching the filter-DSL expression
    /// landed within `window_secs` ("n matching events per period"). The
    /// filter is compiled with the same whitelist/parameterization as the
    /// query API — no SQL injection path.
    Count {
        filter: String,
        #[serde(default = "default_count_comparator")]
        comparator: String,
        #[serde(default)]
        count: f64,
        #[serde(default = "default_window_secs")]
        window_secs: i64,
        #[serde(default = "default_cooldown_secs")]
        cooldown_secs: i64,
        /// See Threshold.thresholds — entries carry `value` (the count).
        #[serde(default)]
        thresholds: Vec<ThresholdEntry>,
    },
    /// Fire when a specific error group (or any group when `fingerprint` is
    /// null) records `value`+ events within `window_secs` (error tracking,
    /// `docs/ERROR_TRACKING.md`).
    ErrorGroupThreshold {
        /// `None` = aggregate across all error groups.
        #[serde(default)]
        fingerprint: Option<String>,
        /// "error" (default; includes fatal) or "fatal".
        #[serde(default = "default_min_level")]
        min_level: String,
        #[serde(default = "default_count_comparator")]
        comparator: String,
        value: f64,
        #[serde(default = "default_window_secs")]
        window_secs: i64,
        #[serde(default = "default_cooldown_secs")]
        cooldown_secs: i64,
    },
}

fn default_min_level() -> String {
    "error".into()
}

/// One escalation level of a multi-threshold rule. Array order defines
/// escalation: the LAST breached entry wins. Severity is a free label
/// ("warning", "critical", …) that must CHANGE between evaluations for a
/// notification to go out — that is what makes the rule non-spammy: one
/// notification per state transition (OK → warning → critical → recovered),
/// never a repeat while the state stays the same.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ThresholdEntry {
    pub comparator: String,
    pub value: f64,
    /// Free-form label. Defaults to "alert" when omitted.
    #[serde(default = "default_entry_severity")]
    pub severity: String,
}

fn default_entry_severity() -> String {
    "alert".into()
}

fn default_window_secs() -> i64 {
    300
}

/// Minimum gap between two firings of the same rule, even while the breach
/// persists — otherwise a sustained threshold breach would re-notify every
/// evaluation cycle.
fn default_cooldown_secs() -> i64 {
    900
}

fn default_min_severity() -> String {
    "high".into()
}

fn default_count_comparator() -> String {
    ">=".into()
}

impl AlertCondition {
    fn window_secs(&self) -> i64 {
        match self {
            AlertCondition::Threshold { window_secs, .. }
            | AlertCondition::Anomaly { window_secs, .. }
            | AlertCondition::Count { window_secs, .. }
            | AlertCondition::ErrorGroupThreshold { window_secs, .. } => *window_secs,
        }
    }
    fn cooldown_secs(&self) -> i64 {
        match self {
            AlertCondition::Threshold { cooldown_secs, .. }
            | AlertCondition::Anomaly { cooldown_secs, .. }
            | AlertCondition::Count { cooldown_secs, .. }
            | AlertCondition::ErrorGroupThreshold { cooldown_secs, .. } => *cooldown_secs,
        }
    }
}

/// A state transition worth notifying: the rule's severity changed between
/// evaluations (including to/from "ok" = recovered). One notification per
/// transition — sustained breaches stay silent until the state changes.
#[derive(Debug, Clone)]
pub struct FiredAlert {
    pub rule_id: i64,
    pub name: String,
    pub metric: String,
    pub value: f64,
    pub message: String,
    /// New severity label ("warning", "critical", "ok" = recovered, …).
    pub severity: String,
    /// Legacy raw webhook URL (rules created before channels existed).
    pub channel: String,
    /// Web-managed channel ids (`alert_channels.id`) to deliver to.
    pub channel_ids: Vec<i64>,
}

enum EvalOutcome {
    Fired(FiredAlert),
    NotBreached { rule_id: i64 },
    Error { rule_id: i64, error: String },
}

struct ActiveRule {
    id: i64,
    name: String,
    metric: String,
    condition_json: String,
    channel: String,
    channels_json: Option<String>,
    last_fired_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Severity label from the previous evaluation (None = was OK / never
    /// evaluated). The transition from this to the current severity is what
    /// triggers a notification.
    last_severity: Option<String>,
}

/// Outcome of one full evaluation cycle, surfaced for logging/metrics.
#[derive(Debug, Clone, Default)]
pub struct AlertCycleStats {
    pub evaluated: usize,
    pub fired: usize,
    pub notified: usize,
}

/// Run one evaluation cycle: load active rules, evaluate each against
/// current data, notify + persist the ones that fired. Safe to call on any
/// interval; rules that aren't breached (or are still in cooldown) are cheap
/// no-ops beyond a `last_evaluated_at` touch. `smtp` enables email channels
/// (pass `None` when the SMTP relay isn't configured).
pub async fn run_alert_cycle(
    store: &Store,
    http: &reqwest::Client,
    smtp: Option<&crate::config::SmtpConfig>,
) -> Result<AlertCycleStats> {
    let whitelist = ColumnWhitelist::standard(store.hot_attributes());
    let (to_fire, not_breached, errored) = {
        let conn = store.lock();
        let rules = load_active_rules(&conn)?;
        let mut fire = Vec::new();
        let mut ok_ids = Vec::new();
        let mut errs = Vec::new();
        for rule in &rules {
            match evaluate_rule(&conn, rule, &whitelist) {
                EvalOutcome::Fired(f) => fire.push(f),
                EvalOutcome::NotBreached { rule_id } => ok_ids.push(rule_id),
                EvalOutcome::Error { rule_id, error } => errs.push((rule_id, error)),
            }
        }
        (fire, ok_ids, errs)
        // MutexGuard dropped here — nothing async holds the DB lock.
    };

    let stats = AlertCycleStats {
        evaluated: to_fire.len() + not_breached.len() + errored.len(),
        fired: to_fire.len(),
        notified: 0,
    };

    {
        let conn = store.lock();
        if !not_breached.is_empty() {
            if let Err(e) = touch_evaluated(&conn, &not_breached) {
                tracing::warn!(?e, "alert cycle: touch_evaluated failed");
            }
        }
        for (rule_id, err) in &errored {
            tracing::warn!(rule_id, error = %err, "alert rule evaluation failed");
            if let Err(e) = record_eval_error(&conn, *rule_id, err) {
                tracing::warn!(?e, rule_id, "alert cycle: record_eval_error failed");
            }
        }
    }

    let mut notified = 0usize;
    for alert in &to_fire {
        // Resolve the channel set: web-managed channel ids first; fall back
        // to the legacy raw-webhook column for pre-channels rules.
        let targets = {
            let conn = store.lock();
            load_channel_targets(&conn, &alert.channel_ids)
        };
        let deliveries: Vec<(String, bool, Option<String>)> = if targets.is_empty() {
            let (ok, err) = send_webhook(http, alert).await;
            vec![("webhook".to_string(), ok, err)]
        } else {
            let mut out = Vec::new();
            for (name, target) in &targets {
                let (ok, err) = deliver(http, smtp, target, alert).await;
                out.push((name.clone(), ok, err));
            }
            out
        };

        let conn = store.lock();
        let mut any_ok = false;
        for (channel_name, ok, err) in &deliveries {
            if *ok {
                any_ok = true;
                notified += 1;
            } else {
                tracing::warn!(rule_id = alert.rule_id, channel = %channel_name, error = ?err, "alert notification failed");
            }
            if let Err(e) = record_alert_event(&conn, alert, channel_name, *ok, err.as_deref()) {
                tracing::warn!(
                    ?e,
                    rule_id = alert.rule_id,
                    "alert cycle: record_alert_event failed"
                );
            }
        }
        let _ = any_ok; // per-channel rows already carry notified/error
    }

    if !to_fire.is_empty() {
        tracing::info!(fired = to_fire.len(), notified, "alert rules fired");
    }

    Ok(AlertCycleStats { notified, ..stats })
}

/// Background task: run [`run_alert_cycle`] on a fixed interval until shutdown.
pub fn spawn_alert_task(
    store: Store,
    http: reqwest::Client,
    smtp: Option<std::sync::Arc<crate::config::SmtpConfig>>,
    interval: std::time::Duration,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(e) = run_alert_cycle(&store, &http, smtp.as_deref()).await {
                        tracing::warn!(?e, "alert evaluation cycle failed");
                    }
                }
            }
        }
    })
}

fn load_active_rules(conn: &Connection) -> Result<Vec<ActiveRule>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, metric, condition_json, COALESCE(channel, ''), channels_json, \
         last_fired_at, last_severity FROM alert_rules WHERE status = 'active'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ActiveRule {
            id: r.get(0)?,
            name: r.get(1)?,
            metric: r.get(2)?,
            condition_json: r
                .get::<_, Option<String>>(3)?
                .unwrap_or_else(|| "{}".into()),
            channel: r.get(4)?,
            channels_json: r.get(5)?,
            last_fired_at: r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(6)?,
            last_severity: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn evaluate_rule(conn: &Connection, rule: &ActiveRule, whitelist: &ColumnWhitelist) -> EvalOutcome {
    let cond: AlertCondition = match serde_json::from_str(&rule.condition_json) {
        Ok(c) => c,
        Err(e) => {
            return EvalOutcome::Error {
                rule_id: rule.id,
                error: format!("bad condition_json: {e}"),
            }
        }
    };
    let window = cond.window_secs().max(1);
    let channel_ids: Vec<i64> = rule
        .channels_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let base = FiredAlert {
        rule_id: rule.id,
        name: rule.name.clone(),
        metric: rule.metric.clone(),
        value: 0.0,
        message: String::new(),
        severity: String::new(),
        channel: rule.channel.clone(),
        channel_ids,
    };

    // ── resolve the current observed value ──
    let observed: Result<Option<(f64, Vec<ThresholdEntry>)>> = match &cond {
        AlertCondition::Threshold {
            comparator,
            value,
            thresholds,
            ..
        } => match resolve_metric(conn, &rule.metric, window) {
            Ok(Some(v)) => {
                let entries = cond_entries(comparator, *value, thresholds);
                Ok(Some((v, entries)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        },
        AlertCondition::Count {
            filter,
            comparator,
            count,
            thresholds,
            ..
        } => match count_matching(conn, filter, window, whitelist) {
            Ok(v) => Ok(Some((v, cond_entries(comparator, *count, thresholds)))),
            Err(e) => Err(e),
        },
        AlertCondition::ErrorGroupThreshold {
            fingerprint,
            min_level,
            comparator,
            value,
            ..
        } => match error_group_count(conn, fingerprint.as_deref(), min_level, window) {
            Ok(v) => Ok(Some((
                v,
                vec![ThresholdEntry {
                    comparator: comparator.clone(),
                    value: *value,
                    severity: "alert".to_string(),
                }],
            ))),
            Err(e) => Err(e),
        },
        AlertCondition::Anomaly { .. } => {
            // Legacy path: anomaly conditions keep their cooldown-based
            // behavior (they don't have a continuous observed value).
            return match recent_anomaly_meets_severity(
                conn,
                &rule.metric,
                cond_anomaly_severity(&cond).as_str(),
                window,
            ) {
                Ok(Some((score, severity))) => {
                    if !cooldown_elapsed(rule.last_fired_at, cond.cooldown_secs()) {
                        return EvalOutcome::NotBreached { rule_id: rule.id };
                    }
                    EvalOutcome::Fired(FiredAlert {
                        value: score,
                        severity: severity.clone(),
                        message: format!(
                            "anomaly on '{}': severity {severity} (score {score:.2}) \
                             within the last {window}s",
                            rule.metric
                        ),
                        ..base
                    })
                }
                Ok(None) => EvalOutcome::NotBreached { rule_id: rule.id },
                Err(e) => EvalOutcome::Error {
                    rule_id: rule.id,
                    error: e.to_string(),
                },
            };
        }
    };

    let (observed, entries) = match observed {
        Ok(Some(v)) => v,
        Ok(None) => {
            return EvalOutcome::Error {
                rule_id: rule.id,
                error: format!(
                    "unsupported metric '{}' (expected volume, error_rate, \
                     p50_latency, p95_latency, or p99_latency)",
                    rule.metric
                ),
            }
        }
        Err(e) => {
            return EvalOutcome::Error {
                rule_id: rule.id,
                error: e.to_string(),
            }
        }
    };

    // ── severity state machine ──
    // current = the LAST entry in escalation order whose comparator breaches;
    // none breached → severity "ok" (recovered / still healthy).
    let mut current: Option<&ThresholdEntry> = None;
    for entry in &entries {
        match compare(observed, &entry.comparator, entry.value) {
            Ok(true) => current = Some(entry),
            Ok(false) => {}
            Err(e) => {
                return EvalOutcome::Error {
                    rule_id: rule.id,
                    error: e.to_string(),
                }
            }
        }
    }
    let current_sev = current.map(|e| e.severity.as_str()).unwrap_or("ok");
    let last_sev = rule.last_severity.as_deref().unwrap_or("ok");

    if current_sev == last_sev {
        // No state change — stay silent (this is the anti-spam guarantee).
        return EvalOutcome::NotBreached { rule_id: rule.id };
    }

    let subject = match &cond {
        AlertCondition::Count { filter, .. } => format!("rows matching '{filter}'"),
        AlertCondition::ErrorGroupThreshold { fingerprint, .. } => match fingerprint {
            Some(fp) => format!("error group {fp}"),
            None => "error-group events".to_string(),
        },
        _ => rule.metric.clone(),
    };
    let message = if current_sev == "ok" {
        format!(
            "RECOVERED: '{}' is back to normal (was '{}'); {} = {:.3} \
             over the last {window}s",
            rule.name, last_sev, subject, observed
        )
    } else {
        let entry = current.expect("severity != ok implies a breached entry");
        format!(
            "{} crossed '{}' threshold: {} {} {} over the last {window}s \
             (current: {:.3})",
            rule.name, current_sev, subject, entry.comparator, entry.value, observed
        )
    };

    EvalOutcome::Fired(FiredAlert {
        value: observed,
        severity: current_sev.to_string(),
        message,
        ..base
    })
}

/// The effective escalation entries: explicit `thresholds` array when
/// non-empty, else a single entry built from the legacy comparator/value pair
/// with the fixed label "alert".
fn cond_entries(
    comparator: &str,
    value: f64,
    thresholds: &[ThresholdEntry],
) -> Vec<ThresholdEntry> {
    if !thresholds.is_empty() {
        return thresholds.to_vec();
    }
    vec![ThresholdEntry {
        comparator: comparator.to_string(),
        value,
        severity: "alert".to_string(),
    }]
}

/// Extract the anomaly min_severity without moving the condition.
fn cond_anomaly_severity(cond: &AlertCondition) -> String {
    match cond {
        AlertCondition::Anomaly { min_severity, .. } => min_severity.clone(),
        _ => "high".to_string(),
    }
}

fn cooldown_elapsed(
    last_fired_at: Option<chrono::DateTime<chrono::Utc>>,
    cooldown_secs: i64,
) -> bool {
    match last_fired_at {
        None => true,
        Some(t) => (chrono::Utc::now() - t).num_seconds() >= cooldown_secs.max(0),
    }
}

/// Count error-tracked events for one group (or all groups when
/// `fingerprint` is None) within the trailing window. Bounded parameters —
/// no interpolation of user values (error tracking, docs/ERROR_TRACKING.md).
fn error_group_count(
    conn: &Connection,
    fingerprint: Option<&str>,
    min_level: &str,
    window_secs: i64,
) -> Result<f64> {
    let since = chrono::Utc::now() - chrono::Duration::seconds(window_secs);
    let until = chrono::Utc::now();
    let mut sql = String::from(
        "SELECT COUNT(*) FROM logs WHERE ts >= ? AND ts <= ? AND fingerprint IS NOT NULL",
    );
    if min_level == "fatal" {
        sql.push_str(" AND level = 'fatal'");
    } else {
        sql.push_str(" AND level IN ('error', 'fatal')");
    }
    if fingerprint.is_some() {
        sql.push_str(" AND fingerprint = ?");
    }
    let mut bind: Vec<String> = vec![since.to_rfc3339(), until.to_rfc3339()];
    if let Some(fp) = fingerprint {
        bind.push(fp.to_string());
    }
    let duck = crate::query::params_as_duck(&bind);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let n: i64 = conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?;
    Ok(n as f64)
}

/// Count rows matching a filter-DSL expression within the trailing window,
/// against the hot `logs` table (same whitelist + parameterized-SQL path as
/// the query API — no injection surface).
fn count_matching(
    conn: &Connection,
    filter: &str,
    window_secs: i64,
    whitelist: &ColumnWhitelist,
) -> Result<f64> {
    let compiled = parse_filter(filter).map_err(|e| crate::Error::invalid_input(e.to_string()))?;
    let (where_body, params) = compiled
        .to_sql(whitelist)
        .map_err(|e| crate::Error::invalid_input(e.to_string()))?;
    let since = chrono::Utc::now() - chrono::Duration::seconds(window_secs);
    let until = chrono::Utc::now();
    let where_clause = if where_body.is_empty() {
        "ts >= ? AND ts <= ?".to_string()
    } else {
        format!("({where_body}) AND ts >= ? AND ts <= ?")
    };
    let sql = format!("SELECT COUNT(*) FROM logs WHERE {where_clause}");
    let mut all = params;
    all.push(since.to_rfc3339());
    all.push(until.to_rfc3339());
    let duck = crate::query::params_as_duck(&all);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let n: i64 = conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?;
    Ok(n as f64)
}

// =====================================================================
// Notification channels
// =====================================================================

/// One resolved delivery target loaded from `alert_channels`.
#[derive(Debug, Clone)]
pub enum ChannelTarget {
    Webhook {
        url: String,
    },
    Telegram {
        bot_token: String,
        chat_id: String,
        api_base: String,
    },
    Email {
        recipients: Vec<String>,
    },
}

/// Load the named targets for `ids`. Unknown/deleted ids are skipped (with a
/// warning) rather than failing the whole delivery.
pub fn load_channel_targets(conn: &Connection, ids: &[i64]) -> Vec<(String, ChannelTarget)> {
    let mut out = Vec::new();
    for id in ids {
        let row = conn.query_row(
            "SELECT name, type, config_json FROM alert_channels WHERE id = ?",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        );
        let (name, ctype, cfg) = match row {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(channel_id = id, "alert channel not found; skipping");
                continue;
            }
        };
        let config: serde_json::Value =
            serde_json::from_str(&cfg).unwrap_or(serde_json::Value::Null);
        let target = match ctype.as_str() {
            "webhook" => ChannelTarget::Webhook {
                url: config["url"].as_str().unwrap_or_default().to_string(),
            },
            "telegram" => ChannelTarget::Telegram {
                bot_token: config["bot_token"].as_str().unwrap_or_default().to_string(),
                chat_id: config["chat_id"].as_str().unwrap_or_default().to_string(),
                api_base: config["api_base"]
                    .as_str()
                    .unwrap_or("https://api.telegram.org")
                    .to_string(),
            },
            "email" => ChannelTarget::Email {
                recipients: config["recipients"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            other => {
                tracing::warn!(channel_id = id, channel_type = %other, "unknown channel type; skipping");
                continue;
            }
        };
        out.push((name, target));
    }
    out
}

/// Deliver one alert to one target. Returns (ok, error).
pub async fn deliver(
    http: &reqwest::Client,
    smtp: Option<&crate::config::SmtpConfig>,
    target: &ChannelTarget,
    alert: &FiredAlert,
) -> (bool, Option<String>) {
    match target {
        ChannelTarget::Webhook { url } => {
            let legacy = FiredAlert {
                channel: url.clone(),
                ..alert.clone()
            };
            send_webhook(http, &legacy).await
        }
        ChannelTarget::Telegram {
            bot_token,
            chat_id,
            api_base,
        } => send_telegram(http, bot_token, chat_id, api_base, alert).await,
        ChannelTarget::Email { recipients } => send_email(smtp, recipients, alert).await,
    }
}

fn alert_text(alert: &FiredAlert) -> String {
    format!(
        "🚨 central-logs: {}\n{}\nrule #{} · {}",
        alert.name, alert.message, alert.rule_id, alert.metric
    )
}

async fn send_telegram(
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    api_base: &str,
    alert: &FiredAlert,
) -> (bool, Option<String>) {
    if bot_token.trim().is_empty() || chat_id.trim().is_empty() {
        return (
            false,
            Some("telegram channel missing bot_token or chat_id".into()),
        );
    }
    let bot_token = bot_token.trim();
    let url = format!(
        "{}/bot{}/sendMessage",
        api_base.trim_end_matches('/'),
        bot_token
    );
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": alert_text(alert),
        "disable_web_page_preview": true,
    });
    match http.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => (true, None),
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let hint = if text.contains("chat not found") {
                " (chat not found — check chat_id; for groups the bot must be a member)"
            } else if text.contains("Unauthorized") {
                " (unauthorized — check bot_token)"
            } else if status == reqwest::StatusCode::NOT_FOUND {
                " (not found — check bot_token/api_base; a bad token makes the URL path invalid)"
            } else {
                ""
            };
            (false, Some(format!("telegram HTTP {status}{hint}")))
        }
        Err(e) => (false, Some(format!("telegram request failed: {e}"))),
    }
}

async fn send_email(
    smtp: Option<&crate::config::SmtpConfig>,
    recipients: &[String],
    alert: &FiredAlert,
) -> (bool, Option<String>) {
    let Some(cfg) = smtp.cloned() else {
        return (
            false,
            Some("email channel but SMTP is not configured — set [smtp] host/from in the                   config (or CENTRAL_LOGS_SMTP__HOST / __FROM) and restart".into()),
        );
    };
    if !cfg.is_configured() {
        return (
            false,
            Some("email channel but SMTP is not configured — set [smtp] host and from,                   then restart".into()),
        );
    }
    if recipients.is_empty() {
        return (false, Some("email channel has no recipients".into()));
    }
    let recipients: Vec<String> = recipients.to_vec();
    let alert = alert.clone();
    // lettre's blocking transport, parked on a worker thread so the async
    // evaluator never blocks on SMTP I/O.
    let res = tokio::task::spawn_blocking(move || send_email_blocking(&cfg, &recipients, &alert))
        .await
        .unwrap_or_else(|e| (false, Some(format!("smtp task: {e}"))));
    res
}

fn send_email_blocking(
    cfg: &crate::config::SmtpConfig,
    recipients: &[String],
    alert: &FiredAlert,
) -> (bool, Option<String>) {
    use lettre::message::{header::ContentType, Mailbox, Message};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{SmtpTransport, Transport};

    let from = match cfg.from.parse::<Mailbox>() {
        Ok(m) => m,
        Err(e) => {
            return (
                false,
                Some(format!("invalid smtp from '{}': {e}", cfg.from)),
            )
        }
    };
    let mut builder = Message::builder()
        .from(from)
        .subject(format!("[central-logs] {}", alert.name));
    for rcpt in recipients {
        match rcpt.trim().parse::<Mailbox>() {
            Ok(m) => builder = builder.to(m),
            Err(e) => {
                return (false, Some(format!("invalid recipient '{rcpt}': {e}")));
            }
        }
    }
    let email = match builder
        .header(ContentType::TEXT_PLAIN)
        .body(alert_text(alert))
    {
        Ok(e) => e,
        Err(e) => return (false, Some(format!("build email: {e}"))),
    };

    let mailer = if cfg.starttls {
        SmtpTransport::starttls_relay(&cfg.host)
    } else {
        SmtpTransport::relay(&cfg.host)
    };
    let mut mailer = match mailer {
        Ok(m) => m.port(cfg.port),
        Err(e) => return (false, Some(format!("smtp relay '{}': {e}", cfg.host))),
    };
    if !cfg.username.is_empty() {
        mailer = mailer.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()));
    }

    match mailer.build().send(&email) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(format!("smtp send: {e}"))),
    }
}

pub(crate) fn compare(value: f64, comparator: &str, threshold: f64) -> Result<bool> {
    Ok(match comparator {
        ">" => value > threshold,
        ">=" => value >= threshold,
        "<" => value < threshold,
        "<=" => value <= threshold,
        "==" | "=" => (value - threshold).abs() < 1e-9,
        other => {
            return Err(crate::Error::invalid_input(format!(
                "unknown comparator '{other}' (expected >, >=, <, <=, or ==)"
            )))
        }
    })
}

/// Resolve a metric name to its current value over the trailing `window_secs`,
/// reading the same `rollup_1m` table the dashboards and MCP tools use.
/// `Ok(None)` means the metric name isn't recognized.
fn resolve_metric(conn: &Connection, metric: &str, window_secs: i64) -> Result<Option<f64>> {
    let since = chrono::Utc::now() - chrono::Duration::seconds(window_secs);
    match metric {
        "volume" => {
            let v: i64 = conn.query_row(
                "SELECT COALESCE(SUM(n), 0) FROM rollup_1m WHERE bucket >= ?",
                [since],
                |r| r.get(0),
            )?;
            Ok(Some(v as f64))
        }
        "error_rate" => {
            let (errs, total): (i64, i64) = conn.query_row(
                "SELECT COALESCE(SUM(CASE WHEN level = 'error' THEN n ELSE 0 END), 0), \
                 COALESCE(SUM(n), 0) FROM rollup_1m WHERE bucket >= ?",
                [since],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(Some(if total > 0 {
                errs as f64 / total as f64
            } else {
                0.0
            }))
        }
        "p50_latency" | "p95_latency" | "p99_latency" => {
            let col = match metric {
                "p50_latency" => "p50",
                "p95_latency" => "p95",
                _ => "p99",
            };
            // Weighted average of the per-bucket approx-quantiles (weighted
            // by row count) — an approximation of the window's percentile,
            // consistent with how rollup_1m already trades exactness for
            // cheap pre-aggregation (architecture §4).
            let sql = format!(
                "SELECT COALESCE(SUM({col} * n) / NULLIF(SUM(n), 0), 0) \
                 FROM rollup_1m WHERE bucket >= ? AND n > 0"
            );
            let v: f64 = conn.query_row(&sql, [since], |r| r.get(0))?;
            Ok(Some(v))
        }
        _ => Ok(None),
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 3,
        "high" => 2,
        "medium" => 1,
        _ => 0, // "low" or unrecognized
    }
}

/// Most severe matching anomaly for `metric` within `window_secs`, if any
/// meets or exceeds `min_severity`.
fn recent_anomaly_meets_severity(
    conn: &Connection,
    metric: &str,
    min_severity: &str,
    window_secs: i64,
) -> Result<Option<(f64, String)>> {
    let since = chrono::Utc::now() - chrono::Duration::seconds(window_secs);
    let min_rank = severity_rank(min_severity);
    let mut stmt = conn.prepare(
        "SELECT score, severity FROM anomalies WHERE metric = ? AND ts >= ? \
         ORDER BY ts DESC LIMIT 20",
    )?;
    let rows = stmt.query_map(params![metric, since], |r| {
        Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (score, sev) = row?;
        if severity_rank(&sev) >= min_rank {
            return Ok(Some((score, sev)));
        }
    }
    Ok(None)
}

/// POST the firing payload to `alert.channel`. Only `http(s)://` URLs are
/// deliverable in this version (see module docs) — anything else is reported
/// as an error rather than attempted.
pub(crate) async fn send_webhook(
    http: &reqwest::Client,
    alert: &FiredAlert,
) -> (bool, Option<String>) {
    let channel = alert.channel.trim();
    if channel.is_empty() {
        return (false, Some("no notification channel configured".into()));
    }
    if !(channel.starts_with("http://") || channel.starts_with("https://")) {
        return (
            false,
            Some(format!(
                "unsupported channel '{channel}': only http(s) webhook URLs are delivered \
                 (paste a Slack/Discord/Teams incoming-webhook URL, or any endpoint that \
                 accepts a JSON POST)"
            )),
        );
    }
    let body = serde_json::json!({
        "rule_id": alert.rule_id,
        "rule_name": alert.name,
        "metric": alert.metric,
        "value": alert.value,
        "message": alert.message,
        "fired_at": chrono::Utc::now().to_rfc3339(),
        "source": "central-logs",
    });
    match http.post(channel).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => (true, None),
        Ok(resp) => (false, Some(format!("webhook HTTP {}", resp.status()))),
        Err(e) => (false, Some(format!("webhook request failed: {e}"))),
    }
}

fn record_alert_event(
    conn: &Connection,
    alert: &FiredAlert,
    channel: &str,
    notified: bool,
    error: Option<&str>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO alert_events (id, rule_id, metric, value, message, notified, error, channel, severity) \
         VALUES (nextval('alert_events_id_seq'), ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            alert.rule_id,
            &alert.metric,
            alert.value,
            &alert.message,
            notified,
            error,
            channel,
            alert.severity
        ],
    )?;
    // Persist the new severity state. "ok" stores NULL so the next breach
    // from healthy is again a transition.
    let stored_sev: Option<&str> = if alert.severity == "ok" {
        None
    } else {
        Some(&alert.severity)
    };
    tx.execute(
        "UPDATE alert_rules SET last_evaluated_at = CURRENT_TIMESTAMP, \
         last_fired_at = CURRENT_TIMESTAMP, last_notify_error = ?, last_severity = ? \
         WHERE id = ?",
        params![error, stored_sev, alert.rule_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_eval_error(conn: &Connection, rule_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE alert_rules SET last_evaluated_at = CURRENT_TIMESTAMP, last_notify_error = ? \
         WHERE id = ?",
        params![error, rule_id],
    )?;
    Ok(())
}

fn touch_evaluated(conn: &Connection, rule_ids: &[i64]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE alert_rules SET last_evaluated_at = CURRENT_TIMESTAMP, last_notify_error = NULL \
             WHERE id = ?",
        )?;
        for id in rule_ids {
            stmt.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::apply_schema;
    use tempfile::tempdir;

    fn test_conn() -> (tempfile::TempDir, Connection) {
        let tmp = tempdir().unwrap();
        let parquet_dir = tmp.path().join("parquet");
        std::fs::create_dir_all(&parquet_dir).unwrap();
        let conn = Connection::open(tmp.path().join("t.duckdb")).unwrap();
        apply_schema(&conn, &parquet_dir, &[]).unwrap();
        (tmp, conn)
    }

    fn insert_rule(
        conn: &Connection,
        metric: &str,
        condition: serde_json::Value,
        channel: &str,
    ) -> i64 {
        // DuckDB has no `last_insert_rowid()`; pull the id from the sequence
        // explicitly, same fix as `mcp::tools::create_alert_rule`.
        let id: i64 = conn
            .query_row("SELECT nextval('alert_rules_id_seq')", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO alert_rules (id, name, metric, condition_json, channel, status) \
             VALUES (?, 'r', ?, ?, ?, 'active')",
            params![id, metric, condition.to_string(), channel],
        )
        .unwrap();
        id
    }

    fn insert_rollup_point(conn: &Connection, n: i64, level: &str) {
        conn.execute(
            "INSERT INTO rollup_1m (bucket, service, level, n, p50, p95, p99, bytes) \
             VALUES (CURRENT_TIMESTAMP, 'svc', ?, ?, 10.0, 20.0, 30.0, 100)",
            params![level, n],
        )
        .unwrap();
    }

    #[test]
    fn threshold_fires_when_breached() {
        let (_tmp, conn) = test_conn();
        insert_rollup_point(&conn, 500, "info");
        let id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 100.0}),
            "https://hooks.example.com/x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert_eq!(rule.id, id);
        match evaluate_rule(&conn, rule, &whitelist) {
            EvalOutcome::Fired(f) => {
                assert_eq!(f.rule_id, id);
                assert_eq!(f.value, 500.0);
            }
            other => panic!(
                "expected Fired, got {other:?}",
                other = debug_outcome(&other)
            ),
        }
    }

    #[test]
    fn threshold_not_breached_below_value() {
        let (_tmp, conn) = test_conn();
        insert_rollup_point(&conn, 10, "info");
        let _id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 100.0}),
            "https://hooks.example.com/x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert!(matches!(
            evaluate_rule(&conn, rule, &whitelist),
            EvalOutcome::NotBreached { .. }
        ));
    }

    #[test]
    fn no_repeat_notification_while_severity_unchanged() {
        let (_tmp, conn) = test_conn();
        insert_rollup_point(&conn, 500, "info");
        let id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 100.0}),
            "https://hooks.example.com/x",
        );
        // Rule already in "alert" state — a sustained breach stays silent.
        conn.execute(
            "UPDATE alert_rules SET last_severity = 'alert' WHERE id = ?",
            params![id],
        )
        .unwrap();
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert!(matches!(
            evaluate_rule(&conn, rule, &whitelist),
            EvalOutcome::NotBreached { .. }
        ));
    }

    #[test]
    fn recovery_notifies_once_then_silence() {
        let (_tmp, conn) = test_conn();
        let id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 100.0}),
            "https://hooks.example.com/x",
        );
        conn.execute(
            "UPDATE alert_rules SET last_severity = 'alert' WHERE id = ?",
            params![id],
        )
        .unwrap();
        // No rollup rows → value 0 → below threshold → recovery transition.
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        match evaluate_rule(&conn, rule, &whitelist) {
            EvalOutcome::Fired(f) => {
                assert_eq!(f.severity, "ok");
                assert!(f.message.contains("RECOVERED"));
            }
            other => panic!(
                "expected Fired(recovery), got {other:?}",
                other = debug_outcome(&other)
            ),
        }
        let _ = id;
    }

    #[test]
    fn multi_threshold_escalation_and_recovery() {
        let (_tmp, conn) = test_conn();
        let _id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 0.0,
            "thresholds": [
                {"severity": "warning", "comparator": ">=", "value": 50.0},
                {"severity": "critical", "comparator": ">=", "value": 200.0}
            ]}),
            "https://hooks.example.com/x",
        );
        let whitelist = ColumnWhitelist::standard(&[]);

        let eval = |rollup_n: i64| -> String {
            conn.execute("DELETE FROM rollup_1m", []).unwrap();
            insert_rollup_point(&conn, rollup_n, "info");
            let rule = &load_active_rules(&conn).unwrap()[0];
            match evaluate_rule(&conn, rule, &whitelist) {
                EvalOutcome::Fired(f) => {
                    // simulate the cycle persisting the new state
                    conn.execute(
                        "UPDATE alert_rules SET last_severity = ? WHERE id = ?",
                        params![f.severity, f.rule_id],
                    )
                    .unwrap();
                    f.severity
                }
                EvalOutcome::NotBreached { .. } => "silent".into(),
                other => panic!("unexpected {other:?}", other = debug_outcome(&other)),
            }
        };

        // 60 → warning (first transition from healthy).
        assert_eq!(eval(60), "warning");
        // Same state → silent.
        assert_eq!(eval(60), "silent");
        // Escalate → critical fires.
        assert_eq!(eval(250), "critical");
        // Same state → silent.
        assert_eq!(eval(300), "silent");
        // De-escalate but still breached → warning fires (state change).
        assert_eq!(eval(60), "warning");
        // Recovered → ok fires.
        assert_eq!(eval(1), "ok");
        // Still healthy → silent.
        assert_eq!(eval(1), "silent");
        // Breach again → warning fires.
        assert_eq!(eval(80), "warning");
    }

    #[test]
    fn error_rate_computed_from_rollup() {
        let (_tmp, conn) = test_conn();
        insert_rollup_point(&conn, 90, "info");
        insert_rollup_point(&conn, 10, "error");
        let value = resolve_metric(&conn, "error_rate", 3600).unwrap().unwrap();
        assert!((value - 0.1).abs() < 1e-9, "got {value}");
    }

    #[test]
    fn unsupported_metric_is_an_error_not_a_silent_skip() {
        let (_tmp, conn) = test_conn();
        let _id = insert_rule(
            &conn,
            "not_a_real_metric",
            serde_json::json!({"type": "threshold", "comparator": ">", "value": 1.0}),
            "https://hooks.example.com/x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        match evaluate_rule(&conn, rule, &whitelist) {
            EvalOutcome::Error { error, .. } => assert!(error.contains("unsupported metric")),
            other => panic!(
                "expected Error, got {other:?}",
                other = debug_outcome(&other)
            ),
        }
    }

    #[test]
    fn anomaly_condition_fires_on_matching_severity() {
        let (_tmp, conn) = test_conn();
        conn.execute(
            "INSERT INTO anomalies (ts, metric, score, method, severity) \
             VALUES (CURRENT_TIMESTAMP, 'volume', 5.0, 'mad', 'critical')",
            [],
        )
        .unwrap();
        let _id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "anomaly", "min_severity": "high"}),
            "https://hooks.example.com/x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert!(matches!(
            evaluate_rule(&conn, rule, &whitelist),
            EvalOutcome::Fired(_)
        ));
    }

    #[test]
    fn anomaly_condition_ignores_low_severity() {
        let (_tmp, conn) = test_conn();
        conn.execute(
            "INSERT INTO anomalies (ts, metric, score, method, severity) \
             VALUES (CURRENT_TIMESTAMP, 'volume', 3.1, 'mad', 'low')",
            [],
        )
        .unwrap();
        let _id = insert_rule(
            &conn,
            "volume",
            serde_json::json!({"type": "anomaly", "min_severity": "high"}),
            "https://hooks.example.com/x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert!(matches!(
            evaluate_rule(&conn, rule, &whitelist),
            EvalOutcome::NotBreached { .. }
        ));
    }

    #[test]
    fn bad_condition_json_is_reported_not_panicked() {
        let (_tmp, conn) = test_conn();
        let _id = insert_rule(
            &conn,
            "volume",
            serde_json::json!("not an object"),
            "https://x",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        assert!(matches!(
            evaluate_rule(&conn, rule, &whitelist),
            EvalOutcome::Error { .. }
        ));
    }

    #[tokio::test]
    async fn count_condition_fires_on_matching_rows() {
        let (tmp, _conn) = test_conn();
        // Count evaluates the hot `logs` table — stand up a full Store in the
        // tempdir so the filter path runs against real schema.
        let parquet = tmp.path().join("parquet");
        let store =
            crate::store::Store::open(&tmp.path().join("t2.duckdb"), parquet, vec![]).unwrap();
        {
            let conn = store.lock();
            for _ in 0..5 {
                conn.execute(
                    "INSERT INTO logs (service, level, message, ts) \
                     VALUES ('api', 'error', 'DB connection refused', CURRENT_TIMESTAMP)",
                    [],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO logs (service, level, message, ts) \
                 VALUES ('api', 'info', 'ok', CURRENT_TIMESTAMP)",
                [],
            )
            .unwrap();
        }
        let whitelist = ColumnWhitelist::standard(store.hot_attributes());
        let id: i64 = {
            let conn = store.lock();
            insert_rule(
                &conn,
                "events_matching_filter",
                serde_json::json!({"type":"count","filter":"service:api level:error","count":3.0}),
                "",
            )
        };
        let fired = {
            let conn = store.lock();
            let rule = load_active_rules(&conn).unwrap().remove(0);
            assert_eq!(rule.id, id);
            match evaluate_rule(&conn, &rule, &whitelist) {
                EvalOutcome::Fired(f) => f,
                other => panic!(
                    "expected Fired, got {other:?}",
                    other = debug_outcome(&other)
                ),
            }
        };
        assert_eq!(fired.value, 5.0);
        assert_eq!(fired.severity, "alert");
        assert!(fired
            .message
            .contains("rows matching 'service:api level:error'"));
    }

    #[test]
    fn count_condition_rejects_bad_filter_as_eval_error() {
        let (_tmp, conn) = test_conn();
        let _id = insert_rule(
            &conn,
            "events_matching_filter",
            serde_json::json!({"type":"count","filter":"totally_bogus_column:1","count":1.0}),
            "",
        );
        let rule = &load_active_rules(&conn).unwrap()[0];
        let whitelist = ColumnWhitelist::standard(&[]);
        match evaluate_rule(&conn, rule, &whitelist) {
            EvalOutcome::Error { error, .. } => {
                assert!(error.contains("invalid filter") || error.contains("unknown"))
            }
            other => panic!(
                "expected Error, got {other:?}",
                other = debug_outcome(&other)
            ),
        }
    }

    #[test]
    fn comparator_rejects_unknown_operator() {
        assert!(compare(1.0, ">", 0.0).unwrap());
        assert!(compare(1.0, "bogus", 0.0).is_err());
    }

    #[tokio::test]
    async fn webhook_channel_validation_rejects_non_http() {
        let http = reqwest::Client::new();
        let alert = FiredAlert {
            rule_id: 1,
            name: "r".into(),
            metric: "volume".into(),
            value: 1.0,
            message: "m".into(),
            severity: "alert".into(),
            channel: "slack:#ops".into(),
            channel_ids: vec![],
        };
        let (ok, err) = send_webhook(&http, &alert).await;
        assert!(!ok);
        assert!(err.unwrap().contains("only http(s)"));
    }

    // Debug helper since EvalOutcome intentionally has no public Debug impl.
    fn debug_outcome(o: &EvalOutcome) -> &'static str {
        match o {
            EvalOutcome::Fired(_) => "Fired",
            EvalOutcome::NotBreached { .. } => "NotBreached",
            EvalOutcome::Error { .. } => "Error",
        }
    }
}
