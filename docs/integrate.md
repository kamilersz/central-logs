# Integrating an app — short version

This page is a quick entry point. The canonical runbook lives in
[`INTEGRATION.md`](INTEGRATION.md) at the repo root (it stays
close to the source so the runbook and the code can't drift apart);
this page exists to surface it from the docs site and to point at the
typed loggers in [`integration-sample/`](integration-sample/).

If you're an AI agent onboarding a new app, see also
[For AI agents](llms.md) for the recipe form.

## TL;DR

```bash
# 1. Pin an admin credential in the server's .env (one-time, idempotent).
if [ ! -f .env ] || ! grep -q '^CENTRAL_LOGS_HTTP_API_KEY=clk_' .env; then
  echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" >> .env
fi

# 2. Start the server (it loads .env).
./target/release/central-logs &

# 3. Mint an insert-only key for the app.
ADMIN=$(grep ^CENTRAL_LOGS_HTTP_API_KEY= .env | cut -d= -f2-)
NEW_KEY=$(curl -sf -X POST http://localhost:8080/v1/api-keys \
  -H "Authorization: Bearer $ADMIN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","scopes":"insert"}' | jq -r .key)
echo "MY_APP_INSERT_KEY=$NEW_KEY" >> .env

# 4. Ship logs.
APP_KEY=$(grep ^MY_APP_INSERT_KEY= .env | cut -d= -f2-)
curl -sf -X POST http://localhost:8080/v1/logs \
  -H "Authorization: Bearer $APP_KEY" \
  -H 'Content-Type: application/x-ndjson' \
  --data-binary '{"service":"my-app","level":"info","msg":"app started"}'

# 5. Verify (~2s after the insert, then retry up to ~5 times).
sleep 2
curl -sf "http://localhost:8080/api/logs?filter=service:my-app&window=1h" \
  -H "Authorization: Bearer $ADMIN"
```

## Typed loggers by language

Ready-made batched NDJSON loggers, no telemetry SDK required:

| Language | File | Deps |
|---|---|---|
| TypeScript / JS | [`js/central-logs.ts`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/js/central-logs.ts) | none (Node 18+/Bun) |
| Python | [`python/central_logs.py`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/python/central_logs.py) | stdlib only |
| Rust | [`rust/central_logs.rs`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/rust/central_logs.rs) | std only |
| PHP | [`php/central_logs.php`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/php/central_logs.php) | ext-curl |
| Java | [`java/CentralLogs.java`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/java/CentralLogs.java) | JDK 11+ |
| C | [`c/central_logs.c`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/c/central_logs.c) | libcurl + pthread |
| C++ | [`cpp/central_logs.cpp`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/cpp/central_logs.cpp) | libcurl + pthread |

Each client reads the same env-var contract:

| Env var | Default | Meaning |
|---|---|---|
| `CENTRAL_LOGS_URL` | `http://localhost:8080` | Server base URL |
| `CENTRAL_LOGS_API_KEY` | (required) | Insert-scoped key |
| `CENTRAL_LOGS_SERVICE` | `default-app` | Stamped on every record's `service` column |
| `CENTRAL_LOGS_FLUSH_MS` | `1000` | Batch flush interval |
| `CENTRAL_LOGS_MAX_BATCH` | `100` | Flush early when queue reaches this |
| `CENTRAL_LOGS_MAX_QUEUE` | `5000` | Bounded queue (oldest dropped when full) |

## Already using OpenTelemetry?

Skip the manual envelope — point the SDK at central-logs over
OTLP/HTTP with `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` and
`OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <key>"`. See
[OTLP/HTTP ingest](ingestion/otlp.md). No collector required.

## Need the full runbook?

Read [`INTEGRATION.md`](INTEGRATION.md) at the repo root. It
covers:

- Step-by-step idempotent setup (each step has a failure-mode table)
- The full record schema with aliases
- A worked end-to-end script
- Re-running after a data-dir reset (admin key survives; insert key
  doesn't — and how to handle that)

## Next

- [HTTP / NDJSON ingest](ingestion/http.md) — record shape details
- [Querying back with the filter DSL](query.md) — verify your logs