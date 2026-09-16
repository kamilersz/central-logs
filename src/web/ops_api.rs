//! Ops + storage API (housekeeping): storage overview, cold-tier browsing,
//! backup listing/trigger, and restore — backing the SPA Storage page.
//!
//! Scopes (see `web::auth::required_scope`): reads are `read`; backup and
//! restore are `admin` (they move real bytes / replace state).

use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::store::{backup, Store};

use super::api::ApiState;

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/storage/overview", get(storage_overview))
        .route("/api/storage/cold", get(storage_cold))
        .route("/api/ops/backups", get(list_backups))
        .route("/api/ops/backup", post(trigger_backup))
        .route("/api/ops/restore", post(restore))
        .with_state(state)
}

fn err(status: StatusCode, msg: String) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn count_parquet_files(root: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("parquet") {
                n += 1;
            }
        }
    }
    n
}

fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

// =====================================================================
// GET /api/storage/overview
// =====================================================================

async fn storage_overview(State(st): State<ApiState>) -> Response {
    let state = {
        let conn = st.store.lock();
        crate::store::store_state(&conn)
    };
    let wal_bytes = dir_size_bytes(&st.data_dir.join("wal"));
    // Feature-gated module; without object-storage just count files directly.
    let cold_files = count_parquet_files(&st.cold_dir);
    let (error_groups, alerts_active) = {
        let conn = st.store.lock();
        let eg: i64 = conn
            .query_row("SELECT COUNT(*) FROM error_groups", [], |r| r.get(0))
            .unwrap_or(0);
        let aa: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM alert_rules WHERE status = 'active'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (eg, aa)
    };
    let backups = match backup::list_runs(&st.store, 10) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    Json(serde_json::json!({
        "hot_rows": state.hot_rows,
        "cold_parquet_bytes": state.cold_parquet_bytes,
        "cold_parquet_files": cold_files,
        "oldest_hot_ts": state.oldest_hot_ts.map(|t| t.to_rfc3339()),
        "newest_hot_ts": state.newest_hot_ts.map(|t| t.to_rfc3339()),
        "wal_bytes": wal_bytes,
        "error_groups": error_groups,
        "alerts_active": alerts_active,
        "retention": {
            "hot_max_age_secs": st
                .retention
                .hot_max_age_duration(24)
                .num_seconds(),
            "hot_max_bytes": st.retention.hot_max_bytes_value(),
            "wal_max_bytes": st.retention.wal_max_bytes_value(),
            "cold_retention_days": st.cold_retention_days,
            "service_rules": st.service_retention_count,
        },
        "backups": backups,
    }))
    .into_response()
}

// =====================================================================
// GET /api/storage/cold?from=YYYY-MM-DD&to=YYYY-MM-DD
// =====================================================================

#[derive(Debug, Deserialize)]
struct ColdQuery {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

async fn storage_cold(State(st): State<ApiState>, Query(q): Query<ColdQuery>) -> Response {
    // Per-date breakdown from Parquet metadata (no full scans: DuckDB's
    // parquet_metadata reads footers only).
    let dir_str = st.cold_dir.to_string_lossy().replace('\'', "''");
    let sql = format!(
        r#"
        SELECT
          regexp_extract(file_name, 'date=([0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}})', 1) AS d,
          COUNT(DISTINCT file_name) AS files,
          SUM(num_rows) AS rows,
          SUM(total_compressed_size) AS bytes
        FROM parquet_metadata('{dir_str}/**/*.parquet')
        {where_clause}
        GROUP BY d ORDER BY d DESC LIMIT 120
        "#,
        where_clause = if q.from.is_some() || q.to.is_some() {
            let mut parts = vec!["d != ''".to_string()];
            if let Some(f) = &q.from {
                parts.push(format!("d >= '{}'", f.replace('\'', "")));
            }
            if let Some(t) = &q.to {
                parts.push(format!("d <= '{}'", t.replace('\'', "")));
            }
            format!("WHERE {}", parts.join(" AND "))
        } else {
            "WHERE d != ''".to_string()
        }
    );
    let conn = st.store.lock();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::OK, format!("cold tier empty ({e})")),
    };
    let rows = stmt.query_map([], |r| {
        Ok(serde_json::json!({
            "date": r.get::<_, Option<String>>(0)?,
            "files": r.get::<_, Option<i64>>(1)?,
            "rows": r.get::<_, Option<i64>>(2)?,
            "bytes": r.get::<_, Option<i64>>(3)?,
        }))
    });
    let out = match rows {
        Ok(rows) => {
            let mut v = Vec::new();
            for row in rows.flatten() {
                v.push(row);
            }
            v
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(serde_json::json!({ "dates": out })).into_response()
}

// =====================================================================
// GET /api/ops/backups
// =====================================================================

async fn list_backups(State(st): State<ApiState>) -> Response {
    match backup::list_runs(&st.store, 50) {
        Ok(runs) => {
            // Also surface the raw local files (covers manual filesystem copies).
            let dir = backup::local_backup_dir(&st.backup, &st.data_dir);
            let mut local_files: Vec<serde_json::Value> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| {
                            let p = e.path();
                            if p.extension().and_then(|e| e.to_str()) == Some("gz") {
                                let meta = e.metadata().ok()?;
                                Some(serde_json::json!({
                                    "file": p.file_name()?.to_string_lossy(),
                                    "bytes": meta.len(),
                                    "modified": meta.modified().ok()
                                        .map(|m| chrono::DateTime::<chrono::Utc>::from(m).to_rfc3339()),
                                }))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            local_files.sort_by(|a, b| {
                b["modified"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(a["modified"].as_str().unwrap_or(""))
            });
            Json(serde_json::json!({ "runs": runs, "local_files": local_files }))
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// =====================================================================
// POST /api/ops/backup
// =====================================================================

async fn trigger_backup(State(st): State<ApiState>) -> Response {
    let store = st.store.clone();
    let data_dir = st.data_dir.clone();
    let cfg = st.backup.clone();
    let port = st.http_port;
    let remote = backup::remote_from_cfg(&cfg);
    tokio::spawn(async move {
        match backup::run_snapshot(store, &data_dir, &cfg, port, "manual", remote).await {
            Ok(s) => tracing::info!(id = s.id, bytes = s.bytes, "manual backup complete"),
            Err(e) => tracing::error!(?e, "manual backup failed"),
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "started": true, "status": "running" })),
    )
        .into_response()
}

// =====================================================================
// POST /api/ops/restore
// =====================================================================

#[derive(Debug, Deserialize)]
struct RestoreBody {
    /// Local .tar.gz path or s3://bucket/key (key under the backup prefix).
    from: String,
    /// Target data directory — must differ from the running instance's dir.
    into: PathBuf,
}

async fn restore(State(st): State<ApiState>, Json(body): Json<RestoreBody>) -> Response {
    // Refuse restoring over the running instance's data dir.
    let active = st.data_dir.canonicalize().unwrap_or_else(|_| st.data_dir.clone());
    let target_canon = body.into.canonicalize().unwrap_or_else(|_| body.into.clone());
    if active == target_canon {
        return err(
            StatusCode::BAD_REQUEST,
            "refusing to restore into the active data dir; pick a fresh directory, \
             then restart the instance with --data-dir pointing at it"
                .into(),
        );
    }
    let remote = backup::remote_from_cfg(&st.backup);
    match backup::restore_snapshot(&body.from, &body.into, remote).await {
        Ok(summary) => Json(serde_json::json!({
            "restored_into": summary.target_dir,
            "checksum": summary.checksum,
            "manifest": summary.manifest,
            "next": format!(
                "restart this instance with --data-dir {} to serve the restored data",
                summary.target_dir
            ),
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}
