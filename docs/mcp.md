# MCP server

central-logs exposes an [MCP](https://modelcontextprotocol.io/) server
for AI agents. Tools cover the priority order laid out in
[Architecture §7](ARCHITECTURE.md#7-mcp-server-design):
**query / summarize → anomaly → forecast → alerting**.

The MCP server runs in-process inside the `central-logs` binary, sharing
the same DuckDB connection pool and rollup/anomaly state directly — no
extra hop, no external service.

## Transport

| Mode | CLI / env | Status |
|---|---|---|
| **stdio** | `--mcp-mode stdio` (or `CENTRAL_LOGS_MCP_MODE=stdio`) | ✅ Implemented — recommended for local Claude Desktop / Claude Code / Codex / OpenCode. |
| off | `--mcp-mode off` | Default. No MCP server started. |
| sse / streamable-HTTP | `--mcp-mode sse` | ❌ Not implemented. `Config::validate` rejects it at startup with a clear error rather than silently starting nothing. Tracked as future work; see [ARCHITECTURE.md §7](ARCHITECTURE.md#7-mcp-server-design) for why. |

> For **remote** MCP access today, run stdio behind an external
> MCP-over-stdio proxy (e.g. `mcp-remote`, `supergateway`). The MCP
> protocol is the same; only the transport differs.

## Starting the server

```bash
./target/release/central-logs --mcp-mode stdio
```

Or in the systemd unit (see [Operations](OPERATIONS.md)):

```ini
ExecStart=/home/central-logs/target/release/central-logs \
    --mcp-mode stdio \
    --mcp-api-key '...'    # optional bearer for SSE/HTTP mode
```

Stdio mode reads JSON-RPC on stdin and writes JSON-RPC on stdout. The
`tracing`/`log` output goes to stderr — don't interleave with the
protocol stream.

## Tools

| Tool | Purpose | Gated by |
|---|---|---|
| [`query_logs`](#query_logs) | Recent log rows with filter & window | — (always on) |
| [`get_dashboard_summary`](#get_dashboard_summary) | Pre-digested counts + top-N + hints | — |
| [`detect_anomalies`](#detect_anomalies) | Anomalies for a metric in a window | — |
| [`forecast`](#forecast) | Point forecasts + 95% intervals | — |
| [`create_alert_rule`](#create_alert_rule) | Create a rule (always `pending_approval`) | `--enable-alert-mcp-tool` |

### `query_logs`

Read-only. Runs against `logs_all` (hot DuckDB + cold Parquet). Hard
cap on rows to prevent runaway scans from agent-generated filters.

```jsonc
// input
{
  "query": "service:api level:error",   // optional; SQL WHERE-clause body or filter DSL
  "from":  "2026-09-22T00:00:00Z",      // optional ISO-8601
  "to":    "2026-09-22T01:00:00Z",      // optional ISO-8601
  "limit": 100                          // 1..=1000; default 100
}

// output
{
  "rows": [
    { "ts": "...", "service": "api", "level": "error", "message": "..." }
  ],
  "total_matched": 42,
  "truncated": false
}
```

### `get_dashboard_summary`

Read-only. Returns summary stats + top-N + narrative hints so the agent
can summarize without re-deriving aggregates.

```jsonc
// input
{ "metric": "error_rate", "from": "...", "to": "..." }

// output
{
  "summary_stats": {
    "metric": "error_rate",
    "total": 12345,
    "errors": 137,
    "error_rate": 0.0111,
    "window": { "from": "...", "to": "..." }
  },
  "top_n": [
    { "service": "checkout", "n": 5400 },
    { "service": "api",      "n": 3200 }
  ],
  "narrative_hints": [
    "Error rate is high (1.11%)."
  ]
}
```

### `detect_anomalies`

Read-only. Reads from the `anomalies` table; if empty, computes on the
fly from `rollup_1m` using the same MAD / seasonal / rate-of-change
detectors that back the dashboard markers.

```jsonc
// input
{ "metric": "volume", "from": "...", "to": "...", "sensitivity": 3.0 }

// output (array)
[
  { "ts": "...", "metric": "volume", "score": 4.2, "method": "mad",  "severity": "high" },
  { "ts": "...", "metric": "volume", "score": 5.1, "method": "rate_of_change", "severity": "critical" }
]
```

### `forecast`

Read-only. ETS / Holt-Winters from `augurs`, reading the same rollup
tables the dashboard uses. Outputs point forecasts + 95% prediction
intervals.

```jsonc
// input
{ "metric": "volume", "horizon": "6h", "granularity": "minute" }
// (horizon accepts "30m", "6h", "2d"; granularity "minute" | "hour")
```

### `create_alert_rule`

Write-side. **Always** creates the rule in `pending_approval` — a human
must approve it via the dashboard **Alerts** page before it can fire
notifications. This bounds the blast radius of an AI agent silently
wiring up paging loops.

The tool itself is gated behind `--enable-alert-mcp-tool` (and
`CENTRAL_LOGS_ENABLE_ALERT_MCP_TOOL=true`). Without it the call returns
a tool-level error explaining the gate.

```jsonc
// input
{
  "name": "API error rate spike",
  "metric": "error_rate",
  "condition": { "type": "threshold", "comparator": ">=", "value": 0.05, "window_secs": 300 },
  "notification_channel": "https://hooks.example.com/incidents"
}

// output
{ "rule_id": 17, "status": "pending_approval" }
```

The full alert-rule grammar (threshold, anomaly, count,
multi-threshold, error_group_threshold, channels, cooldowns) lives in
[Alerts](alerts.md) and [API.md → Alert rules](API.md#alert-rules-architecture-7).

## Configuring Claude Code / OpenCode / Codex

Any MCP client that can speak stdio works. The simplest configuration
just runs the binary directly.

`.mcp.json` (project-local, used by Claude Code and others):

```json
{
  "mcpServers": {
    "central-logs": {
      "command": "/home/central-logs/target/release/central-logs",
      "args": ["--mcp-mode", "stdio"]
    }
  }
}
```

Claude Desktop `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "central-logs": {
      "command": "/home/central-logs/target/release/central-logs",
      "args": ["--mcp-mode", "stdio"]
    }
  }
}
```

## Best practices for agents

- **Start with `get_dashboard_summary`** before drilling into raw rows.
  Summary stats + narrative hints are far cheaper than scanning `logs`.
- **Use `query_logs` only with bounded windows and `limit` ≤ 200.** The
  tool is capped at 1000 but a 1000-row scan is still ~1000 JSON
  objects on the wire.
- **Prefer the `from`/`to` form** over `query` with time predicates —
  per-service query caps only apply when the filter pins a service by
  equality.
- **Never call `create_alert_rule` without operator confirmation.** The
  pending-approval gate is a safety mechanism, not a UX papercut — its
  job is to require a human to confirm every AI-proposed rule fires.
- **`detect_anomalies` and `forecast` are pre-digested** — both read
  the same tables the dashboards use. Trust the `severity` field rather
  than re-deriving thresholds.

## See also

- [For AI agents](llms.md) — recommended `llms.txt` contract and
  agent-friendly prompts.
- [Architecture §7](ARCHITECTURE.md#7-mcp-server-design) — design
  rationale, transport status, and the priority order for tools.