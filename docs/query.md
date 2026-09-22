# Querying logs — the filter DSL

Every read endpoint (`GET /api/logs`, `GET /api/logs/count`, the MCP
`query_logs` tool, the **Ask AI** button in the SPA) speaks the same
**filter DSL**. It is a small, safe expression language that compiles
into parameterized SQL — values are bound, never interpolated, and
columns are validated against a whitelist before any SQL is generated.

## Syntax

```text
filter     := clause (WS+ clause)*
clause     := comparison | bare_text
comparison := key op value
op         := ':' | '!=' | '>=' | '<=' | '>' | '<' | '~'
key        := identifier      (must be in the column whitelist)
value      := quoted("...") | bare_token
bare_text  := token with no key:value form → implicit message ILIKE '%text%'
```

Whitespace separates clauses. A bare token (no `key:`) is treated as a
substring search against the `message` column.

## Operator reference

| Operator | Meaning | Example |
|---|---|---|
| `:` | equality (text columns) / exact match (numeric) | `service:api` |
| `!=` | not-equal | `level:debug` to exclude |
| `>` `>=` `<` `<=` | numeric comparison | `duration_ms>=1000` |
| `~` | case-insensitive substring | `message~timeout` |

Wildcard matching on text columns with `:`: `%` matches any run of
characters, `_` matches exactly one. `message:"cp cp-tekab % success"`
becomes `message ILIKE '%cp cp-tekab % success%'`.

## Examples

| Input | Compiles to |
|---|---|
| `service:api` | `service = ?` |
| `level:error` | `level = ?` |
| `user_id:42` | `user_id = ?` (cast to BIGINT by DuckDB) |
| `raw_len>=100` | `raw_len >= ?` |
| `message~timeout` | `message ILIKE ?` (`%timeout%`) |
| `message:"connection refused"` | `message = ?` |
| `message:"cp cp-tekab % success"` | `message ILIKE ?` (`%` = any chars, `_` = one char) |
| `connection` (bare) | `message ILIKE ?` (`%connection%`) |
| `service:api level:error` | `service = ? AND level = ?` |
| `fingerprint:bac065f4` | `fingerprint = ?` (error tracking) |
| `-service:central-logs` | `service != ?` (exclusion) |
| `service:api OR service:web` | `service = ? OR service = ?` |
| `(service:api OR service:web) level:error` | `(service = ? OR service = ?) AND level = ?` |
| `level:error -(service:api OR service:web)` | `level = ? AND NOT (service = ? OR service = ?)` |

## Boolean logic

Connectors: `AND` (implicit between adjacent clauses, also explicit) and
`OR` (case-insensitive). AND binds tighter than OR; parentheses group
explicitly. A leading `-` negates a clause or a parenthesized group:

```
user_id:-42          →  user_id = -42
-user_id:42          →  user_id != 42
-level:debug         →  level != 'debug'
-(service:api OR service:web)   →  NOT (service = ? OR service = ?)
```

> To search for a literal leading dash in a quoted value, wrap it in
> quotes: `"-42"`.

## What columns are accepted

Two pools, both enforced by the parser:

1. **Built-in columns** (see [API.md → built-ins](API.md)): `ts`,
   `insert_ts`, `source_host`, `service`, `level`, `message`,
   `fingerprint`, `trace_id`, `span_id`, `attributes`, `geo_country`,
   `raw_len`, `protocol`, plus any column promoted via
   `--hot-attribute` / `[hot_attributes]`.

2. **Hot attributes** — JSON keys you've promoted to typed top-level
   columns. Promote a key at startup:

   ```bash
   --hot-attribute 'user_id:bigint'
   --hot-attribute 'route:varchar:$.request.route'
   --hot-attribute 'is_canary:boolean'
   ```

   Or in TOML:

   ```toml
   hot_attributes = [
     { name = "user_id", duckdb_type = "bigint", json_path = "$.user_id" },
     { name = "route",   duckdb_type = "varchar", json_path = "$.request.route" },
   ]
   ```

   Hot attributes participate in the filter DSL and in zonemap-pruned
   scans (no per-row JSON parse).

Unknown keys return `filter_error` in the response rather than reaching
SQL. Numeric operators (`>`, `<=`, etc.) on text columns and `~` on
non-text columns are rejected at parse time.

## Per-service query caps

A query that pins `service:<value>` equality against a service matching
a `[[service_query_limits]]` glob is capped at that window. This is the
single-node analog of OpenObserve's per-stream `max_query_range` — one
wide user/agent scan can't monopolize the node.

```toml
[[service_query_limits]]
pattern = "payment_*"
max_window_secs = 86400        # 24h
```

CLI equivalent:

```bash
--service-query-limit 'payment_*=24h'
```

Unfiltered queries and `~` text matches are not capped.

## HTTP endpoints using the DSL

| Endpoint | Behaviour |
|---|---|
| `GET /api/logs` | rows + total_matched + filter_error |
| `GET /api/logs/count` | `{ "count": N }` |
| `POST /api/ai/query` | LLM translates natural language → DSL (validated before return) |

Common parameters: `window` (`5m`/`1h`/`24h`/`7d`) **or** `from`/`to`
(RFC3339); `limit` (default 100, max 1000); `offset` for pagination.

## Safety guarantees

The DSL compiles to bound-parameter SQL via DuckDB prepared statements.
A representative test case from `src/query.rs`:

```text
parse_filter("service:api';--")        → service = ?   (literal SQL-injection attempt, harmless)
parse_filter("evil_col:1")             → Err(filter_error: "unknown column 'evil_col'")
parse_filter("user_id~42")               → Err(filter_error: "~ only valid on text columns")
```

See the [HTTP API reference](API.md#filter-dsl) for the full grammar
and edge cases.