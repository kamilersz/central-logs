# OpenTelemetry (OTLP/HTTP)

Any OpenTelemetry SDK — Java, Python, Node, Go, Rust, PHP, .NET —
can export directly to central-logs over **OTLP/HTTP** with **no
collector**. Both encodings are accepted on the same routes:

- `application/x-protobuf` (the default for most SDKs)
- `application/json` (OTLP/JSON)

Gzip request bodies are decompressed automatically.

## Endpoints

| Signal | Route |
|---|---|
| Traces | `POST /v1/traces` |
| Logs | `POST /v1/logs` (shared with NDJSON ingest — content-type dispatches) |
| Metrics | `POST /v1/metrics` |

Auth: same insert-scoped `clk_...` API key, passed as a request header.

## Configure the SDK

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=https://your-central-logs-host
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer clk_..."
# (or signal-specific: OTEL_EXPORTER_OTLP_TRACES_HEADERS, _LOGS_HEADERS, _METRICS_HEADERS)
```

The SDK appends `/v1/traces` (or the signal-specific suffix) to the
endpoint. Always send the key: without it the receiver answers `401`
and the protobuf exporter will fail parsing that response with an
`Unexpected wire type` error.

## PHP / Laravel

The `open-telemetry/*` packages read the same env vars:

```bash
export OTEL_PHP_AUTOLOAD_ENABLED=true
export OTEL_SERVICE_NAME=my-laravel-app
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://your-central-logs-host
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer clk_..."
```

## Spring Boot / Java

Set `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` (Spring Boot's default
is gRPC, which central-logs does not implement in this version).

## Storage mapping

All signals land in the same durable WAL → DuckDB pipeline as the
HTTP/JSON path:

| Signal | `protocol` | Notes |
|---|---|---|
| Traces | `otlp_span` | one row per span: `service`, `message` = span name, `trace_id`, `span_id`, `level` = `error` when `STATUS_CODE_ERROR`, `duration_ms`. Span attrs + resource attrs + `parent_span_id` / `span_kind` / `status_code` in `attributes`. |
| Logs | `otlp_log` | `service`, `level` from severity, `message` from body, trace/span ids, log + resource attributes. |
| Metrics | `otlp_metric` | one row per data point: `message` = metric name, `attributes.value` = numeric value, plus labels, `unit`, `metric_type`, and histogram `hist_*` fields. |

Because spans carry `trace_id` / `span_id` / `duration_ms`, they are
queryable with the normal DSL (`trace_id:...`, `protocol:otlp_span`)
and feed the existing latency dashboards / rollups automatically.

## Query back

```bash
# Recent traces
curl -sf "http://localhost:8080/api/traces?window=1h" \
  -H "Authorization: Bearer $ADMIN_KEY"

# One trace, all spans
curl -sf "http://localhost:8080/api/traces/<trace_id>" \
  -H "Authorization: Bearer $ADMIN_KEY"

# Numeric time series for one metric
curl -sf "http://localhost:8080/api/metrics/series?name=requests.count&service=api" \
  -H "Authorization: Bearer $ADMIN_KEY"
```

Filter OTel logs alongside regular ones:

```
protocol:otlp_span
protocol:otlp_log
trace_id:abc123
-(protocol:sentry OR protocol:otlp_metric) level:error
```

## What it doesn't do (yet)

- **OTLP/gRPC (port 4317)** — not implemented in this version. Set
  `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` on the SDK.

## Next

- [Syslog →](syslog.md)
- [Sentry SDK →](sentry.md)
- [Querying back with the filter DSL →](../query.md)