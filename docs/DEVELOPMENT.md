# Development

## Dev workflow

Two terminals:

```bash
# Terminal 1: Rust server on :18080 (uses a temp data dir)
cargo run -- --http-port 18080 --data-dir /tmp/cl-dev --no-syslog-udp --no-syslog-tcp --mcp-mode off

# Terminal 2: Vite dev server on :5173, proxies /api → :18080
cd web && npm run dev
```

Open `http://localhost:5173`. Vite hot-reloads on TS/CSS changes; the Rust
server picks up code changes on `cargo run` restart.

## Production build

```bash
cd web && npm install && npm run build && cd ..  # writes to target/web-dist
cargo build --release                             # embeds the SPA via rust-embed
```

The release binary at `target/release/central-logs` is fully self-contained —
no separate frontend deployment needed.

## Tests

```bash
cargo test --workspace                            # 120 tests: WAL framing, filter DSL,
                                                   # hot-attr extraction, schema gen,
                                                   # compaction SQL + per-service retention,
                                                   # anomaly detection, config validation,
                                                   # dashboard APIs, sharded ingest workers,
                                                   # cold-tier sync round-trip
cargo test --test hot_attributes                  # integration: insert + filter roundtrip
python3 tests/e2e_sentry_errors.py                # real sentry-sdk E2E (start a server first:
                                                  #   fresh data dir, port 18114, static admin key
                                                  #   "e2e-static-admin", error_tracking webhooks on
                                                  #   127.0.0.1:18115 — see the script docstring)
cargo test --workspace --features object-storage  # with the S3/GCS archive compiled in
```

## Project layout

```
central-logs/
├── Cargo.toml
├── build.rs                   # tells cargo to re-run when target/web-dist changes
├── src/
│   ├── main.rs                # CLI + task orchestration
│   ├── lib.rs
│   ├── config.rs              # figment + clap config + validation
│   ├── error.rs
│   ├── errors.rs               # error tracking: fingerprints, error_groups, reconcile
│   ├── hot.rs                 # hot-attribute config, JSON path resolver, type coercion
│   ├── query.rs               # filter DSL parser → safe SQL
│   ├── ai.rs                  # LLM client (OpenAI / Anthropic) → DSL
│   ├── alerts.rs              # alert-rule evaluation + webhook notification
│   ├── wal/
│   │   ├── frame.rs           # CRC32 frame encode/decode
│   │   ├── segment.rs         # segment rotation
│   │   ├── meta.rs            # redb-backed checkpoints
│   │   ├── writer.rs          # group-commit fsync loop
│   │   └── cursor.rs          # ingest tail cursor
│   ├── ingest/
│   │   ├── parse.rs           # JSON/syslog/free-text + hot-attr extraction
│   │   ├── enrich.rs          # GeoIP, hostname tagging
│   │   └── worker.rs          # background ingest workers
│   ├── store/
│   │   ├── schema.rs          # DuckDB DDL
│   │   ├── appender.rs        # bulk insert (dynamic column list)
│   │   ├── rollup.rs          # 1m / 1h rollup jobs
│   │   └── compact.rs         # hot → Parquet export with bloom filters
│   ├── insert/
│   │   ├── http.rs            # /v1/logs endpoint
│   │   ├── syslog.rs          # UDP + TCP listeners
│   │   └── counters.rs        # per-protocol insert metrics
│   ├── analytics/
│   │   ├── forecast.rs        # augurs ETS
│   │   └── anomaly.rs         # MAD / seasonal / rate-of-change
│   ├── mcp/
│   │   ├── tools.rs           # 5 rmcp tool definitions
│   │   └── server.rs          # transport launchers
│   └── web/
│       ├── mod.rs
│       ├── dashboard.rs       # v1 askama dashboard (legacy fallback)
│       ├── templates.rs
│       ├── api.rs             # v2 JSON API (filter DSL, CRUD, AI query)
│       ├── errors_api.rs      # error-groups API (list/detail/resolve/explain)
│       ├── sentry_api.rs      # Sentry-SDK ingest (/api/{project}/envelope|store)
│       ├── ops_api.rs         # storage overview, cold listing, backup/restore
│       └── spa.rs             # embedded React build catch-all
├── templates/                 # askama HTML templates
├── web/                       # SPA source
│   ├── package.json
│   ├── vite.config.ts
│   └── src/
│       ├── main.tsx
│       ├── App.tsx            # router
│       ├── api.ts             # typed API client
│       └── pages/             # Logs, Errors, Dashboards, Alerts, Pipeline, Storage, …
├── integration-sample/          # ready-made insert clients per language
│   ├── README.md                # env contract + per-language table
│   ├── js/central-logs.ts       # TypeScript (Node 18+/Bun, zero deps)
│   ├── python/central_logs.py   # Python (stdlib only)
│   ├── rust/central_logs.rs     # Rust (std only, raw HTTP/1.1)
│   ├── php/central_logs.php     # PHP (ext-curl)
│   ├── java/CentralLogs.java    # Java (JDK 11+ java.net.http)
│   ├── c/central_logs.{c,h}     # C (libcurl + pthreads)
│   └── cpp/central_logs.{cpp,hpp} # C++17 (libcurl + pthreads)
├── tests/
│   ├── hot_attributes.rs      # DuckDB roundtrip for promoted hot-attribute columns
│   ├── end_to_end.rs          # HTTP insert → WAL → ingest → rollup → filter-DSL query
│   └── e2e_sentry_errors.py   # real sentry-sdk E2E (error tracking) — run vs a live server
└── docs/
    └── ARCHITECTURE.md        # full design doc
```
