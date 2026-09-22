# For AI agents

This page is the LLM-friendly counterpart to the rest of the
documentation. It assumes the reader is an AI agent — a coding
assistant, an MCP client, or a tool-using LLM — that needs to know what
central-logs is, how to talk to it, and what it should and shouldn't
do autonomously.

For end-users, the [Getting Started](getting-started.md) page is more
appropriate.

## What central-logs is, in one paragraph

central-logs is a self-hosted, single-node centralized logging
platform. It accepts logs over HTTP (NDJSON / JSON), OpenTelemetry
OTLP/HTTP (any SDK, no collector), syslog UDP+TCP, and a
Sentry-SDK-compatible DSN; writes them durably to a CRC-framed WAL
with fsync-on-ack; parses, indexes, and stores them in DuckDB (hot tier)
with periodic compaction into hive-partitioned Parquet (cold tier);
runs volume / error-rate / latency dashboards, three-method anomaly
detection, ETS time-series forecasting, and Sentry-style error-group
tracking; and exposes all of this over a small JSON HTTP API plus a
five-tool MCP server for AI agents.

One Rust binary. No cluster. No external database.

## Three ways an agent can use it

1. **MCP tools** (recommended) — `query_logs`,
   `get_dashboard_summary`, `detect_anomalies`, `forecast`,
   `create_alert_rule`. Run `central-logs --mcp-mode stdio` as an MCP
   server. See [MCP server](mcp.md) for the tool reference.
2. **HTTP API** — every read/write endpoint is plain HTTP/JSON with
   bearer-token auth. See [HTTP API](API.md).
3. **CLI** — the binary itself is the only CLI; there is no separate
   `cl` command, only `cargo run --` and the binary's own flags.

## Recommended contract: `llms.txt`

Publish a copy of [`llms.txt`](llms.txt) at the root of your
deployment (or at a stable public URL). Modern agents and LLM-powered
tooling fetch `llms.txt` first when discovering a service — it gives
them a one-shot map of capabilities, endpoints, and safety rails.

The shipped `llms.txt` is intentionally short: it is a discoverable
index, not a re-statement of the entire docs. An agent that needs
detail follows the links to the canonical pages.

## An agent-friendly recipe

Below is a self-contained recipe a coding agent (Claude Code, Codex,
OpenCode, …) can follow to stand up central-logs and ship logs into
it from any app on the same host. The corresponding steps for humans
are in [Getting Started](getting-started.md).

```text
1. Build:
   cargo build --release

2. Pin an admin key (idempotent):
   if no CENTRAL_LOGS_HTTP_API_KEY in .env:
     echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" >> .env
     chmod 600 .env

3. Start the server:
   ./target/release/central-logs &

4. Wait for readiness (the loop polls /health, which is public):
   until curl -sf http://localhost:8080/health; do sleep 0.5; done

5. Mint an insert-scoped key for the app and persist it:
   ADMIN=$(grep ^CENTRAL_LOGS_HTTP_API_KEY= .env | cut -d= -f2-)
   curl -sf -X POST http://localhost:8080/v1/api-keys \
     -H "Authorization: Bearer $ADMIN" \
     -H 'Content-Type: application/json' \
     -d '{"name":"my-app","scopes":"insert"}'

   # the response contains `key`: persist it to .env as MY_APP_INSERT_KEY
   # (raw value is shown exactly once).

6. Ship logs (any client works; the canonical body shape is below):
   APP_KEY=$(grep ^MY_APP_INSERT_KEY= .env | cut -d= -f2-)
   curl -sf -X POST http://localhost:8080/v1/logs \
     -H "Authorization: Bearer $APP_KEY" \
     -H 'Content-Type: application/x-ndjson' \
     --data-binary '{"service":"my-app","level":"info","msg":"..."}
                    {"service":"my-app","level":"error","msg":"..."}'

7. Verify (ingest is async; wait ~2s, retry up to ~5 times):
   sleep 2
   curl -sf "http://localhost:8080/api/logs?filter=service:my-app&window=1h" \
     -H "Authorization: Bearer $ADMIN"
```

## Record schema (what you send)

Every client stamps the same minimal record. Extra keys are passed
through to the `attributes` JSON column and become queryable via the
filter DSL; if a matching `--hot-attribute` was configured at startup,
they live in a typed column instead.

| Field | Required | Column | Notes |
|---|---|---|---|
| `service` | yes | `service` | The primary filter key. Always set this. |
| `level` | yes | `level` | `debug` / `info` / `warn` / `error` / `fatal`. |
| `msg` | yes | `message` | Free text. |
| `ts` | optional | `ts` | RFC3339 or unix seconds. Defaults to receive time. |
| `trace_id` | optional | `trace_id` | Correlation id. |
| `duration_ms` | optional | hot column | Feeds latency-percentile dashboards. |
| anything else | optional | `attributes` JSON | Becomes queryable; promote to hot columns at startup. |

Body formats accepted on `POST /v1/logs`, in detection order:

1. JSON **array** of objects
2. Single JSON **object**
3. **NDJSON** (one object per line — preferred for batching)

Content-Type hints: `application/x-ndjson` for NDJSON, `application/json`
for the others. A 16 MiB body cap applies.

## Safety rails the agent must respect

These are hard limits baked into the server — an agent should treat
them as policy, not suggestions.

- **Never auto-approve alert rules.** `create_alert_rule` MCP tool always
  creates rules in `pending_approval`. A human must approve them via
  the dashboard. Don't try to bypass.
- **Don't bypass scope checks.** Use `insert` keys for log shipping,
  `read` keys for queries, `admin` keys only for key CRUD and rule
  approval. The admin key is for operators, not for agents.
- **Body size cap** (`http_max_body_bytes`, default 16 MiB). Don't
  retry with bigger payloads; split the batch.
- **Per-service query caps.** A query that pins a `service:<glob>` that
  matches a `[[service_query_limits]]` rule is window-capped server-side.
  Don't try to widen the window — the cap is enforced.
- **WAL cap.** If the WAL directory exceeds `wal_max_bytes`, insert
  routes return `503` until compaction shrinks it. Respect the backpressure
  signal: drop locally + retry, don't loop hot.
- **Auth bootstrap** — if `api_keys` is empty and no static
  `CENTRAL_LOGS_HTTP_API_KEY` is set, the server mints a one-time
  bootstrap token and prints it to stderr. The agent must save this
  token immediately; it is shown once.

## What central-logs deliberately does not do

- **Cluster / multi-node**. One binary, one box. Scaling means
  sharding by service into separate instances (each on its own port
  with its own data dir).
- **Run a separate database server.** DuckDB is embedded; the file is
  the database.
- **gRPC ingest.** OTLP/HTTP is implemented (`/v1/traces`,
  `/v1/logs`, `/v1/metrics`). OTLP/gRPC (port 4317) is not. Set
  `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` on the SDK.
- **Remote MCP transport.** Streamable-HTTP / SSE is not implemented in
  this version; `mcp_mode = sse` is rejected at startup. Use stdio or
  proxy stdio over HTTP.
- **Alerting integrations other than webhook.** Channels are
  `http(s)` webhooks (any Slack/Discord/Teams/PagerDuty incoming
  webhook) plus SMTP email and Telegram bot sends (managed channels in
  the dashboard). No first-party PagerDuty / OpsGenie / Slack-app SDK.

## Suggested prompts for tool-using LLMs

If you wrap central-logs in an MCP client of your own, the following
prompt templates have been used in practice.

**Investigate a user report:**

> "Use the `central-logs` MCP server. Call `get_dashboard_summary` for
> the `error_rate` metric over the last hour. If error_rate is
> non-zero, call `query_logs` with the filter `level:error user_id:<id>`
> for the last 30 minutes, summarize the unique error messages, and
> call `detect_anomalies` for `error_rate` over the last 6 hours. Tell
> me what you found."

**Stand up a new app:**

> "Run the steps in the 'agent-friendly recipe' above to start
> central-logs and mint an insert key for the app in
> `/home/me/my-app`. Configure the app's logger to ship to
> `http://localhost:8080/v1/logs` with that key and `service:
> my-app`. Verify with one `query_logs` call filtered to that
> service."

**Create an alert rule (the right way):**

> "Use the `create_alert_rule` tool to propose a rule named 'checkout
> p95 spike' on metric `p95_latency` with a threshold of 1000 ms over
> 5 minutes, notified via webhook
> `https://hooks.example.com/incidents`. Tell me the rule id and that
> it's `pending_approval`. Do **not** attempt to approve it."

## See also

- [`llms.txt`](llms.txt) — short discovery file for LLM agents.
- [MCP server](mcp.md) — full tool reference.
- [Integration samples](integration-sample/README.md) — typed
  loggers in TypeScript, Python, Rust, PHP, Java, C, and C++.
- [Architecture](ARCHITECTURE.md) — design reference for the
  underlying pipeline.