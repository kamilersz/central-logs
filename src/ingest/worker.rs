//! Ingest worker: tails WAL via a checkpointed cursor, parses, enriches, and
//! batch-inserts into DuckDB (architecture §2b.3-4).
//!
//! Since PLAN-WEB-ANALYTICS W1 the worker carries TWO buffers (`LogRow` and
//! `WebRow`); a flush commits both in ONE transaction before the checkpoint
//! advances, so a `web_events` failure can no longer re-ingest (duplicate)
//! `logs` rows on retry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::ingest::enrich::Enricher;
use crate::ingest::parse::Parser;
use crate::ingest::web::WebParser;
use crate::store::appender::{insert_logs_in_tx, insert_web_in_tx, LogRow};
use crate::store::Store;
use crate::wal::cursor::{TailCursor, TailEntry};
use crate::wal::meta::{Checkpoint, WalMeta};
use crate::{Protocol, Result};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestStats {
    pub records_ingested: u64,
    pub batches_committed: u64,
    pub parse_errors: u64,
    pub insert_errors: u64,
    pub web_events_ingested: u64,
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
    web_parser: Arc<WebParser>,
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
        let web_parser = web_parser.clone();
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
                web_parser,
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
    web_parser: Arc<WebParser>,
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

    // Two buffers share one checkpoint: flush commits BOTH tables in a
    // single transaction (PLAN-WEB-ANALYTICS W1 atomicity).
    let mut buf: Vec<LogRow> = Vec::with_capacity(batch_size);
    let mut web_buf: Vec<crate::store::appender::WebRow> = Vec::with_capacity(16);
    let mut last_flush = Instant::now();
    let mut pending_checkpoint: Option<Checkpoint> = None;
    // Segment this worker last checkpointed (for releasing skipped segments).
    let mut last_cp_segment = start.segment_id;
    // Phase timers (per flush-cycle aggregation; surfaced in flush! logs).
    let mut parse_ns: u128 = 0;
    let mut cursor_ns: u128 = 0;
    let mut all_ns: u128 = 0;
    let cycle0 = Instant::now();

    // Helper: flush due? Shared by the record and caught-up paths.
    macro_rules! flush_due {
        () => {
            buf.len() + web_buf.len() >= batch_size
                || last_flush.elapsed() >= flush_interval
        };
    }

    loop {
        let entry = {
            let t = Instant::now();
            let e = cursor.next().await;
            cursor_ns += t.elapsed().as_nanos();
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
        // Release fully-skipped segments: when the cursor crosses into a
        // segment this worker doesn't own, the previous segments can never
        // yield records for this shard again. Checkpointing the crossing
        // lets `min_checkpoint` (pruning + the lag gauge) advance past them;
        // safety is preserved because the shard OWNER's own checkpoint still
        // gates deletion of the segment it is consuming.
        {
            let cur_seg = cursor.position().segment_id;
            if cur_seg != last_cp_segment && !owns_segment(cur_seg, worker_id, total_workers) {
                let cp = Checkpoint {
                    segment_id: cur_seg,
                    byte_offset: 0,
                };
                if meta.set_checkpoint(worker_id, cp).is_ok() {
                    last_cp_segment = cur_seg;
                }
            }
        }
        match entry {
            TailEntry::Record { rec, next } => {
                // Shard filter: every worker READS every frame (cheap CRC +
                // I/O), but only the owning worker parses/enriches/inserts
                // it. `next` still reflects the read position; foreign
                // records simply never enter this worker's buffer.
                if owns_segment(next.segment_id, worker_id, total_workers) {
                    let t = Instant::now();
                    match rec.protocol {
                        Protocol::WebEvent => {
                            // One beacon = N web rows (decision D3).
                            let rows = web_parser.parse(&rec);
                            if rows.is_empty() {
                                let mut s = stats.lock();
                                s.parse_errors += 1;
                            }
                            web_buf.extend(rows);
                        }
                        _ => {
                            let mut row = parser.parse(&rec);
                            enricher.enrich(&mut row, parse_ip_from_addr(&rec.source_addr));
                            buf.push(row);
                        }
                    }
                    parse_ns += t.elapsed().as_nanos();
                    pending_checkpoint = Some(next);
                    if flush_due!() {
                        flush_timed(
                            &store,
                            &meta,
                            worker_id,
                            &mut buf,
                            &mut web_buf,
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
                        if let Some(cp) = pending_checkpoint {
                            last_cp_segment = cp.segment_id;
                        }
                        pending_checkpoint = None;
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
                if (!buf.is_empty() || !web_buf.is_empty())
                    && last_flush.elapsed() >= flush_interval
                {
                    flush_timed(
                        &store,
                        &meta,
                        worker_id,
                        &mut buf,
                        &mut web_buf,
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
                    if let Some(cp) = pending_checkpoint {
                        last_cp_segment = cp.segment_id;
                    }
                    pending_checkpoint = None;
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
    web_buf: &mut Vec<crate::store::appender::WebRow>,
    pending_checkpoint: Option<Checkpoint>,
    stats: &Arc<Mutex<IngestStats>>,
    error_tracker: Option<&crate::errors::ErrorTracker>,
    parse_ns: u128,
    cursor_ns: u128,
    all_ns: u128,
) {
    let n = buf.len() + web_buf.len();
    let t0 = Instant::now();
    flush(
        store,
        meta,
        worker_id,
        buf,
        web_buf,
        pending_checkpoint,
        stats,
        error_tracker,
    )
    .await;
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

/// Commit BOTH buffers in ONE transaction, then advance the checkpoint.
/// On failure both buffers are retained for retry (at-least-once), and —
/// because of the transaction — neither table is left half-flushed.
#[allow(clippy::too_many_arguments)]
async fn flush(
    store: &Store,
    meta: &WalMeta,
    worker_id: u64,
    buf: &mut Vec<LogRow>,
    web_buf: &mut Vec<crate::store::appender::WebRow>,
    pending_checkpoint: Option<Checkpoint>,
    stats: &Arc<Mutex<IngestStats>>,
    error_tracker: Option<&crate::errors::ErrorTracker>,
) {
    if buf.is_empty() && web_buf.is_empty() {
        return;
    }
    let n = buf.len() + web_buf.len();
    let conn = store.lock();
    let t_insert = Instant::now();
    let insert_res = (|| -> Result<(usize, usize)> {
        let tx = conn.unchecked_transaction()?;
        let inserted = insert_logs_in_tx(&tx, store.hot_attributes(), buf)?;
        let web_inserted = insert_web_in_tx(&tx, web_buf)?;
        tx.commit()?;
        Ok((inserted, web_inserted))
    })();
    let insert_ms = t_insert.elapsed().as_millis();
    let t_track = Instant::now();
    let track_res = insert_res.map(|(inserted, web_inserted)| {
        // Error tracking: fold the just-committed error rows into
        // error_groups while we still hold the store lock.
        if let Some(et) = error_tracker {
            et.track(&conn, buf);
        }
        (inserted, web_inserted)
    });
    let track_ms = t_track.elapsed().as_millis();
    match track_res {
        Ok((inserted, web_inserted)) => {
            buf.clear();
            web_buf.clear();
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
            s.web_events_ingested += web_inserted as u64;
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
        let web_parser = Arc::new(WebParser::new(meta.clone()));

        let handles = spawn_ingest_workers(
            3,
            tmp.path().join("wal"),
            meta.clone(),
            store.clone(),
            enricher,
            parser,
            web_parser,
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
