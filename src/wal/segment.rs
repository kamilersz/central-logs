//! Segment naming and rotation.

use std::path::{Path, PathBuf};

use crate::Result;

/// Format: `wal-<10-digit-id>.log`.
pub fn segment_filename(id: u64) -> String {
    format!("wal-{id:010}.log")
}

/// Parse a segment filename back into its id. Returns None if not a segment.
pub fn segment_id_from_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("wal-")?;
    let stem = rest.strip_suffix(".log")?;
    stem.parse::<u64>().ok()
}

/// Builds and rotates WAL segment file paths inside a base directory.
pub struct SegmentRotator {
    base_dir: PathBuf,
    max_bytes: u64,
    current_id: u64,
    current_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentRotatorStats {
    pub current_segment_id: u64,
    pub current_segment_bytes: u64,
    pub total_segments_created: u64,
}

impl SegmentRotator {
    /// Discover the highest existing segment id in `base_dir` and resume from there.
    pub fn open(base_dir: PathBuf, max_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&base_dir)?;
        let mut max_id = 0u64;
        for entry in std::fs::read_dir(&base_dir)? {
            let entry = entry?;
            if let Some(id) = segment_id_from_name(entry.file_name().to_str().unwrap_or("")) {
                max_id = max_id.max(id);
            }
        }
        let current_path = base_dir.join(segment_filename(max_id));
        let current_bytes = std::fs::metadata(&current_path)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(Self {
            base_dir,
            max_bytes,
            current_id: max_id,
            current_bytes,
        })
    }

    pub fn current_id(&self) -> u64 {
        self.current_id
    }

    pub fn current_bytes(&self) -> u64 {
        self.current_bytes
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn current_path(&self) -> PathBuf {
        self.base_dir.join(segment_filename(self.current_id))
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Account for `n` additional bytes written into the current segment. Returns
    /// `true` if the caller should rotate before the next write.
    pub fn record_written(&mut self, n: u64) -> bool {
        self.current_bytes += n;
        self.current_bytes >= self.max_bytes
    }

    /// Rotate to a fresh segment. The old segment file remains on disk for ingest
    /// workers to drain.
    pub fn rotate(&mut self) -> Result<PathBuf> {
        self.current_id = self.current_id.checked_add(1).ok_or_else(|| {
            crate::Error::config("segment id overflow — somehow wrote 18 quintillion logs")
        })?;
        self.current_bytes = 0;
        let path = self.current_path();
        // Touch the file to mark it as created; writers will append.
        std::fs::write(&path, b"")?;
        Ok(path)
    }

    /// Enumerate all closed (not current) segment ids in ascending order.
    pub fn closed_segment_ids(&self) -> Result<Vec<u64>> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            if let Some(id) = segment_id_from_name(entry.file_name().to_str().unwrap_or("")) {
                if id < self.current_id {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn segment_path(&self, id: u64) -> PathBuf {
        self.base_dir.join(segment_filename(id))
    }

    /// Delete a closed segment file (called after ingest has checkpointed past it).
    pub fn delete_segment(&self, id: u64) -> Result<()> {
        let path = self.segment_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_name_roundtrip() {
        for id in [0u64, 1, 42, 999_999_999] {
            let name = segment_filename(id);
            assert_eq!(segment_id_from_name(&name), Some(id));
        }
        assert!(segment_id_from_name("notwal.log").is_none());
        assert!(segment_id_from_name("wal-abc.log").is_none());
    }
}
