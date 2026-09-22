# HTTP / NDJSON ingest

The primary ingest path. One `POST /v1/logs` per batch, structured body,
bearer-token auth, ack only after the WAL is fsync'd.

## Endpoint

```
POST /v1/logs          # NDJSON / JSON array / single object
POST /v1/logs/bulk     # alias
```

Auth: `Authorization: Bearer <insert-scoped key>` or `X-API-Key: <key>`.
Body cap: 16 MiB (`http_max_body_bytes`). Content-Types accepted:

- `application/x-ndjson` — one JSON object per line (preferred for batching)
- `application/json` — a single JSON object **or** an array of objects

The server detects the format by parsing, not by header: a JSON array
or single object is accepted regardless of `Content-Type`; NDJSON is
honored when the body parses line-by-line.

## Record schema

| Field | Required | Column | Notes |
|---|---|---|---|
| `service` (alias `logger`) | recommended | `service` | Primary filter key. Always set this. |
| `level` (aliases `severity`, `loglevel`, `lvl`) | yes | `level` | `debug` / `info` / `warn` / `error` / `fatal` |
| `msg` (alias `message`) | yes | `message` | Free text. A JSON object embedded as a string is lifted to `attributes` — see below. |
| `ts` (aliases `timestamp`, `time`, `@timestamp`) | optional | `ts` | RFC3339 string or unix seconds. Defaults to receive time. |
| `trace_id` (alias `traceId`) | optional | `trace_id` | Correlation id. |
| `duration_ms` | optional | hot column | Feeds latency dashboards. |
| anything else | optional | `attributes` JSON | Becomes queryable. Promote with `--hot-attribute` for typed columns. |

## Quick send

```bash
curl -sf -X POST http://localhost:8080/v1/logs \
  -H "Authorization: Bearer $APP_KEY" \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"service":"my-app","level":"info","msg":"app started","env":"prod"}
{"service":"my-app","level":"error","msg":"DB connection refused","duration_ms":1200,"user_id":42,"trace_id":"tr-001"}
{"service":"my-app","level":"warn","msg":"queue lag 12s","duration_ms":80,"user_id":7}'
```

```json
{"accepted":3,"rejected":0,"errors":[]}
```

That 200 is returned **after the WAL has been fsync'd** — the records
are durable. Queryability follows within ~1s as the ingest workers
parse and write to DuckDB.

## Response shape

```json
{
  "accepted": 3,           // rows durably fsync'd
  "rejected": 0,           // rows the insert layer refused
  "errors": []             // per-line error messages for any rejected rows
}
```

Status codes:

| Code | Meaning |
|---|---|
| 200 | All rows accepted (or partial — see `errors` field) |
| 400 | All rows rejected (bad body / no body) |
| 401 | No / revoked credential |
| 403 | Insufficient scope (key not `insert`) |
| 413 | Body > 16 MiB |
| 503 | WAL cap exceeded (inserts paused); see `/api/pipeline` |

## message-JSON lifting

Apps that embed a JSON object as a string inside `msg` get the inner
fields lifted automatically. This is on by default; turn off with
`--no-unwrap-message-json`.

```json
{"service":"my-app","message":"{\"order_id\":\"ORD-1\",\"status\":\"ok\"}"}
```

…becomes queryable as `order_id:ORD-1` in addition to keeping the
original string in `message`. Reserved inner fields (`level`,
`service`, `trace_id`, `duration_ms`) backfill the typed columns.

## Drop-attributes

Strip noisy keys at parse time, before persistence:

```bash
--drop-attribute 'debug_payload'
```

Smaller messages travel faster; storage only pays for what you keep.

## Typed logger clients

Ready-made clients in [`integration-sample/`](https://github.com/kamilersz/central-logs/tree/main/integration-sample):

| Language | File | Deps |
|---|---|---|
| TypeScript / JS | [`js/central-logs.ts`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/js/central-logs.ts) | none (Node 18+/Bun) |
| Python | [`python/central_logs.py`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/python/central_logs.py) | stdlib only |
| Rust | [`rust/central_logs.rs`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/rust/central_logs.rs) | std only |
| PHP | [`php/central_logs.php`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/php/central_logs.php) | ext-curl |
| Java | [`java/CentralLogs.java`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/java/CentralLogs.java) | JDK 11+ |
| C | [`c/central_logs.c`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/c/central_logs.c) | libcurl + pthread |
| C++ | [`cpp/central_logs.cpp`](https://github.com/kamilersz/central-logs/blob/main/integration-sample/cpp/central_logs.cpp) | libcurl + pthread |

Recommended app-side behavior (used by every sample): buffer lines,
flush every ~1s or ~100 lines, retry once on 5xx/backpressure, drop
(with a local stderr copy) on repeated failure — never block the app
on logging.

## Next steps

- [Querying back with the filter DSL](../query.md)
- [OpenTelemetry OTLP/HTTP ingest →](otlp.md)
- [Integration samples →](../integration-sample/README.md)
- [Integration samples on GitHub →](https://github.com/kamilersz/central-logs/tree/main/integration-sample)