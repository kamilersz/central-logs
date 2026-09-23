# Config reference

Single TOML document, optional. Defaults are sensible for a single-node
deployment. Full key table; for the CLI equivalents see
# [CLI flags](cli.md).

`config.toml`:

```toml
# ── Paths ────────────────────────────────────────────────────────────────────
data_dir = "/var/lib/central-logs"
# duckdb_path = "/var/lib/central-logs/central.duckdb"   # default inside data_dir
# parquet_dir = "/var/lib/central-logs/parquet"           # default inside data_dir

# ── HTTP / network ───────────────────────────────────────────────────────────
http_bind = "0.0.0.0"
http_port = 8080
http_api_key = ""               # empty = auth disabled (loopback dev)
http_max_body_bytes = 16777216  # 16 MiB

# ── Syslog listeners ─────────────────────────────────────────────────────────
syslog_udp_enabled = true
syslog_udp_bind   = "0.0.0.0:5140"
syslog_tcp_enabled = true
syslog_tcp_bind   = "0.0.0.0:5140"

# ── Container/orchestrator push protocols (all disabled by default) ─────────
# Full guide: docs/ingestion/containers.md
[ingest.gelf]
enabled = false               # Docker --log-driver=gelf + GELF shippers
udp_bind = "0.0.0.0:12201"    # gzip/zlib + GELF chunked reassembly
tcp_bind = "0.0.0.0:12201"    # JSON + NUL framing
chunk_timeout_secs = 5

[ingest.fluentd]
enabled = false               # Docker --log-driver=fluentd (msgpack forward)
tcp_bind = "0.0.0.0:24224"
ack = true

[ingest.splunk_hec]
enabled = false               # Docker --log-driver=splunk
token = ""                    # optional static token; API keys also work

[ingest.k8s_audit]
enabled = false               # kube-apiserver audit webhook
omit_stages = []              # e.g. ["RequestReceived"]

# ── Built-in pull collectors (all disabled by default) ──────────────────────
[collector.docker]
enabled = false               # follow containers via the Engine API
socket = "unix:///var/run/docker.sock"
api_version = "v1.41"
include = []                  # container-name globs; [] = all
exclude = []
refresh_secs = 30
tail_lines = 0

[collector.kubernetes]
enabled = false               # follow pod logs via the Kubernetes API
api_url = "https://kubernetes.default.svc"
token_file = "/var/run/secrets/kubernetes.io/serviceaccount/token"
ca_file = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt"
token = ""                    # literal token alternative
insecure_tls = false
namespaces = []               # [] = all
label_selector = ""
refresh_secs = 30
tail_lines = 0

# ── MCP ─────────────────────────────────────────────────────────────────────
mcp_mode = "off"               # off | stdio | sse  (sse is rejected at startup)
mcp_http_bind = "0.0.0.0:8081"
mcp_api_key = ""               # bearer for SSE/HTTP mode; empty = no auth

# ── Alert evaluator ──────────────────────────────────────────────────────────
enable_alert_mcp_tool = false
alert_eval_interval_secs = 30

# ── WAL ─────────────────────────────────────────────────────────────────────
wal_segment_max_bytes     = 268435456   # 256 MiB
wal_batch_max_records     = 4096
wal_batch_max_delay_ms    = 10
insert_channel_depth      = 65536
insert_backpressure_timeout_ms = 250

# ── Ingest workers ───────────────────────────────────────────────────────────
ingest_workers       = 2
ingest_batch_size    = 2048
ingest_flush_interval_ms = 1000

# ── Rollup / compaction ──────────────────────────────────────────────────────
rollup_interval_secs    = 60
compaction_interval_secs = 3600
hot_tier_hours         = 48           # overridden by [retention].hot_max_age
retention_days         = 30
parquet_compression    = "zstd"      # zstd | snappy | gzip | lz4 | uncompressed

# ── DuckDB engine tuning ─────────────────────────────────────────────────────
duckdb_memory_limit = ""            # empty = DuckDB default; e.g. "2GB"
duckdb_threads = 0                  # 0 = DuckDB default; cap when sharing the box

# ── Attribute promotion / scrubbing ──────────────────────────────────────────
drop_attributes = ["debug_payload"]
unwrap_message_json = true          # lift JSON objects embedded in msg/message

hot_attributes = [
  { name = "user_id", duckdb_type = "bigint" },
  { name = "route",   duckdb_type = "varchar", json_path = "$.request.route" },
]

# ── Per-service retention / query caps ───────────────────────────────────────
[[service_retention]]
pattern = "audit_*"
days = 90

[[service_query_limits]]
pattern = "payment_*"
max_window_secs = 86400

# ── Forecast cadence ────────────────────────────────────────────────────────
forecast_every_n_rollups = 5

# ── Object-storage cold archive (needs --features object-storage) ────────────
# [cold_storage]
# enabled = true
# backend = "s3"                   # s3 | gcs
# bucket  = "my-log-archive"
# prefix  = "central-logs/parquet"
# endpoint = "http://minio:9000"  # optional (MinIO)
# region   = "us-east-1"           # optional
# keep_local = true                # local parquet stays as the queryable cache

# ── Retention bounds ────────────────────────────────────────────────────────
[retention]
hot_max_age   = "2mo"             # overrides hot_tier_hours; 45s/12h/30d/2mo/1y
hot_max_bytes = "10GB"            # compact oldest rows out when SUM(raw_len) exceeds
wal_max_bytes = "20GB"            # 503 inserts when the WAL dir exceeds

# ── Backups ──────────────────────────────────────────────────────────────────
[backup]
enabled = true
schedule = "daily@01:30"          # daily@HH:MM or size:<N>
timezone = "Asia/Jakarta"
backend = "s3"                    # "" for local-only
bucket  = "cl-backups"
prefix  = "central-logs/backups"
endpoint = ""                     # MinIO / S3-compatible
region = ""
keep_local_copy = true
keep_last = 14
# local_backup_dir = "/var/lib/cl-backups"  # default: <data_dir>/backups
# instance_id = "prod-1"                     # default: "<hostname>-<port>"

# ── LLM provider (off = local keyword-extract fallback) ─────────────────────
[llm]
provider = "anthropic"            # anthropic | openai | 9inference | off
api_key  = "sk-ant-..."
model    = "claude-3-5-sonnet-20241022"
# base_url = ""                    # gateway override (Anthropic / OpenAI compatible)

# ── SMTP relay (alert email channels) ───────────────────────────────────────
# [smtp]
# host = "smtp.example.com"
# port = 587
# username = "alerts@example.com"
# password = "..."
# from = "central-logs@example.com"
# starttls = true                  # false = implicit TLS (port 465)

# ── GeoIP enrichment (needs --features geoip) ────────────────────────────────
# geoip_db_path = "/var/lib/GeoLite2-Country.mmdb"

# ── Error tracking (Sentry-SDK-compatible) ──────────────────────────────────
[error_tracking]
enabled = true                    # mounts the Sentry-protocol routes
projects = { "7" = "checkout" }   # DSN project id → service name
stack_max_frames = 100
stack_max_bytes  = 8192
samples_per_group = 3
reconcile_interval_secs = 3600

[error_tracking.notify]
on_new_group = true
on_regression = true
min_level = "error"               # "fatal" restricts event-time notifications
webhooks = ["https://hooks.example.com/..."]

# ── Tracing ─────────────────────────────────────────────────────────────────
log_filter = "info,central_logs=debug"
```

## Validation

`Config::validate()` runs at startup and rejects:

- `mcp_mode = "sse"` (transport not implemented; see [MCP server](../mcp.md))
- `wal_batch_max_records == 0`, `ingest_workers == 0`, `hot_tier_hours < 1`
- `parquet_compression` outside `zstd | snappy | gzip | lz4 | uncompressed`
- `duckdb_memory_limit` in an unknown unit or non-positive
- `duckdb_threads > 4096`
- Hot attributes that shadow built-in column names
- Duplicate hot attributes
- Overlap between hot attributes and drop attributes
- Bad `service_retention` / `service_query_limits` patterns (empty,
  contain `/`, or non-positive days / window)
- Bad durations / sizes in `[retention]`
- Bad backup `schedule` (`daily@HH:MM` or `size:<N>`) or unknown
  IANA `timezone`
- `[cold_storage].enabled = true` without `--features object-storage`
- `[backup].backend` set without `--features object-storage`

It does **not** reject:

- `http_api_key = ""` on a non-loopback bind — but the server prints
  a loud startup warning in that case.

## Default values (full)

```text
data_dir                  = "./data"
http_bind                 = "0.0.0.0"
http_port                 = 8080
syslog_udp_enabled        = true
syslog_udp_bind           = "0.0.0.0:5140"
syslog_tcp_enabled        = true
syslog_tcp_bind           = "0.0.0.0:5140"
mcp_mode                  = "off"
mcp_http_bind             = "0.0.0.0:8081"
mcp_api_key               = ""
http_api_key              = ""
http_max_body_bytes       = 16777216         # 16 MiB
enable_alert_mcp_tool     = false
alert_eval_interval_secs  = 30
wal_segment_max_bytes     = 268435456       # 256 MiB
wal_batch_max_records     = 4096
wal_batch_max_delay_ms    = 10
insert_channel_depth      = 65536
insert_backpressure_timeout_ms = 250
ingest_workers            = 2
ingest_batch_size         = 2048
ingest_flush_interval_ms  = 1000
rollup_interval_secs      = 60
compaction_interval_secs  = 3600
hot_tier_hours            = 48
retention_days            = 30
parquet_compression       = "zstd"
duckdb_memory_limit       = ""              # auto, ~80% RAM
duckdb_threads            = 0               # auto, one per core
unwrap_message_json       = true
forecast_every_n_rollups  = 5
log_filter                = "info,central_logs=debug"
```

## Next

- [CLI flags](cli.md) — overrides and shorthand
- [Configuration](../CONFIGURATION.md) — narrative + examples
- [LLM providers](llm-providers.md) — provider-specific details