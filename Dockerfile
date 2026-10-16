# syntax=docker/dockerfile:1
#
# Multi-stage build for central-logs:
#   1. web   — build the SPA (Vite) → target/web-dist
#   2. build — compile the Rust binary with the SPA embedded (rust-embed)
#   3. run   — slim Debian runtime with just the binary
#
# Build:  docker build -t central-logs .
# Run:    docker run -p 8080:8080 -v central-logs-data:/data central-logs

# ── 1. SPA ──────────────────────────────────────────────────────────────
FROM node:22-alpine AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
# vite.config.ts writes to ../target/web-dist (i.e. /src/target/web-dist),
# which the Rust build stage embeds via rust-embed.
RUN npm run build

# ── 2. Rust build ───────────────────────────────────────────────────────
FROM rust:1-bookworm AS build
# cmake + g++: DuckDB is compiled from source (bundled feature).
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake g++ \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# SPA goes where rust-embed expects it (folder = "target/web-dist",
# resolved against the crate root). CARGO_TARGET_DIR keeps the build cache
# in a separate mount so it can't shadow this directory.
COPY --from=web /src/target/web-dist target/web-dist
ENV CARGO_TARGET_DIR=/cargo-target
# geoip: enables MaxMind attribution (optional at runtime — the server runs
# fine without a GeoLite2 file and still resolves country via CF-IPCountry).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cargo-target \
    cargo build --release --locked --features geoip \
 && cp /cargo-target/release/central-logs /central-logs

# ── 3. Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -r central \
 && useradd -r -g central -d /data -s /usr/sbin/nologin central \
 && mkdir -p /data /etc/central-logs \
 && chown -R central:central /data /etc/central-logs

COPY --from=build /central-logs /usr/local/bin/central-logs
COPY central-logs.toml.example /etc/central-logs/central-logs.toml.example
COPY scripts/fetch-geolite.sh /usr/local/bin/fetch-geolite.sh

USER central
WORKDIR /data
ENV CENTRAL_LOGS_DATA_DIR=/data

# 8080 HTTP API + dashboard; 5140 syslog UDP+TCP; 12201 GELF (Docker gelf
# driver); 24224 fluentd forward. The latter three are disabled by default —
# enable them in config and publish the ports as needed.
EXPOSE 8080 5140/tcp 5140/udp 12201/tcp 12201/udp 24224/tcp

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=5 \
  CMD curl -fsS "http://127.0.0.1:${CENTRAL_LOGS_HTTP_PORT:-8080}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/central-logs"]
