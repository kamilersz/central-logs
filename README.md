# central-logs

A self-hosted, single-node centralized logging platform: a Graylog-style
alternative that ingests logs from many sources, writes them durably at high
speed, computes runtime dashboards and metrics, forecasts trends, detects
anomalies, groups errors Sentry-SDK-style, and exposes everything to AI
agents over MCP — with a built-in SPA dashboard.

One Rust binary. One DuckDB file. One WAL directory. No cluster, no external
database, no container required.

> **Documentation site**: <https://central-logs.readthedocs.io/>
> (built from `docs/`; this README and `INTEGRATION.md` are kept at the
> repo root for convenience and stay close to the code). For LLM/agent
> discovery, the docs site serves a short `llms.txt` at
> `/llms.txt`.

## Install

No manual setup needed — let your coding agent do it.

1. Open your favourite coding agent: Claude Code, Codex, or OpenCode.
2. Tell it: **"please setup https://github.com/kamilersz/central-logs"**
3. Then tell it: **"integrate my apps at `<YOUR PATH>` into central-logs"**

That's it. The agent builds and starts the server, then wires your apps to
ship logs using the ready-made clients in
[`integration-sample/`](integration-sample/README.md) (TypeScript, Python,
Rust, PHP, Java, C, C++).

Prefer to do it by hand?

```bash
cargo build --release
./target/release/central-logs
```

The server listens on `:8080` (HTTP API + dashboard) and `:5140` (syslog
UDP+TCP), writing data under `./data/`. On first run it prints a one-time
admin token (or set `CENTRAL_LOGS_HTTP_API_KEY` yourself) — see
[docs/SECURITY.md](docs/SECURITY.md).

## How it works

```
source → insert layer → WAL (CRC-framed, fsync'd) → ingest workers → DuckDB hot
                                                                    ↓
                                                              compaction job
                                                                    ↓
                                                              Parquet cold tier
                                                                    ↓
                                       query layer ← dashboards + MCP + AI
```

- **Ingest** — `POST /v1/logs` (NDJSON/JSON), **OpenTelemetry OTLP/HTTP**
  (`/v1/logs`, `/v1/traces`, `/v1/metrics` — protobuf or JSON, any OTel SDK),
  syslog, or a Sentry-SDK DSN (`http://clk_KEY@host:8080/7`). The write is
  fsync'd before the ack; parsing/enrichment happens asynchronously.
- **Store** — DuckDB hot store for recent logs; background compaction exports
  aged rows to hive-partitioned Parquet with per-service retention.
- **Query** — a safe filter DSL (`service:api level:error user_id:42`),
  dashboards (volume / error-rate / latency / anomalies / forecast), and five
  MCP tools for AI agents.
- **Errors** — point any existing Sentry SDK at central-logs: exceptions are
  grouped by fingerprint, stacktraces sampled per group, browsable in the
  **Errors** page (search, filters, trend sparkline, resolve/ignore) with an
  **Explain with AI** button, and notifications fire on first-seen,
  regression, or threshold.
- **Alerts** — create rules in the dashboard: a filter-DSL expression with an
  n-per-period threshold (or metric/anomaly conditions), delivered to email
  lists, Telegram bots, or webhooks — each rule picks its channels, with a
  test-send button per channel. AI-created rules (MCP) always land in
  `pending_approval`; a human approves them before they can fire.

## Documentation

The full documentation is published at
<https://central-logs.readthedocs.io/>. Highlights:

| Page | Contents |
|---|---|
| [docs/index.md](docs/index.md) | Site landing — high-level overview and section map |
| [docs/getting-started.md](docs/getting-started.md) | Ten-minute quickstart (build, ship, query) |
| [docs/llms.md](docs/llms.md) / [docs/llms.txt](docs/llms.txt) | For AI agents — recipe + discovery file |
| [docs/API.md](docs/API.md) | HTTP API reference, filter DSL, MCP tools |
| [docs/mcp.md](docs/mcp.md) | MCP server: tools, transport, agent recipes |
| [docs/query.md](docs/query.md) | Filter DSL reference (operators, examples, safety) |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | CLI flags, env vars, TOML, LLM providers |
| [docs/SECURITY.md](docs/SECURITY.md) | Auth, API keys, scopes, OWASP-aligned defaults |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | systemd service, day-to-day ops, shipper sidecar, backup |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | Dev workflow, builds, tests, project layout |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Full design: schema, pipeline, rationale, roadmap |
| [docs/STATUS.md](docs/STATUS.md) | What's done and tested, what's deferred |
| [docs/ERROR_TRACKING.md](docs/ERROR_TRACKING.md) | Sentry-SDK-compatible error tracking |
| [integration-sample/](integration-sample/README.md) | Ready-made insert clients per language |

The Read the Docs build is configured by
[`.readthedocs.yaml`](.readthedocs.yaml) +
[`mkdocs.yml`](mkdocs.yml). To preview locally: `pip install -r
requirements.txt && mkdocs serve`.

## License

MIT.
