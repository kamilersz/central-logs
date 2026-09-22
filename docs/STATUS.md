# Status

What's done and tested:

- WAL durability, ingest pipeline, DuckDB hot store
- Hot/cold compaction to Parquet with sort + bloom filters
- Rollups (1m/1h) with percentile aggregates
- Filter DSL with type-aware parameterized SQL
- All HTTP APIs in [API.md](API.md)
- MCP server over stdio with all 5 tools
- ETS forecasting, three-method anomaly detection
- Hot-attribute promotion end-to-end (config → schema → parse → insert → filter),
  covered by an HTTP-insert → WAL → ingest → rollup → filter-DSL integration test
- SPA: Logs Explorer, Dashboards Home, Builder, Viewer, 4 presets (Volume+Forecast,
  Error Rate, Latency p50/p95/p99, Anomalies), Pipeline health, Alerts page with
  approve/reject, API Keys (admin). Dark-mode "mission control" design system
  (navy canvas, electric-blue brand, mono machine data, semantic level
  badges, dashed-grid line charts with HTML legends) — see
  [`web/tailwind.config.js`](https://github.com/kamilersz/central-logs/blob/main/web/tailwind.config.js) + [`web/src/index.css`](https://github.com/kamilersz/central-logs/blob/main/web/src/index.css).
- AI auto-filter via OpenAI, Anthropic, or 9inference (nemotron-3-ultra)
- message-JSON lifting, drop-attributes, zstd cold Parquet, per-service
  retention, sharded ingest workers, pipeline health gauges, per-service
  query caps
- Alert-rule evaluation + webhook notification: a background evaluator fires
  threshold/anomaly conditions against active rules and POSTs to the rule's
  `channel`, with per-rule cooldown and firing history (`alert_events`)
- Optional object-storage archive (`--features object-storage`): manifest-
  tracked upload/purge mirroring to S3/GCS/MinIO
- Graceful shutdown: SIGTERM is handled (not just SIGINT), and DuckDB is
  checkpointed on `tokio_util::CancellationToken` cancellation, so the next
  start never has to replay a dirty WAL (the crash that used to take down a
  systemd restart cycle)

- Sentry-compatible error tracking: unmodified Sentry SDKs point their DSN
  at central-logs; errors grouped by fingerprint, stacktraces sampled per
  group, an **Errors** SPA page (search/filter/resolve/AI explain), and
  notifications on first-seen, regression, or threshold — see
  [ERROR_TRACKING.md](ERROR_TRACKING.md); verified end-to-end with the real
  `sentry-sdk` (`tests/e2e_sentry_errors.py`). Sentry-format events are
  marked `protocol=sentry` (DSL-separable from regular logs) and carry a
  group lifecycle: unresolved / resolved (= done; regression re-opens +
  notifies) / ignored (= deferred)

- Housekeeping: configurable retention (`[retention]` hot age/size caps +
  a WAL ingest cap that pauses inserts with 503), timezone-aware backup
  schedules (`[backup]`, daily@HH:MM in any IANA tz, or size-triggered),
  atomic tar.gz snapshots with sha256 manifests to a local dir and/or
  S3/GCS, checksum-verified restore into a fresh dir (CLI flag or Storage
  page), and a Storage page (hot/cold footprint, per-day cold partitions,
  backup runs, one-click backup/restore)

- OpenTelemetry **OTLP/HTTP ingest** (`/v1/traces`, `/v1/logs`, `/v1/metrics`),
  protobuf + OTLP/JSON, gzip bodies, insert-scoped auth — any OTel SDK works
  with no collector. Spans land as `protocol=otlp_span` rows (trace_id /
  span_id / duration_ms, so they feed the explorer + latency rollups), OTel
  logs as `protocol=otlp_log`, metric points as `protocol=otlp_metric`; read
  back via `/api/traces`, `/api/traces/{trace_id}`, `/api/metrics/series`.

What's stubbed or deferred (see [ARCHITECTURE.md](ARCHITECTURE.md) §9):

- MCP SSE/streamable-HTTP transport is not implemented — `--mcp-mode sse` is
  rejected at startup rather than silently running with no server (stdio
  works fully)
- OTLP/gRPC ingest (port 4317) — OTLP/HTTP is implemented; gRPC is still
  phase 2. SDKs whose default is gRPC (e.g. some Spring Boot setups) should
  set `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`.
- A first-party file-tailing shipper is on phase 3 per architecture. Until
  then, see the [Shipper sidecar](OPERATIONS.md#shipper) in OPERATIONS.md.
- Alert notification channels are `http(s)` webhooks only — no dedicated
  SMTP/Slack-API/PagerDuty integration (a webhook URL covers most of those
  in practice; see [Alert rules](API.md#alert-rules-architecture-7))
