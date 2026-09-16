# API reference

## Auth (OWASP A01/A07)

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/login` | public | Server-rendered sign-in HTML form |
| POST | `/api/auth/login` | public | `{api_key}` → `{session_id, key_name, scopes}` + sets `cl_session` cookie |
| POST | `/api/auth/logout` | any auth | Drop the calling session, clear cookie |
| GET | `/api/auth/whoami` | any auth | `{key_id, name, scopes, via}` — used by the SPA boot |
| GET | `/v1/api-keys` | admin | List active keys (no raw values) |
| POST | `/v1/api-keys` | admin | `{name, scopes?}` → `{key, id, key_prefix, scopes}`. **Raw key shown once.** |
| DELETE | `/v1/api-keys/{id}` | admin | Soft-revoke (`revoked_at = now`) |

## Insert

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/logs` | Accept NDJSON, JSON array, single JSON object, **or OTLP logs** (`application/x-protobuf` / OTLP-JSON). Returns when fsync'd. |
| POST | `/v1/logs/bulk` | Alias for the above. |
| POST | `/v1/traces` | OTLP/HTTP spans (`application/x-protobuf` or `application/json`). |
| POST | `/v1/metrics` | OTLP/HTTP metric points (`application/x-protobuf` or `application/json`). |

### OpenTelemetry (OTLP/HTTP)

Any OTel SDK — Laravel/`open-telemetry/*`, Spring Boot, Python, Go, Node — can
export directly to central-logs with no collector. Both OTLP/HTTP encodings are
accepted: `application/x-protobuf` (the default for most SDKs) and
`application/json` (OTLP/JSON). gzip request bodies are decompressed
automatically.

Auth uses the same **`insert`-scoped key** as `/v1/logs`, passed as a request
header. Point the SDK at the base URL and add the header:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=https://your-central-logs-host
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer clk_..."
# (or the signal-specific OTEL_EXPORTER_OTLP_TRACES_ENDPOINT / _TRACES_HEADERS)
```

Storage mapping (all rows land in the same durable WAL → DuckDB pipeline):

| Signal | `protocol` | Notes |
|---|---|---|
| Traces | `otlp_span` | One row per span: `service`, `message` = span name, `trace_id`, `span_id`, `level` = `error` when `STATUS_CODE_ERROR`, `duration_ms`; span attrs + resource attrs + `parent_span_id`/`span_kind`/`status_code` in `attributes`. |
| Logs | `otlp_log` | `service`, `level` from severity, `message` from body, trace/span ids, log + resource attributes. |
| Metrics | `otlp_metric` | One row per data point: `message` = metric name, `attributes.value` = numeric value, plus labels, `unit`, `metric_type`, and histogram `hist_*` fields. |

Because spans carry `trace_id`/`span_id`/`duration_ms`, they are queryable with
the normal DSL (`trace_id:...`, `protocol:otlp_span`) and feed the existing
latency dashboards / rollups automatically.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/traces` | read | Recent traces: one row per `trace_id` (`window`/`from`/`to`/`filter`/`limit`). |
| GET | `/api/traces/{trace_id}` | read | All spans of one trace, start-time order. |
| GET | `/api/metrics/series?name=...` | read | Numeric time series for one OTLP metric (`service`, `window`, `bucket` optional). |

## Logs explorer

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/logs` | Filter DSL + `window` (`5m`/`1h`/`7d`) + `from`/`to` (RFC3339) + `limit`/`offset` |
| GET | `/api/logs/count` | Same filter, just returns `{"count": N}` |
| GET | `/api/schema/hot` | Hot attributes + all filterable columns (for UI autocomplete) |

## Dashboards (architecture §4)

| Method | Path | Returns |
|---|---|---|
| GET | `/api/dashboard/error-rate` | Per-bucket `{ts, errors, total, rate}` |
| GET | `/api/dashboard/latency` | Per-bucket per-service `{ts, service, p50, p95, p99}` |
| GET | `/api/dashboard/anomalies` | Recent `anomalies` table rows |
| GET | `/api/dashboard/forecast` | Point forecasts + 95% intervals via `augurs` ETS |
| GET | `/api/counters` | Insert-layer counters (cheapest metric source) |
| GET | `/api/pipeline` | Channel fill, WAL throughput, ingest lag, audit drops |
| GET | `/api/store` | Hot/cold row counts, oldest/newest timestamps |
| GET | `/metrics` | Prometheus text format (`central_logs_wal_channel_depth`, `central_logs_ingest_lag_bytes`, `central_logs_audit_dropped_total`, …) |

## Dashboard config CRUD (customizable dashboards)

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/dashboard/configs` | List saved dashboards |
| POST | `/api/dashboard/configs` | Create (body: `{name, description?, panels}`) |
| GET | `/api/dashboard/configs/{id}` | Fetch one |
| PUT | `/api/dashboard/configs/{id}` | Update name/description/panels |
| DELETE | `/api/dashboard/configs/{id}` | Delete |

## Alert rules (architecture §7)

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/alert-rules` | List all rules with their status + evaluator state (`last_evaluated_at`, `last_fired_at`, `last_notify_error`) |
| POST | `/api/alert-rules` | Create a rule from the web UI — starts **`active`** (the creating human is the approver). Requires admin |
| PUT | `/api/alert-rules/{id}` | Update name/condition/channels. Requires admin |
| DELETE | `/api/alert-rules/{id}` | Delete the rule (firing history is kept). Requires admin |
| POST | `/api/alert-rules/{id}/approve` | Move a `pending_approval` rule to `active` |
| POST | `/api/alert-rules/{id}/reject` | Move to `rejected` |
| GET | `/api/alert-rules/{id}/events` | Firing history for one rule, newest first (up to 200 rows) |

Rules are created either from the web UI (start active — the human pressing
"save" is the approver) or by AI agents through the `create_alert_rule` MCP
tool (always land in `pending_approval`; a human must approve them in the
dashboard before they can fire).

Once a rule is `active`, the background evaluator (`src/alerts.rs`) re-checks
it every `alert_eval_interval_secs` (default 30s). `condition` is one of:

```jsonc
// Threshold: fire when the metric's value over the trailing window compares
// to `value`. comparator: ">" | ">=" | "<" | "<=" | "==".
{ "type": "threshold", "comparator": ">", "value": 100, "window_secs": 300, "cooldown_secs": 900 }

// Anomaly: fire when an `anomalies` row for this metric at or above
// min_severity ("low" | "medium" | "high" | "critical") landed in the window.
{ "type": "anomaly", "min_severity": "high", "window_secs": 300, "cooldown_secs": 900 }

// Count: fire when n rows matching a filter-DSL expression land in the
// window ("n matching events per period"). Evaluated against the hot `logs`
// table with the same whitelist + parameterized SQL as the query API.
{ "type": "count", "filter": "service:api level:error", "comparator": ">=", "count": 50, "window_secs": 300, "cooldown_secs": 900 }

// Multi-threshold (any condition type): an escalation ladder. Array order is
// escalation order; the last breached entry wins. The evaluator tracks the
// rule's severity state and notifies ONLY on state changes — one
// notification per transition (healthy → warning → critical → recovered),
// never a repeat while the state stays the same. A recovery (severity "ok")
// also notifies once.
{ "type": "count", "filter": "service:api level:error", "window_secs": 300,
  "thresholds": [
    { "severity": "warning",  "comparator": ">=", "value": 50 },
    { "severity": "critical", "comparator": ">=", "value": 200 }
  ] }
```

`metric` (threshold/anomaly) is `volume`, `error_rate`, `p50_latency`,
`p95_latency`, or `p99_latency` — the same metric names
`get_dashboard_summary`/`forecast` use, read from the `rollup_1m` table.
`window_secs` and `cooldown_secs` default to 300 and 900. `cooldown_secs`
prevents a sustained breach from re-notifying every evaluation cycle.

### Notification channels

Each rule notifies one or more managed channels (plus the legacy raw-webhook
`channel` URL kept for MCP-era rules). Bad filters, unknown channels, and
bad configs are rejected at save time — not silently at eval time.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/alert-channels` | List channels (secrets masked — bot tokens never returned) |
| POST | `/api/alert-channels` | Create: `{name, type, config}`. Requires admin |
| PUT | `/api/alert-channels/{id}` | Update; sending back the masked token keeps the stored one. Requires admin |
| DELETE | `/api/alert-channels/{id}` | Delete (rules referencing it skip it). Requires admin |
| POST | `/api/alert-channels/{id}/test` | Send a sample notification through the channel. Requires admin |

Channel types and their `config` payloads:

```jsonc
// email — recipient list, delivered via the SMTP relay configured in
// [smtp] (docs/CONFIGURATION.md). Subject: "[central-logs] <rule name>".
{ "recipients": ["ops@example.com", "oncall@example.com"] }

// telegram — Bot API sendMessage. Get a token from @BotFather; the bot must
// be a member of the target group chat (or the chat_id is a user DM).
{ "bot_token": "123456:ABC-DEF...", "chat_id": "-1001234567890" }

// webhook — POST a JSON payload {rule_id, rule_name, metric, value, message,
// fired_at, source} to any http(s) URL (Slack/Discord/Teams incoming
// webhooks, PagerDuty Events API, custom endpoints).
{ "url": "https://hooks.example.com/..." }
```

Every evaluation writes one `alert_events` row per channel attempt
(`notified: false` + `error` when delivery fails; the `channel` column names
the target), and updates the rule's
`last_evaluated_at`/`last_fired_at`/`last_notify_error`.

## Error groups (error tracking)

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/error-groups` | List. Params: `window` (`1h`/`24h`/`7d`/`30d`), `status` (`unresolved`/`resolved`/`ignored`/`all`), `service`, `q` (title/exception search), `sort` (`recent`/`count`), `limit`/`offset` |
| GET | `/api/error-groups/{fingerprint}` | One group incl. sampled stacktraces + 24h per-minute trend (`spark`) |
| DELETE | `/api/error-groups/{fingerprint}` | Delete the group row (events are kept) |
| POST | `/api/error-groups/{fingerprint}/resolve` | Mark resolved — a later event re-opens it and fires a regression webhook |
| POST | `/api/error-groups/{fingerprint}/unresolve` | Back to unresolved without notifying |
| POST | `/api/error-groups/{fingerprint}/ignore` | Suppress from the default (unresolved) view |
| POST | `/api/error-groups/{fingerprint}/explain` | AI explanation of the error (body: `{"context?": "operator notes"}`). Returns `{provider, explanation}`; with `[llm]` off returns a configuration hint instead |

Reads require the `read` scope, lifecycle mutations `write`. Group status
vocabulary: `unresolved` (open) / `resolved` (done — a new event re-opens
the group and fires a regression webhook) / `ignored` (deferred). Sentry-
format events carry `protocol=sentry`, so `/api/logs?filter=protocol:sentry`
isolates them and `-protocol:sentry` excludes them; underlying events for a
group are `/api/logs?filter=fingerprint:<fp>`.

## AI auto-filter

| Method | Path | Body |
|---|---|---|
| POST | `/api/ai/query` | `{"query": "natural language", "time_hint?": "1h"}` |

Returns `{"filter": "service:api level:error", "provider": "anthropic",
"raw": "..."}`. The `filter` field is always validated through the DSL parser
before being returned, so a misbehaving model can't produce garbage SQL.

## Filter DSL

```
filter     := clause ( WS+ clause )*
clause     := comparison | bare_text
comparison := key op value
op         := ':' | '!=' | '>=' | '<=' | '>' | '<' | '~'
key        := identifier (must be in the column whitelist)
value      := quoted("...") | bare_token
bare_text  := a token with no key:value form → implicit message ILIKE '%text%'
```

| Input | Compiles to |
|---|---|
| `service:api` | `service = ?` |
| `level:error` | `level = ?` |
| `user_id:42` | `user_id = ?` (cast to BIGINT by DuckDB) |
| `raw_len>=100` | `raw_len >= ?` |
| `message~timeout` | `message ILIKE ?` (`%timeout%`) |
| `message:"connection refused"` | `message = ?` |
| `connection` (bare) | `message ILIKE ?` (`%connection%`) |
| `service:api level:error` | `service = ? AND level = ?` |
| `fingerprint:bac065f4` | `fingerprint = ?` (error tracking) |
| `-service:central-logs` | `service != ?` (exclusion) |
| `service:api OR service:web` | `service = ? OR service = ?` |
| `(service:api OR service:web) level:error` | `(service = ? OR service = ?) AND level = ?` |
| `level:error -(service:api OR service:web)` | `level = ? AND NOT (service = ? OR service = ?)` |

Connectors: `AND` (implicit between adjacent clauses, also explicit) and `OR`
(case-insensitive); AND binds tighter than OR, and parentheses group
explicitly. A leading `-` negates a clause or group — `user_id:-42` keeps
`-42` as the value; quote `"-…"` to search for a literal leading dash.

Keys are validated against a whitelist (built-in columns + configured hot
attributes); unknown keys return a `filter_error` in the response rather than
reaching SQL. Numeric operators (`>`, `<=`, etc.) on text columns are rejected
at parse time. ILIKE (`~`) on non-text columns is rejected.

## MCP tools

Start with `--mcp-mode stdio` for local Claude Desktop/Code integration.
There is no remote/HTTP transport in this version: `--mcp-mode sse` is
rejected at config validation rather than silently starting nothing. The
`rmcp` crate's streamable-HTTP server API needs request/response body
adapters this codebase doesn't wire up yet (see `src/mcp/server.rs`); for
remote agent access today, run stdio behind an external MCP-over-stdio proxy.

| Tool | Inputs | Notes |
|---|---|---|
| `query_logs` | `query?`, `from?`, `to?`, `limit?` | Runs against `logs_all` (hot + cold) |
| `get_dashboard_summary` | `metric`, `from?`, `to?` | Pre-digested stats + top-N + narrative hints |
| `detect_anomalies` | `metric`, `from?`, `to?`, `sensitivity?` | Reads the `anomalies` table, falls back to on-the-fly MAD |
| `forecast` | `metric`, `horizon` (e.g. `6h`), `granularity?` | ETS via `augurs` |
| `create_alert_rule` | `name`, `metric`, `condition`, `notification_channel` | Gated behind `--enable-alert-mcp-tool`; always lands as `pending_approval` |
