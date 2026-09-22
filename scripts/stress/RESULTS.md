# Stress test results — central-logs

Hardware: 80-core server, 503 GB RAM, NVMe SSD (shared with other workloads).
Load generator: [loadgen.py](loadgen.py) — closed-loop HTTP clients POSTing JSON
to `POST /v1/logs` (ack = WAL fsync). Monitoring: [monitor.sh](monitor.sh)
(1 Hz CPU / RSS / disk-I/O / WAL-size / Prometheus scrape). Runner:
[run_stress.sh](run_stress.sh), micro-benchmarks: [burst_drain.sh](burst_drain.sh),
[tests/insert_bench.rs](../../tests/insert_bench.rs),
[tests/cursor_stress.rs](../../tests/cursor_stress.rs).

## Headline (after optimization, tuned profile)

| Scenario | Before | After | Change |
|---|---|---|---|
| Sustained ingest, 64 sources × 20-row batches | ~4.7K rec/s | **~19.5K rec/s** | 4.2× |
| Sustained ingest, 64 sources × 5-row batches | ~4.7K rec/s | **~12.2K rec/s** | 2.6× |
| Bulk NDJSON (100K rows in 20 req × 4 threads) | 285 rec/s | **40.5K rec/s** | **142×** |
| Ingest-pipeline drain rate (backlog → DuckDB) | ~1.2K rows/s | keeps pace with 19.5K/s inflow | >16× |
| Isolated DuckDB insert (1M-row bench) | 8.7K rows/s | **181–277K rows/s** | 21–32× |
| Ingest ack latency p50 (small batches) | ~70 ms | ~30 ms | 2.3× |
| WAL pruning / lag gauge under rotation | never pruned, lag stuck | old segments auto-pruned | fixed |

Resource use at ~19.5K rec/s sustained: ~3.4 CPU cores (of 80), ~33 MB/s
sequential writes (of an NVMe that benchmarks >1 GB/s), ~180 MB RSS.
The front door is deliberately durability-bound: throughput =
concurrency × batch ÷ fsync-latency, so bigger shipper batches scale linearly.

## What was wrong (all found via this stress harness)

1. **DuckDB insert path** (`store/appender.rs`): row-by-row prepared INSERTs
   inside a transaction — ~115 µs/row of pure statement overhead on an engine
   built for vectorized batches. Replaced with the DuckDB **Appender**
   interface → 32× (277K rows/s in the bench; 181K rows/s for 1M rows with
   periodic commits, zero stalls).
2. **Bulk ack path** (`insert/http.rs`, `wal/writer.rs`): the endpoint waited
   for one WAL group-commit fsync **per record** (a 1250-row request = 1250
   sequential ~14 ms waits). Added `InsertHandle::append_batch_acked` — the
   FIFO WAL means the *last* record's fsync covers all earlier records, so a
   request now waits **once** → 142× bulk throughput.
3. **WAL read path** (`wal/cursor.rs`): two unbuffered `tokio::fs` reads per
   ~250-byte frame = two `spawn_blocking` round-trips per record (~500 µs).
   Replaced with a 512 KB `BufReader` that decodes frames straight out of the
   buffer (exact-consume slow path for boundary-straddling frames).
4. **Error-group tracking** (`errors.rs`): per-error-row SELECT + JSON-rewrite
   UPDATE, each autocommitted = hundreds of fsync'd point statements per
   flush. Now: aggregate rows per fingerprint in memory, one transactional
   upsert per group per flush (~950 ms → ~50 ms per 8192-row flush).
5. **Checkpoint/lag design bug** (`ingest/worker.rs`): non-owning shard
   workers never advanced their checkpoints, so `min_checkpoint` pinned to
   the first segment forever — `ingest_lag_bytes` never drained past it and
   the WAL pruner could **never delete anything** (unbounded disk growth).
   Workers now release segments they have fully skipped; the shard owner's
   own checkpoint still gates pruning.

## Config used (scripts/stress/tuned.toml)

```toml
ingest_workers = 16          # parse/enrich parallelism (default 2)
ingest_batch_size = 8192     # rows per appender flush (default 2048)
ingest_flush_interval_ms = 200
wal_batch_max_records = 8192
duckdb_threads = 8           # point-op dispatch cost on many-core boxes
```

## Reproduce

```bash
cargo build --release
scripts/stress/run_stress.sh baseline 180 64     # label, duration_s, max_workers
cargo test --release --test insert_bench -- --nocapture
cargo test --release --test cursor_stress
```

Results CSVs land in `scripts/stress/results/`.

## Known limits (honesty section)

- The ingest ack path is fsync-latency-bound by design (durable before ack).
  On this shared NVMe a group-commit fsync costs ~10–70 ms under load; a
  dedicated disk or `fsync` grouping on faster media scales linearly with
  batch size.
- The lag gauge floors at the size of the active WAL segment (checkpoint
  release happens on segment crossings); per-record lag precision would need
  owner-aware checkpoint math.
- Single-node: one DuckDB writer. Sustained analytical drain beyond ~200K
  rows/s on one table is not a goal; compaction to Parquet handles the cold
  tier.

*Numbers measured Sep 22, 2026 — see CSVs in `results/` for raw data.*
