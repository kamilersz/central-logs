# Install & deploy

central-logs is a single Rust binary; deployment is a single
`cargo build --release` plus a systemd unit (or any other supervisor).
This page covers what to think about before deploying, with
operational details in [Operations](OPERATIONS.md) and the full
config knob table in [Configuration](CONFIGURATION.md).

## Build flavors

```bash
# Default — bundled DuckDB, MCP server, embedded SPA.
cargo build --release

# Add S3/GCS cold-tier archive (object_store crate).
cargo build --release --features object-storage

# Drop the embedded dashboard / MCP server / DuckDB bundle if you
# really need a smaller binary (rare — only if you're sure you
# don't need them).
cargo build --release --no-default-features --features 'mcp dashboard'
```

If anything under `web/` changed, rebuild the SPA **before**
`cargo build` so the change gets embedded into the binary:

```bash
cd web && npm ci && npm run build && cd ..
cargo build --release
```

The build script (`build.rs`) re-runs cargo when `target/web-dist`
changes, but a fresh `npm run build` is still required to *produce*
that directory.

## Storage layout

After the first run, your data directory contains:

```
data/
├── central.duckdb        # hot tier
├── central.duckdb.wal    # DuckDB write-ahead log
├── meta.redb             # WAL offsets + ingest checkpoints
├── wal/                  # CRC-framed WAL segments
│   └── seg-000001.log
├── parquet/              # cold tier, hive-partitioned
│   └── date=YYYY-MM-DD/hour=HH/service=<name>/part-N.parquet
└── backups/              # local backup copies (when [backup] runs)
```

**Backups are simple `tar` of this directory** when the service is
stopped, or an atomic tar.gz + sha256 manifest when the service is
running (the Storage page and `scripts/` cover both paths).

## Auth before exposing the port

By default, the server starts with **no auth** when no API keys are
configured and the bind is loopback (the documented local-dev mode).
Two things to know before exposing `:8080` to a network:

1. **Set `CENTRAL_LOGS_HTTP_API_KEY`** in `.env` — a stable admin
   credential that survives restarts and data-dir resets. The
   one-time bootstrap token (printed to stderr on first run) only
   fires when no key exists.
2. **Generate a value, don't paste one you read online:**
   `openssl rand -hex 24 | sed 's/^/clk_/'`. Prefix `clk_` is the
   convention; the server accepts any string but the dashboard uses
   the prefix to identify key shape.

Once an admin credential exists, mint `insert` / `read` / `write` keys
for downstream systems and revoke the admin when no operator needs it.

> The server logs a loud warning at startup when the HTTP API is
> bound to a non-loopback interface without auth — pay attention to it.

## Production knobs worth setting

A short list of `[retention]`, `[backup]`, and hot-attribute flags
that should be in your config before any real load lands. Full table
in [Configuration](CONFIGURATION.md).

```toml
# Retain more hot, but cap disk
[retention]
hot_max_age = "2mo"          # default
hot_max_bytes = "50GB"        # compact oldest rows out when SUM(raw_len) exceeds this
wal_max_bytes = "100GB"      # 503 inserts when WAL dir exceeds this

# Daily backup to local + S3
[backup]
enabled = true
schedule = "daily@01:30"
timezone = "Asia/Jakarta"
backend = "s3"
bucket = "cl-backups"
prefix = "central-logs/backups"
keep_local_copy = true
keep_last = 14

# Promote JSON keys you filter on often (cheaper queries)
hot_attributes = [
  { name = "user_id", duckdb_type = "bigint",   json_path = "$.user_id" },
  { name = "route",   duckdb_type = "varchar",  json_path = "$.request.route" },
  { name = "status",  duckdb_type = "bigint",   json_path = "$.status" },
]

# Per-service retention: keep audit streams long, debug streams short
[[service_retention]]
pattern = "audit_*"
days = 90

[[service_retention]]
pattern = "debug_*"
days = 3

# Cap per-service query windows so one wide scan can't squeeze the node
[[service_query_limits]]
pattern = "payment_*"
max_window_secs = 86400
```

## Running as a systemd service

The repo ships a reference user-level systemd unit at
`deploy/central-logs.service`. The high-level steps:

```bash
# Build (one-time / on upgrade).
cd web && npm ci && npm run build && cd ..
cargo build --release

# Persist an admin credential.
echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" > .env
chmod 600 .env

# Install the unit.
mkdir -p ~/.config/systemd/user
cp deploy/central-logs.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable --now central-logs
systemctl --user status central-logs
```

Edit the unit to set your own `WorkingDirectory`, `EnvironmentFile`,
and `--hot-attribute` flags. Full detail in [Operations](OPERATIONS.md).

## Multi-instance deployments (per-client silos)

The same binary can run multiple instances on different ports with
different data dirs — common when central-logs fronts many small
clients. The bundled `scripts/deploy.sh` is built for this pattern
(it restarts four instances on `:8084`, `:8085`, `:8086`, `:8087`).
Each instance has its own `.env` and `data*` directory:

```
.env        .env-8085   .env-8086   .env-8087
data/       data-8085/  data-8086/  data-8087/
```

Don't `rm` `central.duckdb.wal` while the process is running. The
service now checkpoints on shutdown, so you should never need to.

## Docker / containers

The binary is self-contained — the only filesystem requirement is
that `./data` (or whatever `--data-dir` points at) is persistent.
A minimal Dockerfile:

```dockerfile
FROM rust:1.85-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cd web && npm ci && npm run build && cd .. && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/central-logs /usr/local/bin/
EXPOSE 8080 5140/udp 5140
ENTRYPOINT ["/usr/local/bin/central-logs"]
```

Mount `/data` as a volume.

## Upgrade / deploy

```bash
git pull --ff-only
./scripts/deploy.sh             # for multi-instance
# or, manual:
cargo build --release --locked
systemctl --user restart central-logs
```

The `central-logs.service` unit handles SIGTERM correctly (so
`systemctl restart` is clean — DuckDB checkpoints, WAL prunes
ingested segments). A SIGINT (terminal Ctrl+C) does the same.

## Next

- [Operations →](OPERATIONS.md) — day-to-day, backups, restore, shipper
- [Configuration →](CONFIGURATION.md) — every knob in one place
- [Security →](SECURITY.md) — auth, scopes, OWASP defaults