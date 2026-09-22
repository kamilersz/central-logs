# Dashboards

The dashboard reads from the same rollup tables the MCP and alert
evaluator use — so a query that returns a number in the UI returns the
same number via MCP and triggers the same alert threshold.

## What's built in

The SPA ships with:

- **Logs Explorer** — time-range picker, filter bar with click-to-insert
  column badges, click-to-expand row details, **Ask AI** button that
  translates natural language into filter DSL.
- **Dashboards Home** — built-in presets and your saved custom dashboards.
- **Dashboard Builder** — name a dashboard, add/remove panels
  (volume / error-rate / latency / top-services / log-count), each with
  its own window and filter DSL.
- **Dashboard Viewer** — renders saved panels in a grid.
- **Presets**:
  - **Volume + forecast overlay** (dashed projection)
  - **Error rate** (per-minute + %)
  - **Latency** p50/p95/p99 per service
  - **Anomalies** with severity badges
- **Errors** — Sentry-style error-group browser (see [Error tracking](ERROR_TRACKING.md)).
- **Alerts** — list rules grouped by status; approve/reject buttons.
- **Pipeline** — channel fill, WAL throughput, ingest lag, audit drops.
- **Storage** — hot/cold footprint, backup runs, restore, retention.
- **API Keys** — issue scoped CRUD keys, revoke (admin only).

## Default dashboard set (per [Architecture §4](ARCHITECTURE.md#4-dashboards-metrics))

1. **Ingestion/insert rate** — records/sec and bytes/sec by protocol,
   sourced directly from insert-layer counters (cheapest possible
   metric — doesn't touch DuckDB at all).
2. **Error rate over time** — `count(level='error') / count(*)` per
   time bucket, overall and per-service.
3. **Latency percentiles per service** — p50/p95/p99 via DuckDB
   `approx_quantile()` over a `duration_ms` attribute, pre-aggregated
   into rollups.
4. **Top-N hosts/services** — by volume or error count, over the
   selected time range.
5. **Log volume trend** — stacked area by level or service.
6. **Anomaly markers** — vertical markers/shaded regions on volume/error
   rate charts at flagged timestamps.
7. **Forecast overlay** — dashed projection line extending volume/error
   rate charts past "now."
8. **Pipeline health** — insert-channel fill bar, WAL throughput,
   ingest lag, and drop counters — the "is the buffer about to
   overflow?" page.

## Custom dashboards (CRUD)

Save a dashboard for reuse:

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/dashboard/configs` | List saved dashboards |
| POST | `/api/dashboard/configs` | Create (body: `{name, description?, panels}`) |
| GET | `/api/dashboard/configs/{id}` | Fetch one |
| PUT | `/api/dashboard/configs/{id}` | Update name/description/panels |
| DELETE | `/api/dashboard/configs/{id}` | Delete |

Each `panels[]` entry has a panel kind (`volume`, `error_rate`,
`latency`, `top_services`, `log_count`), its own window, and its own
filter DSL string.

Mutations require `write` scope (admin works too).

## How rollups keep it fast

A background **rollup job** runs every 30-60s:

```sql
INSERT INTO rollup_1m
SELECT
  time_bucket(INTERVAL '1 minute', ts) AS bucket,
  service, level,
  count(*) AS n,
  approx_quantile(duration_ms, 0.5)  AS p50,
  approx_quantile(duration_ms, 0.95) AS p95,
  approx_quantile(duration_ms, 0.99) AS p99
FROM logs
WHERE ts > :last_rollup_ts
GROUP BY 1, 2, 3;
```

Dashboards query `rollup_1m` / `rollup_1h` almost exclusively, so
dashboard load stays fast regardless of total log volume. Ad-hoc
drill-down (clicking into a time window to read raw messages) queries
the raw hot table (or cold Parquet for older windows) directly —
inherently a smaller, bounded query.

## Forecast

The volume and error-rate charts overlay a dashed forecast line +
shaded 95% prediction interval extending past "now." Forecasts are
ETS / Holt-Winters via the [`augurs`](https://github.com/grafana/augurs)
crate, reading from the same rollup tables.

Tunable knobs (in `Config`):

- `forecast_every_n_rollups` — re-compute cadence; default 5 rollup
  cycles per forecast refresh.

## Anomalies

Three complementary methods land in the `anomalies` table; the chart
marks them as vertical bands.

| Method | Strength |
|---|---|
| Rolling MAD | Robust to spikes; primary detector |
| Seasonal-adjusted | Compares actual to expected seasonal value; avoids false positives at daily dips |
| Rate-of-change | Catches sharp step changes faster than rolling windows |

Tunable knobs: `forecast_every_n_rollups` controls how often the
background anomaly cycle runs (it's also a multiple of the rollup
interval).

## Pipeline observability

The **Pipeline** page (and `/api/pipeline`, `/metrics`) exposes:

| Gauge | Meaning |
|---|---|
| `central_logs_wal_channel_depth` / `_capacity` | entries waiting in the insert→WAL channel vs the configured bound |
| `central_logs_wal_bytes_written_total` | WAL throughput |
| `central_logs_ingest_lag_bytes` | WAL bytes not yet consumed+checkpointed |
| `central_logs_audit_dropped_total` | self-audit events dropped because the channel was saturated |

Escalation path: channel fill climbing → raise `insert_channel_depth`
(buys seconds) and investigate disk fsync latency. Ingest lag climbing
→ inserts are the bottleneck → shard more workers / raise
`ingest_batch_size`.

## Next

- [Alerts →](alerts.md) — rules and notification channels
- [AI features →](ai.md) — natural-language query and explain
- [HTTP API →](API.md) — full dashboard JSON endpoints