# Features

- **Insert-first ingest** — HTTP/JSON (`POST /v1/logs`), OpenTelemetry
  **OTLP/HTTP** (`/v1/traces`, `/v1/logs`, `/v1/metrics`; protobuf + OTLP/JSON,
  any SDK/language, no collector), syslog UDP, syslog TCP. Durable
  fsync group-commit on the hot path; parsing/enrichment happens
  asynchronously so the source is acked fast.
- **CRC32-framed WAL** — append-only segments with `crc32fast` checksums and
  rotation by size. `redb` tracks ingest checkpoints so a crash resumes from
  the last durably-committed position.
- **DuckDB analytics store** — typed columns for known fields + native JSON for
  arbitrary attributes. Per-row-group min/max zonemaps make time-range filters
  cheap without manual indexes.
- **Hot attributes** — promote high-cardinality JSON keys (`user_id`,
  `trace_id`, `route`) to typed top-level columns at runtime. Filter queries go
  from per-row JSON parses to zonemap-pruned column scans. Configured via CLI
  flag or TOML; `ALTER TABLE ADD COLUMN IF NOT EXISTS` makes it forward-
  compatible across restarts.
- **message-JSON lifting** — apps that embed a JSON object as a string inside
  `msg`/`message` (`{"message":"{\"order_id\":\"ORD-1\",...}"}`) get the inner
  fields lifted into queryable `attributes` (and hot columns) at parse time.
  Reserved inner fields (`level`, `service`, `trace_id`, …) backfill the typed
  columns; the original string stays in `message` for display. On by default;
  disable with `--no-unwrap-message-json`.
- **Drop-attributes** — strip noisy JSON keys (`--drop-attribute`, repeatable)
  at parse time before persistence, so storage only pays for fields you keep.
- **Hot/cold tiering** — background compaction exports aged rows to hive-
  partitioned Parquet (`date=YYYY-MM-DD/hour=HH/service=<name>/...`) with
  `ORDER BY` clustering, zstd compression (configurable codec), and bloom
  filters on high-cardinality VARCHAR columns. `logs_all` view unions
  hot + cold transparently.
- **Per-service retention** — `[[service_retention]]` glob rules (first match
  wins; e.g. `audit_*` 90 days, `debug_*` 3 days) on top of the global
  `retention_days`. The `service=` partition level makes per-service cold-tier
  purge exact; legacy flat layouts keep working.
- **Sharded ingest workers** — workers split the WAL by segment id (Kafka's
  one-consumer-per-partition rule, local edition); parse/enrich/extract run
  in parallel with at-least-once checkpoint semantics.
- **Pipeline observability** — insert-channel fill gauge, WAL throughput
  counter, ingest-lag bytes (consumer lag), audit-drop counter — all in
  `/metrics` and the SPA **Pipeline** page. Backpressure is visible *before*
  data drops.
- **Rollups** — 1-minute and 1-hour materialized aggregates with `time_bucket`,
  `APPROX_QUANTILE` for p50/p95/p99 latency. Dashboards read rollups almost
  exclusively.
- **Forecasting** — ETS/Holt-Winters via [`augurs`](https://github.com/grafana/augurs).
  Reads from the same rollup tables the dashboards use; surfaces both a
  dashboard overlay and an MCP tool.
- **Anomaly detection** — three complementary methods (rolling MAD,
  seasonal-adjusted for daily/weekly cycles, rate-of-change for spikes).
  Results land in a small `anomalies` table.
- **Object-storage cold archive** (optional) — build with
  `--features object-storage` and configure `[cold_storage]` to mirror
  compacted Parquet to S3/GCS/MinIO. Remote objects follow local retention
  (purged locally → deleted remotely); local files stay as the queryable
  cache (`keep_local = true`, default).
- **Per-service query caps** — `[[service_query_limits]]` glob rules cap the
  query window when a filter pins `service:<name>` (per-stream
  `max_query_range`, so one wide scan can't squeeze the node).
- **Filter DSL** — `service:api level:error user_id:42 "connection refused"`
  parses to safe parameterized SQL. Column whitelist prevents injection; values
  always bound, never interpolated.
- **SPA dashboard** — React + Vite + Tremor, embedded into the binary via
  `rust-embed`. Multi-page app with router:
  - **Logs Explorer** — time-range picker, filter bar with click-to-insert
    column badges, click-to-expand row details, **Ask AI** button that calls
    the LLM provider to translate natural language into a filter DSL.
  - **Dashboards Home** — built-in presets + your saved custom dashboards.
  - **Dashboard Builder** — name a dashboard, add/remove panels
    (volume / error-rate / latency / top-services / log-count), each with its
    own window and filter DSL.
  - **Dashboard Viewer** — renders saved panels in a grid.
  - **Presets** — Volume + forecast overlay (dashed projection),
    Error rate (per-minute + %), Latency p50/p95/p99 per service, Anomalies
    list with severity badges.
  - **Alerts** — list rules grouped by status (pending_approval / active /
    rejected); approve/reject buttons for the human-in-the-loop workflow
    required by architecture §7.
- **Alert evaluation + webhook notification** — a background evaluator
  re-checks every `active` rule on a fixed interval (`alert_eval_interval_secs`,
  default 30s) against the same rollup/anomaly tables the dashboards read,
  and POSTs a JSON payload to the rule's `channel` (any `http(s)` URL — a
  Slack/Discord/Teams incoming webhook or your own endpoint) when a
  threshold or anomaly condition breaches, subject to a per-rule cooldown.
  Every evaluation is recorded in `alert_events`.
- **MCP server** — five tools (`query_logs`, `get_dashboard_summary`,
  `detect_anomalies`, `forecast`, `create_alert_rule`) over stdio for
  Claude Desktop / Code integration. Alert creation is gated behind
  `--enable-alert-mcp-tool` and always lands in `pending_approval` status.
- **AI auto-filter** — natural-language query ("show me errors for user 42 in
  the last hour") → server-side LLM call (OpenAI or Anthropic) → filter DSL
  that the UI applies. The model never sees actual log data, only the schema.
- **Sentry-compatible error tracking** — unmodified Sentry SDKs point their
  DSN at central-logs (`http://clk_KEY@host:8080/7` + a project→service
  mapping); errors are grouped by fingerprint, stacktraces sampled per group
  (first + latest distinct variants, truncated), displayed in the SPA
  **Errors** page (search, status/window/sort filters, trend sparkline,
  resolve/ignore, AI explain), marked `protocol=sentry` so they filter
  separately from regular logs, and notified on first-seen, regression, or
  threshold — see [ERROR_TRACKING.md](ERROR_TRACKING.md).
