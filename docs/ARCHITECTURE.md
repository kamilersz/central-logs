# central-logs — Architecture

A self-hosted, single-node centralized logging platform: a Graylog-style alternative that ingests logs from many source systems, writes them durably at high speed, computes useful runtime dashboards/metrics, forecasts trends with built-in time-series algorithms, and exposes all of it to AI agents over MCP.

This document is the architecture reference for building v1. It is deliberately concrete (schema DDL, crate names, module layout) so a future implementation session can start coding directly from it rather than re-deriving decisions.

**Status**: phases 1–3 of §9 are substantially built, not just planned — see the README's [Status](https://github.com/kamilersz/central-logs#status) section for the current done/deferred split, and §12–§13 below for two things (AI natural-language query, the auth/session system) that were built beyond what this document originally scoped. Where this doc and the code disagree, the code wins; sections below have been updated as they were confirmed against the current implementation, but this file is a design reference, not generated from the source — treat "planned" language in §1–§11 as historical intent unless a section explicitly says otherwise.

## Locked-in constraints

- **Single-node, self-hosted.** One Rust binary (or a small set of co-located async tasks sharing one WAL directory and one DuckDB file) on one machine. No cluster coordination, no required external DB server.
- **Insert first, ingest later.** The write path that accepts a log line must be fast and durable, fully decoupled from parsing/enrichment/indexing.
- **Embedded analytics storage.** Lean on a proven embedded columnar engine rather than a hand-rolled LSM store or a mandatory external server.
- **AI-connectable via MCP**, in priority order: query & summarize → anomaly detection → forecasting → alerting/automation.

---

## 1. High-level architecture

```
┌─────────────┐   ┌─────────────┐   ┌────────────────┐
│ App servers │   │ Syslog boxes│   │ OTel-instrum.   │
│ (HTTP/JSON) │   │ (UDP/TCP)   │   │ services (OTLP) │
└──────┬──────┘   └──────┬──────┘   └──────┬──────────┘
       │                 │                 │
       └────────┬────────┴────────┬────────┘
                 ▼                 ▼
        ┌────────────────────────────────┐
        │     INSERT LAYER (hot path)     │  axum HTTP/JSON endpoint
        │  validate → framed record →     │  tokio UDP/TCP syslog listener
        │  in-mem channel → WAL segment   │  tonic OTLP LogsService (later)
        │  append (group commit/fsync)    │
        └───────────────┬─────────────────┘
                         │ (durable, ack returned to source here)
                         ▼
        ┌────────────────────────────────┐
        │   WAL / append-only segments    │  crc32-framed records,
        │   (+ redb for offsets/cursors)  │  rotated by size/time
        └───────────────┬─────────────────┘
                         │ tailed by
                         ▼
        ┌────────────────────────────────┐
        │   INGEST WORKERS (async, N)     │  parse (JSON vs free text),
        │  parse → enrich → batch-insert  │  field extraction, GeoIP,
        └───────────────┬─────────────────┘  hostname tag, schema infer
                         ▼
        ┌────────────────────────────────┐
        │  EMBEDDED ANALYTICS STORE       │  DuckDB (hot table) +
        │  hot table + rollups + cold     │  Parquet cold tier (hive-
        │  Parquet tier via compaction    │  partitioned, queried in place)
        └───────────────┬─────────────────┘
                         ▼
        ┌────────────────────────────────┐
        │        QUERY / API LAYER        │  axum HTTP API (SQL-ish +
        │                                  │  structured filters)
        └───────┬─────────────────┬───────┘
                 ▼                 ▼
       ┌───────────────┐  ┌──────────────────┐
       │ Dashboard UI   │  │  MCP server       │
       │ (askama+htmx+  │  │  (rmcp, in-process│
       │  uPlot charts) │  │  or stdio/SSE)    │
       └───────────────┘  └──────────────────┘
                                    ▲
                                    │
                              AI agent (Claude)
```

Two cross-cutting concerns thread through every stage (both driven by `docs/LESSON_LEARNED.md`):

- **Pipeline observability** — the insert channel, WAL writer, and ingest checkpoints each export health taps (channel fill, bytes written, ingest lag in WAL bytes). See §2c.
- **Cost tiering** — compaction writes zstd-compressed Parquet partitioned per service, optionally mirrored to object storage (S3/GCS) as an archive. See §3.

### Ingestion protocols

**v1 (minimal, pragmatic set):**
- **HTTP/JSON** — `POST /v1/logs`, newline-delimited JSON or a JSON array. Primary path: trivial for any source system, and existing shippers (Fluent Bit, Vector) can point a generic HTTP output at it with zero custom integration work.
- **Syslog (UDP + TCP, RFC 3164/5424)** — covers legacy/infra devices (routers, firewalls, older daemons) with no source-side changes.

**Built:**
- **OTLP/HTTP (OpenTelemetry)** — `/v1/traces`, `/v1/logs`, `/v1/metrics` accept protobuf and OTLP/JSON, so any OTel SDK exports directly with no collector. Spans are stored as `protocol=otlp_span` rows (trace/span ids + duration), OTel logs as `otlp_log`, metric points as `otlp_metric`.

**Deferred to phase 2/3:**
- **OTLP/gRPC (`LogsService`/`TraceService`, port 4317)** — the HTTP encoding is implemented; the gRPC transport (requires `tonic`) is not.
- **Tiny file-tailing shipper agent** — a thin Rust wrapper around `notify` (file watch) + the HTTP client, for tailing local log files. Built once the core service is stable.
- **Sentry envelope/store ingest** — built: native Sentry-SDK protocol so unmodified Sentry SDKs can ship errors into the pipeline (see [`ERROR_TRACKING.md`](ERROR_TRACKING.md)).

---

## 2. Insert vs. ingest split

This is the core mechanism the whole "fast at saving log" requirement rests on. The two stages are deliberately different processes with different jobs, connected only by the WAL.

### 2a. Insert (synchronous hot path)

What happens, in order, when a log line arrives:

1. **Cheap structural validation only** — valid UTF-8, valid JSON envelope, non-empty. No parsing of the message body, no field extraction, no enrichment. Malformed input is rejected fast (4xx) so bad clients can't fill the WAL.
2. **Wrap into a minimal framed record**: `{receive_ts, source_addr, protocol, raw_bytes}` — "we received these bytes from this place at this time," nothing more.
3. **Push onto a bounded `tokio::mpsc` channel.** This is the backpressure mechanism. If the channel is full (WAL writer can't keep up):
   - HTTP/TCP: await with a short timeout, then return `503`/`429` — these protocols support backpressure.
   - UDP syslog: drop with a counter metric — UDP has no backpressure concept, blocking would just make things worse.
4. **A single dedicated WAL-writer task** drains the channel and appends each record to the current append-only segment file (length-prefixed, CRC32C-checksummed frames), performing **group commit**: batch N records or flush every T milliseconds (e.g. every 5-20ms or every 1-4MB, whichever comes first), then one `fsync`/`fdatasync` covering the whole batch. This amortizes fsync cost across many records — the same technique Kafka, BookKeeper, and most WAL designs use to get high throughput without sacrificing durability.
5. **Ack returned to the source only after the batch containing its record has been fsynced** (HTTP/TCP paths). Once acked, the record survives a process crash or power loss (assuming the disk honors fsync).
6. Segments rotate by size (128-256MB) or age. Old segments are retained until ingest workers have checkpointed past them, then deleted/archived.

**Nothing else happens synchronously.** No JSON field parsing beyond the outer envelope, no GeoIP, no hostname resolution, no schema inference, no DuckDB writes. That is the entire point of the split.

### 2b. Ingest (asynchronous background path)

One or more **ingest worker tasks** independently tail WAL segments via a checkpointed read cursor. For each batch of raw records:

1. **Parse** — try structured (JSON) parse first; fall back to syslog/free-text heuristics (RFC5424 fields, generic `level`/`timestamp` extraction, or store as opaque `message` if nothing matches). New top-level JSON keys are surfaced as queryable structured fields (schema inference). Two LESSON_LEARNED-driven refinements:
   - **message-JSON lifting** (`unwrap_message_json`, default on): a JSON object embedded as a string inside `msg`/`message` has its fields lifted — reserved fields (`level`, `service`, `trace_id`, …) backfill the typed columns when missing, the rest land in `attributes`, and hot-attribute extraction sees them. The original string stays in `message` for display.
   - **drop-attributes** (`drop_attributes`): configured keys are stripped from the attributes residue before persistence — remove junk early, pay only for what you keep.
2. **Enrich** — GeoIP lookup on source IP (`maxminddb` against a local MaxMind GeoLite2 database — verify current license/update terms), hostname/service tagging, trace/span id extraction, severity normalization.
3. **Batch-insert into DuckDB** using the **Appender API** (bulk columnar insert, far faster than row-by-row `INSERT`), flushed every few hundred/thousand rows or every second.
4. **Advance the WAL checkpoint** (persisted in `redb`) only after the batch is durably committed into DuckDB. A crash mid-ingest simply re-processes the last uncommitted batch on restart — ingest is at-least-once; duplicate log lines are a far smaller problem than lost ones.

**Worker sharding**: ingest workers shard the WAL by segment id (`segment % N == worker_id`) — the local analog of Kafka's one-consumer-per-partition rule. Every worker reads every frame (cheap CRC + I/O) but only its shard is parsed/enriched/inserted. Checkpoints are per-worker; the segment pruner deletes only below the *minimum* checkpoint, so a lagging or restarted worker never loses segments it still needs.

### 2c. Backpressure & pipeline observability

The WAL plays the role Kafka plays in cluster-scale pipelines: a disk-backed buffer between a fast insert path and a slower processing path. The LESSON_LEARNED post-mortem's worst incident — a client library buffering 4 GB in RAM until the pod OOM'd — is the failure mode this design structurally cannot have: every buffer between insert and DuckDB is either bounded in memory (the channel) or backed by disk (the segments).

Bounded-but-invisible is still dangerous, so the pipeline exports its own health gauges, surfaced in `/metrics`, `/api/pipeline`, and the SPA **Pipeline** page:

| Gauge | Meaning |
|---|---|
| `central_logs_wal_channel_depth` / `central_logs_wal_channel_capacity` | entries waiting in the insert→WAL channel vs the configured bound; sustained ≈100% means fsync can't keep up and inserts will start timing out |
| `central_logs_wal_bytes_written_total` | WAL throughput counter |
| `central_logs_ingest_lag_bytes` | WAL bytes written but not yet consumed+checkpointed — the local consumer-lag equivalent; sustained growth means DuckDB inserts (or enrichment) are the bottleneck |
| `central_logs_audit_dropped_total` | self-audit events dropped because the channel was saturated; audit emission stays fire-and-forget by design (never blocks a request), but the loss is counted, never silent |

Escalation path: channel fill climbing → raise `insert_channel_depth` (buys seconds) and investigate disk fsync latency. Ingest lag climbing → inserts are the bottleneck → shard more workers / raise `ingest_batch_size`.

### WAL/queue crate choices

| Concern | Choice | Why |
|---|---|---|
| Hot-path buffering | `tokio::sync::mpsc` (bounded) | Zero-cost in-process handoff, natural backpressure signal, no serialization before the WAL write |
| Durable WAL segments | Hand-rolled append-only segment log (framed records + `crc32fast` + manual fsync/rotation) | Narrow, well-understood problem (same shape as Kafka/Bitcask/BookKeeper). A general-purpose embedded DB adds B-tree/MVCC overhead a pure sequential-append log doesn't need, and hand-rolling gives full control over the group-commit/fsync strategy insert speed depends on |
| WAL offsets / ingest checkpoints / small metadata | `redb` | Pure-Rust, stable on-disk format since 1.0 (June 2023), actively maintained, ACID with configurable durability. Preferred over `sled`, which has been in maintenance mode since 2022 with an explicitly unstable on-disk format — a real risk for state that must survive crashes |

Two-tier design: a custom log for bulk data, `redb` for small transactional state. This keeps the insert path free of query-engine overhead entirely.

---

## 3. Storage schema

### Core table (hot tier, DuckDB)

```sql
CREATE TABLE logs (
  ts            TIMESTAMP,        -- event time (parsed from source if present, else receive_ts)
  insert_ts     TIMESTAMP,        -- when it hit the WAL (for insert-vs-ingest lag metrics)
  source_host   VARCHAR,
  service       VARCHAR,
  level         VARCHAR,          -- normalized: trace/debug/info/warn/error/fatal
  message       VARCHAR,
  trace_id      VARCHAR,
  span_id       VARCHAR,
  attributes    JSON,             -- arbitrary structured fields (DuckDB native JSON type)
  geo_country   VARCHAR,          -- enrichment output
  raw_len       INTEGER,          -- size of original raw line, for volume metrics
  protocol      VARCHAR           -- http_json | syslog | otlp
);
```

DuckDB's native `JSON` type lets structured fields be queried (`attributes->>'$.user_id'`) without a rigid predefined schema or per-source migrations.

### Partitioning / hot-cold tiering

DuckDB has no ClickHouse-style native partitioned MergeTree table, so partitioning happens at the file/directory level:

- **Hot tier**: the last N hours/days (configurable, e.g. 24-72h) live as the `logs` table in the live DuckDB file. DuckDB's per-row-group min/max zonemaps make time-range filters cheap even without a manual index.
- **Compaction job** (background, e.g. hourly): exports rows older than the hot-tier cutoff into **hive-partitioned Parquet** (`data/date=2026-08-10/hour=14/service=<name>/part-0.parquet`), clustered by `ORDER BY ts, service` with zstd compression (default) and bloom filters on high-cardinality VARCHAR hot attributes. The extra `service=` level enables **per-service retention** (LESSON_LEARNED: "not all logs are equal" — `[[service_retention]]` glob rules, first match wins, `retention_days` fallback; e.g. audit 90d, debug 3d). Legacy `date=/hour=`-only files remain readable and age out under global retention.
- **Cold tier queries**: DuckDB reads Parquet directly. The `logs_all` view projects the explicit column list (built-ins + hot attributes) on both arms with `union_by_name=true` and `hive_partitioning=false` (the real `service`/`ts` values live in the files; row-group statistics prune via the sort clustering):
  ```sql
  CREATE VIEW logs_all AS
    SELECT <cols> FROM logs
    UNION ALL
    SELECT <cols> FROM read_parquet('data/**/*.parquet', hive_partitioning=false, union_by_name=true);
  ```
- **Object-storage archive** (optional, feature `object-storage`): a sync loop mirrors compacted Parquet to S3/GCS/MinIO under `[cold_storage]` config and deletes remote objects as local retention purges them — the remote copy mirrors the local lifecycle. With `keep_local = true` (default) the local file remains the queryable cache and remote is durability/archive.
- **Downsampling**: before raw rows age past the retention window (e.g. 30-90 days), the compactor pre-computes and permanently retains **rollup rows** (per-minute/hour counts, error counts, p50/p95/p99 latency) so historical trend/forecast queries stay cheap forever, even after raw Parquet is purged on a configurable TTL.

Result: fast recent-window queries (small hot table) and cheap historical aggregates (rollups only, no raw scan), while occasional deep historical raw search against Parquet remains available (slower, but possible).

---

## 4. Dashboards & metrics

### Default dashboard set

1. **Ingestion/insert rate** — records/sec and bytes/sec by protocol, sourced directly from insert-layer counters (cheapest possible metric — doesn't touch DuckDB at all).
2. **Error rate over time** — `count(level='error') / count(*)` per time bucket, overall and per-service.
3. **Latency percentiles per service** — p50/p95/p99 via DuckDB `approx_quantile()` over a `duration_ms` attribute, pre-aggregated into rollups.
4. **Top-N hosts/services** — by volume or error count, over the selected time range.
5. **Log volume trend** — stacked area by level or service.
6. **Anomaly markers** — vertical markers/shaded regions on volume/error-rate charts at flagged timestamps.
7. **Forecast overlay** — dashed projection line extending volume/error-rate charts past "now."
8. **Pipeline health** — insert-channel fill bar, WAL throughput, ingest lag, and drop counters (the §2c gauges); the "is the buffer about to overflow?" page.

### Computation strategy: rollups, not query-on-read

A background **rollup job** runs every 30-60s:

```sql
INSERT INTO rollup_1m
SELECT
  time_bucket(INTERVAL '1 minute', ts) AS bucket,
  service, level,
  count(*) AS n,
  approx_quantile(duration_ms, 0.5)  AS p50,
  approx_quantile(duration_ms, 0.95) AS p95,
  approx_quantile(duration_ms, 0.99) AS p99
FROM logs
WHERE ts > :last_rollup_ts
GROUP BY 1, 2, 3;
```

into small `rollup_1m` / `rollup_1h` tables. Dashboards query these rollups almost exclusively, so dashboard load stays fast regardless of total log volume. Ad-hoc drill-down (a user clicking into a time window to read raw messages) queries the raw hot table (or cold Parquet for older windows) directly — inherently a smaller, bounded query.

### Serving layer

Given the single-binary constraint, favor server-rendered over an SPA:
- **`axum`** — web framework.
- **`askama`** — compile-time-checked HTML templates, no runtime parsing, keeps the binary self-contained.
- **`htmx`** — partial-page swaps / polling for live-updating charts and tables, no JS build pipeline.
- **`uPlot`** (vendored static asset, ~45KB) — canvas-based, built specifically for dense time-series data with smooth zoom/pan; far lighter than Chart.js/D3 for this use case. Dashboard pages fetch rollup JSON from the query API and render it client-side.

Templates + static assets embedded via `rust-embed` (or similar) so the whole dashboard ships inside the one binary with no separate frontend build/deploy step.

---

## 5. Time-series forecasting

**Recommended crate: [`augurs`](https://github.com/grafana/augurs)** (Grafana Labs) — an actively maintained Rust time-series toolkit:
- `augurs-ets` — exponential smoothing (ETS/Holt-Winters) forecasting.
- `augurs-mstl` — multiple seasonal-trend decomposition (handles daily+weekly seasonality in log volume) combined with a trend forecaster for forecasts + prediction intervals.
- `augurs-prophet` — Rust port of Facebook Prophet, for volume series with strong seasonality/holiday-like effects.
- `augurs-forecaster` — high-level API bundling imputation/scaling/back-transform around the above.
- Also includes outlier/changepoint primitives (see §6).

*Verify availability*: confirm exact submodule names and API stability at implementation time — this toolkit is actively evolving.

**What gets forecasted**: log volume (records/sec or /min, per-service and aggregate) and error rate, read straight from the rollup tables the dashboards already use — no separate data pipeline.

**Default model**: start with ETS/Holt-Winters (`augurs-ets`) for simplicity and good behavior on noisy, bursty ops data. Layer in MSTL when clear daily/weekly seasonality is detected (e.g. via an autocorrelation check on the rollup series). Prophet is a reasonable fallback if MSTL+ETS underperforms on a given series — expose the model as a configurable choice rather than hardcoding one.

**Surfacing**:
- Dashboard overlay — dashed forecast line + shaded confidence interval extending past "now," computed on a schedule (e.g. every rollup cycle) and cached.
- MCP tool result — `forecast(metric, horizon)` returns the same series as structured JSON (point forecasts + intervals).

---

## 6. Anomaly detection

Lightweight statistical methods appropriate for a single-node system — no ML infra. Three complementary layers:

1. **Rolling z-score / MAD (median absolute deviation)** over a trailing window (e.g. last 60 rollup points) per metric (volume, error rate, per-service p95 latency). MAD is preferred over stddev-based z-score for robustness to log data's naturally spiky, heavy-tailed distribution. Reuse `augurs`' MAD/outlier primitives if the API fits (verify exact name, e.g. `augurs-outlier`); a hand-rolled fallback via streaming mean/variance (**Welford's algorithm**) is a trivial, safe backup.
2. **Seasonal-adjusted thresholds** — for metrics with clear daily/weekly seasonality (detected via the MSTL decomposition already computed for forecasting), compare actual vs. the seasonal model's expected value rather than a flat rolling window. Avoids false positives from normal daily traffic dips (e.g. overnight).
3. **Sudden rate-of-change detection** — a simple derivative check (volume or error rate jumping >X% within one rollup interval) to catch sharp step-changes (a service starting to error-loop) that a slower rolling-window statistic might miss for a cycle or two.

Anomaly flags land in a small table:

```sql
CREATE TABLE anomalies (
  ts        TIMESTAMP,
  metric    VARCHAR,
  score     DOUBLE,
  method    VARCHAR,   -- 'mad' | 'seasonal' | 'rate_of_change'
  severity  VARCHAR
);
```

computed once per rollup cycle, so both the dashboard overlay and the MCP `detect_anomalies` tool read from the same source of truth.

---

## 7. MCP server design

**Runtime placement**: in-process within the same Rust binary as the core service for v1 — shares the DuckDB connection pool and rollup/anomaly state directly, no extra network hop. Exposed over **stdio transport** (local Claude Desktop/Code integration) only.

**SSE/HTTP transport status**: `rmcp`'s `StreamableHttpService` (feature `transport-streamable-http-server`, already a declared dependency) implements `tower_service::Service`, but its response body type doesn't match axum's directly — mounting it requires a small body-adapter plus wiring session management, `Host`/`Origin` validation, and the `mcp_api_key` bearer gate by hand. That adapter isn't written yet, and this transport carries enough protocol subtlety (session negotiation, SSE framing) that shipping it unverified against a real MCP client felt riskier than being explicit about the gap. `Config::validate` rejects `mcp_mode = sse` outright — selecting it fails loudly at startup rather than silently running with no MCP server (which is what it used to do). Local stdio integration is unaffected; this only blocks a *remote* agent talking to central-logs over HTTP.

**SDK**: `rmcp` — the official Rust MCP SDK (`modelcontextprotocol/rust-sdk`, tokio-based) + `rmcp-macros` for ergonomic tool definitions.

### Tools, in priority order

**1. `query_logs`** (baseline)
- In: `{ query: string (structured filter DSL or constrained SQL subset), time_range: {from, to}, limit?: number }`
- Out: `{ rows: [...], total_matched: number, truncated: bool }`
- Runs against the `logs_all` view (hot + cold) with a hard row/time-range cap to prevent runaway scans from an agent-generated query.

The HTTP logs endpoints enforce an additional per-service window cap on top: `[[service_query_limits]]` glob rules (e.g. `payment_*` → 24h) apply when a filter pins `service:<name>` by equality — the single-node analog of OpenObserve's per-stream `max_query_range`, so one wide user/agent scan can't monopolize the node. Unfiltered queries and `~` text matches are not capped.

**2. `get_dashboard_summary`**
- In: `{ dashboard_id | metric: string, time_range: {from, to} }`
- Out: `{ summary_stats: {...}, top_n: [...], narrative_hints: [...] }` — pre-digested numbers, not raw rows, so the agent can summarize without re-deriving aggregates.

**3. `detect_anomalies`**
- In: `{ metric: string, time_range: {from, to}, sensitivity?: number }`
- Out: `{ anomalies: [{ ts, score, method, severity }] }` — reads the `anomalies` table from §6.

**4. `forecast`**
- In: `{ metric: string, horizon: string (e.g. "6h"), granularity?: string }`
- Out: `{ points: [{ ts, predicted, lower, upper }] }` — from the `augurs`-based forecaster.

**5. `create_alert_rule`** (lowest priority, highest blast radius)
- In: `{ name, metric, condition (threshold or anomaly-based), notification_channel }`
- Out: `{ rule_id, status: "pending_approval" | "active" }`
- **Never auto-activates.** Rules are created `pending_approval`; a human must confirm via the dashboard UI before the rule starts firing. This bounds the risk of an AI agent silently wiring up paging/alerting loops.

All tools are read-only by default except `create_alert_rule`, which should only be registered when an explicit `--enable-alert-mcp-tool` config flag is set — an operator can run query/analysis access without any AI-driven alert creation at all.

### Alert evaluation & notification

Creating and approving a rule only manages its lifecycle (`pending_approval` → `active` / `rejected`); a separate background task (`src/alerts.rs`, `spawn_alert_task`) is what makes an `active` rule actually fire. On a fixed interval (`alert_eval_interval_secs`, default 30s) it:

1. Loads every `active` row from `alert_rules` and parses its `condition_json` into a `Threshold` or `Anomaly` condition (comparator + value + window, or a minimum anomaly severity + window — both carry a `cooldown_secs`, default 900s).
2. Resolves the rule's `metric` (`volume`, `error_rate`, `p50_latency`, `p95_latency`, `p99_latency`) against the trailing window of `rollup_1m` — the same table the dashboards and `get_dashboard_summary`/`forecast` read — or, for `Anomaly` conditions, checks the `anomalies` table (§6) for a matching-or-worse severity within the window.
3. For each rule that's breached and past its cooldown, POSTs a JSON payload (`rule_id`, `rule_name`, `metric`, `value`, `message`, `fired_at`) to `alert_rules.channel`.

**Channel = any `http(s)` URL.** There is no dedicated Slack/email/PagerDuty SDK integration — a plain webhook POST covers Slack/Discord/Teams incoming webhooks and PagerDuty's Events API in practice, and anything a user wants to bridge to email. A non-`http(s)` channel value is recorded as a delivery error rather than silently dropped.

Every evaluation is persisted: a firing (whether or not the webhook delivery itself succeeded) writes an `alert_events` row, and `alert_rules.last_evaluated_at` / `last_fired_at` / `last_notify_error` are updated so the dashboard can show rule health without re-deriving it. A rule whose `condition_json` fails to parse, or whose `metric` isn't one of the five recognized names, is recorded as an evaluation error rather than silently skipped — the operator sees why a rule never fires instead of it just doing nothing.

```sql
CREATE TABLE alert_events (
  id             INTEGER PRIMARY KEY,
  rule_id        INTEGER NOT NULL,
  ts             TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metric         VARCHAR NOT NULL,
  value          DOUBLE,
  message        VARCHAR,
  notified       BOOLEAN NOT NULL,
  error          VARCHAR
);
```

Surfaced at `GET /api/alert-rules/{id}/events` (newest first, capped at 200 rows).

---

## 8. Recommended Rust crate/tech stack

| Layer | Crate(s) | Notes |
|---|---|---|
| Async runtime / web | `tokio`, `axum`, `tower`, `tower-http` | Mature, tokio-native, standard choice |
| Insert-path WAL | Hand-rolled append-only segment log + `crc32fast` | Purpose-built for group-commit throughput |
| WAL offsets / small metadata | `redb` | Stable on-disk format, actively maintained (preferred over `sled`) |
| Embedded analytics store | `duckdb` (`duckdb-rs`, `bundled` feature) | SQL-mature; native JSON + Parquet + Appender API for bulk insert |
| Cold tier / Arrow interop | `arrow`, `parquet` | Hive-partitioned cold storage files, read directly by DuckDB |
| Object-storage archive (optional) | `object_store` (feature `object-storage`; aws/gcp/http backends) | Mirrors compacted Parquet to S3/GCS/MinIO; remote lifecycle follows local retention, local files stay the queryable cache |
| (Future alternative) | `datafusion` | Consider only if custom query-plan extension or heavier Parquet-only workloads outgrow DuckDB; not needed for v1 |
| Forecasting & anomaly detection | `augurs` (`augurs-ets`, `augurs-mstl`, `augurs-prophet`, outlier module — verify exact names) | Actively maintained Rust time-series toolkit (Grafana Labs) |
| MCP server | `rmcp` + `rmcp-macros` | Official Rust MCP SDK |
| Templates / dashboard UI | `askama` + `htmx` + vendored `uPlot` | Server-rendered, no JS build pipeline, single-binary friendly |
| Syslog parsing | tokio UDP/TCP listener + `syslog_loose` (verify availability) | RFC 3164/5424 |
| OTLP receiver (phase 2/3) | `tonic` + generated OTLP proto types (verify crate name) | Building a receiver, not the exporter side |
| GeoIP enrichment | `maxminddb` | Verify GeoLite2 DB licensing/update mechanism |
| Config | `figment` or `config` + `serde` | |
| Self-observability | `tracing` + `tracing-subscriber` | Dogfooding — central-logs' own logs are a good v1 smoke test |
| CLI | `clap` | |
| Checksums | `crc32fast` | WAL frame integrity |

---

## 9. Phased build roadmap

**Phase 1 — Insert, ingest, storage, basic UI**
HTTP/JSON insert endpoint with WAL + group-commit fsync; ingest workers parsing/enriching into a DuckDB hot table via the Appender API; core schema (§3, hot tier only, no compaction yet); simple hard-TTL retention (no downsampling yet); minimal dashboard (volume chart, level breakdown, raw search/filter); syslog input if time allows.
*Goal: prove the insert-vs-ingest split holds up under bursty load and data survives a crash.*

**Phase 2 — Analytics maturity**
Rollup materialization jobs; latency/percentile + top-N dashboards; hot/cold tiering with Parquet export + compaction; forecasting via `augurs` (volume, error rate) with dashboard overlay; anomaly detection (MAD/z-score + seasonal-adjusted) with dashboard markers; OTLP ingestion.
*Goal: "useful runtime dashboards" and "built-in forecasting" requirements fully met.*

**Phase 3 — AI integration**
MCP server (`rmcp`) exposing `query_logs`, `get_dashboard_summary`, `detect_anomalies`, `forecast` as read-only tools first; `create_alert_rule` added last, behind the pending-approval gate and explicit enable flag; webhook notification for activated alerts (§7); file-tailing shipper agent; auth/API-key hardening for the HTTP/MCP surfaces.
*Goal: AI-connectable per the locked-in priority order (query/summarize → anomaly → forecast → alerting).*

**Status as actually built** (this section describes the pre-implementation plan; the codebase has since gone past it in places and is still short in others):
- **Done, beyond plan**: all 5 MCP tools over stdio; alert-rule evaluation + webhook notification (§7) — not just creation/approval; scoped API-key + session auth with centralized route-scope authorization (§13, well past "hardening"); AI natural-language query (§12, not in the original plan at all).
- **Deliberately not done**: MCP SSE/streamable-HTTP transport (§7 — the dependency is declared but unwired; `mcp_mode = sse` is rejected at config validation rather than left as a silent no-op). Alert notification is webhook-only, no dedicated Slack/email/PagerDuty SDK integration.
- **Not started**: OTLP/gRPC ingest (OTLP/HTTP is built), file-tailing shipper agent (both still phase-2/3 as originally scoped).

**Phase 4 — Sentry-compatible error tracking** *(built — see [`ERROR_TRACKING.md`](ERROR_TRACKING.md))*
Native Sentry envelope/store ingest so existing Sentry SDKs point their DSN at central-logs; fingerprint grouping with sampled (not per-event) stacktraces; `error_groups` + `rollup_error_1m` tables; Errors SPA page; webhook notification on first-seen, regression, and threshold via a new evaluator condition type.

---

## 10. Proposed initial module layout

```
central-logs/
├── Cargo.toml                  # workspace: core, wal, store, mcp, web
├── src/
│   ├── wal/
│   │   └── segment.rs          # append-only WAL segment writer/reader,
│   │                            # group-commit/fsync logic (heart of §2)
│   ├── ingest/
│   │   └── worker.rs           # background ingest workers: parse/enrich/
│   │                            # batch-insert into DuckDB, checkpoint via redb
│   ├── store/
│   │   └── schema.rs           # DuckDB DDL, rollup job SQL,
│   │                            # compaction/Parquet-export logic (§3)
│   ├── mcp/
│   │   └── tools.rs            # rmcp tool definitions: query_logs,
│   │                            # get_dashboard_summary, detect_anomalies,
│   │                            # forecast, create_alert_rule
│   └── web/
│       └── dashboard.rs        # axum routes + askama templates (§4)
└── docs/
    └── ARCHITECTURE.md         # this document
```

This is a starting point, not a locked contract — expect the workspace crate boundaries (`core`/`wal`/`store`/`mcp`/`web`) to firm up once Phase 1 implementation begins.

---

## 11. Applied production lessons (`docs/LESSON_LEARNED.md`)

A production ELK → Kafka/Vector/OpenObserve migration (4 Gbps, 75% cost cut) yielded lessons that map onto this single-node design. Cluster-scale machinery (Kafka brokers, KEDA autoscaling, DaemonSet agents) is translated into local equivalents rather than adopted literally — the mechanisms matter, not the fleet.

| Lesson (source) | central-logs mechanism | Where |
|---|---|---|
| Buffer before processing; a disk-backed queue is the safety net | CRC-framed WAL segments between insert and ingest; nothing buffers unbounded in RAM | §2 |
| Hidden in-memory queues OOM (librdkafka's 4 GB default) | every buffer is bounded (channel) or disk-backed (segments); backpressure surfaces as insert timeouts, never as crashes | §2c |
| Make backpressure visible *before* drops start | channel-fill / WAL-throughput / ingest-lag gauges + the Pipeline page | §2c |
| "We cannot afford to lose any audit data" | audit drops are counted (`central_logs_audit_dropped_total`) and warned, never silent | §2c |
| One stream per microservice | `service=` partition level in the cold-tier layout | §3 |
| Not all logs are equal → per-stream retention | `[[service_retention]]` glob rules over the service partition (e.g. audit 90d, debug 3d) | §3 |
| Columnar + aggressive compression ≈ 6× smaller on disk | Parquet cold tier with zstd default (`parquet_compression`), `ORDER BY ts, service` clustering, bloom filters | §3 |
| Object storage ≈ 8× cheaper per GB than node SSD | optional `[cold_storage]` archive mirror (feature `object-storage`), remote lifecycle follows local retention | §3 |
| Parse JSON embedded in `message` at ingest ("message_json") | `unwrap_message_json` lifting — inner fields reach typed columns + `attributes` + hot attributes | §2b |
| Remove unneeded fields early — smaller messages travel faster | `drop_attributes` stripped at parse time, before persistence | §2b |
| More partitions → more consumer throughput | ingest workers shard the WAL by segment id (`segment % N == worker`) | §2b |
| Per-stream `max_query_range` guards | `[[service_query_limits]]` window caps on service-pinned queries | §7 |

Not adopted, with reason: Kafka itself (the WAL *is* the local queue), KEDA cron autoscaling (single node — scale by adding workers, not pods), and Fluent Bit/Vector agents (phase-3 file-tailing shipper covers the collection story).

---

## 12. AI natural-language query

Built during implementation; not anticipated by the original §9 roadmap, which only planned an MCP-side natural-language surface (the agent talking to central-logs). This is the inverse: a human typing free text into the SPA's **Ask AI** button on the Logs Explorer.

**Flow**: `POST /api/ai/query { query, time_hint? }` → `src/ai.rs` builds a system prompt containing the current hot-attribute schema (so the model knows what columns exist) and the user's text → calls a configured LLM provider → the model's text response is parsed and **re-validated through `query::parse_filter`** before it's returned to the client. A misbehaving or adversarial model output that isn't valid filter-DSL syntax, or that references an unknown column, is rejected by the same whitelist-based parser every other query path goes through — it can never become raw SQL. **The LLM never sees log data**, only the schema and the user's query text.

**Providers** (`LlmConfig`, tagged by `provider`): `openai`, `anthropic`, `9inference` (an OpenAI-compatible endpoint behind Cloudflare, hence the browser User-Agent workaround in `call_9inference`), or `off` (default — falls back to a local keyword-extraction heuristic, e.g. "errors" → `level:error`, so the endpoint is still useful with zero configuration). `CENTRAL_LOGS_LLM_API_KEY` + `CENTRAL_LOGS_LLM_MODEL` env vars opt into 9inference without a TOML block.

Every call is audited (`ai.query` event, actor + source IP + the query text) before the LLM call fires — "who asked what" is exactly the audit trail's job (§13).

---

## 13. Authentication & sessions

Also built during implementation, beyond the §9 roadmap's one-line "auth/API-key hardening" placeholder. OWASP A01 (broken access control) / A07 (auth failures) -aligned, implemented in `src/web/auth.rs` + `src/web/auth_api.rs`.

**Model**: scoped API keys (`insert`, `read`, `write`, `admin` — each higher scope implies the ones below it) stored as SHA-256 hashes in a `api_keys` DuckDB table; raw tokens are shown to the operator exactly once at creation and never persisted. Two credential paths:

- **Bearer / `X-API-Key` header** — for programmatic clients (shippers, scripts, the MCP server).
- **Browser sessions** — `POST /api/auth/login` exchanges an API key for a random `cl_session` cookie; the SPA uses this so raw keys never sit in browser storage.

**Authorization** is centralized in one function, `required_scope(method, path)` (`src/web/auth.rs`) — the single source of truth for which scope a route needs, applied as the outermost `axum` middleware layer so unauthorized requests are rejected before any body allocation. Notably: alert rule approve/reject requires `Admin` (highest blast radius per §7), dashboard-config mutations require `Write`, everything else under `/api/`/`/metrics` requires `Read`, and `/v1/logs*` requires `Insert`.

**Bootstrap**: on first run, if the `api_keys` table is empty and no legacy static `http_api_key` is configured, the server mints a one-time admin token, prints it to stderr, and logs a pointer to `/login`. Legacy deployments that already set `http_api_key` keep working unchanged (treated as an implicit static admin credential) — no forced migration.

**Auth-disabled mode**: if neither `http_api_key` nor any API key exists, auth is off entirely (documented default for local/loopback use). `Config::validate` warns loudly if the HTTP API is bound to a non-loopback interface with auth disabled — binding `0.0.0.0` with no `http_api_key` set means any client on the network can read logs, approve alerts, and insert.

Every write-shaped action (login, alert approve/reject, dashboard-config CRUD, api-key CRUD, AI query) emits a self-audit event through the same WAL → ingest → DuckDB pipeline user logs use (`service = "central-logs"`, queryable via the same filter DSL) — dogfooding the platform's own ingestion path, and never logging raw credentials (only `key_id`/`key_name`/`key_prefix`).

---

## 14. Sentry-compatible error tracking (planned)

Implemented in `src/web/sentry_api.rs` + `src/errors.rs` + `src/web/errors_api.rs`
(design: [`ERROR_TRACKING.md`](ERROR_TRACKING.md)) — DSN/auth mapping onto
existing insert-scoped API keys, `envelope/` + `store/` endpoints with gzip
request decompression, fingerprint grouping, sampled stacktrace storage on
`error_groups` (not per-event), the SPA Errors page, and notification triggers
(event-time webhooks + the `error_group_threshold` evaluator condition).
