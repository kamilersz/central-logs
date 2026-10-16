# Install & deploy

central-logs ships three ways — grab a [prebuilt binary](https://github.com/kamilersz/central-logs/releases),
pull the [Docker image](#docker--containers--kubernetes), or `cargo build --release`
from source plus a systemd unit (or any other supervisor).

This page covers what to think about before deploying, with
operational details in [Operations](OPERATIONS.md) and the full
config knob table in [Configuration](CONFIGURATION.md).

## Fastest paths

```bash
# Prebuilt binary (dashboard embedded; Linux x86_64/arm64, macOS arm64)
VERSION=v0.1.0
curl -fsSL "https://github.com/kamilersz/central-logs/releases/download/${VERSION}/central-logs-${VERSION}-x86_64-linux.tar.gz" | tar -xz
./central-logs-*/central-logs

# Or a container
docker run -d -p 8080:8080 -v central-logs-data:/data \
  ghcr.io/kamilersz/central-logs:latest
```

Both land the dashboard on `http://localhost:8080`, write data under
`./data/` (or the `/data` volume), and print a one-time admin token on
first run unless you set `CENTRAL_LOGS_HTTP_API_KEY`. The rest of this page
covers building from source, storage layout, auth, and production knobs —
all of which apply equally to the binary and the container.

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

## Docker / containers / Kubernetes

The repo ships a production `Dockerfile` and a `docker-compose.yml`, and
every release publishes a **multi-arch image** (`linux/amd64` + `linux/arm64`)
to GitHub Container Registry — no build server or Rust toolchain needed on
the target host.

The container is the same single binary with the SPA embedded, running as a
non-root user. **The only thing you must persist is `/data`** (DuckDB, WAL,
Parquet, backups all live there), and the image declares a `HEALTHCHECK`
against `/health`.

### Quick start — Docker Compose

```bash
git clone https://github.com/kamilersz/central-logs && cd central-logs
echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)" >> .env
docker compose up -d        # dashboard on http://localhost:8080
```

### Quick start — plain docker run

```bash
docker run -d --name central-logs \
  -p 8080:8080 \
  -e CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24) \
  -v central-logs-data:/data \
  ghcr.io/kamilersz/central-logs:latest
```

Ports: `8080` (HTTP API + dashboard + ingest), `5140/tcp` + `5140/udp`
(syslog), and — only after enabling the matching `[ingest.*]` config —
`12201` (Docker gelf driver) and `24224` (fluentd forward).

### Pulling a specific version

Images are tagged `latest`, the release semver (`0.1.0`, `0.1`), `main`
(every push to main), and `sha-<short-sha>`. Pin a semver tag (or even a
digest) in production so a surprise `latest` push can't change what you run:

```bash
docker pull ghcr.io/kamilersz/central-logs:0.1.0
```

### Kubernetes

`deploy/k8s.yaml` is a ready Deployment + Service + PVC + Secret. The
essentials it encodes, if you write your own:

- image `ghcr.io/kamilersz/central-logs:<version>` (GHCR is public; on a
  private cluster add an `imagePullSecrets` entry for `ghcr.io`)
- a PVC (or `emptyDir` for throwaway) mounted at `/data`
- `CENTRAL_LOGS_HTTP_API_KEY` from a Secret
- ports 8080 + 5140 (tcp **and** udp for syslog)
- readiness/liveness probes on `GET /health`

```bash
kubectl apply -f deploy/k8s.yaml
kubectl -n central-logs port-forward svc/central-logs 8080:80
```

### Building the image yourself

No release? The multi-stage `Dockerfile` builds from the repo — Node builds
the SPA, Cargo compiles the binary with it embedded, and only the binary
lands in the slim runtime layer:

```bash
docker build -t central-logs .
```

### Where images come from

`.github/workflows/docker.yml` builds each architecture on a native
runner, pushes it to GHCR by digest, then merges the digests into one
multi-arch manifest. `.github/workflows/release.yml` does the same idea for
plain binaries and attaches `.tar.gz` + `.sha256` files to the GitHub
[Release](https://github.com/kamilersz/central-logs/releases) — so the
binary and the container always come from the same tag.

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