//! DDL: hot `logs` table, `logs_all` view (hot + cold Parquet), rollups,
//! anomalies, alert rules (architecture §3, §6, §7 + telemetry optimization).

use std::path::Path;

use duckdb::Connection;

use crate::hot::HotAttribute;
use crate::Result;

pub const LOGS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS logs (
  ts            TIMESTAMP,
  insert_ts     TIMESTAMP,
  source_host   VARCHAR,
  service       VARCHAR,
  level         VARCHAR,
  message       VARCHAR,
  fingerprint   VARCHAR,
  trace_id      VARCHAR,
  span_id       VARCHAR,
  attributes    JSON,
  geo_country   VARCHAR,
  raw_len       INTEGER,
  protocol      VARCHAR
);
"#;

pub const ROLLUP_1M_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rollup_1m (
  bucket        TIMESTAMP,
  service       VARCHAR,
  level         VARCHAR,
  n             BIGINT,
  p50           DOUBLE,
  p95           DOUBLE,
  p99           DOUBLE,
  bytes         BIGINT
);
"#;

pub const ROLLUP_1H_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rollup_1h (
  bucket        TIMESTAMP,
  service       VARCHAR,
  level         VARCHAR,
  n             BIGINT,
  p50           DOUBLE,
  p95           DOUBLE,
  p99           DOUBLE,
  bytes         BIGINT
);
"#;

pub const ANOMALIES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS anomalies (
  ts        TIMESTAMP,
  metric    VARCHAR,
  score     DOUBLE,
  method    VARCHAR,
  severity  VARCHAR
);
"#;

pub const ALERT_RULES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS alert_rules (
  id              INTEGER PRIMARY KEY,
  name            VARCHAR NOT NULL,
  metric          VARCHAR NOT NULL,
  condition_json  JSON,
  channel         VARCHAR,
  status          VARCHAR DEFAULT 'pending_approval',
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  activated_at    TIMESTAMP
);
CREATE SEQUENCE IF NOT EXISTS alert_rules_id_seq;
"#;

/// Notification channels (web-managed, architecture §7). A rule references
/// one or more channels by id (`alert_rules.channels_json`). Config payloads
/// per type:
///   email    → {"recipients": ["ops@example.com", ...]}
///   telegram → {"bot_token": "...", "chat_id": "-1001234..."}
///   webhook  → {"url": "https://hooks.../..."}  (legacy `alert_rules.channel`
///              keeps working for rules created via MCP before this existed)
pub const ALERT_CHANNELS_DDL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS alert_channels_id_seq;
CREATE TABLE IF NOT EXISTS alert_channels (
  id          INTEGER PRIMARY KEY,
  name        VARCHAR NOT NULL,
  type        VARCHAR NOT NULL,
  config_json JSON NOT NULL,
  created_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
"#;

/// Per-firing history for the alert evaluator (`src/alerts.rs`). One row per
/// time a rule's condition was found breached and past its cooldown, whether
/// or not the notification actually made it out (`notified` + `error`
/// distinguish "fired but the webhook failed" from a clean delivery).
pub const ALERT_EVENTS_DDL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS alert_events_id_seq;
CREATE TABLE IF NOT EXISTS alert_events (
  id             INTEGER PRIMARY KEY,
  rule_id        INTEGER NOT NULL,
  ts             TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metric         VARCHAR NOT NULL,
  value          DOUBLE,
  message        VARCHAR,
  notified       BOOLEAN NOT NULL,
  error          VARCHAR
);
"#;

pub const DASHBOARD_CONFIGS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS dashboard_configs (
  id              INTEGER PRIMARY KEY,
  name            VARCHAR NOT NULL UNIQUE,
  description     VARCHAR,
  panels_json     JSON NOT NULL,
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE SEQUENCE IF NOT EXISTS dashboard_configs_id_seq;
"#;

/// API key store (OWASP A01/A07). Raw keys are never persisted — only their
/// SHA-256 hash. `key_prefix` is the first ~8 chars of the public part for UI
/// display ("abcd1234…"). `scopes` is a comma-separated list drawn from
/// `insert,read,write,admin`. Revocation is soft (`revoked_at IS NOT NULL`).
///
/// The sequence is created BEFORE the table so DuckDB doesn't error on the
/// sequence reference (matches the `alert_rules` / `dashboard_configs` DDL
/// pattern). INSERTs supply the id explicitly via `nextval(...)`.
pub const API_KEYS_DDL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS api_keys_id_seq;
CREATE TABLE IF NOT EXISTS api_keys (
  id              INTEGER PRIMARY KEY,
  name            VARCHAR NOT NULL,
  key_hash        VARCHAR NOT NULL UNIQUE,
  key_prefix      VARCHAR NOT NULL,
  scopes          VARCHAR NOT NULL,
  created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_used_at    TIMESTAMP,
  revoked_at      TIMESTAMP
);
"#;

/// Cold-tier upload manifest (object-storage archive). Tracks which local
/// Parquet files have been mirrored remotely and whether the remote copy has
/// since been deleted (local retention purge → remote delete).
pub const COLD_FILES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS cold_files (
  rel_path     VARCHAR PRIMARY KEY,
  remote_key   VARCHAR NOT NULL,
  uploaded_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at   TIMESTAMP
);
"#;

/// Backup run history (housekeeping). One row per snapshot attempt —
/// manual, scheduled, or size-triggered.
pub const BACKUP_RUNS_DDL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS backup_runs_id_seq;
CREATE TABLE IF NOT EXISTS backup_runs (
  id           BIGINT PRIMARY KEY,
  started_at   TIMESTAMP,
  finished_at  TIMESTAMP,
  trigger      VARCHAR,
  status       VARCHAR,
  local_path   VARCHAR,
  remote_key   VARCHAR,
  bytes        BIGINT,
  checksum     VARCHAR,
  error        VARCHAR
);
"#;

/// Error groups (error tracking, `docs/ERROR_TRACKING.md`). One row per
/// fingerprint: first/last seen, retained-event count, human title, and up to
/// `samples_per_group` sampled stacktraces. Group rows survive event
/// retention on purpose — the samples are the durable artifact.
pub const ERROR_GROUPS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS error_groups (
  fingerprint     VARCHAR PRIMARY KEY,
  service         VARCHAR,
  level           VARCHAR,
  title           VARCHAR,
  exception_type  VARCHAR,
  first_seen      TIMESTAMP,
  last_seen       TIMESTAMP,
  total_count     BIGINT,
  status          VARCHAR DEFAULT 'unresolved',
  resolved_at     TIMESTAMP,
  samples         JSON,
  variant_hashes  JSON
);
"#;

/// Service registry (autocomplete for `service:` filters). One row per
/// service ever seen, maintained by the rollup job (`rollup_once` merges each
/// cycle's window; a one-time backfill covers history when the table is
/// empty). Hot rows are compacted away, but the registry keeps totals and
/// last-seen — the pipeline "Services" count and the UI autocomplete read
/// from here instead of scanning.
pub const SERVICES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS services (
  name        VARCHAR PRIMARY KEY,
  first_seen  TIMESTAMP,
  last_seen   TIMESTAMP,
  total       BIGINT
);
"#;

/// Per-fingerprint 1-minute error counts — sparklines + fast threshold
/// evaluation for `error_group_threshold` alert rules.
pub const ROLLUP_ERROR_1M_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rollup_error_1m (
  bucket        TIMESTAMP,
  fingerprint   VARCHAR,
  count         BIGINT
);
"#;

/// Website analytics (PLAN-WEB-ANALYTICS.md §3.3). One row per browser
/// event. IDs we generate (`visitor_id`/`session_id`/`user_id`) are UBIGINT;
/// `trace_id` stays VARCHAR because it is the external W3C trace id shared
/// with `logs.trace_id`. The source IP is NOT stored — it exists only inside
/// the daily-rotating `visitor_id` hash.
pub const WEB_EVENTS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS web_events (
  ts          TIMESTAMP,
  insert_ts   TIMESTAMP,
  site_id     VARCHAR,
  event       VARCHAR,
  name        VARCHAR,
  value       DOUBLE,
  hostname    VARCHAR,
  path        VARCHAR,
  query       VARCHAR,
  referrer    VARCHAR,
  source      VARCHAR,
  medium      VARCHAR,
  utm         JSON,
  visitor_id  UBIGINT,
  session_id  UBIGINT,
  user_id     UBIGINT,
  is_internal BOOLEAN DEFAULT FALSE,
  country     VARCHAR,
  region      VARCHAR,
  city        VARCHAR,
  device      VARCHAR,
  browser     VARCHAR,
  os          VARCHAR,
  screen_w    USMALLINT,
  is_bot      BOOLEAN,
  props       JSON,
  trace_id    VARCHAR,
  span_id     VARCHAR
);
"#;

/// Visit-level rollup filled by the sessionizer job (PLAN-WEB-ANALYTICS W3).
/// Rows are replaced per (site, session) — not append-only — so unique
/// counts stay exact per session.
pub const WEB_SESSIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS web_sessions (
  session_id  UBIGINT,
  site_id     VARCHAR,
  visitor_id  UBIGINT,
  user_id     UBIGINT,
  is_internal BOOLEAN DEFAULT FALSE,
  start_ts    TIMESTAMP,
  end_ts      TIMESTAMP,
  entry_path  VARCHAR,
  exit_path   VARCHAR,
  pageviews   INTEGER,
  events      INTEGER,
  engaged_ms  BIGINT,
  is_bounce   BOOLEAN,
  source      VARCHAR,
  medium      VARCHAR,
  country     VARCHAR,
  region      VARCHAR,
  city        VARCHAR,
  device      VARCHAR,
  browser     VARCHAR,
  os          VARCHAR
);
"#;

/// Per-minute web volume per site (forecast/anomaly series, W4).
/// Replace-by-bucket: each bucket appears at most once.
pub const WEB_ROLLUP_1M_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS web_rollup_1m (
  bucket     TIMESTAMP,
  site_id    VARCHAR,
  pageviews  BIGINT,
  events     BIGINT
);
"#;

/// Per-day per-dimension aggregates (dashboard long-range charts, W4).
/// `visitors` is EXACT within a day (daily-rotating visitor hash cannot be
/// deduplicated across days — documented semantic). Cardinality per dim is
/// capped at rollup time; the tail is folded into `'(other)'`.
pub const WEB_ROLLUP_1D_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS web_rollup_1d (
  day        DATE,
  site_id    VARCHAR,
  dim        VARCHAR,
  dim_value  VARCHAR,
  pageviews  INTEGER,
  visitors   INTEGER,
  sessions   INTEGER,
  bounces    INTEGER,
  engaged_ms BIGINT
);
"#;

/// Web-analytics projects (PLAN-WEB-ANALYTICS.md W5). One row per project
/// created at runtime via `POST /api/web/sites` (admin). Config-seeded
/// `[[web.site]]` entries are NOT stored here — they live in the TOML and
/// are merged in memory at startup, so the table only holds UI-created
/// projects. The hot path never touches this table: rows are loaded once at
/// boot (plus on CRUD) into the in-memory `WebSites` registry.
pub const WEB_SITES_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS web_sites (
  id               VARCHAR PRIMARY KEY,
  allowed_origins  JSON NOT NULL,
  user_header      VARCHAR,
  exclude_ips      JSON NOT NULL DEFAULT '[]',
  created_at       TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
"#;

/// Directory for web-analytics Parquet, a SIBLING of the log `parquet_dir`
/// (decision D9): `logs_all` globs `{parquet_dir}/**/*.parquet`, so web
/// files under it would pollute the log view.
pub fn web_parquet_dir(parquet_dir: &Path) -> std::path::PathBuf {
    parquet_dir
        .parent()
        .unwrap_or(parquet_dir)
        .join("parquet-web")
}

/// Apply all DDL plus the `logs_all` view (hot + parquet). Idempotent across
/// restarts: hot-attribute columns are added with `ALTER TABLE ADD COLUMN IF
/// NOT EXISTS` so a config change is forward-compatible.
pub fn apply_schema(conn: &Connection, parquet_dir: &Path, hot: &[HotAttribute]) -> Result<()> {
    conn.execute_batch(LOGS_DDL)?;
    conn.execute_batch(ROLLUP_1M_DDL)?;
    conn.execute_batch(ROLLUP_1H_DDL)?;
    conn.execute_batch(ANOMALIES_DDL)?;
    conn.execute_batch(ALERT_RULES_DDL)?;
    conn.execute_batch(ALERT_EVENTS_DDL)?;
    conn.execute_batch(ALERT_CHANNELS_DDL)?;
    conn.execute_batch(DASHBOARD_CONFIGS_DDL)?;
    conn.execute_batch(API_KEYS_DDL)?;
    conn.execute_batch(COLD_FILES_DDL)?;
    conn.execute_batch(ERROR_GROUPS_DDL)?;
    conn.execute_batch(ROLLUP_ERROR_1M_DDL)?;
    conn.execute_batch(BACKUP_RUNS_DDL)?;
    conn.execute_batch(SERVICES_DDL)?;
    conn.execute_batch(WEB_EVENTS_DDL)?;
    conn.execute_batch(WEB_SESSIONS_DDL)?;
    conn.execute_batch(WEB_ROLLUP_1M_DDL)?;
    conn.execute_batch(WEB_ROLLUP_1D_DDL)?;
    conn.execute_batch(WEB_SITES_DDL)?;

    // Alert-evaluator bookkeeping columns, added after the original
    // alert_rules DDL shipped — ALTER so existing installs pick them up
    // without a manual migration (same pattern as the hot-attribute columns
    // below). `logs.fingerprint` likewise: added when error tracking shipped
    // (error_tracking / docs/ERROR_TRACKING.md).
    conn.execute_batch(
        "ALTER TABLE alert_rules ADD COLUMN IF NOT EXISTS last_evaluated_at TIMESTAMP; \
         ALTER TABLE alert_rules ADD COLUMN IF NOT EXISTS last_fired_at TIMESTAMP; \
         ALTER TABLE alert_rules ADD COLUMN IF NOT EXISTS last_notify_error VARCHAR; \
         ALTER TABLE alert_rules ADD COLUMN IF NOT EXISTS channels_json JSON; \
         ALTER TABLE alert_rules ADD COLUMN IF NOT EXISTS last_severity VARCHAR; \
         ALTER TABLE alert_events ADD COLUMN IF NOT EXISTS channel VARCHAR; \
         ALTER TABLE alert_events ADD COLUMN IF NOT EXISTS severity VARCHAR; \
         ALTER TABLE logs ADD COLUMN IF NOT EXISTS fingerprint VARCHAR;
         ALTER TABLE web_sites ADD COLUMN IF NOT EXISTS exclude_ips JSON DEFAULT '[]';
         ALTER TABLE web_events ADD COLUMN IF NOT EXISTS region VARCHAR;
         ALTER TABLE web_events ADD COLUMN IF NOT EXISTS city VARCHAR;
         ALTER TABLE web_events ADD COLUMN IF NOT EXISTS is_internal BOOLEAN DEFAULT FALSE;
         ALTER TABLE web_sessions ADD COLUMN IF NOT EXISTS region VARCHAR;
         ALTER TABLE web_sessions ADD COLUMN IF NOT EXISTS city VARCHAR;
         ALTER TABLE web_sessions ADD COLUMN IF NOT EXISTS is_internal BOOLEAN DEFAULT FALSE;",
    )?;

    // Promoted hot-attribute columns (architecture: telemetry optimization).
    for attr in hot {
        let sql = format!(
            "ALTER TABLE logs ADD COLUMN IF NOT EXISTS {} {}",
            attr.name,
            attr.duckdb_type.ddl()
        );
        conn.execute_batch(&sql)
            .map_err(|e| crate::Error::config(format!("adding hot column '{}': {e}", attr.name)))?;
    }

    let dir_str = parquet_dir.to_string_lossy().replace('\'', "''");
    // logs_all unions hot + cold. The SELECT lists are EXPLICIT (built-ins +
    // hot attributes) so the two arms always align: hive-partition columns
    // (date/hour/service path segments) are excluded on purpose — the real
    // `service`/`ts` values live in the files, and keeping the lists explicit
    // keeps `SELECT *` on the view stable as hot attributes are added.
    // `union_by_name` tolerates older files written before a hot attribute
    // existed (missing column → NULL). `hive_partitioning=false` avoids the
    // duplicate `service` column (path segment vs file column).
    let mut col_list = LOGS_BUILTIN_COLUMNS
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    for attr in hot {
        col_list.push(attr.name.clone());
    }
    let col_list = col_list.join(", ");
    let view_sql = format!(
        r#"
        CREATE OR REPLACE VIEW logs_all AS
            SELECT {col_list} FROM logs
            UNION ALL
            SELECT {col_list} FROM read_parquet('{dir_str}/**/*.parquet', hive_partitioning=false, union_by_name=true);
        "#,
    );
    // Fallback: if the parquet glob fails (no files yet), create a view over hot only.
    if let Err(e) = conn.execute_batch(&view_sql) {
        tracing::warn!(
            ?e,
            "creating logs_all with parquet glob failed; falling back to hot-only view"
        );
        let fallback = "CREATE OR REPLACE VIEW logs_all AS SELECT * FROM logs;";
        conn.execute_batch(fallback)?;
    }

    // web_events_all (hot + cold Parquet from the SEPARATE web dir, D9).
    // Same explicit-column pattern as logs_all so `SELECT *` stays stable.
    let web_dir = web_parquet_dir(parquet_dir);
    let _ = std::fs::create_dir_all(&web_dir);
    apply_web_events_all_view(conn, &web_dir)?;

    Ok(())
}

/// (Re)create the `web_events_all` view over the hot table + the web Parquet
/// dir. Called from `apply_schema` at boot and after web compaction changes
/// the file set (mirrors `refresh_logs_all_view`).
pub fn apply_web_events_all_view(conn: &Connection, web_dir: &Path) -> Result<()> {
    let web_dir_str = web_dir.to_string_lossy().replace('\'', "''");
    let col_list_web = crate::store::appender::WEB_COLUMNS.join(", ");
    let web_view_sql = format!(
        r#"
        CREATE OR REPLACE VIEW web_events_all AS
            SELECT {col_list_web} FROM web_events
            UNION ALL
            SELECT {col_list_web} FROM read_parquet('{web_dir_str}/**/*.parquet', hive_partitioning=false, union_by_name=true);
        "#
    );
    if let Err(e) = conn.execute_batch(&web_view_sql) {
        tracing::warn!(
            ?e,
            "creating web_events_all with parquet glob failed; falling back to hot-only view"
        );
        conn.execute_batch("CREATE OR REPLACE VIEW web_events_all AS SELECT * FROM web_events;")?;
    }
    Ok(())
}

/// Built-in `logs` columns, in DDL order. The `logs_all` view projects these
/// (plus configured hot attributes) explicitly on both arms.
pub const LOGS_BUILTIN_COLUMNS: &[&str] = &[
    "ts",
    "insert_ts",
    "source_host",
    "service",
    "level",
    "message",
    "fingerprint",
    "trace_id",
    "span_id",
    "attributes",
    "geo_country",
    "raw_len",
    "protocol",
];

/// Re-create the `logs_all` view after compaction has changed the parquet dir.
pub fn refresh_logs_all_view(
    conn: &Connection,
    parquet_dir: &Path,
    hot: &[HotAttribute],
) -> Result<()> {
    apply_schema(conn, parquet_dir, hot)
}

/// Pull a quick snapshot for dashboards / health endpoints.
pub fn store_state(conn: &Connection) -> crate::store::StoreState {
    let mut state = crate::store::StoreState::default();
    fn count(conn: &Connection, sql: &str) -> i64 {
        match conn.query_row(sql, [], |row| row.get::<_, i64>(0)) {
            Ok(v) => v,
            Err(_) => 0,
        }
    }
    fn ts(conn: &Connection, sql: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        conn.query_row(sql, [], |row| {
            row.get::<_, chrono::DateTime<chrono::Utc>>(0)
        })
        .ok()
    }
    state.hot_rows = count(conn, "SELECT COUNT(*) FROM logs");
    state.rollup_1m_rows = count(conn, "SELECT COUNT(*) FROM rollup_1m");
    state.rollup_1h_rows = count(conn, "SELECT COUNT(*) FROM rollup_1h");
    state.anomalies_rows = count(conn, "SELECT COUNT(*) FROM anomalies");
    state.oldest_hot_ts = ts(conn, "SELECT MIN(ts) FROM logs");
    state.newest_hot_ts = ts(conn, "SELECT MAX(ts) FROM logs");
    state
}
