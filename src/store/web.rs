//! Web-analytics store jobs (PLAN-WEB-ANALYTICS.md W3/W4):
//!
//! - [`web_sessionize_once`] — background sessionization (decision D6: this
//!   runs as a SQL job, NOT in the ingest workers, because workers are
//!   sharded per segment and a visitor's events can land on different
//!   workers). Deterministic session ids → idempotent re-runs.
//! - [`web_rollup_once`] — replace-by-bucket rollups (`web_rollup_1m`,
//!   `web_rollup_1d`). Unique-visitor counts are EXACT per day (counted from
//!   raw data); multi-day ranges sum daily uniques because the daily-rotating
//!   hash cannot be deduplicated across days (documented semantics).
//! - [`web_compact_once`] — age hot `web_events` out to hive-partitioned
//!   Parquet under the SEPARATE web parquet dir (decision D9), then apply the
//!   raw-retention TTL. Sessions and rollups are kept long-term.

use std::path::Path;

use duckdb::Connection;

use crate::Result;

/// A sessionizer/rollup cycle's configuration.
#[derive(Debug, Clone)]
pub struct WebJobConfig {
    /// Inactivity gap that closes a visit (`web.session_gap_minutes`).
    pub session_gap_minutes: u64,
    /// Only process events older than this many seconds (event commits can
    /// arrive out of order across workers; wait for stragglers).
    pub settle_secs: i64,
    /// Max NULL events admitted per sessionize pass (bounds the lock hold).
    pub max_events_per_pass: i64,
    /// Cardinality cap per rollup dimension per day (tail is truncated).
    pub rollup_top_n: i64,
    /// How many days the rollup recomputes each pass (late-event margin).
    pub rollup_backfill_days: i64,
}
impl Default for WebJobConfig {
    fn default() -> Self {
        Self {
            session_gap_minutes: 30,
            settle_secs: 30,
            max_events_per_pass: 50_000,
            rollup_top_n: 100,
            rollup_backfill_days: 2,
        }
    }
}

// =====================================================================
// Sessionizer
// =====================================================================

/// One sessionize pass. Returns the number of events that received a
/// (possibly refreshed) `session_id`.
///
/// Idempotency: a session's id is `hash(site, visitor, start_ts)`, so
/// reprocessing the same events yields the same ids. Late events are handled
/// by recomputing the FULL history of every touched visitor and replacing
/// their sessions wholesale — no dangling ids when an event commits out of
/// order (plan §4.4).
pub fn web_sessionize_once(conn: &Connection, cfg: &WebJobConfig) -> Result<usize> {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(cfg.settle_secs.max(1));
    let gap_secs = (cfg.session_gap_minutes.max(1) * 60) as i64;

    // 0) Fast exit: any settled NULL-session rows at all?
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM web_events
         WHERE session_id IS NULL AND NOT is_bot AND visitor_id <> 0 AND ts < ?",
        [cutoff],
        |r| r.get(0),
    )?;
    // Repair pre-fix rows even on otherwise-idle passes: sessions stamped
    // before entry/exit used IGNORE NULLS keep NULL paths until touched.
    // Guarded by a cheap count so clean installs skip the UPDATE.
    let broken: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM web_sessions WHERE entry_path IS NULL OR exit_path IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if broken > 0 {
        conn.execute_batch(
            "UPDATE web_sessions AS s
             SET entry_path = (SELECT e.path FROM web_events e
                               WHERE e.session_id = s.session_id
                                 AND e.path IS NOT NULL AND e.path <> ''
                               ORDER BY e.ts ASC, e.insert_ts ASC LIMIT 1),
                 exit_path = (SELECT e.path FROM web_events e
                              WHERE e.session_id = s.session_id
                                AND e.path IS NOT NULL AND e.path <> ''
                              ORDER BY e.ts DESC, e.insert_ts DESC LIMIT 1)
             WHERE s.entry_path IS NULL OR s.exit_path IS NULL;",
        )?;
    }
    // Repair column-shifted rows: installs migrated through intermediate
    // DDLs (region/city appended at the end) received positional INSERTs in
    // the newer middle-position order, rotating (region, city, device,
    // browser, os). The predicate is exact, not heuristic: genuine values
    // can never cross these vocabularies (devices/browsers/OSes come from
    // closed classifier lists; region holds ISO codes, city free text).
    // Fresh installs never match, so the UPDATE is a no-op for them.
    let shifted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM web_sessions
             WHERE COALESCE(device, '') NOT IN ('desktop', 'mobile', 'tablet', 'bot', '')
                OR COALESCE(browser, '') NOT IN
                   ('Chrome', 'Firefox', 'Safari', 'Edge', 'Opera', 'Samsung Internet', 'IE', '')
                OR COALESCE(os, '') NOT IN
                   ('Windows', 'macOS', 'iOS', 'iPadOS', 'Android', 'ChromeOS', 'Linux', '')
                OR COALESCE(region, '') IN
                   ('Chrome', 'Firefox', 'Safari', 'Edge', 'Opera', 'Samsung Internet')
                OR COALESCE(city, '') IN
                   ('Windows', 'macOS', 'iOS', 'iPadOS', 'Android', 'ChromeOS', 'Linux')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if shifted > 0 {
        let fixed = conn.execute_batch(
            "UPDATE web_sessions
             SET device = os,
                 browser = region,
                 os = city,
                 region = device,
                 city = browser
             WHERE COALESCE(device, '') NOT IN ('desktop', 'mobile', 'tablet', 'bot', '')
                OR COALESCE(browser, '') NOT IN
                   ('Chrome', 'Firefox', 'Safari', 'Edge', 'Opera', 'Samsung Internet', 'IE', '')
                OR COALESCE(os, '') NOT IN
                   ('Windows', 'macOS', 'iOS', 'iPadOS', 'Android', 'ChromeOS', 'Linux', '')
                OR COALESCE(region, '') IN
                   ('Chrome', 'Firefox', 'Safari', 'Edge', 'Opera', 'Samsung Internet')
                OR COALESCE(city, '') IN
                   ('Windows', 'macOS', 'iOS', 'iPadOS', 'Android', 'ChromeOS', 'Linux');",
        );
        match fixed {
            Ok(_) => tracing::info!(shifted, "web_sessions column-shift repair applied"),
            Err(e) => tracing::warn!(?e, "web_sessions column-shift repair failed"),
        }
    }
    // Repair stale bounce flags + inflated durations on recent sessions:
    // sessions written before the current definitions (bounce = 1 pageview
    // and no deliberate interaction; engagement ≤ wall time) would keep the
    // old values until their visitor is touched again. Bounded to the last
    // 7 days so the check stays cheap on large installs; converges to a
    // no-op once everything matches.
    let mismatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM web_sessions
             WHERE start_ts >= now()::TIMESTAMP - INTERVAL '7 days'
               AND (pageviews < 1
                    OR is_bounce <> (pageviews = 1 AND NOT EXISTS (
                       SELECT 1 FROM web_events e
                       WHERE e.session_id = web_sessions.session_id
                         AND e.event IN ('outbound', 'download', 'custom')))
                    OR engaged_ms > (epoch_ms(end_ts) - epoch_ms(start_ts)))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if mismatched > 0 {
        let fixed = conn.execute_batch(
            "DELETE FROM web_sessions
             WHERE start_ts >= now()::TIMESTAMP - INTERVAL '7 days' AND pageviews < 1;
             UPDATE web_sessions
             SET is_bounce = (pageviews = 1 AND NOT EXISTS (
                    SELECT 1 FROM web_events e
                    WHERE e.session_id = web_sessions.session_id
                      AND e.event IN ('outbound', 'download', 'custom'))),
                 engaged_ms = LEAST(engaged_ms,
                    CAST((epoch_ms(end_ts) - epoch_ms(start_ts)) AS BIGINT))
             WHERE start_ts >= now()::TIMESTAMP - INTERVAL '7 days'
               AND pageviews >= 1;",
        );
        match fixed {
            Ok(_) => tracing::info!(mismatched, "web_sessions bounce/duration repair applied"),
            Err(e) => tracing::warn!(?e, "web_sessions bounce/duration repair failed"),
        }
    }
    if pending == 0 {
        return Ok(0);
    }

    // Temp scratch tables (dropped at the end; temp = per-connection).
    let cleanup = "DROP TABLE IF EXISTS temp.wb; DROP TABLE IF EXISTS temp.wt;
                   DROP TABLE IF EXISTS temp.we; DROP TABLE IF EXISTS temp.ws;
                   DROP TABLE IF EXISTS temp.wss;";
    conn.execute_batch(cleanup)?;

    // 1) Fresh rows drive this pass: NULL session, real visitors only,
    //    settled, size-bounded (plan §4.8: don't hog the shared mutex).
    conn.execute(
        "CREATE OR REPLACE TEMP TABLE wb AS
         SELECT rowid AS rid, site_id, visitor_id
         FROM web_events
         WHERE session_id IS NULL AND NOT is_bot AND visitor_id <> 0
           AND ts < ?
         ORDER BY ts
         LIMIT ?",
        duckdb::params![cutoff, cfg.max_events_per_pass],
    )?;

    let tx = conn.unchecked_transaction()?;

    // 2) Touched visitors.
    tx.execute_batch(
        "CREATE OR REPLACE TEMP TABLE wt AS
         SELECT DISTINCT site_id, visitor_id FROM wb;",
    )?;
    // 3) Their FULL event history (existing sessions included) — so late
    //    events re-cut session boundaries correctly instead of dangling.
    tx.execute_batch(
        "CREATE OR REPLACE TEMP TABLE we AS
         SELECT w.rowid AS rid, w.site_id, w.visitor_id, w.user_id, w.ts, w.insert_ts,
                w.event, w.path, w.value, w.source, w.medium, w.country, w.region, w.city,
                w.device, w.browser, w.os, w.is_internal
         FROM web_events w
         JOIN wt t ON w.site_id = t.site_id AND w.visitor_id = t.visitor_id
         WHERE NOT w.is_bot AND w.visitor_id <> 0;",
    )?;
    // 4) Sessionize: new session when the inactivity gap is exceeded.
    tx.execute_batch(&format!(
        r#"
        CREATE OR REPLACE TEMP TABLE ws AS
        WITH seq AS (
          SELECT *, lag(ts) OVER win AS prev_ts
          FROM we
          WINDOW win AS (PARTITION BY site_id, visitor_id ORDER BY ts, insert_ts, rid)
        ),
        marked AS (
          SELECT *,
                 CASE WHEN prev_ts IS NULL
                            OR date_diff('second', prev_ts, ts) > {gap_secs}
                      THEN 1 ELSE 0 END AS is_new
          FROM seq
        )
        SELECT site_id, visitor_id, rid,
               SUM(is_new) OVER (PARTITION BY site_id, visitor_id
                                 ORDER BY ts, insert_ts, rid
                                 ROWS UNBOUNDED PRECEDING) AS g,
               ts, insert_ts, event, path, value, user_id, source, medium,
               country, region, city, device, browser, os, is_internal
        FROM marked;
        "#
    ))?;
    // 5) One row per session: first/last over the full session frame.
    //    `session_id = hash(site, visitor, start_ts)` is deterministic → the
    //    whole pass is idempotent.
    tx.execute_batch(
        r#"
        CREATE OR REPLACE TEMP TABLE wss AS
        SELECT DISTINCT
          site_id, visitor_id, g,
          -- A session STARTS on a pageview: hash/anchor on the first pageview
          -- ts, not the first event (a stray vital must not open a session).
          HASH(site_id, visitor_id, epoch_ms(
            FIRST_VALUE(CASE WHEN event = 'pageview' THEN ts END IGNORE NULLS) OVER w
          )) AS session_id,
          FIRST_VALUE(CASE WHEN event = 'pageview' THEN ts END IGNORE NULLS) OVER w AS start_ts,
          LAST_VALUE(ts)       OVER w AS end_ts,
          FIRST_VALUE(user_id) OVER w AS user_id,
          -- Paths are NULL on non-pageview events (vitals, errors,
          -- engagement); IGNORE NULLS so entry/exit reflect actual pageviews
          -- instead of whichever event happened to sort first.
          FIRST_VALUE(path IGNORE NULLS) OVER w AS entry_path,
          LAST_VALUE(path IGNORE NULLS)  OVER w AS exit_path,
          -- First-touch attribution from PAGEVIEW rows only: vitals/errors
          -- carry an empty referrer and would read as direct when they sort
          -- ahead of the pageview they belong to.
          FIRST_VALUE(CASE WHEN event = 'pageview' THEN source END IGNORE NULLS) OVER w AS source,
          FIRST_VALUE(CASE WHEN event = 'pageview' THEN medium END IGNORE NULLS) OVER w AS medium,
          FIRST_VALUE(country) OVER w AS country,
          FIRST_VALUE(region)  OVER w AS region,
          FIRST_VALUE(city)     OVER w AS city,
          FIRST_VALUE(device)  OVER w AS device,
          FIRST_VALUE(browser) OVER w AS browser,
          FIRST_VALUE(os)      OVER w AS os,
          BOOL_AND(is_internal) OVER w AS is_internal,
          SUM(CASE WHEN event = 'pageview' THEN 1 ELSE 0 END) OVER w AS pageviews,
          COUNT(*)                                                 OVER w AS events_n,
          -- Deliberate actions only: outbound clicks, downloads, custom tags.
          -- Passive telemetry (vitals, engagement heartbeats) does not break
          -- a bounce — otherwise every real landing would count as engaged.
          SUM(CASE WHEN event IN ('outbound', 'download', 'custom') THEN 1 ELSE 0 END)
              OVER w AS interactions,
          SUM(CASE WHEN event = 'engagement' THEN COALESCE(value, 0) ELSE 0 END) OVER w AS engaged_ms
        FROM ws
        WINDOW w AS (PARTITION BY site_id, visitor_id, g
                     ORDER BY ts, insert_ts, rid
                     ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING);
        "#,
    )?;
    // 6) Stamp events, replace the touched visitors' sessions — one tx.
    tx.execute(
        "UPDATE web_events SET session_id = s.session_id
         FROM temp.ws AS k JOIN temp.wss AS s
           ON k.site_id = s.site_id AND k.visitor_id = s.visitor_id AND k.g = s.g
              AND s.pageviews >= 1
         WHERE web_events.rowid = k.rid;",
        [],
    )?;
    // 6b) Orphans: partitions with NO pageview (a lone vital/heartbeat).
    // They must not open a session, and must never re-enter the pending
    // set — stamp 0 ("deliberately sessionless") so `session_id IS NULL`
    // stops matching them. Event queries treat 0 as no session via NULLIF.
    tx.execute(
        "UPDATE web_events SET session_id = 0
         WHERE session_id IS NULL AND rowid IN (
           SELECT k.rid FROM temp.ws k
           LEFT JOIN temp.wss s
             ON k.site_id = s.site_id AND k.visitor_id = s.visitor_id AND k.g = s.g
                AND s.pageviews >= 1
           WHERE s.site_id IS NULL);",
        [],
    )?;
    tx.execute(
        "DELETE FROM web_sessions s
         WHERE EXISTS (SELECT 1 FROM temp.wt t
                       WHERE t.site_id = s.site_id AND t.visitor_id = s.visitor_id);",
        [],
    )?;
    tx.execute_batch(
        "INSERT INTO web_sessions
             (session_id, site_id, visitor_id, user_id, is_internal,
              start_ts, end_ts, entry_path, exit_path,
              pageviews, events, engaged_ms, is_bounce,
              source, medium, country, region, city, device, browser, os)
         SELECT
             session_id,
             site_id, visitor_id, user_id, is_internal, start_ts, end_ts,
             entry_path, exit_path,
             CAST(pageviews AS INTEGER), CAST(events_n AS INTEGER),
             -- Engagement cannot exceed the session's wall time: pre-fix
             -- trackers reported cumulative totals that double-counted.
             LEAST(CAST(engaged_ms AS BIGINT),
                   CAST((epoch_ms(end_ts) - epoch_ms(start_ts)) AS BIGINT)),
             -- Bounce = exactly one pageview and no deliberate interaction.
             (pageviews = 1 AND interactions = 0),
             source, medium, country, region, city, device, browser, os
         FROM temp.wss
         WHERE pageviews >= 1 AND start_ts IS NOT NULL;",
    )?;
    tx.execute_batch(cleanup)?;
    tx.commit()?;
    Ok(pending.min(cfg.max_events_per_pass) as usize)
}

// =====================================================================
// Rollups (replace-by-bucket)
// =====================================================================

/// Rollup cursor state carried between passes.
#[derive(Debug, Clone, Default)]
pub struct WebRollupState {
    pub last_1m_bucket: Option<chrono::DateTime<chrono::Utc>>,
}

/// One web-rollup pass. 1-minute volume per site feeds forecast/anomaly;
/// per-dimension daily aggregates feed the dashboard's long-range charts.
/// Replace-by-bucket (DELETE affected, then INSERT) — the log rollup's
/// append-only pattern can't represent exact uniques (plan §4.6).
pub fn web_rollup_once(
    conn: &Connection,
    state: &mut WebRollupState,
    cfg: &WebJobConfig,
) -> Result<usize> {
    let now = chrono::Utc::now();
    let mut total = 0usize;
    let tx = conn.unchecked_transaction()?;

    // ---- 1-minute volume per site. Recompute a 10-minute margin: web
    // events settle ~30s, but WAL lag can commit minutes late; the table is
    // tiny, so a generous replace window is cheap insurance.
    let since_1m = state
        .last_1m_bucket
        .unwrap_or(now - chrono::Duration::hours(1))
        - chrono::Duration::minutes(10);
    {
        let mut stmt = tx.prepare("DELETE FROM web_rollup_1m WHERE bucket > ?;")?;
        stmt.execute([since_1m])?;
    }
    {
        let mut stmt = tx.prepare(
            "INSERT INTO web_rollup_1m
             SELECT time_bucket(INTERVAL '1 minute', ts), site_id,
                    SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END),
                    COUNT(*)
             FROM web_events
             WHERE ts > ? AND NOT is_bot
             GROUP BY 1, 2;",
        )?;
        total += stmt.execute([since_1m])?;
    }
    state.last_1m_bucket = Some(minute_bucket(now) - chrono::Duration::minutes(1));

    // ---- Daily aggregates over a small backfill window (late events).
    let today = now.date_naive();
    let first_day = today - chrono::Duration::days(cfg.rollup_backfill_days.max(1));
    {
        let mut stmt = tx.prepare("DELETE FROM web_rollup_1d WHERE day >= ?;")?;
        stmt.execute([first_day])?;
        drop(stmt);
    }
    let top = cfg.rollup_top_n.max(1);
    let sql = daily_rollup_sql(top);
    {
        // Each dimension arm binds the day once → one param per arm.
        let mut stmt = tx.prepare(&sql)?;
        for offset in (0..=cfg.rollup_backfill_days.max(1)).rev() {
            let day = today - chrono::Duration::days(offset);
            total += stmt.execute(duckdb::params![
                day, day, day, day, day, day, day, day // path, source, country, region, city, device, browser, os
            ])?;
        }
    }
    tx.commit()?;
    Ok(total)
}

fn minute_bucket(t: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    let ts = t.timestamp();
    chrono::DateTime::from_timestamp(ts - ts.rem_euclid(60), 0).unwrap_or(t)
}

/// One UNION-ALL arm of the daily dims: `(day, site, dim, dim_value, …)`.
/// `visitors` is EXACT per day (`COUNT(DISTINCT visitor_id)` over the day's
/// raw rows); `bounces` joins the session table.
fn dim_arm(col: &str) -> String {
    format!(
        r#"
        SELECT CAST(ts AS DATE) AS day, site_id, '{col}' AS dim,
               COALESCE(NULLIF(TRIM({col}), ''), '(none)') AS dim_value,
               SUM(CASE WHEN event='pageview' THEN 1 ELSE 0 END) AS pageviews,
               COUNT(DISTINCT visitor_id) AS visitors,
               COUNT(DISTINCT session_id) AS sessions,
               COUNT(DISTINCT CASE WHEN session_id IN (
                        SELECT session_id FROM web_sessions WHERE is_bounce)
                   THEN session_id END) AS bounces,
               SUM(CASE WHEN event='engagement' THEN COALESCE(value, 0) ELSE 0 END) AS engaged_ms
        FROM web_events
        WHERE NOT is_bot AND CAST(ts AS DATE) = CAST(? AS DATE)
        GROUP BY 1, 2, 3, 4"#
    )
}

fn daily_rollup_sql(top: i64) -> String {
    let arms = ["path", "source", "country", "region", "city", "device", "browser", "os"]
        .iter()
        .map(|c| dim_arm(c))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    format!(
        r#"
        INSERT INTO web_rollup_1d
        WITH ranked AS (
          SELECT *, ROW_NUMBER() OVER (PARTITION BY dim ORDER BY pageviews DESC, dim_value) AS rn
          FROM ({arms})
        ),
        top AS (SELECT * FROM ranked WHERE rn <= {top}),
        rest AS (
          SELECT day, site_id, dim, '(other)' AS dim_value,
                 SUM(pageviews) AS pageviews,
                 SUM(visitors)  AS visitors,
                 SUM(sessions)  AS sessions,
                 SUM(bounces)   AS bounces,
                 SUM(engaged_ms) AS engaged_ms
          FROM ranked WHERE rn > {top}
          GROUP BY 1, 2, 3
        )
        SELECT day, site_id, dim, dim_value, CAST(pageviews AS INTEGER),
               CAST(visitors AS INTEGER), CAST(sessions AS INTEGER),
               CAST(bounces AS INTEGER), CAST(engaged_ms AS BIGINT) FROM top
        UNION ALL
        SELECT day, site_id, dim, dim_value, CAST(pageviews AS INTEGER),
               CAST(visitors AS INTEGER), CAST(sessions AS INTEGER),
               CAST(bounces AS INTEGER), CAST(engaged_ms AS BIGINT) FROM rest
        WHERE EXISTS (SELECT 1 FROM ranked WHERE rn > {top});
        "#
    )
}

// =====================================================================
// Compaction / retention (cold tier in the SEPARATE web dir, decision D9)
// =====================================================================

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct WebCompactStats {
    pub exported_files: u64,
    pub deleted_hot_rows: u64,
    pub purged_partitions: u64,
}

/// One web-compaction pass: export aged hot rows to
/// `<web_dir>/date=YYYY-MM-DD/site=<id>/part-<n>.parquet`, delete them from
/// the hot table, purge cold partitions past `retention_days`. Sessions and
/// rollups are intentionally NOT compacted or purged (small; they are the
/// long-term artifact).
///
/// Safe against the sessionizer: `hot_cutoff` (hours) ≫ settle window
/// (seconds), so events are sessionized long before they go cold.
pub fn web_compact_once(
    conn: &Connection,
    web_dir: &Path,
    hot_cutoff: chrono::DateTime<chrono::Utc>,
    retention_days: i64,
    compression: &str,
) -> Result<WebCompactStats> {
    let mut stats = WebCompactStats::default();
    let dir_str = web_dir.to_string_lossy().replace('\'', "''");

    // 1) (date, site) groups to archive.
    let mut stmt = conn.prepare(
        "SELECT STRFTIME('%Y-%m-%d', ts) AS d, site_id
         FROM web_events WHERE ts < ? GROUP BY 1, 2;",
    )?;
    let groups: Vec<(String, String)> = stmt
        .query_map([hot_cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    for (d, site) in &groups {
        // Sanitize the site id into a path segment (ids are config-validated,
        // but never trust path material blindly).
        let site_seg: String = site
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let sub_dir = format!("{dir_str}/date={d}/site={site_seg}");
        std::fs::create_dir_all(&sub_dir)?;
        let part_path = format!(
            "{sub_dir}/part-{}.parquet",
            chrono::Utc::now().timestamp_millis()
        );
        let sql = format!(
            r#"
            COPY (
                SELECT ts, insert_ts, site_id, event, name, value, hostname, path,
                       query, referrer, source, medium, utm, visitor_id, session_id,
                       user_id, is_internal, country, region, city, device, browser, os, screen_w, is_bot,
                       props, trace_id, span_id
                FROM web_events
                WHERE ts < ? AND STRFTIME('%Y-%m-%d', ts) = ? AND site_id = ?
                ORDER BY ts, site_id
            ) TO '{part_path}' (FORMAT PARQUET, ROW_GROUP_SIZE 100000, COMPRESSION '{codec}');
            "#,
            codec = compression.to_ascii_uppercase(),
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.execute(duckdb::params![hot_cutoff, d.as_str(), site.as_str()])?;
        drop(stmt);
        stats.exported_files += 1;
    }

    // 2) Delete exported rows from the hot table.
    stats.deleted_hot_rows =
        conn.execute("DELETE FROM web_events WHERE ts < ?", [hot_cutoff])? as u64;

    // 3) Purge cold partitions past the raw retention TTL (partition-date
    //    based, so pruning is a directory walk — no DB scan).
    stats.purged_partitions = purge_old_web_parquet(web_dir, retention_days);

    // 4) Refresh the view — the file set just changed (same reason
    //    `compact_once` refreshes `logs_all`).
    crate::store::schema::apply_web_events_all_view(conn, web_dir)?;

    Ok(stats)
}

/// Delete `date=…` partition directories older than `retention_days`.
fn purge_old_web_parquet(web_dir: &Path, retention_days: i64) -> u64 {
    let cutoff = chrono::Utc::now().date_naive() - chrono::Duration::days(retention_days.max(0));
    let mut purged = 0u64;
    let Ok(date_dirs) = std::fs::read_dir(web_dir) else {
        return 0;
    };
    for d in date_dirs.flatten() {
        let name = d.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(day) = name.strip_prefix("date=") else { continue };
        let Ok(parsed) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
            continue;
        };
        if parsed < cutoff && std::fs::remove_dir_all(d.path()).is_ok() {
            purged += 1;
        }
    }
    purged
}

// =====================================================================
// Background job wiring
// =====================================================================

/// Spawn the periodic web job (sessionizer, then rollup). Each pass holds the
/// single DuckDB connection only for its bounded batch, so ingest flushes
/// interleave between passes (plan §4.8).
pub fn spawn_web_job(
    store: crate::store::Store,
    cfg: WebJobConfig,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut rollup_state = WebRollupState::default();
        tick.tick().await; // first tick fires immediately — skip it
        loop {
            tick.tick().await;
            let conn = store.conn();
            let guard = conn.lock();
            match web_sessionize_once(&guard, &cfg) {
                Ok(n) if n > 0 => {
                    tracing::debug!(events = n, "web sessionize pass");
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(?e, "web sessionize pass failed");
                    continue;
                }
            }
            if let Err(e) = web_rollup_once(&guard, &mut rollup_state, &cfg) {
                tracing::warn!(?e, "web rollup pass failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::appender::{insert_web_in_tx, WebRow};

    fn store() -> (tempfile::TempDir, crate::store::Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        (tmp, store)
    }

    fn row(ts: chrono::DateTime<chrono::Utc>, visitor: u64, event: &str, site: &str) -> WebRow {
        WebRow {
            ts,
            insert_ts: ts,
            site_id: site.into(),
            event: event.into(),
            visitor_id: visitor,
            ..Default::default()
        }
    }

    fn insert_rows(store: &crate::store::Store, rows: &[WebRow]) {
        let conn = store.lock();
        let tx = conn.unchecked_transaction().unwrap();
        insert_web_in_tx(&tx, rows).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn sessionize_splits_on_inactivity_gap() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(120);
        // Visitor 11: events at +0, +5 (one session), +45, +46 (new session).
        insert_rows(
            &store,
            &[
                row(t0, 11, "pageview", "s"),
                row(t0 + chrono::Duration::minutes(5), 11, "pageview", "s"),
                row(t0 + chrono::Duration::minutes(45), 11, "pageview", "s"),
                row(t0 + chrono::Duration::minutes(46), 11, "custom", "s"),
                // Visitor 22: single pageview → bounced session.
                row(t0 + chrono::Duration::minutes(2), 22, "pageview", "s"),
            ],
        );
        let conn = store.lock();
        let cfg = WebJobConfig::default();
        let n = web_sessionize_once(&conn, &cfg).unwrap();
        assert_eq!(n, 5);

        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 3, "40-minute gap must split visitor 11 in two");

        // Every event's session id matches a session row.
        let mapped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM web_events w JOIN web_sessions s USING (session_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mapped, 5);

        // The second visitor-11 session: 1 pageview + 1 custom event
        // (a deliberate interaction, so NOT a bounce).
        let (pv, ev): (i64, i64) = conn
            .query_row(
                "SELECT pageviews, events FROM web_sessions
                 WHERE visitor_id = 11 AND pageviews = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((pv, ev), (1, 2));

        let bounces: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_sessions WHERE is_bounce", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bounces, 1, "only visitor 22's single-event session bounces");
    }

    #[test]
    fn sessionize_is_idempotent_and_handles_late_events() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        insert_rows(&store, &[row(t0, 7, "pageview", "s")]);
        let conn = store.lock();
        let cfg = WebJobConfig::default();
        web_sessionize_once(&conn, &cfg).unwrap();
        let sid: u64 = conn
            .query_row(
                "SELECT session_id FROM web_events WHERE event='pageview'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Second pass: same session id, still one session.
        web_sessionize_once(&conn, &cfg).unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "idempotent pass must not duplicate sessions");
        let sid2: u64 = conn
            .query_row(
                "SELECT session_id FROM web_events WHERE event='pageview'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid, sid2);
        drop(conn);

        // A late event lands within the gap → same session id after re-cut.
        insert_rows(&store, &[row(t0 + chrono::Duration::minutes(10), 7, "custom", "s")]);
        let conn = store.lock();
        web_sessionize_once(&conn, &cfg).unwrap();
        let sid3: u64 = conn
            .query_row(
                "SELECT session_id FROM web_events WHERE event='pageview'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid, sid3, "late in-gap event must keep the session id");
        let joined: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM web_events w JOIN web_sessions s USING (session_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joined, 2);
    }

    #[test]
    fn sessionize_respects_settle_window_and_bots() {
        let (_tmp, store) = store();
        let now = chrono::Utc::now();
        insert_rows(
            &store,
            &[
                row(now, 1, "pageview", "s"), // too fresh → skipped this pass
                row(now - chrono::Duration::minutes(5), 2, "pageview", "s"),
                WebRow {
                    is_bot: true,
                    ..row(now - chrono::Duration::minutes(5), 3, "pageview", "s")
                }, // bot → never
            ],
        );
        let conn = store.lock();
        let cfg = WebJobConfig::default();
        let n = web_sessionize_once(&conn, &cfg).unwrap();
        assert_eq!(n, 1, "only the settled, non-bot event is processed");
        let bot_session: Option<u64> = conn
            .query_row(
                "SELECT session_id FROM web_events WHERE visitor_id = 3",
                [],
                |r| r.get::<_, Option<u64>>(0),
            )
            .unwrap();
        assert!(bot_session.is_none(), "bots never sessionize");
    }

    #[test]
    fn rollup_counts_exact_daily_uniques() {
        let (_tmp, store) = store();
        let day0 = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            chrono::Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap(),
            chrono::Utc,
        );
        let day1 = day0 - chrono::Duration::days(1);
        // Day 0: visitors 1,2 on /a; visitor 1 on /b. Day 1: visitor 1 on /a.
        // Visitor 1 appears on BOTH days → daily uniques must NOT dedupe.
        let mut rows = vec![
            WebRow { path: "/a".into(), ..row(day0 + chrono::Duration::hours(1), 1, "pageview", "s") },
            WebRow { path: "/a".into(), ..row(day0 + chrono::Duration::hours(2), 2, "pageview", "s") },
            WebRow { path: "/b".into(), ..row(day0 + chrono::Duration::hours(3), 1, "pageview", "s") },
            WebRow { path: "/a".into(), ..row(day1 + chrono::Duration::hours(1), 1, "pageview", "s") },
        ];
        for r in rows.iter_mut() {
            r.session_id = Some(r.visitor_id * 10);
        }
        insert_rows(&store, &rows);

        let conn = store.lock();
        let cfg = WebJobConfig::default();
        let mut state = WebRollupState::default();
        web_rollup_once(&conn, &mut state, &cfg).unwrap();

        // Daily: /a on day0 has visitors {1,2} → 2 (exact); /a on day1 has 1.
        let v: i64 = conn
            .query_row(
                "SELECT visitors FROM web_rollup_1d
                 WHERE dim='path' AND dim_value='/a' AND day = ?",
                [day0.date_naive()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 2, "unique visitors per day are exact");
        let v1: i64 = conn
            .query_row(
                "SELECT visitors FROM web_rollup_1d
                 WHERE dim='path' AND dim_value='/a' AND day = ?",
                [day1.date_naive()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v1, 1);
        // Same visitor across days sums to 3 — the documented semantic.
        let sum: i64 = conn
            .query_row(
                "SELECT SUM(visitors) FROM web_rollup_1d WHERE dim='path' AND dim_value='/a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sum, 3);
    }

    #[test]
    fn rollup_replaces_affected_buckets() {
        let (_tmp, store) = store();
        let t = minute_bucket(chrono::Utc::now() - chrono::Duration::minutes(5));
        insert_rows(
            &store,
            &[row(t, 1, "pageview", "s"), row(t, 2, "pageview", "s")],
        );
        let conn = store.lock();
        let cfg = WebJobConfig::default();
        let mut state = WebRollupState::default();
        web_rollup_once(&conn, &mut state, &cfg).unwrap();

        // Add another event in the SAME minute, re-roll: the bucket must be
        // REPLACED (pageviews 3), not appended (which would read 2+3=5).
        drop(conn);
        insert_rows(&store, &[row(t + chrono::Duration::seconds(1), 3, "pageview", "s")]);
        let conn = store.lock();
        web_rollup_once(&conn, &mut state, &cfg).unwrap();
        let views: i64 = conn
            .query_row(
                "SELECT pageviews FROM web_rollup_1m WHERE bucket = ?",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(views, 3, "replace-by-bucket, no double counting");
    }

    #[test]
    fn compaction_exports_and_applies_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        let web_dir = crate::store::schema::web_parquet_dir(&tmp.path().join("parquet"));
        let now = chrono::Utc::now();
        let old = now - chrono::Duration::days(10);
        insert_rows(
            &store,
            &[
                row(old, 1, "pageview", "site_1"),
                row(old + chrono::Duration::hours(1), 2, "pageview", "site_1"),
                row(now - chrono::Duration::minutes(10), 3, "pageview", "site_1"), // stays hot
            ],
        );
        let conn = store.lock();
        let stats = web_compact_once(
            &conn,
            &web_dir,
            now - chrono::Duration::hours(24),
            90,
            "zstd",
        )
        .unwrap();
        assert_eq!(stats.exported_files, 1);
        assert_eq!(stats.deleted_hot_rows, 2);
        let hot: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hot, 1);
        let all: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_events_all", [], |r| r.get(0))
            .unwrap();
        assert_eq!(all, 3, "cold rows remain queryable via web_events_all");

        // Retention purge removes whole date= partitions older than TTL.
        // (In production the purge only runs inside `web_compact_once`, which
        // refreshes the view afterwards; do the same here.)
        let purged = purge_old_web_parquet(&web_dir, 5);
        assert_eq!(purged, 1);
        crate::store::schema::apply_web_events_all_view(&conn, &web_dir).unwrap();
        let all2: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_events_all", [], |r| r.get(0))
            .unwrap();
        assert_eq!(all2, 1, "purged partition disappears from the view");
    }

    #[test]
    fn lone_beacons_do_not_open_sessions() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        // Visitor 30: a vital with NO pageview anywhere → orphan, dropped.
        let mut vital = row(t0, 30, "vital", "s");
        vital.name = "LCP".into();
        vital.value = Some(900.0);
        // Visitor 31: pageview, then (within the gap) vitals + heartbeats —
        // they ATTACH to the open session.
        let mut pv = row(t0 + chrono::Duration::minutes(1), 31, "pageview", "s");
        pv.path = "/x".into();
        let mut vital2 = row(t0 + chrono::Duration::minutes(2), 31, "vital", "s");
        vital2.name = "CLS".into();
        vital2.value = Some(0.02);
        let mut eng = row(t0 + chrono::Duration::minutes(3), 31, "engagement", "s");
        eng.value = Some(60_000.0);
        insert_rows(&store, &[vital, pv, vital2, eng]);

        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM web_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "the pageview-less vital must not open a session");
        let orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM web_events WHERE visitor_id = 30 AND session_id = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan, 1, "orphans are stamped 0 so they never reprocess");
        let attached: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM web_events WHERE visitor_id = 31 AND session_id <> 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attached, 3, "in-gap beacons attach to the open session");
    }

    #[test]
    fn bounce_needs_one_pageview_and_no_interaction() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        // 41: single pageview + passive telemetry (vital, engagement) → bounce.
        let mut pv = row(t0, 41, "pageview", "s");
        pv.path = "/".into();
        let mut vit = row(t0 + chrono::Duration::seconds(2), 41, "vital", "s");
        vit.name = "LCP".into();
        vit.value = Some(1500.0);
        let mut eng = row(t0 + chrono::Duration::seconds(40), 41, "engagement", "s");
        eng.value = Some(30_000.0);
        // 42: single pageview + an outbound click → NOT a bounce.
        let mut pv2 = row(t0 + chrono::Duration::minutes(1), 42, "pageview", "s");
        pv2.path = "/links".into();
        let mut out = row(t0 + chrono::Duration::minutes(2), 42, "outbound", "s");
        out.name = "wa.me".into();
        // 43: two pageviews → NOT a bounce.
        let mut pv3 = row(t0 + chrono::Duration::minutes(3), 43, "pageview", "s");
        pv3.path = "/a".into();
        let mut pv4 = row(t0 + chrono::Duration::minutes(4), 43, "pageview", "s");
        pv4.path = "/b".into();
        insert_rows(&store, &[pv, vit, eng, pv2, out, pv3, pv4]);

        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let bounce: bool = conn
            .query_row("SELECT is_bounce FROM web_sessions WHERE visitor_id = 41", [], |r| r.get(0))
            .unwrap();
        assert!(bounce, "vitals/heartbeats are passive, not interaction");
        let click: bool = conn
            .query_row("SELECT is_bounce FROM web_sessions WHERE visitor_id = 42", [], |r| r.get(0))
            .unwrap();
        assert!(!click, "an outbound click is interaction");
        let two_pv: bool = conn
            .query_row("SELECT is_bounce FROM web_sessions WHERE visitor_id = 43", [], |r| r.get(0))
            .unwrap();
        assert!(!two_pv);
    }

    #[test]
    fn first_touch_source_comes_from_the_pageview() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        // A vital sorts FIRST (same second) and carries no referrer; the
        // session source must still come from the pageview's referrer.
        let mut vit = row(t0, 51, "vital", "s");
        vit.name = "LCP".into();
        vit.value = Some(1200.0);
        let mut pv = row(t0, 51, "pageview", "s");
        pv.path = "/landing".into();
        pv.source = "google".into();
        pv.medium = "organic".into();
        insert_rows(&store, &[vit, pv]);
        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let (src, med): (String, String) = conn
            .query_row(
                "SELECT source, medium FROM web_sessions WHERE visitor_id = 51",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((src.as_str(), med.as_str()), ("google", "organic"));
    }

    #[test]
    fn engaged_time_cannot_exceed_session_wall_time() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        // Pre-fix trackers reported cumulative totals; a 2-minute session
        // carrying a 1-hour engagement value must clamp to its wall time.
        let mut pv = row(t0, 61, "pageview", "s");
        pv.path = "/".into();
        let mut eng = row(t0 + chrono::Duration::minutes(2), 61, "engagement", "s");
        eng.value = Some(3_600_000.0);
        insert_rows(&store, &[pv, eng]);
        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let (engaged, wall_ms): (i64, i64) = conn
            .query_row(
                "SELECT engaged_ms, CAST((epoch_ms(end_ts) - epoch_ms(start_ts)) AS BIGINT)
                 FROM web_sessions WHERE visitor_id = 61",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(engaged <= wall_ms, "engaged {engaged} ms > wall {wall_ms} ms");
    }

    #[test]
    fn sessionize_entry_exit_skip_null_paths() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        // A vital (NULL path) sorts FIRST, a pageview last: without
        // IGNORE NULLS both entry and exit would come out NULL.
        let mut vital = row(t0, 5, "vital", "s");
        vital.name = "LCP".into();
        vital.value = Some(1200.0);
        let mut pv = row(t0 + chrono::Duration::minutes(1), 5, "pageview", "s");
        pv.path = "/pricing".into();
        let mut engage = row(t0 + chrono::Duration::minutes(2), 5, "engagement", "s");
        engage.value = Some(30_000.0);
        insert_rows(&store, &[vital, pv, engage]);

        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let (entry, exit): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT entry_path, exit_path FROM web_sessions WHERE visitor_id = 5",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(entry.as_deref(), Some("/pricing"));
        assert_eq!(exit.as_deref(), Some("/pricing"));
    }

    #[test]
    fn column_shift_repair_unrotates_geo_device_columns() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        let conn = store.lock();
        // Simulate a row written positionally into a migrated table
        // (region/city appended at the end): true values
        // {device: desktop, browser: Chrome, os: Windows, region: JB,
        //  city: Bandung} land rotated as {device: JB, browser: Bandung,
        //  os: desktop, region: Chrome, city: Windows}.
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce, country, region, city,
                                       device, browser, os)
             VALUES (4242, 's', 9, ?, ?, '/a', '/b', 1, 1, 1000, true,
                     'ID', 'Chrome', 'Windows', 'JB', 'Bandung', 'desktop')",
            duckdb::params![t0, t0],
        )
        .unwrap();
        // And a correct row that must NOT be touched.
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce, country, region, city,
                                       device, browser, os)
             VALUES (4243, 's', 10, ?, ?, '/a', '/b', 1, 1, 1000, true,
                     'ID', 'JB', 'Bandung', 'desktop', 'Chrome', 'Windows')",
            duckdb::params![t0, t0],
        )
        .unwrap();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let fixed: (String, String, String, String, String) = conn
            .query_row(
                "SELECT device, browser, os, region, city FROM web_sessions WHERE session_id = 4242",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            fixed,
            (
                "desktop".to_string(),
                "Chrome".to_string(),
                "Windows".to_string(),
                "JB".to_string(),
                "Bandung".to_string()
            )
        );
        let kept: (String, String, String, String, String) = conn
            .query_row(
                "SELECT device, browser, os, region, city FROM web_sessions WHERE session_id = 4243",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(kept, fixed, "correct rows must be untouched");
    }

    #[test]
    fn sessionize_repairs_null_entry_exit() {
        let (_tmp, store) = store();
        // Simulate a pre-fix session row: events have paths, the session
        // row has NULL entry/exit (FIRST_VALUE without IGNORE NULLS).
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        let mut pv1 = row(t0, 8, "pageview", "s");
        pv1.path = "/a".into();
        let mut pv2 = row(t0 + chrono::Duration::minutes(1), 8, "pageview", "s");
        pv2.path = "/b".into();
        insert_rows(&store, &[pv1, pv2]);
        let conn = store.lock();
        // Stamp a broken session row by hand (as the old code would).
        let sid: u64 = 0xbeef;
        conn.execute(
            "INSERT INTO web_sessions (session_id, site_id, visitor_id, start_ts, end_ts,
                                       entry_path, exit_path, pageviews, events,
                                       engaged_ms, is_bounce)
             VALUES (?, 's', 8, ?, ?, NULL, NULL, 2, 2, 1000, false)",
            duckdb::params![sid, t0, t0],
        )
        .unwrap();
        // Mark events sessionized so the pass is otherwise idle: the repair
        // must still run (it sits before the pending==0 early return).
        conn.execute(
            "UPDATE web_events SET session_id = ? WHERE visitor_id = 8",
            [sid],
        )
        .unwrap();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let (entry, exit): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT entry_path, exit_path FROM web_sessions WHERE session_id = ?",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(entry.as_deref(), Some("/a"));
        assert_eq!(exit.as_deref(), Some("/b"));
    }

    #[test]
    fn sessionize_marks_all_internal_sessions() {
        let (_tmp, store) = store();
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(60);
        let mut internal_row = row(t0, 6, "pageview", "s");
        internal_row.path = "/a".into();
        internal_row.is_internal = true;
        let mut external_row = row(t0 + chrono::Duration::minutes(1), 7, "pageview", "s");
        external_row.path = "/a".into();
        insert_rows(&store, &[internal_row, external_row]);

        let conn = store.lock();
        web_sessionize_once(&conn, &WebJobConfig::default()).unwrap();
        let internal: bool = conn
            .query_row(
                "SELECT is_internal FROM web_sessions WHERE visitor_id = 6",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(internal);
        let external: bool = conn
            .query_row(
                "SELECT is_internal FROM web_sessions WHERE visitor_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!external);
    }
}
