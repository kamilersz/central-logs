//! redb-backed metadata store for WAL offsets and ingest checkpoints.
//!
//! Tables:
//! - `meta` (str key -> str value): well-known scalar fields like `last_segment_id`.
//! - `checkpoints` (u64 worker_id -> (u64 segment_id, u64 byte_offset)): per-ingest-worker
//!   read cursors, persisted so a crash resumes from the last durably-committed position.

use std::path::Path;

use rand::{rngs::OsRng, RngCore};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::Result;

const META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("meta");
const CHECKPOINT_TABLE: TableDefinition<u64, &[u8; 16]> = TableDefinition::new("checkpoints");
const MIN_SEG_TABLE: TableDefinition<&str, u64> = TableDefinition::new("min_seg");
/// Daily salts for the web-analytics visitor hash (PLAN-WEB-ANALYTICS W1):
/// ISO date string → 32 random bytes. Salts older than two days are pruned —
/// after that, past visitor hashes can no longer be reconstructed, which is
/// exactly the privacy property we want (decision D5).
const WEB_SALT_TABLE: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("web_salts");

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
            let _ = tx.open_table(WEB_SALT_TABLE)?;
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

    // =================================================================
    // Web-analytics visitor-hash salts (PLAN-WEB-ANALYTICS W1, D5)
    // =================================================================

    /// Salt for the given event date, creating a fresh random one on first
    /// use (lazy). The date is the EVENT date, not the ingest date, so
    /// late-arriving events hash consistently with same-day ones. Old salts
    /// (≥ 2 days) are pruned opportunistically — hashes for those days become
    /// unrecoverable by design.
    pub fn web_salt_for_date(&self, date: chrono::NaiveDate) -> Result<[u8; 32]> {
        let key = date.format("%Y-%m-%d").to_string();
        let tx = self.db.begin_write()?;
        let existing: Option<[u8; 32]> = {
            let table = tx.open_table(WEB_SALT_TABLE)?;
            let guard = table.get(key.as_str())?;
            guard.map(|g| *g.value())
        };
        let salt = match existing {
            Some(s) => s,
            None => {
                let mut s = [0u8; 32];
                OsRng.fill_bytes(&mut s);
                let mut table = tx.open_table(WEB_SALT_TABLE)?;
                table.insert(key.as_str(), &s)?;
                s
            }
        };
        tx.commit()?;
        Ok(salt)
    }

    /// Delete salts for dates strictly before `before` (keep ≥ 2 days so
    /// pending WAL records can still be hashed consistently).
    pub fn prune_web_salts(&self, before: chrono::NaiveDate) -> Result<usize> {
        let tx = self.db.begin_write()?;
        let mut table = tx.open_table(WEB_SALT_TABLE)?;
        let mut removed = 0usize;
        let stale: Vec<String> = table
            .range::<&str>(..)?
            .filter_map(|e| e.ok())
            .filter_map(|(k, _)| {
                let key = k.value().to_string();
                chrono::NaiveDate::parse_from_str(&key, "%Y-%m-%d")
                    .ok()
                    .filter(|d| *d < before)
                    .map(|_| key)
            })
            .collect();
        for key in &stale {
            table.remove(key.as_str())?;
            removed += 1;
        }
        drop(table);
        tx.commit()?;
        Ok(removed)
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
        meta.set_checkpoint(
            7,
            Checkpoint {
                segment_id: 3,
                byte_offset: 1234,
            },
        )
        .unwrap();
        let cp = meta.get_checkpoint(7).unwrap();
        assert_eq!(cp.segment_id, 3);
        assert_eq!(cp.byte_offset, 1234);
    }

    #[test]
    fn web_salts_are_stable_per_day_and_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = WalMeta::open(&tmp.path().join("meta.redb")).unwrap();
        let d0 = chrono::NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
        let d1 = chrono::NaiveDate::from_ymd_opt(2026, 1, 11).unwrap();

        let a = meta.web_salt_for_date(d0).unwrap();
        let b = meta.web_salt_for_date(d0).unwrap();
        assert_eq!(a, b, "same day → same salt");
        let c = meta.web_salt_for_date(d1).unwrap();
        assert_ne!(a, c, "different day → different salt");

        // Prune keeps d1 (not before d1) and drops d0.
        let before = chrono::NaiveDate::from_ymd_opt(2026, 1, 11).unwrap();
        assert_eq!(meta.prune_web_salts(before).unwrap(), 1);
        // Recreating the pruned day's salt yields a DIFFERENT value — the old
        // hash is unrecoverable (the privacy property).
        let again = meta.web_salt_for_date(d0).unwrap();
        assert_ne!(a, again);
    }
}
