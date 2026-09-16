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

/// Per-fingerprint 1-minute error counts — sparklines + fast threshold
/// evaluation for `error_group_threshold` alert rules.
pub const ROLLUP_ERROR_1M_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rollup_error_1m (
  bucket        TIMESTAMP,
  fingerprint   VARCHAR,
  count         BIGINT
);
"#;

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
         ALTER TABLE logs ADD COLUMN IF NOT EXISTS fingerprint VARCHAR;",
    )?;

    // Promoted hot-attribute columns (architecture: telemetry optimization).
    for attr in hot {
        let sql = format!(
            "ALTER TABLE logs ADD COLUMN IF NOT EXISTS {} {}",
            attr.name,
            attr.duckdb_type.ddl()
        );
        conn.execute_batch(&sql)
            .map_err(|e| crate::Error::config(format!(
                "adding hot column '{}': {e}",
                attr.name
            )))?;
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
        tracing::warn!(?e, "creating logs_all with parquet glob failed; falling back to hot-only view");
        let fallback = "CREATE OR REPLACE VIEW logs_all AS SELECT * FROM logs;";
        conn.execute_batch(fallback)?;
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
pub fn refresh_logs_all_view(conn: &Connection, parquet_dir: &Path, hot: &[HotAttribute]) -> Result<()> {
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
        conn.query_row(sql, [], |row| row.get::<_, chrono::DateTime<chrono::Utc>>(0))
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
