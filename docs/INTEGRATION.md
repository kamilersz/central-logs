# Integrating a New App with central-logs

A deterministic, step-by-step runbook for onboarding a new application as a
log source. Every step has an exact command, an expected result, and a
failure-mode check — suitable for execution by an AI agent or a human.

The full flow: **start the server → get an admin credential → mint a scoped
key for the app → send logs → verify them with a query**.

Conventions used below:

- Base URL: `http://localhost:8080` (default). Replace `$BASE` wherever seen.
- `jq` is used to extract fields from responses; substitute plain parsing if
  unavailable.
- All commands are idempotent-safe to re-run except key creation (each run
  mints a new key).

---

## Step 0 — Prerequisites

1. Rust toolchain (stable) if the binary must be built.
2. The server binary: `target/release/central-logs` (built in Step 1).
3. `curl` and `jq` on PATH.

## Step 1 — Build and start the server

Skip the build if `target/release/central-logs` already exists and is current.

```bash
cargo build --release
```

The server reads a local `.env` file at startup (see `.env.example`). Pin the
admin key there **once** so the credential is stable and editable across
restarts *and* data-dir resets — this is the source of truth for keys:

```bash
# Idempotent: only generates a key if .env doesn't already pin one.
if [ ! -f .env ] || ! grep -q '^CENTRAL_LOGS_HTTP_API_KEY=clk_' .env; then
  echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" >> .env
fi
```

Start the server — it picks the key up from `.env` automatically (real
environment variables take precedence if both are set):

```bash
./target/release/central-logs \
  --data-dir ./data \
  > /tmp/central-logs.log 2>&1 &
```

Expected: process stays alive; log file contains a startup line and a warning
if HTTP is bound non-loopback without auth (we pin a key, so no warning).

**Verify** (this endpoint is public, no auth):

```bash
curl -sf http://localhost:8080/health
```

Expected: HTTP 200 with a healthy body. If connection refused, wait ~1s and
retry up to 10 times; then inspect `/tmp/central-logs.log`.

> **Why keys used to "change every restart/reset":** without a pinned static
> key, the server mints a one-time bootstrap admin token whenever the
> `api_keys` table is empty (i.e. after a data-dir reset) and shows the raw
> value exactly once. With `CENTRAL_LOGS_HTTP_API_KEY` pinned in `.env`, the
> bootstrap path never fires and the admin credential never changes. CRUD
> keys (Step 3) live in DuckDB, so a data-dir reset still invalidates those —
> re-run Step 3 and update `.env`.

## Step 2 — Confirm admin access

Load the pinned key from `.env` (or export it from your real environment) and
verify it works — static keys are implicit **admin**:

```bash
export ADMIN_KEY=$(grep '^CENTRAL_LOGS_HTTP_API_KEY=' .env | cut -d= -f2-)
curl -sf http://localhost:8080/api/auth/whoami \
  -H "Authorization: Bearer $ADMIN_KEY"
```

Expected: HTTP 200, body contains `"scopes"` including `"admin"`.

Failure modes:

| Symptom | Cause | Fix |
|---|---|---|
| `401` JSON body | Wrong/revoked key | Check the `CENTRAL_LOGS_HTTP_API_KEY=` line in `.env` matches what the server was started with; restart the server after editing `.env` |
| Connection refused | Server not running | Return to Step 1 |

## Step 3 — Create the app ("creating the app")

There is no separate "app registration" object. **An app = an API key with a
name.** The key name becomes the app's identity in the audit trail; the app
itself chooses a `service` name it stamps on every log line.

Mint a key scoped to **insert only** (least privilege — this key can only
write logs, never read or administer):

```bash
curl -sf -X POST http://localhost:8080/v1/api-keys \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name": "my-app", "scopes": "insert"}'
```

Expected: HTTP 200 with:

```json
{
  "key": "clk_XXXXXXXXXXXXXXXX",
  "id": 2,
  "name": "my-app",
  "key_prefix": "clk_XXX",
  "scopes": ["insert"]
}
```

**The raw `key` value is returned exactly once and is never retrievable
again** (only a SHA-256 hash is stored). Persist it immediately — pin it in
`.env` next to the admin key so it stays editable and survives shell
sessions:

```bash
echo "MY_APP_INSERT_KEY=<paste the raw clk_... key here>" >> .env
export APP_KEY=$(grep '^MY_APP_INSERT_KEY=' .env | cut -d= -f2-)
```

Failure modes:

| Symptom | Cause | Fix |
|---|---|---|
| `401` | Caller lacks admin scope | Use the admin key from Step 2 |
| `400` "name must be 1..=128 chars" | Empty/too-long name | Retry with a shorter name |
| `400` "no valid scopes" | Bad scopes string | Use only `insert`, `read`, `write`, `admin` |

If the key is lost (or the data dir was reset, which wipes CRUD keys), mint a
replacement and revoke the old one:

```bash
# list keys (note the id of the dead key)
curl -sf http://localhost:8080/v1/api-keys -H "Authorization: Bearer $ADMIN_KEY"
# soft-revoke by id
curl -sf -X DELETE http://localhost:8080/v1/api-keys/<id> -H "Authorization: Bearer $ADMIN_KEY"
```

Then update the `MY_APP_INSERT_KEY=` line in `.env` with the new raw value.
Revocation is instant (in-memory cache drop + `revoked_at` flag) — apps using
the old key will start getting 401 immediately. Rotate, don't share.

## Step 4 — Send logs from the app ("hitting the endpoint")

Endpoint: `POST /v1/logs` (alias: `POST /v1/logs/bulk`). Accepts, in this
order of detection:

1. a JSON **array** of objects,
2. a single **JSON object**,
3. **NDJSON** (one JSON object per line — preferred for batching).

Request body cap: 16 MiB (`http_max_body_bytes`). Auth: the insert-scoped key
via `Authorization: Bearer` or `X-API-Key`.

### Record schema

| Field | Column | Notes |
|---|---|---|
| `service` (or `logger`) | `service` | **Always set this.** The primary filter key for your app. |
| `level` (or `severity`, `loglevel`, `lvl`) | `level` | e.g. `info`, `warn`, `error` |
| `msg` (or `message`) | `message` | Free text. If it contains a **stringified JSON object**, the inner fields are lifted into queryable `attributes` automatically. |
| `ts` / `timestamp` / `time` / `@timestamp` | `ts` | RFC3339 string or unix seconds. Omit → receive time is used. |
| `trace_id` (or `traceId`) | `trace_id` | Correlation id |
| `duration_ms` | `duration_ms` | Feeds latency percentiles |
| anything else | `attributes` JSON | Arbitrary key/values, queryable via the DSL |

### Minimal send (NDJSON batch)

```bash
curl -sf -X POST http://localhost:8080/v1/logs \
  -H "Authorization: Bearer $APP_KEY" \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"service":"my-app","level":"info","msg":"app started","env":"prod"}
{"service":"my-app","level":"error","msg":"DB connection refused","duration_ms":1200,"user_id":42,"env":"prod","trace_id":"tr-001"}
{"service":"my-app","level":"warn","msg":"queue lag 12s","duration_ms":80,"user_id":7,"env":"prod"}'
```

Expected: HTTP 200 and:

```json
{"accepted":3,"rejected":0,"errors":[]}
```

The response is returned only after the WAL has **fsynced** — a 200 means the
records are durably accepted (queryability follows within ~a second; see
Step 5).

Failure modes:

| Symptom | Cause | Fix |
|---|---|---|
| `401` | Key missing/revoked/no insert scope | Re-do Step 3; check header spelling |
| `403` "insufficient scope: requires 'insert'" | Key lacks `insert` | Mint a new key with `"scopes":"insert"` |
| `413` | Body > 16 MiB | Split the batch |
| `{"accepted":0,"rejected":N,...}` + HTTP 400 | All records rejected (e.g. WAL channel closed / backpressure timeout) | Check `/api/pipeline` for lag; retry |
| `errors` non-empty but `accepted > 0` | Partial rejection (HTTP 200) | Inspect `errors[]`, resend only the rejected lines |

### Instrumenting real application code

Any HTTP client works. The contract is one POST per batch:

```
POST {BASE}/v1/logs
Authorization: Bearer <insert-scoped key>
Content-Type: application/x-ndjson

{"service":"my-app","level":"info","msg":"...", ...}
{"service":"my-app","level":"info","msg":"...", ...}
```

Recommended app-side behavior: buffer lines, flush every ~1s or ~100 lines,
retry once on 5xx/backpressure, drop (with a local stderr copy) on repeated
failure — never block the app on logging.

### OpenTelemetry (OTLP/HTTP)

If the app is already instrumented with OpenTelemetry, skip the manual
envelope above and export straight to central-logs — no collector needed. The
receiver accepts OTLP/HTTP in both encodings (`application/x-protobuf` is what
most SDKs send, `application/json` also works) on `/v1/traces`, `/v1/logs`,
and `/v1/metrics`.

Auth is the same insert-scoped key, passed as a header. Configure the SDK:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=https://<your-central-logs-host>
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <APP_KEY>"
```

PHP/Laravel example (the `open-telemetry/*` packages read the same env vars):

```bash
export OTEL_PHP_AUTOLOAD_ENABLED=true
export OTEL_SERVICE_NAME=my-laravel-app
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://<your-central-logs-host>
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer <APP_KEY>"
```

> The endpoint is the base URL — the SDK appends `/v1/traces` itself. Always
> send the key: without it the receiver answers `401`, and the protobuf
> exporter will fail parsing that response (`Unexpected wire type`).

Verify: `GET /api/traces?window=1h` lists traces; `GET
/api/traces/<trace_id>` returns their spans; OTel logs are searchable with the
DSL (`protocol:otlp_log`).

### Alternatives

- **Syslog** — UDP/TCP listeners on port `5140`. Point rsyslog/syslog-ng or
  any syslog library at it; no auth on that transport. Fine for
  infrastructure devices; prefer HTTP/JSON for apps (structured fields,
  backpressure, ack).
- **JSON string in `msg`** — `{"service":"my-app","message":"{\"order_id\":\"ORD-1\",\"status\":\"ok\"}"}`.
  The inner JSON is lifted into `attributes` at parse time; reserved inner
  fields (`level`, `service`, `trace_id`, `duration_ms`) backfill the typed
  columns.

## Step 5 — Observe the log ("observing")

### 5a. Ingest-side confirmation

Insert counters are the cheapest check (read scope required — the admin key
works):

```bash
curl -sf "http://localhost:8080/api/counters" -H "Authorization: Bearer $ADMIN_KEY"
```

Expected: the `http_json` counter's accepted byte/record count has advanced.

### 5b. Query the logs back

`GET /api/logs` with the **filter DSL**. Ingest is asynchronous (insert ack →
background parse → DuckDB), so **wait ~1s and retry a few times** before
concluding failure.

First query — all rows for this app in the last hour:

```bash
sleep 2
curl -sf "http://localhost:8080/api/logs?filter=service:my-app&window=1h" \
  -H "Authorization: Bearer $ADMIN_KEY"
```

Expected: HTTP 200 and a body shaped like:

```json
{
  "rows": [
    { "service": "my-app", "level": "info",  "message": "app started", "...": "..." },
    { "service": "my-app", "level": "error", "message": "DB connection refused", "user_id": 42, "...": "..." }
  ],
  "total_matched": 3,
  "truncated": false,
  "filter_error": null
}
```

Success criteria: `filter_error` is `null`, `total_matched >= 3`, and the
inserted messages appear in `rows`.

DSL quick reference (URL-encode spaces as `%20`):

| Filter | Meaning |
|---|---|
| `service:my-app` | all logs from this app |
| `service:my-app level:error` | errors only |
| `service:my-app user_id:42` | hot/typed attribute match |
| `service:my-app trace_id:tr-001` | follow one request |
| `service:my-app "connection refused"` | substring search |
| `service:my-app duration_ms>=1000` | slow calls |

Parameters: `window` (`5m`/`1h`/`24h`/`7d`) or `from`/`to` (RFC3339);
`limit` (default 100, max 1000); `offset` for pagination.

Count-only check:

```bash
curl -sf "http://localhost:8080/api/logs/count?filter=service:my-app%20level:error&window=1h" \
  -H "Authorization: Bearer $ADMIN_KEY"
# → {"count":1}
```

Failure modes:

| Symptom | Cause | Fix |
|---|---|---|
| `rows: []`, `total_matched: 0` | Ingest lag (async pipeline) | Sleep 2s, retry up to ~5 times before diagnosing |
| `filter_error` non-null | Bad DSL (unknown column, wrong operator for type) | Use `GET /api/schema/hot` to list valid columns; quoted values for text with spaces |
| `401` | No/expired credential | Re-authenticate; sessions die on server restart |

### 5c. Dashboards / UI

Open `http://localhost:8080/`, log in with the admin key, and use **Logs
Explorer** (filter `service:my-app`), the **Volume** preset, and
**Latency p50/p95/p99** (populated once your app sends `duration_ms`).

## Step 6 — Done checklist

- [ ] `/health` returns 200
- [ ] App key created with `insert` scope, raw value stored securely
- [ ] `POST /v1/logs` returns `{"accepted":N,"rejected":0,"errors":[]}`
- [ ] `GET /api/logs?filter=service:my-app` returns the sent rows with `filter_error: null`
- [ ] (Optional) dashboards show the app's volume/latency

## Appendix — Full worked script

```bash
set -euo pipefail
BASE=http://localhost:8080

# 1. pin an admin key in .env once (idempotent)
if [ ! -f .env ] || ! grep -q '^CENTRAL_LOGS_HTTP_API_KEY=clk_' .env; then
  echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" >> .env
fi
export ADMIN_KEY=$(grep '^CENTRAL_LOGS_HTTP_API_KEY=' .env | cut -d= -f2-)

# 2. mint an insert-only key for the app and pin it in .env
if ! grep -q '^MY_APP_INSERT_KEY=clk_' .env; then
  NEW_KEY=$(curl -sf -X POST "$BASE/v1/api-keys" \
    -H "Authorization: Bearer $ADMIN_KEY" \
    -H 'Content-Type: application/json' \
    -d '{"name":"my-app","scopes":"insert"}' | jq -r .key)
  echo "MY_APP_INSERT_KEY=$NEW_KEY" >> .env
fi
export APP_KEY=$(grep '^MY_APP_INSERT_KEY=' .env | cut -d= -f2-)

# 3. send logs
curl -sf -X POST "$BASE/v1/logs" \
  -H "Authorization: Bearer $APP_KEY" \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"service":"my-app","level":"info","msg":"app started","env":"prod"}
{"service":"my-app","level":"error","msg":"DB connection refused","duration_ms":1200,"user_id":42}'

# 4. observe (ingest is async — poll)
sleep 2
curl -sf "$BASE/api/logs?filter=service:my-app&window=1h" \
  -H "Authorization: Bearer $ADMIN_KEY" | jq .
```

> The script is re-runnable after a data-dir reset:
>
> - The **admin key** survives, because it's pinned in `.env` and never
>   stored in DuckDB — re-run the whole script as-is.
> - The **insert key** lives in the `api_keys` DuckDB table (only its
>   SHA-256 hash), so a data-dir reset invalidates it. Delete the
>   `MY_APP_INSERT_KEY=` line from `.env` first, then re-run so Step 3 mints
>   a fresh key.
