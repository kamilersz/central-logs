//! Ingest worker: tails WAL via a checkpointed cursor, parses, enriches, and
//! batch-inserts into DuckDB (architecture §2b.3-4).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::ingest::enrich::Enricher;
use crate::ingest::parse::Parser;
use crate::store::appender::{insert_batch, LogRow};
use crate::store::Store;
use crate::wal::cursor::{TailCursor, TailEntry};
use crate::wal::meta::{Checkpoint, WalMeta};
use crate::Result;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestStats {
    pub records_ingested: u64,
    pub batches_committed: u64,
    pub parse_errors: u64,
    pub insert_errors: u64,
    pub last_commit_ts: Option<chrono::DateTime<chrono::Utc>>,
}

/// Spawn `n` ingest worker tasks. Workers shard the WAL by segment id
/// (`segment % n == worker_id`) — the local analog of Kafka's
/// one-consumer-per-partition rule (LESSON_LEARNED). Parse/enrich/hot-attr
/// extraction run in parallel; DuckDB appends stay serialized through the
/// store mutex.
///
/// Each worker keeps an independent checkpoint. The WAL pruner deletes
/// segments only below the MINIMUM checkpoint across workers, so a lagging
/// or restarted worker never loses segments it still needs.
///
/// `error_tracker` folds error-level rows into `error_groups` and emits
/// event-time notifications (error tracking, docs/ERROR_TRACKING.md).
#[allow(clippy::too_many_arguments)]
pub fn spawn_ingest_workers(
    n: usize,
    wal_dir: PathBuf,
    meta: Arc<WalMeta>,
    store: Store,
    enricher: Arc<Enricher>,
    parser: Arc<Parser>,
    batch_size: usize,
    flush_interval: Duration,
    error_tracker: Option<Arc<crate::errors::ErrorTracker>>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::with_capacity(n);
    for worker_id in 0..n {
        let wal_dir = wal_dir.clone();
        let meta = meta.clone();
        let store = store.clone();
        let enricher = enricher.clone();
        let parser = parser.clone();
        let batch_size = batch_size;
        let flush_interval = flush_interval;
        let total = n;
        let error_tracker = error_tracker.clone();
        handles.push(tokio::spawn(async move {
            let stats = Arc::new(Mutex::new(IngestStats::default()));
            if let Err(e) = run_worker(
                worker_id as u64,
                total,
                wal_dir,
                meta,
                store,
                enricher,
                parser,
                batch_size,
                flush_interval,
                stats.clone(),
                error_tracker,
            )
            .await
            {
                tracing::error!(worker_id, ?e, "ingest worker exited with error");
            }
        }));
    }
    handles
}

/// Shard ownership: worker `k` of `total` owns segments where
/// `segment_id % total == k`.
fn owns_segment(segment_id: u64, worker_id: u64, total: usize) -> bool {
    total > 0 && segment_id % (total as u64) == worker_id
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    worker_id: u64,
    total_workers: usize,
    wal_dir: PathBuf,
    meta: Arc<WalMeta>,
    store: Store,
    enricher: Arc<Enricher>,
    parser: Arc<Parser>,
    batch_size: usize,
    flush_interval: Duration,
    stats: Arc<Mutex<IngestStats>>,
    error_tracker: Option<Arc<crate::errors::ErrorTracker>>,
) -> Result<()> {
    let start = meta.get_checkpoint(worker_id)?;
    let mut cursor = TailCursor::new(wal_dir.clone(), start);
    tracing::info!(
        worker_id,
        total_workers,
        ?start,
        hot_attrs = parser.hot().len(),
        "ingest worker starting (shard: segment_id % {total_workers} == {worker_id})"
    );

    let mut buf: Vec<LogRow> = Vec::with_capacity(batch_size);
    let mut last_flush = Instant::now();
    let mut pending_checkpoint: Option<Checkpoint> = None;
    // Phase timers (per flush-cycle aggregation; surfaced in flush! logs).
    let mut parse_ns: u128 = 0;
    let mut cursor_ns: u128 = 0;
    let mut all_ns: u128 = 0;
    let cycle0 = Instant::now();

    loop {
        let entry = {
            let t = Instant::now();
            let e = cursor.next().await;
            cursor_ns += (t.elapsed().as_nanos());
            all_ns = cycle0.elapsed().as_nanos();
            e
        }
        .map_err(|e| {
            tracing::warn!(worker_id, ?e, "ingest cursor read failed");
            e
        });
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        match entry {
            TailEntry::Record { rec, next } => {
                // Shard filter: every worker READS every frame (cheap CRC +
                // I/O), but only the owning worker parses/enriches/inserts
                // it. `next` still reflects the read position; foreign
                // records simply never enter this worker's buffer.
                if owns_segment(next.segment_id, worker_id, total_workers) {
                    let t = Instant::now();
                    let mut row = parser.parse(&rec);
                    enricher.enrich(&mut row, parse_ip_from_addr(&rec.source_addr));
                    parse_ns += t.elapsed().as_nanos();
                    buf.push(row);
                    pending_checkpoint = Some(next);
                    if buf.len() >= batch_size || last_flush.elapsed() >= flush_interval {
                        flush_timed(
                            &store,
                            &meta,
                            worker_id,
                            &mut buf,
                            pending_checkpoint,
                            &stats,
                            error_tracker.as_deref(),
                            parse_ns,
                            cursor_ns,
                            all_ns,
                        )
                        .await;
                        parse_ns = 0;
                        cursor_ns = 0;
                        all_ns = cycle0.elapsed().as_nanos();
                        last_flush = Instant::now();
                    }
                }
            }
            TailEntry::BadFrame { error, next } => {
                tracing::warn!(worker_id, ?error, "ingest: skipping bad frame");
                {
                    let mut s = stats.lock();
                    s.parse_errors += 1;
                }
                let _ = meta.set_checkpoint(worker_id, next);
            }
            TailEntry::CaughtUp { .. } => {
                if !buf.is_empty() && last_flush.elapsed() >= flush_interval {
                    flush_timed(
                        &store,
                        &meta,
                        worker_id,
                        &mut buf,
                        pending_checkpoint,
                        &stats,
                        error_tracker.as_deref(),
                        parse_ns,
                        cursor_ns,
                        all_ns,
                    )
                    .await;
                    parse_ns = 0;
                    cursor_ns = 0;
                    last_flush = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Flush with per-phase timing logs (insert / error-track / checkpoint vs
/// parse+enrich and cursor-read time). Keeps `flush` semantics identical.
#[allow(clippy::too_many_arguments)]
async fn flush_timed(
    store: &Store,
    meta: &WalMeta,
    worker_id: u64,
    buf: &mut Vec<LogRow>,
    pending_checkpoint: Option<Checkpoint>,
    stats: &Arc<Mutex<IngestStats>>,
    error_tracker: Option<&crate::errors::ErrorTracker>,
    parse_ns: u128,
    cursor_ns: u128,
    all_ns: u128,
) {
    let n = buf.len();
    let t0 = Instant::now();
    flush(store, meta, worker_id, buf, pending_checkpoint, stats, error_tracker).await;
    let ms = t0.elapsed().as_millis();
    if ms > 50 {
        tracing::debug!(
            worker_id,
            n,
            flush_ms = ms,
            parse_ms = parse_ns / 1_000_000,
            cursor_ms = cursor_ns / 1_000_000,
            cycle_ms = all_ns / 1_000_000,
            "ingest flush timing"
        );
    }
}

async fn flush(
    store: &Store,
    meta: &WalMeta,
    worker_id: u64,
    buf: &mut Vec<LogRow>,
    pending_checkpoint: Option<Checkpoint>,
    stats: &Arc<Mutex<IngestStats>>,
    error_tracker: Option<&crate::errors::ErrorTracker>,
) {
    if buf.is_empty() {
        return;
    }
    let n = buf.len();
    let conn = store.lock();
    let t_insert = Instant::now();
    let insert_res = insert_batch(&conn, store.hot_attributes(), buf);
    let insert_ms = t_insert.elapsed().as_millis();
    let t_track = Instant::now();
    let track_res = insert_res.map(|inserted| {
        // Error tracking: fold the just-committed error rows into
        // error_groups while we still hold the store lock.
        if let Some(et) = error_tracker {
            et.track(&conn, buf);
        }
        inserted
    });
    let track_ms = t_track.elapsed().as_millis();
    match track_res {
        Ok(inserted) => {
            buf.clear();
            let t_cp = Instant::now();
            if let Some(cp) = pending_checkpoint {
                if let Err(e) = meta.set_checkpoint(worker_id, cp) {
                    tracing::warn!(worker_id, ?e, "ingest: persist checkpoint failed");
                }
            }
            let cp_ms = t_cp.elapsed().as_millis();
            if insert_ms + track_ms + cp_ms > 200 {
                tracing::info!(
                    worker_id,
                    n,
                    insert_ms = insert_ms as u64,
                    track_ms = track_ms as u64,
                    checkpoint_ms = cp_ms as u64,
                    "ingest flush breakdown"
                );
            }
            let mut s = stats.lock();
            s.records_ingested += inserted as u64;
            s.batches_committed += 1;
            s.last_commit_ts = Some(chrono::Utc::now());
        }
        Err(e) => {
            tracing::warn!(worker_id, ?e, n, "ingest batch insert failed; will retry");
            let mut s = stats.lock();
            s.insert_errors += 1;
        }
    }
}

fn parse_ip_from_addr(addr: &str) -> Option<std::net::IpAddr> {
    // addr may be "ip:port" or just "ip"
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    host.parse::<std::net::IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::writer::WalWriter;

    #[test]
    fn shard_ownership_partitions_segments() {
        // Every segment belongs to exactly one worker.
        for total in 1usize..6 {
            let mut owner_counts = vec![0usize; total];
            for seg in 0u64..50 {
                let owner = (0..total as u64)
                    .filter(|w| owns_segment(seg, *w, total))
                    .count();
                assert_eq!(owner, 1, "seg {seg} total {total}");
                owner_counts[(seg % total as u64) as usize] += 1;
            }
            assert!(owner_counts.iter().all(|c| *c > 0));
        }
        assert!(owns_segment(4, 0, 2));
        assert!(!owns_segment(5, 0, 2));
        assert!(owns_segment(5, 1, 2));
    }

    /// End-to-end: multiple sharded workers ingest every appended record
    /// exactly once (at-least-once within a worker, no cross-worker
    /// duplication, no shard gaps).
    #[cfg(not(feature = "geoip"))]
    #[tokio::test]
    async fn sharded_workers_ingest_all_records_exactly_once() {
        use crate::Protocol;
        use crate::RawRecord;

        let tmp = tempfile::tempdir().expect("tempdir");
        let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).expect("wal meta"));
        let (writer, handle) = WalWriter::new(
            tmp.path().join("wal"),
            8 * 1024, // small segments → force rotation across many shards
            meta.clone(),
            1024,
            64,
            Duration::from_millis(2),
        )
        .expect("wal writer");
        tokio::spawn(async move {
            let _ = writer.run().await;
        });

        let store = crate::store::Store::open(
            &tmp.path().join("cl.duckdb"),
            tmp.path().join("parquet"),
            Vec::new(),
        )
        .expect("store");
        let enricher = Arc::new(Enricher::new("unknown", "localhost"));
        let parser = Arc::new(Parser::empty());

        let handles = spawn_ingest_workers(
            3,
            tmp.path().join("wal"),
            meta.clone(),
            store.clone(),
            enricher,
            parser,
            32,
            Duration::from_millis(20),
            None,
        );

        // Append distinct records; their bodies carry the index so we can
        // verify exactly-once downstream.
        for i in 0..300u32 {
            let rec = RawRecord {
                receive_ts: chrono::Utc::now(),
                source_addr: "127.0.0.1:9".into(),
                protocol: Protocol::HttpJson,
                raw: bytes::Bytes::from(format!(r#"{{"service":"t","msg":"row-{i}"}}"#)),
            };
            handle.append_unacked(rec).await.expect("append");
        }

        // Poll until all 300 land in DuckDB (or time out).
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut count = 0i64;
        loop {
            {
                let conn = store.lock();
                count = conn
                    .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
                    .unwrap_or(0);
            }
            if count >= 300 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timeout: only {count}/300 records ingested"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for h in handles {
            h.abort();
        }

        // Exactly once: no duplicates and no gaps.
        {
            let conn = store.lock();
            let distinct: i64 = conn
                .query_row("SELECT COUNT(DISTINCT message) FROM logs", [], |r| r.get(0))
                .expect("distinct");
            assert_eq!(distinct, 300, "duplicated messages?");
        }
    }
}
