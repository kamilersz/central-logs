# Integration samples — inserting logs into central-logs

Typed logger clients for `POST /v1/logs`, one per language. All follow the
same contract as [`INTEGRATION.md`](../INTEGRATION.md):

> **Already using OpenTelemetry?** Don't hand-roll a client — point the OTel
> SDK straight at central-logs over OTLP/HTTP (`/v1/traces`, `/v1/logs`,
> `/v1/metrics`) with `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` and
> `OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <insert key>"`. See
> [API.md § OpenTelemetry](../docs/API.md). No collector required.

| Env var | Default | Meaning |
|---|---|---|
| `CENTRAL_LOGS_URL` | `http://localhost:8080` | Server base URL |
| `CENTRAL_LOGS_API_KEY` | *(required)* | Insert-scoped key (`clk_...`) |
| `CENTRAL_LOGS_SERVICE` | `default-app` | Value stamped into the `service` column |
| `CENTRAL_LOGS_FLUSH_MS` | `1000` | Batch flush interval |
| `CENTRAL_LOGS_MAX_BATCH` | `100` | Flush early when the queue reaches this |
| `CENTRAL_LOGS_MAX_QUEUE` | `5000` | Bounded queue; oldest dropped when full |

## Samples

| Language | File | Deps | Run |
|---|---|---|---|
| TypeScript/JS | [`js/central-logs.ts`](js/central-logs.ts) | none (Node 18+/Bun) | `import { log } from "./central-logs.ts"` |
| Python | [`python/central_logs.py`](python/central_logs.py) | stdlib only | `from central_logs import log` |
| Rust | [`rust/central_logs.rs`](rust/central_logs.rs) | std only (raw HTTP/1.1) | `mod central_logs;` |
| PHP | [`php/central_logs.php`](php/central_logs.php) | ext-curl (standard) | `require 'central_logs.php';` |
| Java | [`java/CentralLogs.java`](java/CentralLogs.java) | JDK 11+ (`java.net.http`) | `new CentralLogs().log(...)` |
| C | [`c/central_logs.c`](c/central_logs.c) | libcurl, pthread | `cc app.c central_logs.c -lcurl -lpthread` |
| C++ | [`cpp/central_logs.cpp`](cpp/central_logs.cpp) | libcurl, pthread | `g++ -std=c++17 app.cpp central_logs.cpp -lcurl -lpthread` |

## Record shape (what every client sends)

```json
{"service":"my-app","level":"info","msg":"app started","ts":"2026-01-01T00:00:00Z","user_id":42}
```

- `service` / `level` / `msg` / `ts` are always set by the client.
- Everything else you pass (e.g. `user_id`, `trace_id`, `duration_ms`, `status`)
  lands in the queryable `attributes` JSON — or in typed hot columns if you
  start the server with the matching `--hot-attribute name:type` flag.
- `duration_ms` feeds the latency-percentile dashboards; `level` should be
  one of `debug` / `info` / `warn` / `error` / `fatal`.

## Setup, once per server

```bash
# 1. Pin the admin key in the server's .env (see INTEGRATION.md Step 1)
# 2. Mint an insert-only key for the app (see INTEGRATION.md Step 3):
curl -sf -X POST http://localhost:8080/v1/api-keys \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","scopes":"insert"}'
# 3. Export CENTRAL_LOGS_API_KEY=<the raw clk_... value> for the app
```

Then verify from the server side:

```bash
curl -sf "http://localhost:8080/api/logs?filter=service:my-app&window=1h" \
  -H "Authorization: Bearer $ADMIN_KEY" | jq .
```
