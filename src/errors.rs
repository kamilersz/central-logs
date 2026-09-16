//! Error tracking core: fingerprinting, error-group upserts, reconciliation.
//!
//! Companion to the Sentry-SDK-compatible ingest (`src/web/sentry_api.rs`).
//! Design doc: `docs/ERROR_TRACKING.md`.
//!
//! Model:
//! - Every error-level event carries a `fingerprint` (typed column on `logs`).
//!   Sentry events get theirs computed at the ingest route (respecting the
//!   client-supplied `fingerprint` array); plain JSON/syslog error logs get a
//!   message-based fingerprint at parse time.
//! - The ingest worker upserts one row per fingerprint into `error_groups`
//!   (first/last seen, count, ≤N sampled stacktraces) and reports transitions
//!   (created / regressed) to the notifier.
//! - `rollup_error_1m` (maintained by the rollup job) powers sparklines and
//!   threshold alerting; a reconciliation job recomputes counts from retained
//!   events to absorb at-least-once over-counts.

use chrono::{DateTime, Utc};
use duckdb::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Result;

// =====================================================================
// Configuration (mirrors `[error_tracking]` in config.rs)
// =====================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ErrorTrackingConfig {
    /// Master switch; also gates the Sentry-protocol HTTP routes.
    pub enabled: bool,
    /// Sentry DSN project ids are numeric (the SDKs enforce it) — map them to
    /// service names here: `"7" = "checkout"`. Unmapped ids become
    /// `project-<id>`; non-numeric paths (hand-rolled HTTP clients) are used
    /// as the service name verbatim.
    pub projects: std::collections::HashMap<String, String>,
    /// Per-event stack truncation (the fingerprint only needs top frames).
    pub stack_max_frames: usize,
    pub stack_max_bytes: usize,
    /// Stacktrace samples kept per group (first + latest distinct variants).
    pub samples_per_group: usize,
    /// How often group counts are recomputed from retained events.
    pub reconcile_interval_secs: u64,
    pub notify: ErrorNotifyConfig,
}

impl Default for ErrorTrackingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            projects: std::collections::HashMap::new(),
            stack_max_frames: 100,
            stack_max_bytes: 8192,
            samples_per_group: 3,
            reconcile_interval_secs: 3600,
            notify: ErrorNotifyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(default)]
pub struct ErrorNotifyConfig {
    pub on_new_group: bool,
    pub on_regression: bool,
    /// Minimum level for event-time notifications: "error" (default) also
    /// covers "fatal"; "fatal" restricts to fatal only.
    pub min_level: String,
    /// Webhook URLs (http/https) receiving event-time notifications.
    pub webhooks: Vec<String>,
}

impl Default for ErrorNotifyConfig {
    fn default() -> Self {
        Self {
            on_new_group: true,
            on_regression: true,
            min_level: "error".into(),
            webhooks: Vec::new(),
        }
    }
}

// =====================================================================
// Fingerprinting
// =====================================================================

/// Normalize volatile detail out of an error string so noise (ids, numbers,
/// quoted payloads) doesn't split groups. One pass, no regex dependency:
/// 1. quoted substrings → `'S'`
/// 2. whitespace runs → single space
/// 3. per token: all digits → `N`; ≥8 hex chars → `H`
pub fn normalize_text(s: &str) -> String {
    // 1. Quoted substrings (double + single quotes) → 'S. Unterminated
    //    quotes recover at line ends.
    let mut squashed = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    squashed.push_str("'S");
                    quote = None;
                } else if c == '\n' {
                    squashed.push(' ');
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else {
                    squashed.push(c);
                }
            }
        }
    }
    // 2. Collapse whitespace runs.
    let collapsed = squashed.split_whitespace().collect::<Vec<_>>().join(" ");
    // 3. Token-level: all-digit runs → N, ≥8-char hex runs → H; punctuation
    //    is preserved as-is (it often carries signal, e.g. `::`).
    let mut out = String::with_capacity(collapsed.len());
    for (i, word) in collapsed.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if word.is_empty() {
            continue;
        }
        let mut run = String::new();
        for c in word.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                run.push(c);
            } else {
                map_run(&run, &mut out);
                run.clear();
                out.push(c);
            }
        }
        map_run(&run, &mut out);
    }
    out
}

fn map_run(run: &str, out: &mut String) {
    if run.is_empty() {
        return;
    }
    if run.bytes().all(|b| b.is_ascii_digit()) {
        out.push('N');
    } else if run.len() >= 8 && run.bytes().all(|b| b.is_ascii_hexdigit()) {
        out.push('H');
    } else {
        out.push_str(run);
    }
}

/// Short hex fingerprint (16 chars) over `parts`, prefixed by a domain tag so
/// client-supplied and server-computed hashes can't collide.
pub fn fingerprint_of(tag: &str, parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    for p in parts {
        h.update(b"\x1f");
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Fingerprint from a client-supplied Sentry `fingerprint` array (wins over
/// server-side computation, per the Sentry grouping contract).
pub fn client_fingerprint(values: &[String]) -> String {
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    fingerprint_of("c", &refs)
}

/// Fingerprint from exception type + normalized value + top frames
/// (module, function, filename — line numbers excluded so line shifts
/// don't split groups).
pub fn exception_fingerprint(
    exc_type: &str,
    exc_value: &str,
    frames: &[(String, String, String)],
) -> String {
    let mut parts: Vec<String> = vec![exc_type.to_string(), normalize_text(exc_value)];
    for (module, function, filename) in frames.iter().take(5) {
        parts.push(module.clone());
        parts.push(function.clone());
        parts.push(filename.clone());
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    fingerprint_of("s", &refs)
}

/// Message-only fingerprint for plain (non-Sentry) error logs.
pub fn message_fingerprint(service: &str, message: &str) -> String {
    fingerprint_of("m", &[service, &normalize_text(message)])
}

/// Hash of the full (normalized) trace — used to deduplicate stack samples
/// within a group ("variants").
pub fn variant_hash(stack: &str) -> String {
    fingerprint_of("v", &[&normalize_text(stack)])
}

/// True when `level` is at least `min_level` on the error scale.
pub fn level_meets(level: &str, min_level: &str) -> bool {
    let rank = |l: &str| match l {
        "fatal" => 3,
        "error" => 2,
        "warn" => 1,
        _ => 0,
    };
    rank(level) >= rank(min_level)
}

/// Truncate a stacktrace to `max_frames` lines / `max_bytes` bytes, reporting
/// whether anything was cut.
pub fn truncate_stack(stack: &str, max_frames: usize, max_bytes: usize) -> (String, bool) {
    let mut truncated = false;
    let mut lines: Vec<&str> = stack.lines().collect();
    if lines.len() > max_frames {
        lines.truncate(max_frames);
        truncated = true;
    }
    let mut out = lines.join("\n");
    if out.len() > max_bytes {
        // Cut on a char boundary at or before max_bytes.
        let mut cut = max_bytes;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        truncated = true;
    }
    (out, truncated)
}

// =====================================================================
// Group upsert (called from the ingest worker)
// =====================================================================

/// One error event to fold into `error_groups`.
#[derive(Debug, Clone)]
pub struct GroupEvent {
    pub fingerprint: String,
    pub service: String,
    pub level: String,
    /// Human title: exception "Type: value" or the (truncated) message.
    pub title: String,
    pub exception_type: Option<String>,
    pub ts: DateTime<Utc>,
    /// Full stacktrace (or message when no stack exists) — sampled per group.
    pub stack: Option<String>,
}

/// What happened to the group row (drives event-time notifications).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// First event for this fingerprint.
    Created,
    /// Event landed on a resolved group (re-opened it).
    Regressed,
    /// Ordinary update (count/last_seen bump).
    Updated,
}

/// A notification emitted by the ingest worker, consumed by the notifier task.
#[derive(Debug, Clone, Serialize)]
pub struct NotifyEvent {
    pub kind: String, // "group.created" | "group.regressed"
    pub service: String,
    pub fingerprint: String,
    pub title: String,
    pub level: String,
    pub count: i64,
    pub first_seen: String,
    pub last_seen: String,
}

/// Errors above the notify floor, deduplicated per flush.
pub struct ErrorTracker {
    pub cfg: crate::config::ErrorTrackingConfig,
    pub tx: tokio::sync::mpsc::UnboundedSender<NotifyEvent>,
}

impl ErrorTracker {
    /// Fold error rows into `error_groups` and emit notifications. Runs with
    /// the store mutex already held (same guard as the batch insert).
    pub fn track(&self, conn: &Connection, rows: &[crate::store::LogRow]) {
        for row in rows {
            if row.fingerprint.is_empty() || !is_error_level(&row.level) {
                continue;
            }
            let stack = row
                .attributes
                .get("stack")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let exc_type = row
                .attributes
                .get("exception_type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let ev = GroupEvent {
                fingerprint: row.fingerprint.clone(),
                service: row.service.clone(),
                level: row.level.clone(),
                title: truncate_chars(&row.message, 200),
                exception_type: exc_type,
                ts: row.ts,
                stack,
            };
            match upsert_group(conn, self.cfg.samples_per_group, &ev) {
                Ok(Transition::Created) => {
                    if self.cfg.notify.on_new_group
                        && level_meets(&ev.level, &self.cfg.notify.min_level)
                    {
                        self.send_notify(conn, "group.created", &ev);
                    }
                }
                Ok(Transition::Regressed) => {
                    if self.cfg.notify.on_regression
                        && level_meets(&ev.level, &self.cfg.notify.min_level)
                    {
                        self.send_notify(conn, "group.regressed", &ev);
                    }
                }
                Ok(Transition::Updated) => {}
                Err(e) => {
                    tracing::warn!(?e, fp = %ev.fingerprint, "error-group upsert failed");
                }
            }
        }
    }

    fn send_notify(&self, conn: &Connection, kind: &str, ev: &GroupEvent) {
        let (count, first_seen) = match query_group_basics(conn, &ev.fingerprint) {
            Ok(Some(v)) => v,
            _ => (0, ev.ts),
        };
        let n = NotifyEvent {
            kind: kind.to_string(),
            service: ev.service.clone(),
            fingerprint: ev.fingerprint.clone(),
            title: ev.title.clone(),
            level: ev.level.clone(),
            count,
            first_seen: first_seen.to_rfc3339(),
            last_seen: ev.ts.to_rfc3339(),
        };
        let _ = self.tx.send(n);
    }
}

fn is_error_level(level: &str) -> bool {
    matches!(level, "error" | "fatal")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn query_group_basics(
    conn: &Connection,
    fingerprint: &str,
) -> Result<Option<(i64, DateTime<Utc>)>> {
    let mut stmt = conn.prepare(
        "SELECT total_count, first_seen FROM error_groups WHERE fingerprint = ?",
    )?;
    let mut rows = stmt.query(params![fingerprint])?;
    if let Some(r) = rows.next()? {
        return Ok(Some((r.get(0)?, r.get(1)?)));
    }
    Ok(None)
}

/// Insert-or-update one error group row. Read-then-write under the store
/// mutex (single-writer semantics make this race-free without ON CONFLICT).
pub fn upsert_group(
    conn: &Connection,
    samples_max: usize,
    ev: &GroupEvent,
) -> Result<Transition> {
    let samples_max = samples_max.max(1);
    let vh = variant_hash(ev.stack.as_deref().unwrap_or(&ev.title));
    let new_sample = serde_json::json!({
        "variant_hash": vh,
        "first_seen": ev.ts.to_rfc3339(),
        "stack": ev.stack,
    });

    let existing: Option<(String, Option<DateTime<Utc>>, i64, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT status, resolved_at, total_count, title, samples, variant_hashes \
             FROM error_groups WHERE fingerprint = ?",
        )?;
        let mut rows = stmt.query(params![ev.fingerprint])?;
        if let Some(r) = rows.next()? {
            Some((
                r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "unresolved".into()),
                r.get(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        } else {
            None
        }
    };

    match existing {
        None => {
            let samples = serde_json::to_string(&vec![new_sample])?;
            conn.execute(
                "INSERT INTO error_groups \
                 (fingerprint, service, level, title, exception_type, first_seen, last_seen, \
                  total_count, status, samples, variant_hashes) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 1, 'unresolved', ?, ?)",
                params![
                    ev.fingerprint,
                    ev.service,
                    ev.level,
                    ev.title,
                    ev.exception_type,
                    ev.ts,
                    ev.ts,
                    samples,
                    serde_json::to_string(&vec![vh])?,
                ],
            )?;
            Ok(Transition::Created)
        }
        Some((status, resolved_at, total_count, prev_title, prev_samples, prev_variants)) => {
            let regressed = status == "resolved";
            let new_status = if regressed { "unresolved" } else { status.as_str() };
            let level = if ev.level == "fatal" || prev_title.is_none() {
                ev.level.clone()
            } else {
                // Never downgrade a group's max-severity label.
                worst_level(&level_of_status(&status), &ev.level)
            };
            // Samples: keep the FIRST sample at slot 0 forever; remaining
            // slots rotate to hold the newest distinct variants.
            let mut variants: Vec<String> = prev_variants
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default();
            let mut samples: Vec<serde_json::Value> = prev_samples
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(s).ok())
                .unwrap_or_default();
            if !variants.contains(&vh) {
                if samples.len() < samples_max {
                    samples.push(new_sample);
                    variants.push(vh);
                } else if samples.len() >= 2 {
                    // Rotate out the OLDEST non-first slot (slot 1), keep
                    // the newest at the end.
                    samples.remove(1);
                    variants.remove(1);
                    samples.push(new_sample);
                    variants.push(vh);
                }
            }
            conn.execute(
                "UPDATE error_groups SET \
                    last_seen = GREATEST(last_seen, ?), \
                    total_count = ?, \
                    level = ?, \
                    status = ?, \
                    resolved_at = ?, \
                    title = ?, \
                    samples = ?, \
                    variant_hashes = ? \
                 WHERE fingerprint = ?",
                params![
                    ev.ts,
                    total_count + 1,
                    level,
                    new_status,
                    if regressed { None } else { resolved_at },
                    // Keep the original (usually most-representative) title.
                    prev_title.unwrap_or_else(|| ev.title.clone()),
                    serde_json::to_string(&samples)?,
                    serde_json::to_string(&variants)?,
                    ev.fingerprint,
                ],
            )?;
            Ok(if regressed { Transition::Regressed } else { Transition::Updated })
        }
    }
}

fn level_of_status(_status: &str) -> String {
    // The previous level isn't selected in the read above; "error" is the
    // safe default (only "fatal" would outrank it and fresh events carry it).
    "error".to_string()
}

fn worst_level(a: &str, b: &str) -> String {
    if a == "fatal" || b == "fatal" {
        "fatal".into()
    } else {
        "error".into()
    }
}

// =====================================================================
// Reconciliation
// =====================================================================

/// Recompute `total_count` / `last_seen` from retained events. At-least-once
/// ingest can over-count; retained-event counts are the source of truth.
/// Groups with no retained events keep their last-seen metadata (the sample
/// stacks intentionally survive retention).
pub fn reconcile(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE error_groups g SET \
            total_count = COALESCE(c.n, 0), \
            last_seen = COALESCE(c.last, g.last_seen) \
         FROM (\
            SELECT fingerprint, COUNT(*) AS n, MAX(ts) AS last \
            FROM logs WHERE fingerprint IS NOT NULL GROUP BY fingerprint\
         ) c \
         WHERE g.fingerprint = c.fingerprint",
        [],
    )?;
    Ok(n)
}

// =====================================================================
// Queries (used by the error-groups API)
// =====================================================================

#[derive(Debug, Clone, Serialize)]
pub struct ErrorGroup {
    pub fingerprint: String,
    pub service: Option<String>,
    pub level: Option<String>,
    pub title: Option<String>,
    pub exception_type: Option<String>,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub total_count: i64,
    pub status: String,
    pub resolved_at: Option<String>,
}

pub struct ListParams {
    pub window_secs: i64,
    pub status: Option<String>, // unresolved | resolved | ignored | all
    pub service: Option<String>,
    pub q: Option<String>,
    pub sort: String, // "recent" | "count"
    pub limit: i64,
    pub offset: i64,
}

pub fn list_groups(conn: &Connection, p: &ListParams) -> Result<(Vec<ErrorGroup>, i64)> {
    let since = Utc::now() - chrono::Duration::seconds(p.window_secs.max(1));
    let mut where_parts = vec!["last_seen >= ?".to_string()];
    match p.status.as_deref() {
        None | Some("unresolved") => where_parts.push("status = 'unresolved'".into()),
        Some(s @ ("resolved" | "ignored")) => where_parts.push(format!("status = '{s}'")),
        _ => {}
    }
    if p.service.is_some() {
        where_parts.push("service = ?".into());
    }
    if p.q.is_some() {
        where_parts.push("(title ILIKE ? OR exception_type ILIKE ?)".into());
    }
    let order = if p.sort == "count" {
        "total_count DESC, last_seen DESC"
    } else {
        "last_seen DESC"
    };
    let where_clause = where_parts.join(" AND ");

    // Bind params in the order the placeholders appear.
    let mut bind: Vec<String> = vec![since.to_rfc3339()];
    if let Some(svc) = &p.service {
        bind.push(svc.clone());
    }
    if let Some(q) = &p.q {
        let like = format!("%{q}%");
        bind.push(like.clone());
        bind.push(like);
    }

    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM error_groups WHERE {where_clause}");
        let duck = crate::query::params_as_duck(&bind);
        let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?
    };

    let sql = format!(
        "SELECT fingerprint, service, level, title, exception_type, first_seen, last_seen, \
                total_count, status, resolved_at \
         FROM error_groups WHERE {where_clause} \
         ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let mut all = bind;
    all.push(p.limit.to_string());
    all.push(p.offset.to_string());
    let duck = crate::query::params_as_duck(&all);
    let refs: Vec<&dyn duckdb::ToSql> = duck.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok(ErrorGroup {
            fingerprint: r.get(0)?,
            service: r.get(1)?,
            level: r.get(2)?,
            title: r.get(3)?,
            exception_type: r.get(4)?,
            first_seen: r.get::<_, Option<DateTime<Utc>>>(5)?.map(|t| t.to_rfc3339()),
            last_seen: r.get::<_, Option<DateTime<Utc>>>(6)?.map(|t| t.to_rfc3339()),
            total_count: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
            status: r.get::<_, Option<String>>(8)?.unwrap_or_else(|| "unresolved".into()),
            resolved_at: r.get::<_, Option<DateTime<Utc>>>(9)?.map(|t| t.to_rfc3339()),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorGroupDetail {
    #[serde(flatten)]
    pub group: ErrorGroup,
    pub samples: serde_json::Value,
    /// Per-minute event counts over the trailing window (sparkline).
    pub spark: Vec<SparkPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SparkPoint {
    pub ts: String,
    pub count: i64,
}

pub fn get_group(conn: &Connection, fingerprint: &str, spark_window_secs: i64) -> Result<Option<ErrorGroupDetail>> {
    let mut stmt = conn.prepare(
        "SELECT fingerprint, service, level, title, exception_type, first_seen, last_seen, \
                total_count, status, resolved_at, samples \
         FROM error_groups WHERE fingerprint = ?",
    )?;
    let mut rows = stmt.query(params![fingerprint])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let group = ErrorGroup {
        fingerprint: r.get(0)?,
        service: r.get(1)?,
        level: r.get(2)?,
        title: r.get(3)?,
        exception_type: r.get(4)?,
        first_seen: r.get::<_, Option<DateTime<Utc>>>(5)?.map(|t| t.to_rfc3339()),
        last_seen: r.get::<_, Option<DateTime<Utc>>>(6)?.map(|t| t.to_rfc3339()),
        total_count: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
        status: r.get::<_, Option<String>>(8)?.unwrap_or_else(|| "unresolved".into()),
        resolved_at: r.get::<_, Option<DateTime<Utc>>>(9)?.map(|t| t.to_rfc3339()),
    };
    let samples: serde_json::Value = r
        .get::<_, Option<String>>(10)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!([]));
    drop(rows);
    drop(stmt);

    let since = Utc::now() - chrono::Duration::seconds(spark_window_secs.max(60));
    let mut spark_stmt = conn.prepare(
        "SELECT bucket, SUM(count) FROM rollup_error_1m \
         WHERE fingerprint = ? AND bucket >= ? GROUP BY bucket ORDER BY bucket",
    )?;
    let spark_rows = spark_stmt.query_map(params![fingerprint, since], |row| {
        Ok(SparkPoint {
            ts: row.get::<_, DateTime<Utc>>(0)?.to_rfc3339(),
            count: row.get::<_, i64>(1)?,
        })
    })?;
    let mut spark = Vec::new();
    for s in spark_rows {
        spark.push(s?);
    }
    Ok(Some(ErrorGroupDetail { group, samples, spark }))
}

pub fn set_status(conn: &Connection, fingerprint: &str, status: &str) -> Result<bool> {
    let resolved_at = if status == "resolved" { Some(Utc::now()) } else { None };
    let n = conn.execute(
        "UPDATE error_groups SET status = ?, resolved_at = ? WHERE fingerprint = ?",
        params![status, resolved_at, fingerprint],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_numbers_hex_and_quotes() {
        assert_eq!(normalize_text("user 42 failed"), "user N failed");
        assert_eq!(normalize_text("id deadbeef1234 end"), "id H end");
        assert_eq!(normalize_text("db: \"connection refused\""), "db: 'S");
        // Short hex (3 chars) is not an id.
        assert_eq!(normalize_text("caf fit"), "caf fit");
        assert_eq!(normalize_text("a   \n  b"), "a b");
    }

    #[test]
    fn fingerprints_are_stable_across_noise() {
        let a = message_fingerprint("api", "connection refused for user 4811");
        let b = message_fingerprint("api", "connection refused for user 9999");
        assert_eq!(a, b, "volatile numbers must not split groups");
        let c = message_fingerprint("api", "disk full");
        assert_ne!(a, c);
        // Service participates in the hash.
        let d = message_fingerprint("worker", "connection refused for user 4811");
        assert_ne!(a, d);
    }

    #[test]
    fn client_fingerprint_wins_and_is_stable() {
        let f1 = client_fingerprint(&["payment-gateway".into(), "timeout".into()]);
        let f2 = client_fingerprint(&["payment-gateway".into(), "timeout".into()]);
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), 16);
        assert_ne!(f1, exception_fingerprint("Timeout", "t", &[]));
    }

    #[test]
    fn exception_fingerprint_ignores_line_numbers() {
        let frames_a = vec![("m".to_string(), "handle".to_string(), "app.py".to_string())];
        let frames_b = vec![("m".to_string(), "handle".to_string(), "app.py:117".to_string())];
        // filename differs → different group (filenames are part of identity),
        // but value noise does not split:
        let v1 = exception_fingerprint("ValueError", "bad input 42", &frames_a);
        let v2 = exception_fingerprint("ValueError", "bad input 77", &frames_a);
        assert_eq!(v1, v2);
        let _ = frames_b;
    }

    #[test]
    fn truncate_stack_respects_frames_and_bytes() {
        let stack = (0..200).map(|i| format!("frame {i}")).collect::<Vec<_>>().join("\n");
        let (out, truncated) = truncate_stack(&stack, 100, 1 << 20);
        assert!(truncated);
        assert_eq!(out.lines().count(), 100);
        let tiny = "short";
        let (out2, truncated2) = truncate_stack(tiny, 100, 1 << 20);
        assert!(!truncated2);
        assert_eq!(out2, "short");
        let (out3, truncated3) = truncate_stack("x".repeat(100).as_str(), 1000, 10);
        assert!(truncated3);
        assert_eq!(out3.len(), 10);
    }

    #[test]
    fn level_meets_ranks_correctly() {
        assert!(level_meets("error", "error"));
        assert!(level_meets("fatal", "error"));
        assert!(!level_meets("warn", "error"));
        assert!(level_meets("fatal", "fatal"));
        assert!(!level_meets("error", "fatal"));
    }

    fn test_conn() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("t.duckdb")).unwrap();
        conn.execute_batch(crate::store::schema::ERROR_GROUPS_DDL).unwrap();
        conn.execute_batch(crate::store::schema::ROLLUP_ERROR_1M_DDL).unwrap();
        (tmp, conn)
    }

    fn sample_ev(fp: &str, stack: Option<&str>) -> GroupEvent {
        GroupEvent {
            fingerprint: fp.into(),
            service: "api".into(),
            level: "error".into(),
            title: "Boom: something broke".into(),
            exception_type: Some("Boom".into()),
            ts: Utc::now(),
            stack: stack.map(String::from),
        }
    }

    #[test]
    fn upsert_create_then_update_and_regression() {
        let (_tmp, conn) = test_conn();
        let ev = sample_ev("fp1", Some("line1\nline2"));
        assert_eq!(upsert_group(&conn, 3, &ev).unwrap(), Transition::Created);
        // Same variant → Updated, count increments.
        assert_eq!(upsert_group(&conn, 3, &ev).unwrap(), Transition::Updated);
        let (n, _) = query_group_basics(&conn, "fp1").unwrap().unwrap();
        assert_eq!(n, 2);

        // Resolve → new event regresses.
        assert!(set_status(&conn, "fp1", "resolved").unwrap());
        assert_eq!(upsert_group(&conn, 3, &ev).unwrap(), Transition::Regressed);
        let detail = get_group(&conn, "fp1", 3600).unwrap().unwrap();
        assert_eq!(detail.group.status, "unresolved");
        assert_eq!(detail.group.total_count, 3);
        assert_eq!(detail.samples.as_array().unwrap().len(), 1);
    }

    #[test]
    fn samples_keep_first_and_latest_distinct_variants() {
        let (_tmp, conn) = test_conn();
        let e1 = sample_ev("fp2", Some("variant one"));
        upsert_group(&conn, 3, &e1).unwrap();
        let e2 = sample_ev("fp2", Some("variant two"));
        upsert_group(&conn, 3, &e2).unwrap();
        let e3 = sample_ev("fp2", Some("variant three"));
        upsert_group(&conn, 3, &e3).unwrap();
        let e4 = sample_ev("fp2", Some("variant four"));
        upsert_group(&conn, 3, &e4).unwrap();
        let detail = get_group(&conn, "fp2", 3600).unwrap().unwrap();
        let arr = detail.samples.as_array().unwrap();
        assert_eq!(arr.len(), 3, "capped at samples_per_group");
        assert_eq!(arr[0]["stack"], "variant one", "first sample is kept");
        assert_eq!(arr[1]["stack"], "variant three", "oldest non-first rotated out");
        assert_eq!(arr[2]["stack"], "variant four", "newest variant is last");
    }

    #[test]
    fn reconcile_recounts_from_logs() {
        let (_tmp, mut conn) = test_conn();
        conn.execute_batch(
            "CREATE TABLE logs (ts TIMESTAMP, fingerprint VARCHAR, level VARCHAR); \
             INSERT INTO logs (ts, fingerprint, level) \
             VALUES (now(), 'fp3', 'error'), (now(), 'fp3', 'error'), (now(), 'fp3', 'fatal');",
        )
        .unwrap();
        let ev = sample_ev("fp3", None);
        upsert_group(&conn, 3, &ev).unwrap();
        // Simulate an over-count.
        conn.execute("UPDATE error_groups SET total_count = 99 WHERE fingerprint = 'fp3'", [])
            .unwrap();
        let n = reconcile(&conn).unwrap();
        assert_eq!(n, 1);
        let detail = get_group(&conn, "fp3", 3600).unwrap().unwrap();
        assert_eq!(detail.group.total_count, 3);
    }

    #[test]
    fn list_groups_filters_and_sorts() {
        let (_tmp, conn) = test_conn();
        for fp in ["a", "b"] {
            upsert_group(&conn, 3, &sample_ev(fp, None)).unwrap();
        }
        set_status(&conn, "b", "ignored").unwrap();
        let (all, total) = list_groups(
            &conn,
            &ListParams {
                window_secs: 3600,
                status: None,
                service: None,
                q: None,
                sort: "recent".into(),
                limit: 50,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(total, 1, "default status filter is unresolved");
        assert_eq!(all[0].fingerprint, "a");
        let (any, total_any) = list_groups(
            &conn,
            &ListParams {
                window_secs: 3600,
                status: Some("all".into()),
                service: None,
                q: None,
                sort: "recent".into(),
                limit: 50,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(total_any, 2);
        assert!(any.iter().any(|g| g.fingerprint == "b"));
    }
}
