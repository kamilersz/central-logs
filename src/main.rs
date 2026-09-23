//! central-logs main binary.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use central_logs::alerts::spawn_alert_task;
use central_logs::analytics::anomaly::detect_anomalies_once;
use central_logs::config::{Cli, McpMode};
use central_logs::ingest::enrich::Enricher;
use central_logs::ingest::parse::Parser as LogParser;
use central_logs::ingest::spawn_ingest_workers;
use central_logs::insert::counters::InsertCountersRef;
use central_logs::insert::http::{router as http_router, InsertState};
use central_logs::insert::syslog::{spawn_syslog_listeners, SyslogState};
use central_logs::store::compact::spawn_compaction_task;
use central_logs::store::rollup::{spawn_rollup_task, RollupConfig};
use central_logs::store::{DuckDbTuning, Store};
use central_logs::wal::meta::WalMeta;
use central_logs::wal::writer::WalWriter;

#[cfg(feature = "mcp")]
use central_logs::mcp::server::{spawn_mcp_sse, spawn_mcp_stdio};

#[cfg(feature = "dashboard")]
use central_logs::web::{router as web_router, WebState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load a local .env (if present) before config parsing so
    // CENTRAL_LOGS_* keys pinned there are stable across restarts/resets.
    // Pre-existing environment variables take precedence; a missing file is
    // fine.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let cfg = central_logs::config::load(&cli)?;

    let filter = EnvFilter::try_new(&cfg.log_filter).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    if cli.print_config {
        println!("{}", serde_json::to_string_pretty(&cfg).unwrap_or_default());
        return Ok(());
    }

    // Bring up storage layers.
    std::fs::create_dir_all(&cfg.data_dir)?;
    let meta_path = cfg.data_dir.join("meta.redb");
    let meta = Arc::new(WalMeta::open(&meta_path).context("opening WAL meta")?);

    // ── Restore-from-snapshot (pre-boot) ────────────────────────────────
    // Runs BEFORE the store opens: extracts the archive into the data dir
    // and lets normal startup proceed against it. See docs/OPERATIONS.md.
    if let Some(from) = cli.restore_from.as_deref() {
        let data_dir = cli.data_dir.clone().unwrap_or_else(|| cfg.data_dir.clone());
        let target = data_dir.clone();
        if target.exists() {
            let is_empty = std::fs::read_dir(&target)
                .map(|mut rd| rd.next().is_none())
                .unwrap_or(true);
            if !is_empty {
                if !cli.restore_force_wipe {
                    return Err(anyhow::anyhow!(
                        "restore target {} is not empty; use --restore-force-wipe to clear it",
                        target.display()
                    ));
                }
                std::fs::remove_dir_all(&target)?;
                std::fs::create_dir_all(&target)?;
            }
        } else {
            std::fs::create_dir_all(&target)?;
        }
        tracing::warn!(from, into = %target.display(), "restoring snapshot before boot");
        let remote = backup_remote(&cfg.backup);
        let summary = tokio::runtime::Handle::current().block_on(
            central_logs::store::backup::restore_snapshot(from, &target, remote),
        )?;
        tracing::info!(
            checksum = %summary.checksum,
            rows = summary.manifest.hot_rows,
            "restore complete; continuing startup"
        );
    }

    let duckdb_tuning = DuckDbTuning {
        memory_limit: cfg.duckdb_memory_limit.clone(),
        threads: cfg.duckdb_threads,
    };
    if !duckdb_tuning.memory_limit.is_empty() || duckdb_tuning.threads > 0 {
        tracing::info!(
            memory_limit = %cfg.duckdb_memory_limit,
            threads = cfg.duckdb_threads,
            "applying duckdb resource limits"
        );
    }
    let store = Store::open_with(
        &cfg.duckdb_path,
        cfg.parquet_dir.clone(),
        cfg.hot_attributes.clone(),
        &duckdb_tuning,
    )
    .context("opening DuckDB store")?;
    if !cfg.hot_attributes.is_empty() {
        tracing::info!(
            count = cfg.hot_attributes.len(),
            cols = cfg
                .hot_attributes
                .iter()
                .map(|h| format!("{}:{}", h.name, h.duckdb_type))
                .collect::<Vec<_>>()
                .join(", "),
            "promoted hot attributes",
        );
    }

    // Parser owns the hot-attribute list; shared between ingest workers.
    let parser = Arc::new(LogParser::with_options(
        cfg.hot_attributes.clone(),
        cfg.drop_attributes.clone(),
        cfg.unwrap_message_json,
    ));

    // Open WAL writer; hand out InsertHandle to insert endpoints.
    let (writer, handle) = WalWriter::new(
        cfg.data_dir.join("wal"),
        cfg.wal_segment_max_bytes,
        meta.clone(),
        cfg.insert_channel_depth,
        cfg.wal_batch_max_records,
        cfg.wal_batch_delay(),
    )?;
    // Health gauges shared between writer, insert handle, and /metrics.
    let wal_gauges = writer.gauges().clone();
    let counters = InsertCountersRef::new();

    // Set up ingest enricher.
    #[cfg(feature = "geoip")]
    let geo = if let Some(p) = &cfg.geoip_db_path {
        match central_logs::ingest::enrich::GeoEnricher::open(p) {
            Ok(g) => Some(Arc::new(g)),
            Err(e) => {
                tracing::warn!(?e, "geoip enricher open failed; skipping");
                None
            }
        }
    } else {
        None
    };

    let enricher = Arc::new(Enricher::new(
        #[cfg(feature = "geoip")]
        geo,
        "unknown",
        &hostname::get()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "localhost".to_string()),
    ));

    // Cancellation token for graceful shutdown.
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        // Handle both SIGINT (ctrl-c) and SIGTERM (systemd stop / docker
        // stop). Without SIGTERM handling systemd kills the process outright
        // and DuckDB never checkpoints — the next start then fails to replay
        // the dirty WAL (observed as `Failure while replaying WAL file`).
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
        shutdown_clone.cancel();
    });

    // 1) WAL writer task.
    let wal_shutdown = shutdown.clone();
    let wal_handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = wal_shutdown.cancelled() => {
                tracing::info!("wal writer received shutdown");
            }
            res = writer.run() => {
                if let Err(e) = res {
                    tracing::error!(?e, "wal writer exited with error");
                }
            }
        }
    });

    // Spawn a pruner that deletes fully-ingested segments.
    let _pruner = WalWriter::spawn_pruner(cfg.data_dir.join("wal"), meta.clone());

    // 2) Ingest workers.
    //
    // Error tracking (docs/ERROR_TRACKING.md): a shared ErrorTracker folds
    // error-level rows into `error_groups` after each committed batch and
    // emits event-time notifications (group.created / group.regressed) to
    // the configured webhooks. A reconciliation job periodically recomputes
    // group counts from retained events.
    let (error_tracker, error_notify_rx) = if cfg.error_tracking.enabled {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Some(Arc::new(central_logs::errors::ErrorTracker {
                cfg: cfg.error_tracking.clone(),
                tx,
            })),
            Some(rx),
        )
    } else {
        (None, None)
    };
    if let Some(rx) = error_notify_rx {
        let hooks = cfg.error_tracking.notify.webhooks.clone();
        if !hooks.is_empty() {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;
            let notify_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                loop {
                    tokio::select! {
                        biased;
                        _ = notify_shutdown.cancelled() => break,
                        ev = rx.recv() => {
                            let Some(ev) = ev else { break };
                            for url in &hooks {
                                if let Err(e) = client.post(url).json(&ev).send().await {
                                    tracing::warn!(?e, url, "error-tracking notification failed");
                                }
                            }
                        }
                    }
                }
            });
        }
    }
    {
        let rec_store = store.clone();
        let rec_shutdown = shutdown.clone();
        let every =
            std::time::Duration::from_secs(cfg.error_tracking.reconcile_interval_secs.max(60));
        tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.tick().await; // first tick fires immediately — skip it
            loop {
                tokio::select! {
                    biased;
                    _ = rec_shutdown.cancelled() => break,
                    _ = t.tick() => {
                        let conn = rec_store.lock();
                        match central_logs::errors::reconcile(&conn) {
                            Ok(n) if n > 0 => {
                                tracing::debug!(groups = n, "error-group reconciliation")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(?e, "error-group reconciliation failed"),
                        }
                    }
                }
            }
        });
    }

    let ingest_handles = spawn_ingest_workers(
        cfg.ingest_workers,
        cfg.data_dir.join("wal"),
        meta.clone(),
        store.clone(),
        enricher.clone(),
        parser.clone(),
        cfg.ingest_batch_size,
        cfg.ingest_flush_interval(),
        error_tracker.clone(),
    );

    // 3) Rollup + compaction jobs.
    let _rollup = spawn_rollup_task(store.conn(), RollupConfig::default(), cfg.rollup_interval());

    let _compaction = spawn_compaction_task(
        store.conn(),
        cfg.parquet_dir.clone(),
        Arc::new(cfg.hot_attributes.clone()),
        cfg.hot_tier_hours,
        cfg.retention_days,
        cfg.parquet_compression.clone(),
        Arc::new(cfg.service_retention.clone()),
        cfg.compaction_interval(),
    );

    // Optional object-storage cold-tier archive (feature: object-storage).
    // Mirrors compacted Parquet to S3/GCS and follows local retention.
    #[cfg(feature = "object-storage")]
    if cfg.cold_storage.enabled {
        match central_logs::store::cold::ColdStore::from_config(&cfg.cold_storage) {
            Ok(cs) => {
                tracing::info!(
                    backend = %cfg.cold_storage.backend,
                    bucket = %cfg.cold_storage.bucket,
                    keep_local = cfg.cold_storage.keep_local,
                    "cold-tier object-storage archive enabled"
                );
                central_logs::store::cold::spawn_cold_sync(
                    Arc::new(cs),
                    store.clone(),
                    cfg.parquet_dir.clone(),
                    cfg.cold_storage.keep_local,
                    std::time::Duration::from_secs(60),
                );
            }
            Err(e) => {
                tracing::error!(?e, "cold-storage init failed; continuing local-only");
            }
        }
    }

    // Hot-tier size cap (`retention.hot_max_bytes`): periodically compact
    // oldest rows to Parquet until SUM(raw_len) is back under the cap.
    if let Some(cap) = cfg.retention.hot_max_bytes_value() {
        let cap_store = store.clone();
        let cap_dir = cfg.parquet_dir.clone();
        let cap_hot = Arc::new(cfg.hot_attributes.clone());
        let cap_rules = Arc::new(cfg.service_retention.clone());
        let codec = cfg.parquet_compression.clone();
        let shutdown_cap = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_cap.cancelled() => break,
                    _ = tick.tick() => {
                        let conn = cap_store.lock();
                        match central_logs::store::compact::enforce_hot_cap(
                            &conn,
                            &cap_dir,
                            &cap_hot,
                            cap,
                            &codec,
                            &cap_rules,
                        ) {
                            Ok(n) if n > 0 => {
                                tracing::info!(rows = n, "hot size cap enforced; rows compacted")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(?e, "hot size cap enforcement failed"),
                        }
                    }
                }
            }
        });
    }

    // Backup scheduler (`[backup]`): timezone-aware daily time, or a size
    // trigger on newly compacted Parquet. Manual /api/ops/backup works even
    // when enabled = false.
    {
        let bcfg = cfg.backup.clone();
        let sched = bcfg.parsed_schedule().ok();
        let bstore = store.clone();
        let bdata = cfg.data_dir.clone();
        let bport = cfg.http_port;
        let shutdown_b = shutdown.clone();
        let last_cold_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        tokio::spawn(async move {
            // Local dir exists even when disabled (manual backups use it).
            let _ = tokio::fs::create_dir_all(central_logs::store::backup::local_backup_dir(
                &bcfg, &bdata,
            ))
            .await;
            if !bcfg.enabled {
                return; // manual-only mode
            }
            let remote = backup_remote(&bcfg);
            use chrono::Utc;
            let tz = bcfg.tz();
            let mut next_daily: Option<chrono::DateTime<chrono_tz::Tz>> = None;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_b.cancelled() => break,
                    _ = tick.tick() => {
                        match sched {
                            Some(central_logs::config::Schedule::Daily { at }) => {
                                // Compute the next local run time once, then
                                // fire when the tz-aware wall clock passes it.
                                if next_daily.is_none() {
                                    let now_local = chrono::Utc::now().with_timezone(&tz);
                                    let today = now_local
                                        .date_naive()
                                        .and_time(at);
                                    let today_utc = today.and_local_timezone(tz).single()
                                        .map(|d| d.with_timezone(&Utc));
                                    next_daily = Some(match today_utc {
                                        Some(t) if t > Utc::now() =>
                                            today.and_local_timezone(tz).single().unwrap().with_timezone(&tz),
                                        _ => {
                                            let tomorrow = (now_local.date_naive() + chrono::Duration::days(1)).and_time(at);
                                            tomorrow
                                                .and_local_timezone(tz)
                                                .single()
                                                .map(|d| d.with_timezone(&tz))
                                                .unwrap_or_else(|| chrono::Utc::now().with_timezone(&tz))
                                        }
                                    });
                                }
                                if let Some(next) = next_daily {
                                    if chrono::Utc::now() >= next.with_timezone(&chrono::Utc) {
                                        next_daily = None; // recompute next occurrence
                                        let _ = central_logs::store::backup::run_snapshot(
                                            bstore.clone(), &bdata, &bcfg, bport, "schedule", remote.clone(),
                                        ).await;
                                    }
                                }
                            }
                            Some(central_logs::config::Schedule::Size(threshold)) => {
                                let state = central_logs::store::store_state(&bstore.lock());
                                let cold = state.cold_parquet_bytes;
                                let last = last_cold_bytes.load(std::sync::atomic::Ordering::Relaxed);
                                if cold >= last.saturating_add(threshold) {
                                    last_cold_bytes.store(cold, std::sync::atomic::Ordering::Relaxed);
                                    let _ = central_logs::store::backup::run_snapshot(
                                        bstore.clone(), &bdata, &bcfg, bport, "size", remote.clone(),
                                    ).await;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });
    }
    // --backup-now: one manual snapshot right after boot.
    if cli.backup_now {
        let bstore = store.clone();
        let bcfg = cfg.backup.clone();
        let bdata = cfg.data_dir.clone();
        let bport = cfg.http_port;
        let remote = backup_remote(&cfg.backup);
        tokio::spawn(async move {
            match central_logs::store::backup::run_snapshot(
                bstore, &bdata, &bcfg, bport, "manual", remote,
            )
            .await
            {
                Ok(s) => tracing::info!(id = s.id, bytes = s.bytes, "--backup-now complete"),
                Err(e) => tracing::error!(?e, "--backup-now failed"),
            }
        });
    }

    // 4) HTTP server: insert endpoint + dashboard + query APIs.
    //
    // WAL cap (`retention.wal_max_bytes`): a monitor task flips this flag
    // when the WAL directory exceeds the cap; insert routes answer 503
    // until it clears.
    let ingest_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(cap) = cfg.retention.wal_max_bytes_value() {
        let wal_dir = cfg.data_dir.join("wal");
        let paused = ingest_paused.clone();
        let shutdown_wal = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_wal.cancelled() => break,
                    _ = tick.tick() => {
                        let size = dir_size_bytes(&wal_dir);
                        let over = size >= cap;
                        paused.store(over, std::sync::atomic::Ordering::Relaxed);
                        if over {
                            tracing::warn!(bytes = size, cap, "wal cap exceeded; ingest paused");
                        }
                    }
                }
            }
        });
    }

    let insert_state = InsertState {
        handle: handle.clone(),
        counters: counters.clone(),
        backpressure_timeout: cfg.insert_backpressure_timeout(),
        peer_header: None,
        ingest_paused: ingest_paused.clone(),
    };
    let mut http_app = http_router(insert_state);

    // Splunk HEC-compatible ingest (Docker `splunk` driver) + Kubernetes
    // audit webhook — mounted in all builds so `--no-default-features`
    // deployments keep the insert surface complete.
    http_app = http_app
        .merge(central_logs::insert::splunk_hec_router(
            central_logs::insert::splunk_hec::SplunkHecState {
                handle: handle.clone(),
                counters: counters.clone(),
                cfg: Arc::new(parking_lot::Mutex::new(cfg.clone())),
                backpressure_timeout: cfg.insert_backpressure_timeout(),
                ingest_paused: ingest_paused.clone(),
            },
        ))
        .merge(central_logs::insert::k8s_audit::router(
            central_logs::insert::k8s_audit::K8sAuditState {
                handle: handle.clone(),
                counters: counters.clone(),
                cfg: Arc::new(parking_lot::Mutex::new(cfg.ingest.k8s_audit.clone())),
                backpressure_timeout: cfg.insert_backpressure_timeout(),
                ingest_paused: ingest_paused.clone(),
            },
        ));

    // Shared alert-notification infrastructure (HTTP for webhook/telegram
    // test sends, SMTP config for email channels).
    let smtp = Arc::new(cfg.smtp.clone());
    let alert_http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("building alert HTTP client")?;

    #[cfg(feature = "dashboard")]
    {
        let web_state = WebState {
            store: store.clone(),
            meta: meta.clone(),
            counters: counters.clone(),
            cfg: Arc::new(parking_lot::Mutex::new(cfg.clone())),
            wal: wal_gauges.clone(),
        };
        // v1 askama dashboard (HTML routes) — kept as a fallback.
        let web_r = web_router(web_state);

        // v2 JSON API for the SPA: filter DSL, dashboard CRUD, alert approval,
        // AI query, missing §4 dashboards.
        // One shared audit handle (single drop counter) cloned into both
        // API and auth-api state.
        let audit = central_logs::audit::AuditHandle::new(handle.clone(), counters.clone());
        let api_state = central_logs::web::api::ApiState {
            store: store.clone(),
            meta: meta.clone(),
            llm: Arc::new(cfg.llm.clone()),
            audit: audit.clone(),
            query_limits: Arc::new(cfg.service_query_limits.clone()),
            smtp: smtp.clone(),
            http: alert_http.clone(),
            data_dir: cfg.data_dir.clone(),
            cold_dir: cfg.parquet_dir.clone(),
            retention: cfg.retention.clone(),
            cold_retention_days: cfg.retention_days,
            service_retention_count: cfg.service_retention.len(),
            backup: cfg.backup.clone(),
            http_port: cfg.http_port,
        };
        let api_r = central_logs::web::api::router(api_state.clone());

        // Housekeeping surface: storage overview, cold-tier browsing,
        // backup/restore (admin-gated writes).
        let ops_r = central_logs::web::ops_api::router(api_state.clone());

        // Error-groups management API (list / detail / resolve / AI explain).
        let errors_r = central_logs::web::errors_api::router(api_state);

        // Sentry-SDK-compatible ingest: /api/{project}/envelope[/] and
        // /api/{project}/store[/] (error tracking, docs/ERROR_TRACKING.md).
        let sentry_r =
            central_logs::web::sentry_api::router(central_logs::web::sentry_api::SentryState {
                handle: handle.clone(),
                counters: counters.clone(),
                backpressure_timeout: cfg.insert_backpressure_timeout(),
                cfg: cfg.error_tracking.clone(),
                ingest_paused: ingest_paused.clone(),
            });

        // SPA catch-all — serves the embedded React build (or a placeholder
        // if `web/dist` hasn't been built yet). Use `.nest` so the SPA's routes
        // are tried LAST after the stateful routers above.
        let spa_r = central_logs::web::spa_router();

        // OWASP A01/A07: build the auth state.
        //
        // 1. Load existing CRUD API keys from DuckDB into the in-memory cache.
        // 2. If the table is empty (fresh install) AND no legacy static
        //    `http_api_key` is configured, mint a one-time bootstrap admin
        //    token and print it to stderr. The operator uses it to log in and
        //    create scoped per-source keys.
        // 3. If `http_api_key` is set (legacy single-static-key mode), accept
        //    it as an implicit admin credential. Existing deployments keep
        //    working unchanged.
        let conn = store.conn();
        let conn_guard = conn.lock();
        let keys = central_logs::web::auth::load_api_keys(&conn_guard);
        let table_empty = central_logs::web::auth::api_keys_table_is_empty(&conn_guard);
        let needs_bootstrap = table_empty && cfg.http_api_key.trim().is_empty();
        let bootstrap_token: Option<String> = if needs_bootstrap {
            match central_logs::web::auth::insert_bootstrap_admin_key(
                &conn_guard,
                "bootstrap-admin",
            ) {
                Ok(raw) => {
                    tracing::warn!(
                        "No API keys found and no http_api_key set; minting a one-time \
                         bootstrap admin token. Use it to log in at /login, then create \
                         scoped per-source keys at /v1/api-keys and revoke this one."
                    );
                    eprintln!(
                        "\n========================================\n\
                         central-logs bootstrap admin token (shown ONCE):\n\
                         {raw}\n\
                         Log in at /login with this token.\n\
                         ========================================\n"
                    );
                    Some(raw)
                }
                Err(e) => {
                    tracing::warn!(?e, "failed to mint bootstrap admin token");
                    None
                }
            }
        } else {
            None
        };
        drop(conn_guard);

        let mut keys = keys;
        if let Some(raw) = &bootstrap_token {
            // Add the just-inserted bootstrap key to the cache without a
            // second DB read. Reconstruct its in-memory form from the raw
            // value (we know it's admin-scoped).
            keys.push(central_logs::web::auth::ApiKey {
                id: 0, // not used for anything before the cache refresh; the
                // real id lives in DuckDB and is loaded on next boot.
                name: "bootstrap-admin".into(),
                key_hash: central_logs::web::auth::hash_token(raw),
                key_prefix: central_logs::web::auth::token_display_prefix(raw),
                scopes: central_logs::web::auth::Scope::Admin as u8,
                created_at: chrono::Utc::now(),
                last_used_at: None,
                revoked_at: None,
            });
        }
        // Splunk HEC static token (`[ingest.splunk_hec].token`): the Docker
        // `splunk` driver requires *some* token; register it as an
        // insert-scoped synthetic key so `Authorization: Splunk <token>`
        // passes the auth middleware like a CRUD key would.
        let hec_token = cfg.ingest.splunk_hec.token.trim().to_string();
        if !hec_token.is_empty() {
            keys.push(central_logs::web::auth::ApiKey {
                id: -1,
                name: "splunk-hec-token".into(),
                key_hash: central_logs::web::auth::hash_token(&hec_token),
                key_prefix: central_logs::web::auth::token_display_prefix(&hec_token),
                scopes: central_logs::web::auth::Scope::Insert as u8,
                created_at: chrono::Utc::now(),
                last_used_at: None,
                revoked_at: None,
            });
        }
        let static_admin = if cfg.http_api_key.trim().is_empty() {
            None
        } else {
            Some(cfg.http_api_key.clone())
        };
        let static_admin_set = static_admin.is_some();
        let auth_state =
            central_logs::web::auth::AuthState::with_store(keys, static_admin, Some(store.clone()));
        let auth_enabled = auth_state.auth_enabled();
        if auth_enabled {
            tracing::info!(
                keys = auth_state.list_keys().len(),
                static_admin = static_admin_set,
                "HTTP auth enabled (OWASP A01/A07)"
            );
        }

        // Auth + login + api-key CRUD routes (mounted before SPA catch-all).
        let auth_api_state = central_logs::web::auth_api::AuthApiState {
            auth: auth_state.clone(),
            store: store.clone(),
            audit: audit.clone(),
        };
        let auth_r = central_logs::web::auth_api::router(auth_api_state);

        http_app = http_app
            .merge(web_r)
            .merge(api_r)
            .merge(errors_r)
            .merge(ops_r)
            .merge(sentry_r)
            .merge(auth_r)
            .merge(spa_r);

        tracing::info!(
            "routers merged: dashboard + api + errors + sentry + auth + spa (axum will try more-specific routes before the SPA catch-all)"
        );

        // Apply the auth middleware as the OUTERMOST layer so unauthorized
        // requests are rejected before any body allocation happens. Public
        // paths (/login, /api/auth/login, /health) bypass inside the
        // middleware; everything else requires a valid cookie/bearer.
        if auth_enabled {
            http_app = http_app.layer(axum::middleware::from_fn_with_state(
                auth_state,
                central_logs::web::auth::require_auth,
            ));
        }
    }

    // OWASP A05: cap request bodies to prevent unbounded-memory DoS. The
    // insert endpoint reads the full body into Bytes; without a cap a single
    // connection can OOM the process. The layer wraps the body stream so it
    // doesn't allocate until the handler actually reads — applying it
    // globally is cheap and protects every route.
    //
    // Request decompression sits INSIDE the byte limit (applied first), so
    // the limit counts COMPRESSED wire bytes — Sentry SDKs gzip their
    // envelopes (error tracking, docs/ERROR_TRACKING.md).
    {
        http_app = http_app.layer(tower_http::decompression::RequestDecompressionLayer::new());
        let max_bytes = cfg.http_max_body_bytes;
        http_app = http_app.layer(tower_http::limit::RequestBodyLimitLayer::new(max_bytes));
        tracing::debug!(max_bytes, "HTTP request body limit applied");
    }

    let http_shutdown = shutdown.clone();
    let http_listen = cfg.http_listen();
    let http_handle = tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(&http_listen).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %http_listen, ?e, "http bind failed");
                return;
            }
        };
        tracing::info!(addr = %http_listen, "HTTP API + dashboard listening");
        // `into_make_connect_info()` populates the request's `ConnectInfo`
        // extension with the TCP peer, so handlers can resolve the client
        // IP for audit logging when no proxy header is present.
        axum::serve(
            listener,
            http_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            http_shutdown.cancelled().await;
            tracing::info!("http server graceful shutdown triggered");
        })
        .await
        .ok();
    });

    // 5) Syslog listeners.
    let syslog_state = SyslogState {
        handle: handle.clone(),
        counters: counters.clone(),
    };
    let syslog_udp = if cfg.syslog_udp_enabled {
        Some(cfg.syslog_udp_bind.as_str())
    } else {
        None
    };
    let syslog_tcp = if cfg.syslog_tcp_enabled {
        Some(cfg.syslog_tcp_bind.as_str())
    } else {
        None
    };
    let _syslog =
        spawn_syslog_listeners(syslog_udp, syslog_tcp, syslog_state, shutdown.clone()).await;

    // 5b) Engine/cluster push listeners: GELF (Docker `gelf` log driver) and
    // Fluentd forward protocol (Docker `fluentd` log driver). The Splunk HEC
    // + Kubernetes audit endpoints ride the main HTTP router below.
    {
        use central_logs::insert::{spawn_fluentd_listener, spawn_gelf_listeners, FluentdState, GelfState};
        if cfg.ingest.gelf.enabled {
            let chunk_timeout =
                std::time::Duration::from_secs(cfg.ingest.gelf.chunk_timeout_secs.max(1));
            let _gelf = spawn_gelf_listeners(
                (!cfg.ingest.gelf.udp_bind.trim().is_empty())
                    .then_some(cfg.ingest.gelf.udp_bind.as_str()),
                (!cfg.ingest.gelf.tcp_bind.trim().is_empty())
                    .then_some(cfg.ingest.gelf.tcp_bind.as_str()),
                GelfState {
                    handle: handle.clone(),
                    counters: counters.clone(),
                },
                shutdown.clone(),
                chunk_timeout,
            )
            .await;
        }
        if cfg.ingest.fluentd.enabled {
            let _fluentd = spawn_fluentd_listener(
                Some(cfg.ingest.fluentd.tcp_bind.as_str()),
                FluentdState {
                    handle: handle.clone(),
                    counters: counters.clone(),
                    ack: cfg.ingest.fluentd.ack,
                },
                shutdown.clone(),
            )
            .await;
        }
    }

    // 5c) Built-in pull collectors (Docker Engine API + Kubernetes API).
    {
        use central_logs::collector::{docker as docker_collector, kubernetes as k8s_collector};
        let _docker_c = docker_collector::spawn(
            cfg.collector.docker.clone(),
            handle.clone(),
            counters.clone(),
            shutdown.clone(),
        );
        let _k8s_c = k8s_collector::spawn(
            cfg.collector.kubernetes.clone(),
            handle.clone(),
            counters.clone(),
            shutdown.clone(),
        );
    }

    // 6) MCP server (if enabled).
    #[cfg(feature = "mcp")]
    let mcp_handle = {
        let mode = cfg.mcp_mode;
        let store = store.clone();
        let meta = meta.clone();
        let enable_alert_tool = cfg.enable_alert_mcp_tool;
        let shutdown = shutdown.clone();
        let mcp_http_bind = cfg.mcp_http_bind.clone();
        let mcp_api_key = cfg.mcp_api_key.clone();
        match mode {
            McpMode::Off => None,
            McpMode::Stdio => Some(tokio::spawn(async move {
                if let Err(e) = spawn_mcp_stdio(store, meta, enable_alert_tool, shutdown).await {
                    tracing::error!(?e, "stdio MCP server exited");
                }
            })),
            McpMode::Sse => Some(tokio::spawn(async move {
                if let Err(e) = spawn_mcp_sse(
                    mcp_http_bind,
                    mcp_api_key,
                    store,
                    meta,
                    enable_alert_tool,
                    shutdown,
                )
                .await
                {
                    tracing::error!(?e, "SSE MCP server exited");
                }
            })),
        }
    };

    // 7) Periodic anomaly detector. (Forecasts can be triggered on demand via MCP.)
    let anom_store = store.clone();
    let anom_shutdown = shutdown.clone();
    let anomaly_every = cfg.rollup_interval() * cfg.forecast_every_n_rollups.max(1) as u32;
    let _anomaly = tokio::spawn(async move {
        let mut t = tokio::time::interval(anomaly_every);
        t.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = anom_shutdown.cancelled() => break,
                _ = t.tick() => {
                    if let Err(e) = run_anomaly_cycle(&anom_store) {
                        tracing::warn!(?e, "anomaly cycle failed");
                    }
                }
            }
        }
    });

    // 8) Alert rule evaluator: re-checks active rules against rollup/anomaly
    // data and fires webhook notifications for breaches (architecture §7).
    let _alerts = spawn_alert_task(
        store.clone(),
        alert_http,
        Some(smtp),
        cfg.alert_eval_interval(),
        shutdown.clone(),
    );

    // Wait for ctrl-c / shutdown, then join.
    shutdown.cancelled().await;
    tracing::info!("waiting for tasks to drain");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let _ = wal_handle.await;
        let _ = http_handle.await;
        for h in ingest_handles {
            let _ = h.await;
        }
        #[cfg(feature = "mcp")]
        if let Some(h) = mcp_handle {
            let _ = h.await;
        }
    })
    .await;
    if let Err(e) = store.checkpoint() {
        tracing::warn!(?e, "duckdb checkpoint on shutdown failed");
    }
    tracing::info!("central-logs shutdown complete");
    Ok(())
}

/// Pull the latest rollup window, run the three detectors, persist anomalies.
fn run_anomaly_cycle(store: &Store) -> anyhow::Result<()> {
    use duckdb::params;
    let conn = store.conn();
    let conn = conn.lock();
    let mut stmt = conn.prepare(
        "SELECT bucket, SUM(n)::DOUBLE FROM rollup_1m GROUP BY 1 ORDER BY 1 DESC LIMIT 120",
    )?;
    let pairs: Vec<(chrono::DateTime<chrono::Utc>, f64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, chrono::DateTime<chrono::Utc>>(0)?,
                row.get::<_, f64>(1)?,
            ))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);
    if pairs.len() < 5 {
        return Ok(());
    }
    let (mut ts, mut vs): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    ts.reverse();
    vs.reverse();
    let anomalies = detect_anomalies_once("volume", &vs, &ts, 3.0)?;
    if anomalies.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO anomalies (ts, metric, score, method, severity) VALUES (?, ?, ?, ?, ?)",
        )?;
        for a in &anomalies {
            ins.execute(params![
                a.ts,
                &a.metric,
                a.score,
                a.method.as_str(),
                &a.severity,
            ])?;
        }
    }
    tx.commit()?;
    tracing::info!(n = anomalies.len(), "anomalies detected");
    Ok(())
}

mod hostname {
    pub fn get() -> Option<std::ffi::OsString> {
        std::env::var_os("HOSTNAME")
            .or_else(|| std::env::var_os("COMPUTERNAME"))
            .or_else(|| std::env::var_os("hostname"))
    }
}

/// Recursive directory size (WAL-cap monitor).
fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
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

/// Remote backup target when the object-storage feature is built and
/// `[backup]` names a backend; `None` = local-only snapshots.
#[cfg(feature = "object-storage")]
fn backup_remote(
    cfg: &central_logs::config::BackupConfig,
) -> Option<std::sync::Arc<central_logs::store::cold::ColdStore>> {
    cfg.remote_store().unwrap_or(None).map(std::sync::Arc::new)
}

#[cfg(not(feature = "object-storage"))]
fn backup_remote(_cfg: &central_logs::config::BackupConfig) -> Option<()> {
    None
}
