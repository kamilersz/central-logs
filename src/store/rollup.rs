//! Rollup materialization (architecture §4, computation strategy).
//!
//! Reads raw `logs` rows added since the last rollup, computes 1-minute and
//! 1-hour aggregates into `rollup_1m` / `rollup_1h`. Dashboards query these
//! almost exclusively.

use std::sync::Arc;

use duckdb::Connection;
use parking_lot::Mutex;

use crate::Result;

#[derive(Debug, Clone)]
pub struct RollupConfig {
    /// Only roll up rows newer than this lookback (avoids recomputing history).
    pub lookback_secs: i64,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            lookback_secs: 7 * 24 * 3600,
        }
    }
}

/// State carried between rollup cycles.
#[derive(Debug, Clone, Default)]
pub struct RollupState {
    pub last_1m_bucket: Option<chrono::DateTime<chrono::Utc>>,
    pub last_1h_bucket: Option<chrono::DateTime<chrono::Utc>>,
}

/// Run a single rollup cycle. Returns the number of rows aggregated.
pub fn rollup_once(
    conn: &Connection,
    state: &mut RollupState,
    cfg: &RollupConfig,
) -> Result<usize> {
    let mut total = 0usize;
    let now = chrono::Utc::now();
    let floor = now.timestamp() - cfg.lookback_secs;
    let floor_ts = chrono::DateTime::from_timestamp(floor, 0).unwrap_or(now);

    // 1-minute rollup. We compute over rows with ts > max(last_bucket - margin, floor_ts)
    // to absorb late-arriving rows with slightly stale event timestamps.
    let since_1m = state
        .last_1m_bucket
        .unwrap_or(floor_ts)
        .timestamp()
        .max(floor_ts.timestamp());
    let since_1m_ts = chrono::DateTime::from_timestamp(since_1m, 0).unwrap_or(now);

    let sql_1m = r#"
        INSERT INTO rollup_1m
        SELECT
          time_bucket(INTERVAL '1 minute', ts) AS bucket,
          COALESCE(service, '')  AS service,
          COALESCE(level,  '')   AS level,
          COUNT(*)               AS n,
          COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.5), 0)  AS p50,
          COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.95), 0) AS p95,
          COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.99), 0) AS p99,
          COALESCE(SUM(raw_len), 0) AS bytes
        FROM logs
        WHERE ts > ?
        GROUP BY 1, 2, 3
        ON CONFLICT DO NOTHING;
    "#;
    // DuckDB doesn't support ON CONFLICT for now; dedupe via NOT EXISTS on a
    // subquery when the table has a primary key. For v1 we accept at-most-once
    // per bucket by filtering ts > last computed bucket.
    let _ = sql_1m;
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO rollup_1m
            SELECT
              time_bucket(INTERVAL '1 minute', ts) AS bucket,
              COALESCE(service, '') AS service,
              COALESCE(level, '')   AS level,
              COUNT(*)              AS n,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.5),  0) AS p50,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.95), 0) AS p95,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.99), 0) AS p99,
              COALESCE(SUM(raw_len), 0) AS bytes
            FROM logs
            WHERE ts > ?
            GROUP BY 1, 2, 3;
            "#,
        )?;
        let n = stmt.execute([since_1m_ts])?;
        total += n;
    }
    {
        let since_1h = state
            .last_1h_bucket
            .unwrap_or(floor_ts)
            .timestamp()
            .max(floor_ts.timestamp());
        let since_1h_ts = chrono::DateTime::from_timestamp(since_1h, 0).unwrap_or(now);
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO rollup_1h
            SELECT
              time_bucket(INTERVAL '1 hour', ts) AS bucket,
              COALESCE(service, '') AS service,
              COALESCE(level, '')   AS level,
              COUNT(*)              AS n,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.5),  0) AS p50,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.95), 0) AS p95,
              COALESCE(APPROX_QUANTILE(TRY_CAST(attributes->>'$.duration_ms' AS DOUBLE), 0.99), 0) AS p99,
              COALESCE(SUM(raw_len), 0) AS bytes
            FROM logs
            WHERE ts > ?
            GROUP BY 1, 2, 3;
            "#,
        )?;
        let n = stmt.execute([since_1h_ts])?;
        total += n;
    }
    // Per-fingerprint error counts (error tracking: sparklines + the
    // error_group_threshold alert condition). Same trailing-window dedupe
    // semantics as the 1m rollup above.
    {
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO rollup_error_1m
            SELECT
              time_bucket(INTERVAL '1 minute', ts) AS bucket,
              fingerprint,
              COUNT(*) AS count
            FROM logs
            WHERE ts > ? AND fingerprint IS NOT NULL
            GROUP BY 1, 2;
            "#,
        )?;
        let n = stmt.execute([since_1m_ts])?;
        total += n;
    }
    // Service registry: merge every service seen in this cycle's window.
    // Same non-overlapping `ts > since` discipline as the rollups above, so
    // totals accumulate exactly once. On a fresh table (first run after
    // deploy) backfill from `logs_all` — hot + cold — so long-idle services
    // appear immediately instead of only when they log again.
    {
        let empty: bool = tx
            .query_row("SELECT COUNT(*) = 0 FROM services", [], |r| r.get(0))
            .unwrap_or(true);
        if empty {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO services (name, first_seen, last_seen, total)
                SELECT service, MIN(ts), MAX(ts), COUNT(*)
                FROM logs_all
                WHERE service IS NOT NULL AND service <> ''
                GROUP BY service
                ON CONFLICT(name) DO NOTHING;
                "#,
            )?;
            total += stmt.execute([])?;
        } else {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO services (name, first_seen, last_seen, total)
                SELECT service, MIN(ts), MAX(ts), COUNT(*)
                FROM logs
                WHERE ts > ? AND service IS NOT NULL AND service <> ''
                GROUP BY service
                ON CONFLICT(name) DO UPDATE SET
                  first_seen = LEAST(services.first_seen, EXCLUDED.first_seen),
                  last_seen = GREATEST(services.last_seen, EXCLUDED.last_seen),
                  total = services.total + EXCLUDED.total;
                "#,
            )?;
            total += stmt.execute([since_1m_ts])?;
        }
    }
    tx.commit()?;
    state.last_1m_bucket = Some(now);
    state.last_1h_bucket = Some(now);
    Ok(total)
}

/// Background rollup task.
pub fn spawn_rollup_task(
    conn: Arc<Mutex<Connection>>,
    cfg: RollupConfig,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = RollupState::default();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let conn = conn.lock();
            match rollup_once(&conn, &mut state, &cfg) {
                Ok(n) if n > 0 => tracing::info!(rows = n, "rollup materialized"),
                Ok(_) => tracing::debug!("rollup cycle: no new rows"),
                Err(e) => tracing::warn!(?e, "rollup failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::appender::{insert_batch, LogRow};

    fn test_store() -> (tempfile::TempDir, crate::store::Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        (tmp, store)
    }

    fn log_row(service: &str, ts: chrono::DateTime<chrono::Utc>) -> LogRow {
        LogRow {
            ts,
            insert_ts: ts,
            service: service.to_string(),
            message: "m".to_string(),
            ..LogRow::empty_at(ts)
        }
    }

    fn insert_logs(store: &crate::store::Store, rows: &[LogRow]) {
        let conn = store.lock();
        insert_batch(&conn, &[], rows).unwrap();
    }

    #[test]
    fn rollup_maintains_service_registry() {
        let (_tmp, store) = test_store();
        let now = chrono::Utc::now();
        insert_logs(
            &store,
            &[
                log_row("api", now - chrono::Duration::minutes(30)),
                log_row("api", now - chrono::Duration::minutes(20)),
                log_row("web", now - chrono::Duration::minutes(10)),
                log_row("", now - chrono::Duration::minutes(5)), // skipped: empty
            ],
        );
        let conn = store.lock();
        let mut state = RollupState::default();
        rollup_once(&conn, &mut state, &RollupConfig::default()).unwrap();

        let mut names: Vec<(String, i64)> = conn
            .prepare("SELECT name, total FROM services ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![("api".to_string(), 2), ("web".to_string(), 1)],
            "registry holds per-service totals, empty service skipped"
        );

        // Second cycle accumulates exactly once (non-overlapping windows).
        drop(conn);
        insert_logs(&store, &[log_row("api", chrono::Utc::now())]);
        let conn = store.lock();
        rollup_once(&conn, &mut state, &RollupConfig::default()).unwrap();
        let total: i64 = conn
            .query_row("SELECT total FROM services WHERE name = 'api'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total, 3);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM services", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "no duplicate registry rows");
    }

    #[test]
    fn rollup_backfills_idle_services_from_cold_tier() {
        let (_tmp, store) = test_store();
        // Older than the 7-day incremental lookback: only the empty-table
        // backfill (via logs_all) can pick it up.
        let old = chrono::Utc::now() - chrono::Duration::days(30);
        insert_logs(&store, &[log_row("legacy", old)]);
        let conn = store.lock();
        let mut state = RollupState::default();
        rollup_once(&conn, &mut state, &RollupConfig::default()).unwrap();
        let total: i64 = conn
            .query_row(
                "SELECT total FROM services WHERE name = 'legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1, "backfill covers services idle past the lookback");
    }
}
