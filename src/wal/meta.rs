//! redb-backed metadata store for WAL offsets and ingest checkpoints.
//!
//! Tables:
//! - `meta` (str key -> str value): well-known scalar fields like `last_segment_id`.
//! - `checkpoints` (u64 worker_id -> (u64 segment_id, u64 byte_offset)): per-ingest-worker
//!   read cursors, persisted so a crash resumes from the last durably-committed position.

use std::path::Path;

use redb::{Database, ReadableTable, ReadableDatabase, TableDefinition};

use crate::Result;

const META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("meta");
const CHECKPOINT_TABLE: TableDefinition<u64, &[u8; 16]> = TableDefinition::new("checkpoints");
const MIN_SEG_TABLE: TableDefinition<&str, u64> = TableDefinition::new("min_seg");

/// Re-exported checkpoint coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub segment_id: u64,
    pub byte_offset: u64,
}

impl Checkpoint {
    pub fn encode(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&self.segment_id.to_le_bytes());
        out[8..].copy_from_slice(&self.byte_offset.to_le_bytes());
        out
    }
    pub fn decode(bytes: &[u8; 16]) -> Self {
        let segment_id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let byte_offset = u64::from_le_bytes(bytes[8..].try_into().unwrap());
        Self {
            segment_id,
            byte_offset,
        }
    }
    pub fn zero() -> Self {
        Self {
            segment_id: 0,
            byte_offset: 0,
        }
    }
}

/// Wrapper around a redb [`Database`] holding WAL metadata.
pub struct WalMeta {
    db: Database,
}

impl WalMeta {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(META_TABLE)?;
            let _ = tx.open_table(CHECKPOINT_TABLE)?;
            let _ = tx.open_table(MIN_SEG_TABLE)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(META_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_string()))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(META_TABLE)?;
            t.insert(key, value)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_checkpoint(&self, worker_id: u64) -> Result<Checkpoint> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(CHECKPOINT_TABLE)?;
        Ok(table
            .get(worker_id)?
            .map(|v| Checkpoint::decode(v.value()))
            .unwrap_or_else(Checkpoint::zero))
    }

    pub fn set_checkpoint(&self, worker_id: u64, cp: Checkpoint) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(CHECKPOINT_TABLE)?;
            let bytes = cp.encode();
            t.insert(worker_id, &bytes)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Minimum checkpoint across all workers — anything below this can be deleted.
    pub fn min_checkpoint(&self) -> Result<Option<Checkpoint>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(CHECKPOINT_TABLE)?;
        let mut min: Option<Checkpoint> = None;
        for entry in table.iter()? {
            let (_, v) = entry?;
            let cp = Checkpoint::decode(v.value());
            min = Some(match min {
                None => cp,
                Some(m) if cp.segment_id < m.segment_id => cp,
                Some(m) if cp.segment_id == m.segment_id && cp.byte_offset < m.byte_offset => cp,
                Some(m) => m,
            });
        }
        Ok(min)
    }

    pub fn list_checkpoints(&self) -> Result<Vec<(u64, Checkpoint)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(CHECKPOINT_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push((k.value(), Checkpoint::decode(v.value())));
        }
        Ok(out)
    }
}

/// Convenience trait so ingest workers can be parameterized over the store.
pub trait CheckpointStore: Send + Sync {
    fn get_checkpoint(&self, worker_id: u64) -> Result<Checkpoint>;
    fn set_checkpoint(&self, worker_id: u64, cp: Checkpoint) -> Result<()>;
}

impl CheckpointStore for WalMeta {
    fn get_checkpoint(&self, worker_id: u64) -> Result<Checkpoint> {
        WalMeta::get_checkpoint(self, worker_id)
    }
    fn set_checkpoint(&self, worker_id: u64, cp: Checkpoint) -> Result<()> {
        WalMeta::set_checkpoint(self, worker_id, cp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.redb");
        let meta = WalMeta::open(&path).unwrap();
        meta.set_meta("k", "v").unwrap();
        assert_eq!(meta.get_meta("k").unwrap().as_deref(), Some("v"));
        meta.set_checkpoint(7, Checkpoint { segment_id: 3, byte_offset: 1234 })
            .unwrap();
        let cp = meta.get_checkpoint(7).unwrap();
        assert_eq!(cp.segment_id, 3);
        assert_eq!(cp.byte_offset, 1234);
    }
}
