//! Backup snapshots + restore (housekeeping, `docs/OPERATIONS.md`).
//!
//! A snapshot is an atomic tar.gz of the data dir — `central.duckdb`
//! (CHECKPOINTed first, so no live WAL), `meta.redb`, WAL segments, and the
//! cold Parquet tree — plus a sibling `manifest.json` carrying a SHA-256 of
//! the archive. Targets: a local directory (always, when
//! `keep_local_copy`) and optionally an S3/GCS bucket under
//! `<prefix>/<instance_id>/<utc-date>/backup.tar.gz`.
//!
//! Every attempt is recorded in the `backup_runs` DuckDB table, which backs
//! the Storage page and `/api/ops/backups`.
//!
//! Restore (`restore_snapshot`) is the reverse: fetch (local path or
//! `s3://bucket/key`), verify the checksum against the manifest, extract
//! into a target dir that is NOT the active data dir, then sanity-open the
//! DuckDB file. The operator then points the instance at that dir.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::config::BackupConfig;
#[cfg(feature = "object-storage")]
use crate::store::cold::ColdStore;
use crate::store::Store;
use crate::Result;

/// Optional remote (S3/GCS) snapshot target. `None` without the
/// object-storage feature (local-only backups).
#[cfg(feature = "object-storage")]
pub type Remote = Option<std::sync::Arc<ColdStore>>;
#[cfg(not(feature = "object-storage"))]
pub type Remote = Option<()>;

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotManifest {
    pub version: u32,
    pub created_at: String,
    pub instance_id: String,
    pub trigger: String,
    /// sha256 of backup.tar.gz (hex).
    pub checksum: String,
    pub bytes: u64,
    pub hot_rows: i64,
    pub error_groups: i64,
    /// Hot attributes configured on the source instance (informational —
    /// a mismatch on restore is surfaced as a warning, not a refusal).
    pub hot_attributes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunSummary {
    pub id: i64,
    pub local_path: Option<String>,
    pub remote_key: Option<String>,
    pub bytes: u64,
    pub checksum: String,
}

fn top_level_entries(data_dir: &Path) -> Vec<&'static str> {
    // central.duckdb.wal exists only when the engine hasn't checkpointed to
    // disk yet; include it when present (post-CHECKPOINT it's tiny/absent).
    let mut out = vec!["central.duckdb", "meta.redb", "wal", "parquet"];
    if data_dir.join("central.duckdb.wal").exists() {
        out.insert(1, "central.duckdb.wal");
    }
    out.retain(|p| data_dir.join(p).symlink_metadata().is_ok());
    out
}

fn append_dir(
    tar: &mut tar::Builder<&mut GzEncoder<std::fs::File>>,
    root: &Path,
    entry: &str,
) -> Result<u64> {
    let path = root.join(entry);
    tar.append_path_with_name(&path, entry)
        .map_err(|e| crate::Error::invalid_input(format!("tar append {entry}: {e}")))?;
    // Directory size (approximate, for the byte total reported to ops).
    fn dir_size(p: &Path) -> u64 {
        let mut n = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let ep = e.path();
                if ep.is_dir() {
                    n += dir_size(&ep);
                } else if let Ok(m) = e.metadata() {
                    n += m.len();
                }
            }
        }
        n
    }
    Ok(if path.is_dir() { dir_size(&path) } else { path.metadata().map(|m| m.len()).unwrap_or(0) })
}

/// Insert a `backup_runs` row; returns its id.
fn record_run(
    store: &Store,
    trigger: &str,
    started_at: chrono::DateTime<Utc>,
) -> Result<i64> {
    let conn = store.lock();
    let id: i64 = conn.query_row("SELECT nextval('backup_runs_id_seq')", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO backup_runs (id, started_at, trigger, status) VALUES (?, ?, ?, 'running')",
        duckdb::params![id, started_at, trigger],
    )?;
    Ok(id)
}

fn finish_run(
    store: &Store,
    id: i64,
    status: &str,
    local_path: Option<&str>,
    remote_key: Option<&str>,
    bytes: i64,
    checksum: Option<&str>,
    error: Option<&str>,
) {
    let conn = store.lock();
    let _ = conn.execute(
        "UPDATE backup_runs SET finished_at = ?, status = ?, local_path = ?, remote_key = ?, \
         bytes = ?, checksum = ?, error = ? WHERE id = ?",
        duckdb::params![
            Utc::now(),
            status,
            local_path,
            remote_key,
            bytes,
            checksum,
            error,
            id
        ],
    );
}

/// Build the configured remote target (feature-aware; `None` = local-only).
pub fn remote_from_cfg(cfg: &BackupConfig) -> Remote {
    #[cfg(feature = "object-storage")]
    {
        cfg.remote_store().unwrap_or(None).map(std::sync::Arc::new)
    }
    #[cfg(not(feature = "object-storage"))]
    {
        let _ = cfg;
        None
    }
}

/// Resolve the local backup directory (config override or `<data>/backups`).
pub fn local_backup_dir(cfg: &BackupConfig, data_dir: &Path) -> PathBuf {
    cfg.local_backup_dir
        .clone()
        .unwrap_or_else(|| data_dir.join("backups"))
}

/// Run one snapshot: CHECKPOINT → tar.gz (+sha256) → local copy → upload →
/// record. Never panics on IO errors; failures land in `backup_runs`.
pub async fn run_snapshot(
    store: Store,
    data_dir: &Path,
    cfg: &BackupConfig,
    http_port: u16,
    trigger: &str,
    remote: Remote,
) -> Result<RunSummary> {
    let started_at = Utc::now();
    let id = record_run(&store, trigger, started_at)?;

    match run_snapshot_inner(&store, data_dir, cfg, http_port, trigger, remote, id, started_at)
        .await
    {
        Ok(summary) => {
            finish_run(
                &store,
                id,
                "ok",
                summary.local_path.as_deref(),
                summary.remote_key.as_deref(),
                summary.bytes as i64,
                Some(&summary.checksum),
                None,
            );
            Ok(summary)
        }
        Err(e) => {
            finish_run(&store, id, "error", None, None, 0, None, Some(&e.to_string()));
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_snapshot_inner(
    store: &Store,
    data_dir: &Path,
    cfg: &BackupConfig,
    http_port: u16,
    trigger: &str,
    remote: Remote,
    id: i64,
    started_at: chrono::DateTime<Utc>,
) -> Result<RunSummary> {
    // 1. Checkpoint so central.duckdb is self-consistent on disk.
    store.checkpoint()?;

    let instance_id = cfg.resolved_instance_id(http_port);
    let out_dir = local_backup_dir(cfg, data_dir);
    tokio::fs::create_dir_all(&out_dir).await?;

    // 2. Stream the tar.gz, hashing the compressed bytes as we go.
    // The run id guarantees uniqueness even for back-to-back snapshots.
    let stamp = format!("{}-{id}", started_at.format("%Y%m%d-%H%M%S"));
    let tmp_path = out_dir.join(format!("{instance_id}-{stamp}.tar.gz.tmp"));
    let final_path = out_dir.join(format!("{instance_id}-{stamp}.tar.gz"));

    let raw_file = std::fs::File::create(&tmp_path)?;
    let mut hasher = Sha256::new();
    let mut enc = GzEncoder::new(raw_file, Compression::default());
    {
        let mut tar = tar::Builder::new(&mut enc);
        tar.follow_symlinks(false);
        let mut approx_bytes = 0u64;
        for entry in top_level_entries(data_dir) {
            approx_bytes += append_dir(&mut tar, data_dir, entry)?;
        }
        tar.finish()
            .map_err(|e| crate::Error::invalid_input(format!("tar finish: {e}")))?;
    }
    let mut out_file = enc.finish()?;
    out_file.flush()?;
    drop(out_file);
    // Hash the compressed file (streaming, bounded memory).
    {
        let mut f = std::fs::File::open(&tmp_path)?;
        std::io::copy(&mut f, &mut hasher)?;
    }
    let checksum = format!("{:x}", hasher.finalize());
    let bytes = std::fs::metadata(&tmp_path)?.len();

    // 3. Manifest as a sibling file (checksum of the tar can't live inside
    //    the tar).
    let (hot_rows, error_groups, hot_attrs) = {
        let conn = store.lock();
        let hot: i64 = conn
            .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
            .unwrap_or(0);
        let eg: i64 = conn
            .query_row("SELECT COUNT(*) FROM error_groups", [], |r| r.get(0))
            .unwrap_or(0);
        let attrs: Vec<String> = store
            .hot_attributes()
            .iter()
            .map(|h| h.name.clone())
            .collect();
        (hot, eg, attrs)
    };
    let manifest = SnapshotManifest {
        version: MANIFEST_VERSION,
        created_at: started_at.to_rfc3339(),
        instance_id: instance_id.clone(),
        trigger: trigger.to_string(),
        checksum: checksum.clone(),
        bytes,
        hot_rows,
        error_groups,
        hot_attributes: hot_attrs,
    };
    let manifest_path = out_dir.join(format!("{instance_id}-{stamp}.manifest.json"));
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap_or_else(|_| b"{}".to_vec()),
    )?;
    std::fs::rename(&tmp_path, &final_path)?;

    // 4. Upload (if a remote is configured).
    let mut remote_key_out: Option<String> = None;
    #[cfg(feature = "object-storage")]
    if let Some(cold) = remote {
        let date = started_at.format("%Y-%m-%d");
        let key = format!(
            "{}/{}/{}/backup.tar.gz",
            cfg.prefix.trim_matches('/'),
            instance_id,
            date
        );
        cold.upload(&final_path, &key).await?;
        let manifest_key = key.replace("backup.tar.gz", "manifest.json");
        cold.upload(&manifest_path, &manifest_key).await?;
        remote_key_out = Some(key);
    }
    #[cfg(not(feature = "object-storage"))]
    let _ = (&remote, &started_at, &instance_id, &cfg);

    // 5. Prune local copies beyond keep_last.
    prune_local(&out_dir, cfg.keep_last);

    tracing::info!(
        id,
        bytes,
        %checksum,
        local = %final_path.display(),
        remote = remote_key_out.as_deref().unwrap_or("-"),
        "backup snapshot complete"
    );

    Ok(RunSummary {
        id,
        local_path: Some(final_path.to_string_lossy().into_owned()),
        remote_key: remote_key_out,
        bytes,
        checksum,
    })
}

/// Remove oldest local `*.tar.gz` copies beyond `keep_last` (their
/// manifests go with them).
fn prune_local(dir: &Path, keep_last: usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gz"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.len() > keep_last {
        for victim in &files[..files.len() - keep_last] {
            let _ = std::fs::remove_file(victim);
            let manifest = victim.with_file_name(
                victim
                    .file_name()
                    .map(|f| f.to_string_lossy().replace(".tar.gz", ".manifest.json"))
                    .unwrap_or_default(),
            );
            let _ = std::fs::remove_file(manifest);
        }
    }
}

/// List completed runs (newest first) for the Storage page.
pub fn list_runs(store: &Store, limit: i64) -> Result<Vec<serde_json::Value>> {
    let conn = store.lock();
    let mut stmt = conn.prepare(
        "SELECT id, started_at, finished_at, trigger, status, local_path, remote_key, \
                bytes, checksum, error \
         FROM backup_runs ORDER BY id DESC LIMIT ?",
    )?;
    let rows = stmt.query_map(duckdb::params![limit], |r| {
        let started: Option<chrono::DateTime<Utc>> = r.get(1)?;
        let finished: Option<chrono::DateTime<Utc>> = r.get(2)?;
        Ok(serde_json::json!({
            "id": r.get::<_, Option<i64>>(0)?,
            "started_at": started.map(|t| t.to_rfc3339()),
            "finished_at": finished.map(|t| t.to_rfc3339()),
            "trigger": r.get::<_, Option<String>>(3)?,
            "status": r.get::<_, Option<String>>(4)?,
            "local_path": r.get::<_, Option<String>>(5)?,
            "remote_key": r.get::<_, Option<String>>(6)?,
            "bytes": r.get::<_, Option<i64>>(7)?,
            "checksum": r.get::<_, Option<String>>(8)?,
            "error": r.get::<_, Option<String>>(9)?,
        }))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// =====================================================================
// Restore
// =====================================================================

#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreSummary {
    pub target_dir: String,
    pub checksum: String,
    pub manifest: SnapshotManifest,
}

/// Restore `from` (a local tar.gz path or `s3://bucket/key`) into `into`
/// (must exist as a dir or be creatable; must NOT be the active data dir —
/// the caller enforces that). Verifies the archive's sha256 against the
/// sibling manifest, extracts, then sanity-opens the DuckDB file.
pub async fn restore_snapshot(
    from: &str,
    into: &Path,
    remote: Remote,
) -> Result<RestoreSummary> {
    #[cfg(not(feature = "object-storage"))]
    if from.starts_with("s3://") {
        return Err(crate::Error::config(
            "s3:// restore requires a build with --features object-storage",
        ));
    }
    #[allow(unused_variables)]
    let (tar_bytes, manifest): (Vec<u8>, SnapshotManifest) = if let Some(path) = from.strip_prefix("s3://") {
        #[cfg(feature = "object-storage")]
        let cold = remote
            .ok_or_else(|| crate::Error::config("s3:// restore requires backup.backend/bucket to be configured"))?;
        #[cfg(not(feature = "object-storage"))]
        let cold: () = unreachable!("guarded above");
        // Accept either a full s3://bucket/key or just the key under the
        // configured prefix/bucket.
        #[cfg(feature = "object-storage")]
        {
            let key = path.split_once('/').map(|(_, k)| k).unwrap_or(path);
            let mkey = key.replace("backup.tar.gz", "manifest.json");
            let mbytes = cold.fetch(mkey.as_str()).await?;
            let manifest: SnapshotManifest = serde_json::from_slice(&mbytes)
                .map_err(|e| crate::Error::invalid_input(format!("bad manifest: {e}")))?;
            (cold.fetch(key).await?, manifest)
        }
        #[cfg(not(feature = "object-storage"))]
        {
            unreachable!("guarded above")
        }
    } else {
        let p = Path::new(from);
        if !p.exists() {
            return Err(crate::Error::invalid_input(format!("no such file: {from}")));
        }
        let mpath = p.with_file_name(
            p.file_name()
                .map(|f| f.to_string_lossy().replace(".tar.gz", ".manifest.json"))
                .unwrap_or_default(),
        );
        let mbytes = std::fs::read(&mpath).map_err(|_| {
            crate::Error::invalid_input(format!(
                "missing sibling manifest {} (cannot verify checksum)",
                mpath.display()
            ))
        })?;
        let manifest: SnapshotManifest = serde_json::from_slice(&mbytes)
            .map_err(|e| crate::Error::invalid_input(format!("bad manifest: {e}")))?;
        (std::fs::read(p)?, manifest)
    };

    // Verify checksum BEFORE touching the target dir.
    let mut hasher = Sha256::new();
    hasher.update(&tar_bytes);
    let checksum = format!("{:x}", hasher.finalize());
    if checksum != manifest.checksum {
        return Err(crate::Error::invalid_input(format!(
            "checksum mismatch: manifest {} vs actual {}",
            manifest.checksum, checksum
        )));
    }

    // Extract.
    tokio::fs::create_dir_all(into).await?;
    let is_empty = std::fs::read_dir(into)?.next().is_none();
    if !is_empty {
        return Err(crate::Error::invalid_input(format!(
            "target dir {} is not empty (use a fresh directory)",
            into.display()
        )));
    }
    let decoder = flate2::read::GzDecoder::new(&tar_bytes[..]);
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    unpack_in_dir(&mut tar, into)?;

    // Sanity-open the restored DuckDB.
    {
        let db = into.join("central.duckdb");
        if !db.exists() {
            return Err(crate::Error::invalid_input(
                "snapshot did not contain central.duckdb",
            ));
        }
        let conn = duckdb::Connection::open(&db)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))?;
        tracing::info!(restored_rows = n, "restored duckdb sanity check passed");
    }

    Ok(RestoreSummary {
        target_dir: into.to_string_lossy().into_owned(),
        checksum,
        manifest,
    })
}

/// Extract into `into`. `Archive::unpack_in` refuses path traversal
/// (`..`, absolute paths) entries itself.
fn unpack_in_dir<R: std::io::Read>(tar: &mut tar::Archive<R>, into: &Path) -> Result<()> {
    for entry in tar
        .entries()
        .map_err(|e| crate::Error::invalid_input(format!("tar read: {e}")))?
    {
        let mut entry = entry.map_err(|e| crate::Error::invalid_input(format!("tar entry: {e}")))?;
        // Entry::unpack_in refuses paths that escape `into` (traversal).
        entry
            .unpack_in(into)
            .map_err(|e| crate::Error::invalid_input(format!("extract: {e}")))?;
    }
    Ok(())
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetentionConfig;

    fn test_store(tmp: &Path) -> Store {
        let store = Store::open(
            &tmp.join("central.duckdb"),
            tmp.join("parquet"),
            vec![],
        )
        .unwrap();
        store
            .lock()
            .execute(
                "INSERT INTO logs (ts, level, service, message, raw_len) VALUES \
                 (now(), 'info', 'svc', 'hello backup', 12), \
                 (now(), 'error', 'svc', 'boom', 4)",
                [],
            )
            .unwrap();
        store
    }

    fn backup_cfg(tmp: &Path) -> BackupConfig {
        BackupConfig {
            enabled: false,
            schedule: "daily@01:30".into(),
            timezone: "UTC".into(),
            backend: String::new(),
            bucket: String::new(),
            prefix: "test/backups".into(),
            endpoint: None,
            region: None,
            keep_local_copy: true,
            local_backup_dir: Some(tmp.join("backups")),
            keep_last: 5,
            instance_id: Some("test-inst".into()),
        }
    }

    #[tokio::test]
    async fn snapshot_roundtrip_local() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = test_store(&data_dir);
        let cfg = backup_cfg(tmp.path());

        let summary = run_snapshot(
            store.clone(),
            &data_dir,
            &cfg,
            8084,
            "manual",
            None,
        )
        .await
        .unwrap();
        assert!(summary.bytes > 0);
        assert!(Path::new(summary.local_path.as_ref().unwrap()).exists());

        // Restore into a fresh dir and verify the row count survived.
        let into = tmp.path().join("restored");
        let restored = restore_snapshot(summary.local_path.as_ref().unwrap(), &into, None)
            .await
            .unwrap();
        assert_eq!(restored.manifest.hot_rows, 2);
        assert!(into.join("central.duckdb").exists());
        assert!(into.join("wal").exists() || !data_dir.join("wal").exists());
    }

    #[tokio::test]
    async fn restore_refuses_checksum_mismatch_and_nontarget() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = test_store(&data_dir);
        let cfg = backup_cfg(tmp.path());
        let summary = run_snapshot(store.clone(), &data_dir, &cfg, 8084, "manual", None)
            .await
            .unwrap();

        // Corrupt one byte of the archive → checksum must refuse.
        let p = Path::new(summary.local_path.as_ref().unwrap());
        let mut bytes = std::fs::read(p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(p, &bytes).unwrap();
        let into = tmp.path().join("restored2");
        let err = restore_snapshot(p.to_str().unwrap(), &into, None)
            .await
            .expect_err("corrupt archive must be refused");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    }

    #[tokio::test]
    async fn restore_refuses_nonempty_target() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = test_store(&data_dir);
        let cfg = backup_cfg(tmp.path());
        let summary = run_snapshot(store, &data_dir, &cfg, 8084, "manual", None)
            .await
            .unwrap();
        let into = tmp.path().join("nonempty");
        std::fs::create_dir_all(&into).unwrap();
        std::fs::write(into.join("stale.txt"), b"x").unwrap();
        let err = restore_snapshot(summary.local_path.as_ref().unwrap(), &into, None)
            .await
            .expect_err("non-empty target must be refused");
        assert!(err.to_string().contains("not empty"), "{err}");
    }

    #[tokio::test]
    async fn keep_last_prunes_old_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = test_store(&data_dir);
        let mut cfg = backup_cfg(tmp.path());
        cfg.keep_last = 2;
        for _ in 0..4 {
            run_snapshot(store.clone(), &data_dir, &cfg, 8084, "manual", None)
                .await
                .unwrap();
        }
        let dir = local_backup_dir(&cfg, &data_dir);
        let gz: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|e| e.to_str()) == Some("gz"))
            .collect();
        assert_eq!(gz.len(), 2, "keep_last prunes old snapshots");
    }

    #[test]
    fn manifest_version_is_current() {
        assert_eq!(MANIFEST_VERSION, 1);
        // Retention parse helpers used by config validation.
        assert!(RetentionConfig::default().hot_max_bytes_value().is_none());
    }
}
