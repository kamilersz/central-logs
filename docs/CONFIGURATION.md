# Configuration

All knobs are CLI flags, env vars (`CENTRAL_LOGS_*`), or a TOML file passed via
`--config`. Defaults are sensible for a single-node deployment.

| Flag | Default | Purpose |
|---|---|---|
| `--data-dir` | `./data` | WAL segments + DuckDB file + redb metadata |
| `--http-port` | `8080` | HTTP API + dashboard + SPA |
| `--no-syslog-udp` / `--no-syslog-tcp` | off | Disable a syslog listener |
| `--mcp-mode` | `off` | `off` / `stdio`. `sse` parses but is rejected at startup — the streamable-HTTP transport isn't implemented (see [MCP tools](API.md#mcp-tools)) |
| `--enable-alert-mcp-tool` | off | Allow the `create_alert_rule` MCP tool |
| `--hot-attribute` | none | Promote a JSON key to a typed column (repeatable) |
| `--drop-attribute` | none | Strip a JSON key at parse time before persistence (repeatable) |
| `--no-unwrap-message-json` | off | Disable JSON-object lifting from inside `msg`/`message` |
| `--service-retention` | none | Per-service retention override, `GLOB=DAYS` (repeatable, first match wins) |
| `--service-query-limit` | none | Per-service query window cap, `GLOB=DURATION` e.g. `payment_*=24h` (repeatable) |
| `--http-api-key` | none | Bearer token required on every HTTP API/insert route (OWASP A01/A07). Empty = auth disabled. |
| `--no-sentry-ingest` | off | Disable Sentry-SDK ingest + error-group tracking |
| `--print-config` | — | Print effective config as JSON and exit |

`--hot-attribute` takes a shorthand `name:type[:json_path]`:

```
--hot-attribute 'user_id:bigint'
--hot-attribute 'route:varchar:$.request.route'
--hot-attribute 'is_canary:boolean'
```

Supported types: `bigint`, `double`, `varchar`, `boolean`. Names must match
`[a-z_][a-z0-9_]*` and cannot shadow built-in columns (`service`, `level`,
`trace_id`, etc. — the validator rejects these at startup).

## Retention & backups

```toml
[retention]
hot_max_age = "2mo"      # default; 45s / 12h / 30d / 2mo / 1y all parse
hot_max_bytes = "10GB"   # compact oldest rows out when SUM(raw_len) exceeds this
wal_max_bytes = "20GB"   # refuse inserts (503) when the WAL dir exceeds this

[backup]
enabled = false
schedule = "daily@01:30" # or "size:512MiB" (snapshot per 512 MiB of new cold data)
timezone = "Asia/Jakarta" # IANA tz for the daily schedule
backend = "s3"            # "" = local copies only; s3 | gcs
bucket = "cl-backups"
prefix = "central-logs/backups"
endpoint = "http://minio:9000"  # optional (MinIO)
region = "us-east-1"
keep_local_copy = true
# local_backup_dir = "/var/lib/cl-backups"  # default: <data_dir>/backups
keep_last = 14              # prune local copies beyond the newest N
# instance_id = "prod-1"    # default: "<hostname>-<port>"
```

Notes: `hot_max_age` overrides `hot_tier_hours`. S3/GCS backups require the
`object-storage` build feature (`cargo build --release --features
object-storage`). Snapshots are atomic tar.gz + sibling `manifest.json`
(sha256); restore verifies the checksum and refuses the live data dir —
see [OPERATIONS.md](OPERATIONS.md).

## Error tracking

```toml
[error_tracking]
enabled = true                  # mounts /api/{project}/envelope/ + /store/
projects = { "7" = "checkout" } # DSN project id -> service name (ids are numeric)
stack_max_frames = 100
stack_max_bytes = 8192
samples_per_group = 3
reconcile_interval_secs = 3600

[error_tracking.notify]
on_new_group = true             # webhook on first event of a new group
on_regression = true            # webhook when a resolved group re-opens
min_level = "error"             # "fatal" restricts event-time notifications
webhooks = ["https://hooks.example.com/..."]
```

See [ERROR_TRACKING.md](ERROR_TRACKING.md) for the full design.

## TOML example

```toml
data_dir = "/var/lib/central-logs"
http_port = 8080
hot_attributes = [
  { name = "user_id", duckdb_type = "bigint", json_path = "$.user_id" },
  { name = "env",     duckdb_type = "varchar", json_path = "$.env" },
]
drop_attributes = ["debug_payload"]   # strip before persistence
unwrap_message_json = true            # lift JSON embedded in msg/message
parquet_compression = "zstd"          # zstd | snappy | gzip | lz4 | uncompressed

# Per-service retention: first matching glob wins; retention_days is fallback.
[[service_retention]]
pattern = "audit_*"
days = 90

[[service_retention]]
pattern = "debug_*"
days = 3

# Per-service query window caps (applies when the filter pins service:<name>).
[[service_query_limits]]
pattern = "payment_*"
max_window_secs = 86400

[llm]
provider = "anthropic"
api_key = "sk-ant-..."
model = "claude-3-5-sonnet-20241022"

# SMTP relay for alert email channels (omit entirely to disable email
# delivery — email channels will record a delivery error instead).
# [smtp]
# host = "smtp.example.com"
# port = 587
# username = "alerts@example.com"
# password = "..."
# from = "central-logs@example.com"
# starttls = true          # false = implicit TLS (port 465)

# Optional object-storage archive (requires --features object-storage).
# [cold_storage]
# enabled = true
# backend = "s3"                 # s3 (incl. MinIO via endpoint) | gcs
# bucket = "my-log-archive"
# prefix = "central-logs/parquet"
# endpoint = "http://minio:9000" # optional
# region = "us-east-1"           # optional
# keep_local = true              # local parquet stays the queryable cache
```

For LLM-free operation (default), set `[llm]` to nothing or omit it — the
`/api/ai/query` endpoint falls back to a local keyword-extract heuristic.

## Supported LLM providers

| Provider | `[llm].provider` | Notes |
|---|---|---|
| OpenAI | `openai` | `gpt-4o-mini` is a good default. `base_url` overrides for OpenAI-compatible relays. |
| Anthropic | `anthropic` | `claude-3-5-sonnet-20241022`. `base_url` overrides for Anthropic-protocol gateways — e.g. MiniMax coding plan (`https://api.minimax.io/anthropic`, model `MiniMax-M3`). Sends both `x-api-key` and `Authorization: Bearer`. |
| 9inference.cloud | `9inference` | OpenAI-compatible. `nemotron-3-ultra`. Behind Cloudflare — sends a browser User-Agent. Always uses `stream: true` and parses SSE chunks. |
| Off (default) | `off` | Local keyword-extract fallback |

### .env setup (no TOML needed)

When `[llm]` is unset, four flat env vars configure the provider — this is
what the systemd instances use:

```bash
CENTRAL_LOGS_LLM_PROVIDER=anthropic              # anthropic (default) | openai | 9inference
CENTRAL_LOGS_LLM_API_KEY=sk-...                  # required to enable LLM features
CENTRAL_LOGS_LLM_MODEL=MiniMax-M3                # provider's model id
CENTRAL_LOGS_LLM_BASE_URL=https://api.minimax.io/anthropic   # optional gateway override
```

TOML `[llm]` takes precedence when present. **Never hardcode keys in
source** — keys live in `.env` files (gitignored) or the TOML config.

## SMTP (alert email channels)

| Key | Env | Default | Purpose |
|---|---|---|---|
| `[smtp].host` | `CENTRAL_LOGS_SMTP__HOST` | *(empty)* | Relay hostname. Empty = email delivery disabled |
| `[smtp].port` | `CENTRAL_LOGS_SMTP__PORT` | `587` | Relay port |
| `[smtp].username` | `CENTRAL_LOGS_SMTP__USERNAME` | *(empty)* | AUTH username (omit for no auth) |
| `[smtp].password` | `CENTRAL_LOGS_SMTP__PASSWORD` | *(empty)* | AUTH password |
| `[smtp].from` | `CENTRAL_LOGS_SMTP__FROM` | *(empty)* | From address (required when host is set) |
| `[smtp].starttls` | `CENTRAL_LOGS_SMTP__STARTTLS` | `true` | `true` = STARTTLS (587); `false` = implicit TLS (465) |

Channels of type `email` are managed in the dashboard (Alerts → Notification
channels); the relay is global so credentials live in one place.
