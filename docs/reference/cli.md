# CLI flags

`central-logs` is configured via CLI flags + `CENTRAL_LOGS_*` env vars +
an optional TOML file passed with `--config`. The full precedence:

```
defaults ← TOML file (if --config) ← env (CENTRAL_LOGS_*) ← CLI overrides
```

Most knobs come from config; CLI flags exist for the things you
typically want to override at launch. The non-obvious ones (e.g.
`--hot-attribute`, `--service-retention`) accept shorthand for
operators who prefer the command line.

Run `central-logs --help` for the current list. The authoritative
list lives in [`src/config.rs`](https://github.com/kamilersz/central-logs/blob/main/src/config.rs)
(`Cli` struct).

## Common flags

| Flag | Default | Notes |
|---|---|---|
| `--config <path>` | none | TOML config file. Resolved relative to CWD. |
| `--data-dir <path>` | `./data` | WAL segments + DuckDB + redb. |
| `--http-port <port>` | `8080` | HTTP API + dashboard. |
| `--http-api-key <key>` | env `CENTRAL_LOGS_HTTP_API_KEY` | Static admin key (OWASP A01/A07). Empty = auth disabled. |
| `--no-syslog-udp` / `--no-syslog-tcp` | enabled | Disable a syslog listener. |
| `--mcp-mode <off\|stdio\|sse>` | `off` | `sse` is rejected at startup (not implemented). |
| `--enable-alert-mcp-tool` | off | Expose the `create_alert_rule` MCP tool. |
| `--hot-attribute <spec>` | none | Promote a JSON key to a typed column (repeatable). Shorthand: `name:type[:json_path]`. |
| `--drop-attribute <key>` | none | Strip a JSON key at parse time (repeatable). |
| `--no-unwrap-message-json` | off | Disable JSON-object lifting from `msg`/`message`. |
| `--service-retention <GLOB=DAYS>` | none | Per-service retention override (repeatable, first match wins). |
| `--service-query-limit <GLOB=DURATION>` | none | Per-service query window cap (repeatable). |
| `--no-sentry-ingest` | off | Disable Sentry-SDK ingest + error-group tracking. |
| `--backup-now` | off | One manual snapshot at startup (local copy always; remote if `[backup]` is configured). |
| `--restore-from <path>` | none | Restore a snapshot into `--data-dir` BEFORE opening the DB. Server must be stopped; data dir must be empty (or `--restore-force-wipe`). |
| `--print-config` | — | Print effective config as JSON and exit. |
| `--duckdb-memory-limit <limit>` | empty | `512MB` / `2GB` (empty = auto, ~80% RAM). |
| `--duckdb-threads <n>` | 0 | 0 = auto (one per core). |

## Hot-attribute shorthand

```bash
--hot-attribute 'user_id:bigint'                              # typed column, JSON path defaults to $.user_id
--hot-attribute 'route:varchar:$.request.route'               # explicit JSON path
--hot-attribute 'is_canary:boolean'                           # default json_path = $.is_canary
```

Supported types: `bigint`, `double`, `varchar`, `boolean`. Names must
match `[a-z_][a-z0-9_]*` and cannot shadow built-in columns
(`service`, `level`, `trace_id`, etc. — the validator rejects these at
startup).

TOML equivalent:

```toml
hot_attributes = [
  { name = "user_id",    duckdb_type = "bigint" },
  { name = "route",      duckdb_type = "varchar", json_path = "$.request.route" },
  { name = "is_canary",  duckdb_type = "boolean" },
]
```

## Per-service retention / query caps

```bash
--service-retention 'audit_*=90' --service-retention 'debug_*=3'
--service-query-limit 'payment_*=24h'
```

`pattern` is a glob (`*`, `?`) matched against the sanitized service
partition dir name on the cold tier (`audit_*` → 90 days; anything
else falls back to `retention_days`). Patterns can't contain `/`.

## Subcommands

There are no subcommands — the binary is the service. Pre-startup
actions are CLI flags:

| Flag | Purpose |
|---|---|
| `--print-config` | Print effective config and exit. Useful for CI: `central-logs --print-config --config prod.toml`. |
| `--backup-now` | Take one snapshot at startup, then keep serving. |
| `--restore-from <local-or-s3>` | Restore a snapshot before opening it. |
| `--help` / `--version` | Standard. |

## Environment variables

| Variable | Maps to |
|---|---|
| `CENTRAL_LOGS_CONFIG` | `--config` |
| `CENTRAL_LOGS_DATA_DIR` | `--data-dir` |
| `CENTRAL_LOGS_HTTP_PORT` | `--http-port` |
| `CENTRAL_LOGS_HTTP_API_KEY` | `--http-api-key` |
| `CENTRAL_LOGS_MCP_MODE` | `--mcp-mode` |
| `CENTRAL_LOGS_ENABLE_ALERT_MCP_TOOL` | `--enable-alert-mcp-tool` |
| `CENTRAL_LOGS_DUCKDB_MEMORY_LIMIT` | `--duckdb-memory-limit` |
| `CENTRAL_LOGS_DUCKDB_THREADS` | `--duckdb-threads` |
| `CENTRAL_LOGS_LLM_PROVIDER` / `_API_KEY` / `_MODEL` / `_BASE_URL` | Flat LLM config (when `[llm]` is unset) |
| `CENTRAL_LOGS_SMTP__HOST` / `_PORT` / `_USERNAME` / `_PASSWORD` / `_FROM` / `_STARTTLS` | SMTP relay (double underscore = nested key) |

The `.env` file at startup is auto-loaded (real environment wins on
conflict). `.env` is gitignored — never commit secrets.

## Next

- [Configuration](../CONFIGURATION.md) — every TOML key with defaults
- [Operations](../OPERATIONS.md) — day-to-day, including service restarts