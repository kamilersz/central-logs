# Ingesting logs

central-logs accepts logs from many source systems. Pick the path that
matches how your apps emit data — most apps only need one.

| Path | Best for | Endpoint |
|---|---|---|
| [HTTP / NDJSON](http.md) | Generic apps, scripts, anything with an HTTP client. | `POST /v1/logs` |
| [OpenTelemetry OTLP/HTTP](otlp.md) | Apps already instrumented with an OTel SDK — zero source-side changes. | `POST /v1/traces`, `/v1/logs`, `/v1/metrics` |
| [Syslog](syslog.md) | Routers, firewalls, OS daemons, anything that already speaks syslog. | UDP/TCP on `:5140` |
| [Sentry SDK](sentry.md) | Existing Sentry SDKs in any language — point them at central-logs, get fingerprint grouping, stack sampling, error notifications. | `/api/{project}/envelope/` and `/store/` |
| [Docker Engine & Kubernetes](containers.md) | Container logs via Docker's `gelf`/`fluentd`/`splunk` drivers, `kube-apiserver` audit webhooks, or the built-in pull collectors. | `:12201`, `:24224`, `/services/collector/event/1.0`, `/ingest/kubernetes/audit` |

All paths share the same durable insert layer: the request is acked
only after the WAL is **fsync'd**, then parsing/enrichment happens
asynchronously. The insert path is fast and bounded — see
[Architecture §2](../ARCHITECTURE.md#2-insert-vs-ingest-split) for the
mechanics.

## Choosing a path

- **You control the app code** → HTTP/NDJSON, with a typed client from
  [`integration-sample/`](../integration-sample/README.md). Lowest
  friction.
- **The app is already instrumented with OpenTelemetry** → OTLP/HTTP.
  No new client, no collector, just point the SDK.
- **The source is a device or OS daemon** → syslog. Configure
  rsyslog/syslog-ng or your device's syslog target at `:5140`.
- **The source is a Sentry SDK already in production** → swap the DSN,
  restart the SDK. No code change.

## Record shape

Regardless of path, every accepted row lands in the same `logs` table
with a common set of typed columns plus a JSON `attributes` column for
anything else. The full schema is in [HTTP API](../API.md).

| Field | Required | Where it comes from |
|---|---|---|
| `service` | recommended | app-supplied; `service:api` is the primary filter key |
| `level` | yes | app-supplied; `info` / `warn` / `error` / `fatal` / `debug` |
| `message` | yes | app-supplied |
| `ts` | optional | app-supplied; defaults to receive time |
| `trace_id` / `span_id` | optional | OTel spans or app-supplied |
| `duration_ms` | optional | feeds the latency p50/p95/p99 dashboards |
| `attributes.*` | optional | app-supplied; queryable via the [filter DSL](../query.md) |
| `protocol` | auto | `http_json` / `syslog_udp` / `syslog_tcp` / `otlp_log` / `otlp_span` / `otlp_metric` / `sentry` / `gelf` / `fluentd` / `splunk_hec` / `k8s_audit` / `docker_api` / `k8s_api` |

`duration_ms` is the only "magic" field: any log line that carries it
participates in the latency dashboards.

## Promoting fields to typed columns

JSON keys in `attributes` are queryable today, but each filter is a
per-row JSON parse. Promote keys you filter on often to typed top-level
columns and DuckDB's row-group min/max zonemaps do the pruning for
you:

```bash
--hot-attribute 'user_id:bigint'
--hot-attribute 'route:varchar:$.request.route'
--hot-attribute 'is_canary:boolean'
```

TOML equivalent lives in [Configuration → hot_attributes](../CONFIGURATION.md).

## Dropping

Strip noisy keys before persistence (`debug_payload`, large blobs, …):

```bash
--drop-attribute 'debug_payload'
```

Removing it early is cheaper than storing it and never using it.

## What's next

- [HTTP / NDJSON ingest →](http.md)
- [OpenTelemetry OTLP/HTTP →](otlp.md)
- [Syslog →](syslog.md)
- [Sentry SDK →](sentry.md)
- [Docker Engine & Kubernetes →](containers.md)