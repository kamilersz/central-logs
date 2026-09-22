# Getting Started

A ten-minute path from a fresh checkout to your first batch of queryable
logs. By the end you will have:

- the binary built and running on `:8080`,
- an admin credential pinned in `.env`,
- an `insert`-only API key for your app,
- a few log lines shipped and verified with the filter DSL.

## 1. Prerequisites

- **Rust** (stable, ≥ 1.85 — the `rust-version` in `Cargo.toml`)
- **Node 18+** and **npm** — only needed when you change files under
  `web/` (the SPA is embedded into the release binary at build time)
- **curl** + **jq** for the shell snippets below

A 64-bit Linux/macOS machine is the recommended deployment target. The
binary is fully self-contained — no Postgres, no Redis, no Docker
required.

## 2. Build

```bash
git clone https://github.com/kamilersz/central-logs.git
cd central-logs
cargo build --release
```

The release binary lands at `target/release/central-logs`. The first build
takes a few minutes; subsequent builds are incremental.

> **Need the S3 / GCS cold-tier archive?** Rebuild with
> `--features object-storage`. See
> [Configuration → Cold storage](CONFIGURATION.md).

## 3. Pin an admin credential

The server mints a one-time admin token on first boot, but the
> **one-time** part bites you later (after a data-dir reset, or a backup
> restore into a fresh dir). Pin a stable credential in `.env` instead:

```bash
cat > .env <<EOF
CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)
EOF
chmod 600 .env
```

This key is treated as an implicit `admin` credential by every endpoint.
See [Security](SECURITY.md) for the full model.

## 4. Start the server

```bash
./target/release/central-logs
```

You should see lines like:

```
INFO central_logs: HTTP API + dashboard listening addr=0.0.0.0:8080
INFO central_logs: WAL writer ready
INFO central_logs: ingest workers=2 started
INFO central_logs: rollup task started every=60s
INFO central_logs: central-logs ready
```

Browse to `http://localhost:8080/login` and paste the value from
`CENTRAL_LOGS_HTTP_API_KEY` in `.env` to log in.

> **Heads-up**: when the server is bound to a non-loopback interface
> without an admin key, it warns loudly at startup. Don't ship that to
> prod — set the key.

## 5. Mint an insert-only key for your app

The admin key should never be on an application host. Mint a
**least-privilege** `insert`-scoped key per app and persist it next to
your app's secrets.

```bash
ADMIN=$(grep '^CENTRAL_LOGS_HTTP_API_KEY=' .env | cut -d= -f2-)

curl -sf -X POST http://localhost:8080/v1/api-keys \
  -H "Authorization: Bearer $ADMIN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","scopes":"insert"}'
```

The response contains the raw `clk_...` value **once**:

```json
{
  "key": "clk_XXXXXXXXXXXXXXXX",
  "id": 2,
  "name": "my-app",
  "key_prefix": "clk_XXX",
  "scopes": ["insert"]
}
```

Pin it for the app:

```bash
echo "MY_APP_INSERT_KEY=clk_XXXXXXXXXXXXXXXX" >> .env
```

## 6. Ship your first batch

```bash
APP_KEY=$(grep '^MY_APP_INSERT_KEY=' .env | cut -d= -f2-)

curl -sf -X POST http://localhost:8080/v1/logs \
  -H "Authorization: Bearer $APP_KEY" \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"service":"my-app","level":"info","msg":"app started","env":"prod"}
{"service":"my-app","level":"error","msg":"DB connection refused","duration_ms":1200,"user_id":42}
{"service":"my-app","level":"warn","msg":"queue lag 12s","duration_ms":80,"user_id":7}'
```

A successful response looks like:

```json
{"accepted":3,"rejected":0,"errors":[]}
```

That `200` is returned **after the WAL has been fsync'd** — the records
are durable. Queryability follows within ~1s as the ingest workers parse
and write to DuckDB.

## 7. Query them back

Wait ~2s for the asynchronous ingest to catch up, then query with the
[filter DSL](query.md):

```bash
sleep 2
ADMIN=$(grep '^CENTRAL_LOGS_HTTP_API_KEY=' .env | cut -d= -f2-)

curl -sf "http://localhost:8080/api/logs?filter=service:my-app&window=1h" \
  -H "Authorization: Bearer $ADMIN" | jq .
```

You should see all three rows in the `rows` array, with `total_matched: 3`
and `filter_error: null`.

A few useful variants:

| Filter | Meaning |
|---|---|
| `service:my-app` | all logs from this app |
| `service:my-app level:error` | errors only |
| `service:my-app user_id:42` | hot/typed attribute match |
| `service:my-app trace_id:tr-001` | follow one request |
| `service:my-app "connection refused"` | substring search |
| `service:my-app duration_ms>=1000` | slow calls |

## 8. Done checklist

- [ ] `/health` returns 200 (no auth)
- [ ] `CENTRAL_LOGS_HTTP_API_KEY` pinned in `.env`
- [ ] App key created with `insert` scope, raw value persisted
- [ ] `POST /v1/logs` returns `{"accepted":N,"rejected":0,"errors":[]}`
- [ ] `GET /api/logs?filter=service:my-app` returns the sent rows

## What to read next

- [Configuration](CONFIGURATION.md) — knobs you'll likely want to set
  before any real load lands: retention, hot attributes, parquet codec.
- [For AI agents](llms.md) — point your coding agent at this server
  with MCP, or use the **Ask AI** button in the SPA.
- [Integration samples](integration-sample/README.md) — typed
  loggers in TypeScript, Python, Rust, PHP, Java, C, and C++.

For a deeper introduction to the system's internals, the
[Architecture](ARCHITECTURE.md) document is the canonical reference.