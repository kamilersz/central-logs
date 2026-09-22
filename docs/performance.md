# Performance

Sooner or later the question arrives: *how much can this thing actually
take?* It deserves a real answer, measured — not vibes. This page walks
through what central-logs does under load, what the numbers mean, and how
to reproduce them on your own hardware.

## The experiment

Imagine 64 of your services, all writing logs at once, as fast as they can.
That's the test: 64 independent clients hammering the HTTP ingest endpoint
for minutes, with a monitor sampling the server's CPU, memory, and disk
every second, and Prometheus-style metrics scraped live to watch the
internal pipeline.

Nothing was warmed up, nothing was special-cased. One release binary, one
DuckDB file, one WAL directory — the same setup from
[Getting Started](getting-started.md).

## What happened

**The line barely moved.**

- **~20,000 log lines per second**, sustained for minutes, from 64
  concurrent sources — while the server used about **3 CPU cores out of
  80** and **33 MB/s** of an NVMe drive that can write 30× that.
- Latency stayed pinned at **~30 ms p50** whether one client was writing or
  sixty-four. No cliffs, no runaway queues.
- **Zero data loss**: every accepted line was fsync'd to the WAL before the
  sender got its HTTP 200. Kill the process mid-storm and everything you
  were told "accepted" survives the restart.
- A **100,000-line NDJSON backfill** posted as bulk requests is absorbed at
  **~40,000 lines/second** — parsed, enriched, and searchable moments later.

While all this was happening, the ingest pipeline — the part that parses,
enriches, and files every record into DuckDB — kept pace with the incoming
firehose in real time. Senders stop, the backlog drains to zero in seconds,
and queries over the fresh data come back immediately.

## Why it works this way

central-logs splits writing into two stages with very different jobs:

1. **The front door (WAL)** — append bytes, fsync, answer. Its whole cost is
   one fsync per group of records, so it's deliberately *durability-bound*:
   throughput = senders × batch size ÷ fsync latency. On the test machine
   an fsync costs ~10–30 ms, which is exactly what the numbers reflect.
2. **The filing room (DuckDB)** — parse, enrich, and insert in large
   vectorized batches. Measured in isolation it moves **hundreds of
   thousands of rows per second**, which is why the front door can run flat
   out without anything piling up behind it.

That's also why the "20K/s" figure reads as a floor, not a ceiling: it's
bounded by disk sync latency on that box, not by CPU. Faster storage or
larger shipper batches scale it linearly.

## The numbers at a glance

| Scenario | Result |
|---|---|
| 64 sources × 20-line batches, 2+ minutes sustained | ~19,500 lines/s, 0 errors |
| 64 sources × 5-line batches | ~12,200 lines/s, ~30 ms p50 |
| 100K-line NDJSON bulk backfill | ~40,000 lines/s |
| Server cost at full tilt | ~3 CPU cores, ~33 MB/s writes, ~180 MB RSS |
| Pipeline after senders stop | drains to zero lag in seconds |

Tested on an 80-core / 503 GB RAM / NVMe Linux box, Sep 2026. Your numbers
will vary with hardware — the harness below is how you find out.

## Run it yourself

The entire torture test ships with the repo:

```bash
# 3-minute ramp to 64 concurrent sources + a bulk backfill, fully monitored
scripts/stress/run_stress.sh baseline 180 64

# micro-benchmarks (DuckDB insert throughput, WAL cursor correctness)
cargo test --release --test insert_bench -- --nocapture
cargo test --release --test cursor_stress
```

CSV results land in `scripts/stress/results/`. The heavy-duty
`insert_batch` micro-benchmark is handy for capacity planning on new
hardware: it tells you your disk+DuckDB ceiling in about ten seconds.

Curious what changed to make it this fast? The engineering notes — before/
after comparisons and the specific bottlenecks that were removed — live in
[`scripts/stress/RESULTS.md`](https://github.com/kamilersz/central-logs/blob/main/scripts/stress/RESULTS.md).
