//! Optional object-storage cold-tier archive (feature: `object-storage`).
//!
//! LESSON_LEARNED: the single biggest cost lever in the reference stack was
//! moving cold data to object storage (~8x cheaper per GB than node disks).
//! central-logs stays single-node by design, so this is an *archive mirror*:
//!
//! - After compaction writes a Parquet file locally, the sync loop uploads
//!   it under `<prefix>/<relative-path>` (mirroring the hive layout).
//! - When local retention purges a file, the sync loop deletes the remote
//!   object too — the remote copy mirrors the local lifecycle.
//! - With `keep_local = true` (default) the local file remains the
//!   queryable cache; remote is durability/archive. With `keep_local =
//!   false` the local file is removed after upload — data is archived but
//!   NOT queryable through `logs_all` in this version.
//!
//! A `cold_files` DuckDB table tracks what has been uploaded (manifest).

use std::path::Path;
use std::sync::Arc;

use object_store::ObjectStore;

use crate::config::ColdStorageConfig;
use crate::Result;

pub struct ColdStore {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
}

impl ColdStore {
    pub fn from_config(cfg: &ColdStorageConfig) -> Result<Self> {
        let inner: Arc<dyn ObjectStore> = match cfg.backend.as_str() {
            "s3" => {
                let mut b = object_store::aws::AmazonS3Builder::new()
                    .with_bucket_name(&cfg.bucket)
                    // MinIO & friends need an explicit endpoint + plain HTTP.
                    .with_allow_http(
                        cfg.endpoint
                            .as_deref()
                            .is_some_and(|e| e.starts_with("http://")),
                    );
                if let Some(e) = &cfg.endpoint {
                    b = b.with_endpoint(e);
                }
                if let Some(r) = &cfg.region {
                    b = b.with_region(r);
                }
                Arc::new(
                    b.build()
                        .map_err(|e| crate::Error::config(format!("cold_storage s3 build: {e}")))?,
                )
            }
            "gcs" => {
                let b = object_store::gcp::GoogleCloudStorageBuilder::new()
                    .with_bucket_name(&cfg.bucket);
                Arc::new(
                    b.build().map_err(|e| {
                        crate::Error::config(format!("cold_storage gcs build: {e}"))
                    })?,
                )
            }
            other => {
                return Err(crate::Error::config(format!(
                    "cold_storage backend '{other}' not supported (use s3 | gcs)"
                )))
            }
        };
        Ok(Self {
            inner,
            prefix: cfg.prefix.trim_matches('/').to_string(),
        })
    }

    /// Remote object key for a local parquet file's path relative to the
    /// parquet dir (always forward slashes).
    pub fn key_for(&self, rel_path: &str) -> String {
        let rel = rel_path.replace('\\', "/");
        if self.prefix.is_empty() {
            rel
        } else {
            format!("{}/{}", self.prefix, rel)
        }
    }

    pub async fn upload(&self, local: &Path, key: &str) -> Result<u64> {
        let data = tokio::fs::read(local).await?;
        let len = data.len() as u64;
        let path: object_store::path::Path = key
            .try_into()
            .map_err(|e| crate::Error::config(format!("cold_storage key '{key}': {e}")))?;
        self.inner.put(&path, data.into()).await?;
        Ok(len)
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let path: object_store::path::Path = key
            .try_into()
            .map_err(|e| crate::Error::config(format!("cold_storage key '{key}': {e}")))?;
        self.inner.delete(&path).await?;
        Ok(())
    }

    /// Remote object exists? (Used by tests / verification.)
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let path: object_store::path::Path = key
            .try_into()
            .map_err(|e| crate::Error::config(format!("cold_storage key '{key}': {e}")))?;
        self.inner.head(&path).await.map(|_| true).or_else(|e| {
            if matches!(e, object_store::Error::NotFound { .. }) {
                Ok(false)
            } else {
                Err(crate::Error::config(format!("cold_storage head: {e}")))
            }
        })
    }

    /// Fetch a remote object's bytes (tests / verification).
    pub async fn fetch(&self, key: &str) -> Result<Vec<u8>> {
        let path: object_store::path::Path = key
            .try_into()
            .map_err(|e| crate::Error::config(format!("cold_storage key '{key}': {e}")))?;
        let bytes = self.inner.get(&path).await?.bytes().await?;
        Ok(bytes.to_vec())
    }
}

/// Recursively list `.parquet` files under `root` as relative paths
/// (forward slashes, subdirs included).
pub fn list_local_parquet(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out.sort();
    out
}

/// One synchronization pass:
/// 1. Upload local parquet files missing from the manifest.
/// 2. Delete remote objects whose local file has been purged.
/// 3. Honor `keep_local = false` by removing local files after upload.
///
/// The store mutex is taken only around short manifest statements — never
/// across network awaits — so this stays `Send` inside a spawned task.
///
/// Returns (uploaded, remote-deleted, local-removed).
pub async fn sync_once(
    cold: &ColdStore,
    store: &crate::store::Store,
    parquet_dir: &Path,
    keep_local: bool,
) -> Result<(usize, usize, usize)> {
    let local = list_local_parquet(parquet_dir);
    let local_set: std::collections::HashSet<&String> = local.iter().collect();

    // Manifest rows that are live (uploaded, not yet deleted).
    let mut known: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let conn = store.lock();
        let mut stmt =
            conn.prepare("SELECT rel_path, remote_key FROM cold_files WHERE deleted_at IS NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows.flatten() {
            known.insert(row.0, row.1);
        }
    }

    let mut uploaded = 0usize;
    let mut remote_deleted = 0usize;
    let mut local_removed = 0usize;

    // 1. Upload new local files.
    for rel in &local {
        if known.contains_key(rel) {
            continue;
        }
        let key = cold.key_for(rel);
        let local_path = parquet_dir.join(rel);
        match cold.upload(&local_path, &key).await {
            Ok(bytes) => {
                uploaded += 1;
                tracing::info!(rel = %rel, key = %key, bytes, "cold-tier upload");
                let conn = store.lock();
                conn.execute(
                    "INSERT OR REPLACE INTO cold_files (rel_path, remote_key, uploaded_at) \
                     VALUES (?, ?, CURRENT_TIMESTAMP)",
                    duckdb::params![rel, key],
                )?;
                drop(conn);
                if !keep_local {
                    match std::fs::remove_file(&local_path) {
                        Ok(()) => local_removed += 1,
                        Err(e) => {
                            tracing::warn!(?e, %rel, "cold-tier: failed to remove local file")
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(?e, %rel, "cold-tier upload failed; will retry next cycle");
            }
        }
    }

    // 2. Remote-delete objects whose local file is gone (purged by retention).
    for (rel, key) in &known {
        if !local_set.contains(rel) {
            match cold.delete(key).await {
                Ok(()) => {
                    remote_deleted += 1;
                    tracing::info!(%rel, %key, "cold-tier remote delete (purged locally)");
                    let conn = store.lock();
                    conn.execute(
                        "UPDATE cold_files SET deleted_at = CURRENT_TIMESTAMP WHERE rel_path = ?",
                        duckdb::params![rel],
                    )?;
                }
                Err(e) => {
                    tracing::warn!(?e, %rel, "cold-tier remote delete failed; will retry")
                }
            }
        }
    }

    Ok((uploaded, remote_deleted, local_removed))
}

/// Background sync loop — wakes periodically and reconciles remote with local.
pub fn spawn_cold_sync(
    cold: Arc<ColdStore>,
    store: crate::store::Store,
    parquet_dir: std::path::PathBuf,
    keep_local: bool,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip immediate fire
        loop {
            ticker.tick().await;
            match sync_once(&cold, &store, &parquet_dir, keep_local).await {
                Ok((0, 0, 0)) => tracing::debug!("cold-tier sync: nothing to do"),
                Ok((u, d, l)) => tracing::info!(u, d, l, "cold-tier sync pass"),
                Err(e) => tracing::warn!(?e, "cold-tier sync failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_prefixes_and_normalizes_slashes() {
        let cfg = ColdStorageConfig {
            enabled: true,
            bucket: "b".into(),
            ..ColdStorageConfig::default()
        };
        let cold = ColdStore::from_config(&cfg).expect("local store builds");
        assert_eq!(
            cold.key_for(r"date=2026-08-01\hour=05\service=api\part-1.parquet"),
            "central-logs/parquet/date=2026-08-01/hour=05/service=api/part-1.parquet"
        );
    }

    #[test]
    fn list_local_parquet_finds_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let d1 = base.join("date=2026-08-01/hour=01/service=api");
        let d2 = base.join("date=2026-08-01/hour=02/service=web");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d1.join("a.parquet"), b"x").unwrap();
        std::fs::write(d2.join("b.parquet"), b"x").unwrap();
        std::fs::write(d1.join("notes.txt"), b"x").unwrap();
        let files = list_local_parquet(base);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.ends_with(".parquet")));
        assert!(files.iter().any(|f| f.contains("service=api")));
    }

    /// Round-trip against a local-filesystem object store: upload a file,
    /// verify bytes remotely, sync deletes the remote copy after local purge.
    #[tokio::test]
    async fn sync_once_upload_then_remote_delete_on_purge() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("parquet");
        let svc_dir = local_dir.join("date=2026-08-01/hour=00/service=api");
        std::fs::create_dir_all(&svc_dir).unwrap();
        let body = b"parquet-bytes";
        std::fs::write(svc_dir.join("part-0.parquet"), body).unwrap();

        let store =
            crate::store::Store::open(&tmp.path().join("cl.duckdb"), local_dir.clone(), Vec::new())
                .unwrap();

        // In-memory-ish object store backed by a local directory.
        let remote_root = tmp.path().join("remote");
        std::fs::create_dir_all(&remote_root).unwrap();
        let cold = ColdStore {
            inner: Arc::new(
                object_store::local::LocalFileSystem::new_with_prefix(remote_root.clone()).unwrap(),
            ),
            prefix: "archive".into(),
        };

        let (u, d, l) = sync_once(&cold, &store, &local_dir, true).await.unwrap();
        assert_eq!((u, d, l), (1, 0, 0));
        let key = "archive/date=2026-08-01/hour=00/service=api/part-0.parquet";
        assert!(cold.exists(key).await.unwrap());
        assert_eq!(cold.fetch(key).await.unwrap(), body);

        // Purge locally, then sync: remote copy goes away, manifest marked.
        std::fs::remove_file(svc_dir.join("part-0.parquet")).unwrap();
        let (u, d, l) = sync_once(&cold, &store, &local_dir, true).await.unwrap();
        assert_eq!((u, d, l), (0, 1, 0));
        assert!(!cold.exists(key).await.unwrap());
        let marked: i64 = {
            let conn = store.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM cold_files WHERE deleted_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(marked, 1);
    }
}
