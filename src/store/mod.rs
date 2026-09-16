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
pub use backup::{list_runs as list_backup_runs, restore_snapshot, run_snapshot, RestoreSummary, RunSummary};
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

impl Store {
    pub fn open(
        db_path: &std::path::Path,
        parquet_dir: PathBuf,
        hot: Vec<HotAttribute>,
    ) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&parquet_dir)?;
        let conn = Connection::open(db_path)?;
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
