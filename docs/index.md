# central-logs

> **A self-hosted, single-node centralized logging platform.** Ingests logs
> from many sources, writes them durably at high speed, computes runtime
> dashboards and metrics, forecasts trends, detects anomalies, groups
> errors Sentry-SDK-style, and exposes everything to AI agents over MCP —
> with a built-in SPA dashboard.

**One Rust binary. One DuckDB file. One WAL directory.** No cluster, no
external database, no container required.

---

## Where to start

<div class="grid cards" markdown>

- :material-rocket-launch:{ .lg .middle } **Getting Started**

    ---

    Have the server running and your first app shipping logs in under ten
    minutes.

    [:octicons-arrow-right-24: Start here](getting-started.md)

- :material-robot:{ .lg .middle } **For AI agents**

    ---

    How to let Claude Code, Codex, OpenCode, or your own LLM use
    central-logs as a tool — including the recommended `llms.txt` contract.

    [:octicons-arrow-right-24: See agent guide](llms.md)

- :material-api:{ .lg .middle } **HTTP API reference**

    ---

    Every route, every scope, every request/response shape. Filter DSL,
    alert rules, error groups, MCP-style endpoints.

    [:octicons-arrow-right-24: Browse the API](API.md)

- :material-cog:{ .lg .middle } **Configuration**

    ---

    CLI flags, env vars, TOML keys, retention/backup knobs, LLM providers.

    [:octicons-arrow-right-24: Read the config guide](CONFIGURATION.md)

</div>

## What it does

```
source → insert layer → WAL (CRC-framed, fsync'd) → ingest workers → DuckDB hot
                                                                     ↓
                                                               compaction job
                                                                     ↓
                                                               Parquet cold tier
                                                                     ↓
                                        query layer ← dashboards + MCP + AI
```

- **Ingest** — `POST /v1/logs` (NDJSON / JSON / **OTLP**), syslog UDP+TCP,
  or a Sentry-SDK DSN (`http://clk_KEY@host:8080/7`). The write is
  **fsync'd before the ack**; parsing/enrichment happens asynchronously.
- **Store** — DuckDB hot store for recent logs; background compaction
  exports aged rows to hive-partitioned Parquet with per-service retention.
- **Analyze** — safe filter DSL, volume / error-rate / latency dashboards,
  three-method anomaly detection (MAD / seasonal / rate-of-change), ETS
  forecasting with confidence intervals, pre-digested rollups.
- **Errors** — point any existing Sentry SDK at central-logs: exceptions
  are grouped by fingerprint, stacktraces sampled per group, browsable in
  the **Errors** page, with first-seen / regression / threshold
  notifications.
- **Alerts** — create rules in the dashboard: filter-DSL expression +
  threshold (or anomaly / error-group threshold), delivered to email
  lists, Telegram bots, or webhooks. AI-created rules (MCP) always land
  in `pending_approval`; a human approves them before they fire.
- **AI** — five MCP tools for agents (`query_logs`,
  `get_dashboard_summary`, `detect_anomalies`, `forecast`,
  `create_alert_rule`) over stdio; an **Ask AI** button in the SPA that
  translates natural-language queries into safe filter DSL via OpenAI /
  Anthropic.

## Quick command

```bash
cargo build --release
./target/release/central-logs
```

The server listens on `:8080` (HTTP API + dashboard) and `:5140` (syslog
UDP+TCP), writing data under `./data/`. On first run it prints a one-time
admin token (or set `CENTRAL_LOGS_HTTP_API_KEY` yourself) — see
[Security](SECURITY.md).

## Documentation map

| Section | Contents |
|---|---|
| [Getting Started](getting-started.md) | Quickstart: build, run, ship your first batch. |
| [For AI agents](llms.md) | MCP server, `llms.txt`, agent-friendly prompts. |
| [HTTP API](API.md) | Routes, scopes, filter DSL, error groups, AI query. |
| [MCP tools](mcp.md) | `query_logs`, `get_dashboard_summary`, `detect_anomalies`, `forecast`, `create_alert_rule`. |
| [Configuration](CONFIGURATION.md) | CLI flags, env vars, TOML, retention, backups, LLM providers. |
| [Security](SECURITY.md) | Auth, scopes, OWASP defaults, audit log. |
| [Operations](OPERATIONS.md) | systemd unit, deploys, backups, restore, shipper sidecar. |
| [Architecture](ARCHITECTURE.md) | Pipeline, schema, WAL, rollups, anomaly, MCP — design reference. |
| [Error tracking](ERROR_TRACKING.md) | Sentry-SDK ingest, fingerprinting, Errors page, notifications. |
| [Status](STATUS.md) | What's done and tested, what's deferred. |
| [Development](DEVELOPMENT.md) | Dev workflow, builds, tests, project layout. |

## Project layout

```
central-logs/
├── Cargo.toml              # single crate, single binary
├── mkdocs.yml              # Read the Docs site config
├── .readthedocs.yaml       # RTD build pipeline
├── docs/                   # Markdown source for the RTD site (you are here)
├── src/
│   ├── wal/                # append-only WAL + group-commit fsync + redb metadata
│   ├── ingest/             # parse → enrich → batch-insert into DuckDB
│   ├── store/              # DuckDB schema, Appender, rollups, compaction
│   ├── insert/             # HTTP / OTLP / syslog hot paths
│   ├── analytics/          # forecasting (augurs) + anomaly (MAD/seasonal/ROC)
│   ├── alerts/             # periodic rule evaluation + webhook notification
│   ├── ai.rs               # LLM client → filter DSL
│   ├── query.rs            # filter-DSL parser → safe parameterized SQL
│   ├── mcp/                # rmcp server: 5 tools, stdio transport
│   └── web/                # axum + askama dashboard + React SPA
├── web/                    # SPA source (React + Vite + Tremor)
├── integration-sample/     # ready-made insert clients (TS, Py, Rust, PHP, Java, C, C++)
└── tests/                  # cargo tests + a real sentry-sdk E2E
```

## License

MIT. See [LICENSE](https://github.com/kamilersz/central-logs/blob/main/LICENSE).