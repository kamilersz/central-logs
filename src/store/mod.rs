//! DuckDB-backed analytics store: schema DDL, Appender bulk insert, rollups,
//! hot/cold compaction to Parquet.
//!
//! See architecture §3 for the schema and the "telemetry optimization" notes
//! for the promoted hot-attribute columns.

pub mod appender;
pub mod backup;
pub mod compact;
pub mod rollup;
pub mod schema;

#[cfg(feature = "object-storage")]
pub mod cold;

use std::path::PathBuf;
use std::sync::Arc;

use duckdb::Connection;
use parking_lot::Mutex;

use crate::hot::HotAttribute;
use crate::Result;

pub use appender::{insert_batch, LogAppender, LogRow};
pub use backup::{
    list_runs as list_backup_runs, restore_snapshot, run_snapshot, RestoreSummary, RunSummary,
};
pub use rollup::{rollup_once, RollupConfig};
pub use schema::{apply_schema, store_state};

/// A shared DuckDB connection guarded by a Mutex.
///
/// DuckDB allows concurrent reads from separate connections but writes need
/// serialization; for v1 we use a single connection wrapped in a Mutex.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    parquet_dir: PathBuf,
    hot: Arc<Vec<HotAttribute>>,
}

/// DuckDB engine resource limits, applied right after the connection opens
/// and before any query work. Empty/0 = DuckDB defaults (auto-detect: ~80%
/// of system RAM, one thread per core). Bounds what a heavy query or
/// compaction can claim so the ingest path stays healthy on shared boxes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DuckDbTuning {
    /// `SET memory_limit` — DuckDB byte spec, e.g. "512MB", "2GB".
    /// Empty = DuckDB default.
    pub memory_limit: String,
    /// `SET threads`. 0 = DuckDB default (one thread per core).
    pub threads: usize,
}

/// Apply the tuning as SET statements. Called once at open time; DuckDB
/// settings are per-connection, and v1 uses a single shared connection.
fn apply_tuning(conn: &Connection, tuning: &DuckDbTuning) -> Result<()> {
    let ml = tuning.memory_limit.trim();
    if !ml.is_empty() {
        // Interpolated into a SET statement. Config validation whitelists
        // the byte-spec format; stripping quotes here is defense in depth.
        let safe = ml.replace('\'', "");
        conn.execute_batch(&format!("SET memory_limit='{safe}'"))?;
    }
    if tuning.threads > 0 {
        conn.execute_batch(&format!("SET threads={}", tuning.threads))?;
    }
    Ok(())
}

impl Store {
    pub fn open(
        db_path: &std::path::Path,
        parquet_dir: PathBuf,
        hot: Vec<HotAttribute>,
    ) -> Result<Self> {
        Self::open_with(db_path, parquet_dir, hot, &DuckDbTuning::default())
    }

    pub fn open_with(
        db_path: &std::path::Path,
        parquet_dir: PathBuf,
        hot: Vec<HotAttribute>,
        tuning: &DuckDbTuning,
    ) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&parquet_dir)?;
        let conn = Connection::open(db_path)?;
        apply_tuning(&conn, tuning)?;
        apply_schema(&conn, &parquet_dir, &hot)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            parquet_dir,
            hot: Arc::new(hot),
        })
    }

    pub fn conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    pub fn parquet_dir(&self) -> &std::path::Path {
        &self.parquet_dir
    }

    pub fn hot_attributes(&self) -> &[HotAttribute] {
        &self.hot
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(sql)?;
        Ok(())
    }

    /// Flush the DuckDB WAL into the main database file. Called on graceful
    /// shutdown so the next start never has to replay a large (or, after a
    /// crash, potentially inconsistent) WAL.
    pub fn checkpoint(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch("CHECKPOINT")?;
        Ok(())
    }
}

/// In-memory store state surfaced to dashboards (counts, lag).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreState {
    pub hot_rows: i64,
    pub rollup_1m_rows: i64,
    pub rollup_1h_rows: i64,
    pub anomalies_rows: i64,
    pub cold_parquet_bytes: u64,
    pub oldest_hot_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub newest_hot_ts: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_tuning_is_applied_to_the_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_with(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
            &DuckDbTuning {
                memory_limit: "512MB".to_string(),
                threads: 1,
            },
        )
        .unwrap();
        let conn = store.lock();
        let mem: String = conn
            .query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
            .unwrap();
        let bytes = parse_duckdb_memory_limit(&mem);
        // '512MB' is 512×10^6 bytes; DuckDB reports ~488.2 MiB (2^20 base).
        let expected = 512.0 * 1_000_000.0;
        assert!(
            (bytes - expected).abs() / expected < 0.01,
            "memory_limit should be ~512MB, got '{mem}' ({bytes} bytes)"
        );
        let threads: i64 = conn
            .query_row("SELECT current_setting('threads')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(threads, 1);
    }

    /// Parse DuckDB's memory_limit display form ("488.2 MiB", "402.7 GiB")
    /// into bytes. Units are binary (KiB/MiB/GiB/TiB).
    fn parse_duckdb_memory_limit(s: &str) -> f64 {
        let s = s.trim();
        let num_len = s
            .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == ' ')
            .len();
        let (num, unit) = s.split_at(num_len);
        let n: f64 = num.trim().parse().unwrap_or(0.0);
        let mult = match unit.trim().to_ascii_uppercase().replace("I", "") {
            u if u == "B" => 1.0,
            u if u == "KB" => 1024.0,
            u if u == "MB" => 1024.0f64.powi(2),
            u if u == "GB" => 1024.0f64.powi(3),
            u if u == "TB" => 1024.0f64.powi(4),
            _ => 0.0,
        };
        n * mult
    }

    #[test]
    fn duckdb_default_tuning_leaves_settings_auto() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(
            &tmp.path().join("t.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .unwrap();
        let conn = store.lock();
        // No explicit memory_limit → DuckDB's ~80% RAM default (a large
        // byte value, not the small cap we set in the other test).
        let mem: String = conn
            .query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
            .unwrap();
        assert!(
            parse_duckdb_memory_limit(&mem) > 512.0 * 1_000_000.0,
            "default memory_limit should exceed 512MB, got '{mem}'"
        );
    }
}
