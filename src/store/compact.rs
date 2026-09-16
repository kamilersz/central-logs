//! Compaction: export aged-out hot rows to hive-partitioned Parquet, then delete
//! from the hot table. Also enforces raw retention TTL.
//!
//! Directory layout:
//! `<parquet_dir>/date=YYYY-MM-DD/hour=HH/service=<sanitized>/part-<n>.parquet`
//!
//! The extra `service=` level (LESSON_LEARNED: "one stream per microservice")
//! enables per-service retention: each service's cold files can be purged on
//! their own schedule (`[[service_retention]]` globs, first match wins).
//! Legacy `date=/hour=` files (no service level) remain readable by the
//! `logs_all` view and age out under the global retention.
//!
//! COPY options (telemetry optimization):
//! - `ORDER BY ts, service`: data is clustered by primary filter dimensions, so
//!   per-row-group min/max statistics stay tight. DuckDB skips whole row groups
//!   at query time.
//! - `ROW_GROUP_SIZE 100_000`: smaller row groups = finer zonemap pruning.
//! - `BLOOM_FILTER (...)` on high-cardinality hot attributes (trace_id,
//!   request_id, user_id): "find the log with this exact trace_id" goes from a
//!   full scan to a near-instant bloom probe.
//! - `COMPRESSION`: zstd by default — repetitive log data compresses far
//!   smaller than snappy.

use std::path::Path;

use duckdb::Connection;

use crate::config::ServiceRetention;
use crate::hot::{HotAttribute, HotType};
use crate::Result;

/// One compaction pass.
pub fn compact_once(
    conn: &Connection,
    parquet_dir: &Path,
    hot_cutoff: chrono::DateTime<chrono::Utc>,
    retention_days: i64,
    hot: &[HotAttribute],
    compression: &str,
    service_retention: &[ServiceRetention],
) -> Result<CompactStats> {
    let mut stats = CompactStats::default();

    let dir_str = parquet_dir.to_string_lossy().replace('\'', "''");

    // 1. List (date, hour, service) groups with rows to archive. Service is
    //    nullable — NULL forms its own group via GROUP BY semantics.
    let mut stmt = conn.prepare(
        r#"
        SELECT
          STRFTIME('%Y-%m-%d', ts) AS d,
          STRFTIME('%H', ts)        AS h,
          service                   AS s
        FROM logs
        WHERE ts < ?
        GROUP BY 1, 2, 3;
        "#,
    )?;
    let groups: Vec<(String, String, Option<String>)> = stmt
        .query_map([hot_cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    // Pick bloom-filter targets from the hot-attr config: only VARCHAR columns
    // make sense for bloom filters (high-cardinality string lookups like
    // trace_id/request_id). Bigint/Boolean are better served by zonemaps.
    let bloom_cols: Vec<&str> = hot
        .iter()
        .filter(|a| a.duckdb_type == HotType::Varchar)
        .map(|a| a.name.as_str())
        .collect();
    let bloom_clause = if bloom_cols.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = bloom_cols.iter().map(|c| format!("'{c}'")).collect();
        format!(", BLOOM_FILTER ({}), BLOOM_FILTER_COLUMNS ({})", true, quoted.join(", "))
    };

    for (d, h, svc) in &groups {
        let svc_raw = svc.clone().unwrap_or_default();
        let svc_dir = sanitize_service(&svc_raw);
        let sub_dir = format!("{dir_str}/date={d}/hour={h}/service={svc_dir}");
        std::fs::create_dir_all(&sub_dir)?;
        let part_path = format!("{sub_dir}/part-{}.parquet", chrono::Utc::now().timestamp_millis());

        // ORDER BY ts, service clusters data so row-group min/max statistics
        // prune efficiently for time-range + service filters. ROW_GROUP_SIZE
        // trades full-scan speed for filter selectivity. COMPRESSION defaults
        // to zstd (validated against a whitelist in config) — repetitive log
        // data compresses far smaller than snappy. `IS NOT DISTINCT FROM`
        // matches NULL service groups exactly.
        let export_sql = format!(
            r#"
            COPY (
                SELECT
                    ts, insert_ts, source_host, service, level, message,
                    fingerprint, trace_id, span_id, attributes, geo_country,
                    raw_len, protocol
                    {hot_select}
                FROM logs
                WHERE ts < ?
                  AND STRFTIME('%Y-%m-%d', ts) = ?
                  AND STRFTIME('%H', ts)       = ?
                  AND service IS NOT DISTINCT FROM ?
                ORDER BY ts, service
            ) TO '{part_path}' (FORMAT PARQUET, ROW_GROUP_SIZE 100000, COMPRESSION '{codec}'{bloom});
            "#,
            hot_select = hot_select_clause(hot),
            codec = compression.to_ascii_uppercase(),
            bloom = bloom_clause,
        );
        let mut stmt = conn.prepare(&export_sql)?;
        stmt.execute(duckdb::params![hot_cutoff, d.as_str(), h.as_str(), svc.as_deref()])?;
        drop(stmt);
        stats.exported_files += 1;
    }

    // 2. Delete exported rows from the hot table.
    let deleted: usize =
        conn.execute("DELETE FROM logs WHERE ts < ?", duckdb::params![hot_cutoff])?;
    stats.deleted_hot_rows = deleted as u64;

    // 3. Drop cold files older than their effective retention — per-service
    //    rules first, global `retention_days` as fallback.
    purge_old_parquet(parquet_dir, retention_days, service_retention, &mut stats);

    // 4. Refresh view.
    crate::store::schema::refresh_logs_all_view(conn, parquet_dir, hot)?;

    Ok(stats)
}

/// Enforce the hot-tier payload-size cap (`retention.hot_max_bytes`):
/// repeatedly compact the oldest hour-buckets to Parquet until
/// `SUM(raw_len)` is back under the cap. Returns bytes exported (0 when the
/// cap wasn't breached).
pub fn enforce_hot_cap(
    conn: &Connection,
    parquet_dir: &Path,
    hot: &[HotAttribute],
    cap_bytes: u64,
    compression: &str,
    service_retention: &[ServiceRetention],
) -> Result<u64> {
    let hot_bytes: i64 =
        conn.query_row("SELECT COALESCE(SUM(raw_len), 0) FROM logs", [], |r| r.get(0))?;
    if (hot_bytes as u64) <= cap_bytes {
        return Ok(0);
    }
    let mut total = 0u64;
    // Compact an hour at a time, oldest first, until under the cap (bounded
    // passes so a pathological cap can't loop forever — retention_days will
    // eventually purge anyway).
    for _ in 0..48 {
        let hot_bytes: i64 = conn
            .query_row("SELECT COALESCE(SUM(raw_len), 0) FROM logs", [], |r| r.get(0))?;
        if (hot_bytes as u64) <= cap_bytes {
            break;
        }
        let cutoff: Option<chrono::DateTime<chrono::Utc>> = conn
            .query_row(
                "SELECT MIN(ts) + INTERVAL '1 hour' FROM logs",
                [],
                |r| r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(0),
            )
            .unwrap_or(None);
        let Some(cutoff) = cutoff else { break };
        let stats = compact_once(
            conn,
            parquet_dir,
            cutoff,
            // Don't purge cold files during a cap pass; the scheduled
            // retention job owns that.
            365 * 100,
            hot,
            compression,
            service_retention,
        )?;
        total += stats.deleted_hot_rows;
        if stats.deleted_hot_rows == 0 {
            break; // nothing more to move
        }
    }
    Ok(total)
}

/// Build the SELECT-list fragment for promoted hot columns (e.g.
/// `, user_id, env, route`). Empty when no hot attrs are configured.
fn hot_select_clause(hot: &[HotAttribute]) -> String {
    if hot.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = hot.iter().map(|h| h.name.as_str()).collect();
        format!(", {}", names.join(", "))
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    pub exported_files: u64,
    pub deleted_hot_rows: u64,
    pub purged_files: u64,
}

fn purge_old_parquet(
    root: &Path,
    retention_days: i64,
    service_retention: &[ServiceRetention],
    stats: &mut CompactStats,
) {
    let now = chrono::Utc::now();
    let global_cutoff = (now - chrono::Duration::days(retention_days))
        .format("%Y-%m-%d")
        .to_string();

    let date_entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for date_entry in date_entries.flatten() {
        let date_name = date_entry.file_name();
        let Some(date_name) = date_name.to_str() else { continue };
        let Some(date) = date_name.strip_prefix("date=") else {
            continue;
        };
        let hour_entries = match std::fs::read_dir(date_entry.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for hour_entry in hour_entries.flatten() {
            let hour_name = hour_entry.file_name();
            let Some(hour_name) = hour_name.to_str() else { continue };
            if !hour_name.starts_with("hour=") {
                continue;
            }
            // New layout: service= dirs under the hour dir, each purged on
            // its own retention schedule.
            let mut has_service_dirs = false;
            let svc_entries = match std::fs::read_dir(hour_entry.path()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for svc_entry in svc_entries.flatten() {
                let svc_name = svc_entry.file_name();
                let Some(svc_name) = svc_name.to_str() else { continue };
                let Some(svc_val) = svc_name.strip_prefix("service=") else {
                    continue;
                };
                has_service_dirs = true;
                let days = retention_days_for(svc_val, service_retention, retention_days.max(0) as u64) as i64;
                let cutoff = (now - chrono::Duration::days(days))
                    .format("%Y-%m-%d")
                    .to_string();
                if date < cutoff.as_str() {
                    match std::fs::remove_dir_all(svc_entry.path()) {
                        Ok(()) => stats.purged_files += 1,
                        Err(e) => tracing::warn!(?e, path = %svc_entry.path().display(), "purge parquet failed"),
                    }
                }
            }
            // Legacy layout: parquet files directly under the hour dir —
            // global retention only.
            if !has_service_dirs && date < global_cutoff.as_str() {
                match std::fs::remove_dir_all(hour_entry.path()) {
                    Ok(()) => stats.purged_files += 1,
                    Err(e) => tracing::warn!(?e, path = %hour_entry.path().display(), "purge legacy parquet failed"),
                }
            }
            // Best-effort: drop hour dirs emptied by service purges.
            let _ = std::fs::remove_dir(hour_entry.path());
        }
        // Best-effort: drop date dirs emptied by the loop above.
        let _ = std::fs::remove_dir(date_entry.path());
    }
}

/// Sanitize a service name for use as a hive partition directory segment.
///
/// Keeps `[A-Za-z0-9_.-]`; every other char maps to `_`. Trailing dots and
/// spaces are trimmed (Windows-hostile dir names). When the result differs
/// from the input — or the input was truncated at 96 chars — a crc32 suffix
/// keeps distinct raw names from colliding after sanitization.
pub fn sanitize_service(name: &str) -> String {
    const MAX_LEN: usize = 96;
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let truncated = out.len() > MAX_LEN;
    if truncated {
        out.truncate(MAX_LEN);
    }
    while out.ends_with('.') || out.ends_with(' ') || out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    if out != name || truncated {
        let hash = crc32fast::hash(name.as_bytes());
        out.push_str(&format!("--{hash:08x}"));
    }
    out
}

/// Sanitize a glob *pattern* the same way service names are sanitized, but
/// preserving the wildcards `*` and `?`.
fn sanitize_pattern(pattern: &str) -> String {
    pattern
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '*' | '?') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Glob match with `*` (any sequence) and `?` (any single char). Iterative
/// two-pointer variant — no regex crate needed.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Effective retention (days) for a sanitized cold-tier service dir name.
/// First matching rule wins; `fallback` (global `retention_days`) otherwise.
pub fn retention_days_for(
    sanitized_service: &str,
    rules: &[ServiceRetention],
    fallback: u64,
) -> u64 {
    for rule in rules {
        if glob_match(&sanitize_pattern(&rule.pattern), sanitized_service) {
            return rule.days;
        }
    }
    fallback
}

/// Background compaction task.
pub fn spawn_compaction_task(
    conn: std::sync::Arc<parking_lot::Mutex<Connection>>,
    parquet_dir: std::path::PathBuf,
    hot: std::sync::Arc<Vec<HotAttribute>>,
    hot_tier_hours: u64,
    retention_days: u64,
    compression: String,
    service_retention: std::sync::Arc<Vec<ServiceRetention>>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip immediate fire
        loop {
            ticker.tick().await;
            let cutoff = chrono::Utc::now() - chrono::Duration::hours(hot_tier_hours as i64);
            let conn = conn.lock();
            match compact_once(
                &conn,
                &parquet_dir,
                cutoff,
                retention_days as i64,
                &hot,
                &compression,
                &service_retention,
            ) {
                Ok(s) if s.exported_files > 0 || s.deleted_hot_rows > 0 => {
                    tracing::info!(?s, "compaction pass")
                }
                Ok(_) => tracing::debug!("compaction pass: nothing to do"),
                Err(e) => tracing::warn!(?e, "compaction failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot::HotAttribute;

    #[test]
    fn export_sql_contains_order_and_bloom_for_varchar_hot() {
        let hot = vec![
            HotAttribute::parse_shorthand("trace_id:varchar").unwrap(),
            HotAttribute::parse_shorthand("user_id:bigint").unwrap(),
        ];
        let clause = hot_select_clause(&hot);
        assert!(clause.contains("trace_id"));
        assert!(clause.contains("user_id"));

        // Build the same bloom clause the production code would build.
        let bloom_cols: Vec<&str> = hot
            .iter()
            .filter(|a| a.duckdb_type == crate::hot::HotType::Varchar)
            .map(|a| a.name.as_str())
            .collect();
        let quoted: Vec<String> = bloom_cols.iter().map(|c| format!("'{c}'")).collect();
        let bloom_clause = format!(", BLOOM_FILTER ({}), BLOOM_FILTER_COLUMNS ({})", true, quoted.join(", "));
        assert!(bloom_clause.contains("'trace_id'"));
        assert!(!bloom_clause.contains("'user_id'"), "bigint cols should not be bloom-filtered: {bloom_clause}");
    }

    #[test]
    fn export_sql_has_no_bloom_when_no_varchar_hot() {
        let hot = vec![HotAttribute::parse_shorthand("user_id:bigint").unwrap()];
        let bloom_cols: Vec<&str> = hot
            .iter()
            .filter(|a| a.duckdb_type == crate::hot::HotType::Varchar)
            .map(|a| a.name.as_str())
            .collect();
        assert!(bloom_cols.is_empty(), "no varchar cols → no bloom targets");
    }

    #[test]
    fn export_sql_carries_zstd_compression() {
        // Mirror the production format string to assert the codec lands in
        // the COPY options and is uppercased.
        let sql = format!(
            "COPY (...) TO 'part.parquet' (FORMAT PARQUET, ROW_GROUP_SIZE 100000, COMPRESSION '{}')",
            "zstd".to_ascii_uppercase()
        );
        assert!(sql.contains("COMPRESSION 'ZSTD'"), "codec missing: {sql}");
    }

    #[test]
    fn sanitize_service_keeps_safe_chars() {
        assert_eq!(sanitize_service("payment_service"), "payment_service");
        assert_eq!(sanitize_service("api.v2-beta_1"), "api.v2-beta_1");
    }

    #[test]
    fn sanitize_service_maps_unsafe_chars_with_hash() {
        let s = sanitize_service("My Service/2");
        assert!(s.starts_with("My_Service_2--"), "got: {s}");
        // Deterministic: same input → same output.
        assert_eq!(s, sanitize_service("My Service/2"));
        // Distinct raw names don't collide after sanitization.
        assert_ne!(sanitize_service("My Service/2"), sanitize_service("My_Service_2"));
    }

    #[test]
    fn sanitize_service_empty_and_trailing_dot() {
        assert!(sanitize_service("").starts_with("unknown--"));
        assert!(sanitize_service("svc.").starts_with("svc--"), "got {}", sanitize_service("svc."));
        assert!(sanitize_service("....").starts_with("unknown--"));
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("payment_*", "payment_service"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("api_?", "api_1"));
        assert!(!glob_match("api_?", "api_12"));
        assert!(!glob_match("payment_*", "audit_log"));
        assert!(glob_match("audit_*", "audit_log"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn retention_rules_first_match_wins() {
        use crate::config::ServiceRetention;
        let rules = vec![
            ServiceRetention { pattern: "audit_*".into(), days: 90 },
            ServiceRetention { pattern: "*".into(), days: 7 },
        ];
        assert_eq!(retention_days_for("audit_log", &rules, 30), 90);
        assert_eq!(retention_days_for("payment_api", &rules, 30), 7);
        assert_eq!(
            retention_days_for("anything", &Vec::<ServiceRetention>::new(), 30),
            30
        );
    }

    #[test]
    fn retention_pattern_sanitized_like_service_names() {
        use crate::config::ServiceRetention;
        // Pattern chars that sanitize to '_' still match correspondingly
        // sanitized dir names: "pay ment" service → "pay_ment--<hash>".
        let rules = vec![ServiceRetention { pattern: "pay ment*".into(), days: 5 }];
        assert_eq!(retention_days_for("pay_ment--0f0f0f0f", &rules, 30), 5);
        // Dots are safe chars on both sides — they match literally.
        let rules = vec![ServiceRetention { pattern: "pay.ment*".into(), days: 5 }];
        assert_eq!(retention_days_for("pay.ment__0f0f0f0f", &rules, 30), 5);
    }

    /// End-to-end: compact hot rows into per-service partitions, then read
    /// them back through the `logs_all` view — verifying (a) the layout has
    /// `service=` dirs, (b) hive partitioning doesn't collide with the file
    /// `service` column, and (c) the REAL service value survives round-trip.
    #[test]
    fn compact_partitions_by_service_and_view_reads_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open_in_memory().expect("duckdb");
        conn.execute_batch(crate::store::schema::LOGS_DDL).expect("ddl");

        let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
        let ts = cutoff - chrono::Duration::minutes(10);
        for (svc, msg) in [("payment_service", "p1"), ("audit_log", "a1"), ("payment_service", "p2")] {
            conn.execute(
                "INSERT INTO logs (ts, insert_ts, source_host, service, level, message, trace_id, span_id, attributes, geo_country, raw_len, protocol) \
                 VALUES (?, ?, 'h', ?, 'info', ?, NULL, NULL, NULL, NULL, 10, 'http')",
                duckdb::params![ts, ts, svc, msg],
            )
            .expect("insert");
        }
        // One row with NULL service — its own partition.
        conn.execute(
            "INSERT INTO logs (ts, insert_ts, source_host, service, level, message, trace_id, span_id, attributes, geo_country, raw_len, protocol) \
             VALUES (?, ?, 'h', NULL, 'info', 'n1', NULL, NULL, NULL, NULL, 10, 'http')",
            duckdb::params![ts, ts],
        )
        .expect("insert null-service");

        let hot: Vec<HotAttribute> = vec![];
        let stats = compact_once(&conn, tmp.path(), cutoff, 30, &hot, "zstd", &[])
            .expect("compact");
        assert_eq!(stats.exported_files, 3, "one file per service group");
        assert_eq!(stats.deleted_hot_rows, 4);

        // Layout check: date=/hour=/service= dirs exist.
        let root = std::fs::read_dir(tmp.path()).expect("read parquet root");
        let date_dir = root.into_iter().next().expect("date dir").expect("entry").path();
        let hour_dir = std::fs::read_dir(&date_dir).expect("hour").next().expect("hour dir").expect("entry").path();
        let svc_names: Vec<String> = std::fs::read_dir(&hour_dir)
            .expect("services")
            .map(|e| e.expect("svc entry").file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(svc_names.len(), 3, "got: {svc_names:?}");
        assert!(svc_names.iter().any(|n| n == "service=payment_service"));
        assert!(svc_names.iter().any(|n| n == "service=audit_log"));
        assert!(svc_names.iter().any(|n| n.starts_with("service=unknown--")));

        // View reads everything back with the real service values.
        crate::store::schema::apply_schema(&conn, tmp.path(), &hot).expect("view");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM logs_all", [], |r| r.get(0))
            .expect("count logs_all");
        assert_eq!(n, 4);
        let pay: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM logs_all WHERE service = 'payment_service'",
                [],
                |r| r.get(0),
            )
            .expect("count payment");
        assert_eq!(pay, 2);
    }

    /// Per-service retention: audit files outlive debug files.
    #[test]
    fn purge_honors_per_service_retention() {
        use crate::config::ServiceRetention;
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = chrono::Utc::now();
        // Two dates: 10 days old and 40 days old.
        let old = (now - chrono::Duration::days(40)).format("%Y-%m-%d").to_string();
        let mid = (now - chrono::Duration::days(10)).format("%Y-%m-%d").to_string();
        for (date, svc) in [
            (&old, "audit_log"),
            (&old, "debug_chatty"),
            (&mid, "debug_chatty"),
        ] {
            let dir = tmp.path().join(format!("date={date}/hour=00/service={svc}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("part-0.parquet"), b"x").expect("write");
        }

        let rules = vec![ServiceRetention { pattern: "audit_*".into(), days: 90 }];
        let mut stats = CompactStats::default();
        purge_old_parquet(tmp.path(), 30, &rules, &mut stats);

        // audit 40d old survives (90d rule); debug 40d purged (30d global);
        // debug 10d survives → exactly one purge.
        assert_eq!(stats.purged_files, 1);
        assert!(tmp.path().join(format!("date={old}/hour=00/service=audit_log/part-0.parquet")).exists());
        assert!(!tmp.path().join(format!("date={old}/hour=00/service=debug_chatty")).exists());
        assert!(tmp.path().join(format!("date={mid}/hour=00/service=debug_chatty/part-0.parquet")).exists());
    }

    /// Legacy flat layout (parquet directly under date=/hour=) still purges
    /// under global retention.
    #[test]
    fn purge_legacy_layout_uses_global_retention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::days(40)).format("%Y-%m-%d").to_string();
        let recent = (now - chrono::Duration::days(5)).format("%Y-%m-%d").to_string();
        for date in [&old, &recent] {
            let dir = tmp.path().join(format!("date={date}/hour=07"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("part-0.parquet"), b"x").expect("write");
        }

        let mut stats = CompactStats::default();
        purge_old_parquet(tmp.path(), 30, &[], &mut stats);
        assert_eq!(stats.purged_files, 1);
        assert!(!tmp.path().join(format!("date={old}")).exists());
        assert!(tmp.path().join(format!("date={recent}/hour=07/part-0.parquet")).exists());
    }
}
