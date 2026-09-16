# Error tracking (Sentry-SDK-compatible)

> **Status: implemented.** Sentry-SDK-compatible ingest, fingerprint
> grouping, sampled stacktraces, the Errors SPA page, event-time +
> threshold notifications, and the AI explain endpoint are all live and
> covered by `cargo test` plus the real-SDK end-to-end
> (`tests/e2e_sentry_errors.py`, run against a started server).

Goal: Sentry-like error tracking built on the existing pipeline — group
errors by fingerprint, save stacktraces (sampled, deduplicated), display
them in the SPA, and notify on first-seen, regression, and threshold —
**without requiring any new client library**: unmodified Sentry SDKs
(python, rust, JS, java, PHP, …) point their existing DSN at central-logs.

## 1. Why this fits the existing architecture

| Need | Existing seam |
|---|---|
| Durable ingest | WAL insert path (`POST /v1/logs` pattern) — new routes reuse it |
| Event storage | `logs` table + `attributes` JSON; hot/cold Parquet tiering |
| Derived state | `error_groups` upsert job — same pattern as rollups/anomalies |
| Trend data | new `rollup_error_1m` — same pattern as `rollup_1m` |
| Event-time notifications | dedicated notifier task fed by the ingest worker; new `error_group_threshold` evaluator condition |
| UI | new SPA page on the existing router/design system |

The only genuinely new logic is fingerprinting, envelope parsing, and the
group table maintenance.

## 2. Sentry SDK compatibility (transport layer)

### 2.1 DSN mapping — no new auth code

A Sentry DSN is `{scheme}://{public_key}[:{secret}]@{host}:{port}/{project_id}`.
Map it onto existing credentials:

```
http://clk_YOUR_INSERT_KEY@logs.example.com:8080/7
      └── insert-scoped API key ──┘       └─ numeric project id ─┘
```

SDKs enforce a NUMERIC project id. Map ids to service names in config:

```toml
[error_tracking.projects]
"7" = "checkout"
```

Unmapped ids become `project-<id>`; hand-rolled HTTP clients may use a
service name in the path directly (it is used verbatim).

- `sentry_key` (the DSN public key) **is** an existing `clk_...` API key
  with `insert` scope. Validation reuses the current key cache; mint the
  key via the existing **API Keys** page / `/v1/api-keys`.
- `project_id` maps to the `service` column. SDKs treat it as an opaque
  string, so `my-service` is fine (sentry.io uses numbers; that's not a
  protocol requirement).

### 2.2 Endpoints to implement

| Route | Purpose |
|---|---|
| `POST /api/{project}/envelope/` | Primary — all modern SDKs (sentry-python ≥2, sentry-rust, JS, java, PHP) |
| `POST /api/{project}/store/` | Legacy JSON event + `X-Sentry-Auth: Sentry sentry_key=...` header **or** `?sentry_key=` query param |
| `OPTIONS /api/{project}/(envelope\|store)/` | CORS preflight — required for `@sentry/browser` |

Implementation notes:

- Accept `sentry_key` from (in order): `X-Sentry-Auth` header, `?sentry_key=`
  query param, `Authorization: Bearer`, `X-API-Key` — browser SDKs cannot set
  custom headers on cross-origin requests, so query-param auth must work.
- CORS response headers on all three routes: `Access-Control-Allow-Origin: *`,
  allow headers `X-Sentry-Auth, X-Sentry-Last-Event-Id, Content-Type`.
- Accept `Content-Encoding: gzip`/`deflate` bodies (SDKs compress large
  envelopes).
- Success response: `200` + `{"id": "<event_id>"}` (SDKs parse this).
  Always ack after WAL fsync, same contract as `/v1/logs`.
- Envelope format: first line `{"event_id": ...}` headers, then per item a
  `{"type": ...}` header line + payload. Item handling:
  - `event` / `transaction` (only events needed) → parse & ingest
  - `session` → fold into counters only (crash-free rate later); no row
  - `attachment` → acked but not persisted (still ack — retries would do
    more harm)
- Minidump endpoint (`POST /api/{project}/minidump/`) is **deferred** —
  native-crash dumps are the one payload shape that argues for the
  object-storage archive, not DuckDB.

### 2.3 Event → central-logs record mapping

| Sentry field | Maps to |
|---|---|
| *(transport)* | `protocol` = **`sentry`** — marks the row so error-tracked events separate from regular logs (DSL: `protocol:sentry`, or `-protocol:sentry` to exclude) |
| *(DSN project)* | `service` |
| `level` | `level` (identical vocabulary: `fatal/error/warning/info/debug`; normalize `warning`→`warn`) |
| `exception.values[last].type: value` or `message.formatted` | `msg` — `"<Type>: <value>"` |
| `exception.values[last].stacktrace.frames` (+ top-level `stacktrace`, `threads.values[].stacktrace`) | `attributes.stack` — serialized newest-first, truncated per §4.2 |
| `timestamp` | `ts` (fallback: receive time) |
| `user` | `attributes.user` (+ `user_id` hot column when `user.id` set) |
| `request` (url/method/status) | `attributes.request` |
| `tags` | merged into `attributes`; whitelisted tag keys can become hot columns |
| `environment`, `release`, `server_name`, `sdk.name/version`, `modules`, `breadcrumbs`, `extra` | `attributes.*` |
| `fingerprint` (client-supplied array) | grouping override, see §3.1 |

Parsing/enrichment happens in the ingest worker (async), exactly like syslog
free-text parsing — the insert path only frame-checks and fsyncs.

## 3. Grouping

### 3.1 Fingerprint algorithm

```
if event.fingerprint is a non-empty array:
    fp = sha256("c|" + join(event.fingerprint, "\x1f"))     # client wins
else:
    exc  = exception.values[last]  (else stacktrace, else normalized msg)
    top  = first 5 in_app frames of exc.stacktrace
    inputs = exc.type
           + normalize(exc.value)
           + for each frame: module, function, filename   # no lineno
    fp = sha256("s|" + join(inputs, "\x1f"))
```

`normalize()` strips volatile detail (one pass of regexes, ordered):

```
integers          \b\d+\b                → N
hex ids           \b[0-9a-f]{8,}\b       → H
uuids             [0-9a-f-]{36}          → U
ip addresses      \d+\.\d+\.\d+\.\d+     → IP
quoted strings    "…" / '…'              → 'S'
urls/paths        ids in segments        → …/{id}/…
whitespace        \s+                    → single space
```

Trade-off notes: excluding `lineno` from frames stops line-shift churn from
splitting groups (Sentry's rolling groupers do the same); messages *without*
a stacktrace group on normalized message alone, which is correct for
`log.error("...")` style use.

### 3.2 Variant tracking (trace drift)

`variant_hash = sha256(normalized_full_trace)`. A group keeps at most
`samples_per_group` (default 3) stacks: **first-seen**, **latest**, and one
additional distinct variant. When a 4th variant appears, it replaces the
oldest non-first sample. This catches stack drift (frames shifting across
releases) without unbounded growth.

### 3.3 Separation from regular logs + lifecycle marking

Every Sentry-format event row is marked at insert time so it never gets
confused with ordinary application logs:

- `protocol = 'sentry'` (typed column, DSL-filterable both ways:
  `protocol:sentry` for only error-tracked events, `-protocol:sentry` to
  exclude them from a general search), plus a non-null `fingerprint` and
  `attributes.sentry` SDK metadata.
- Regular `/v1/logs` + syslog rows keep `http_json` / `syslog_*` protocols
  and are untouched.
- Lifecycle marking lives **on the group** (one row per fingerprint in
  `error_groups.status`), not per event:

| Status | Meaning | Set via |
|---|---|---|
| `unresolved` | open / needs attention (default) | ingest, or `POST .../unresolve` |
| `resolved` | done — fixed; a new event re-opens it (`group.regressed` + webhook) | `POST .../resolve` |
| `ignored` | deferred / muted — hidden from the default view, still tracked | `POST .../ignore` |

The SPA **Errors** page shows the status pill per group and hosts the
buttons; Logs Explorer rows stay raw events (filter them with
`protocol:sentry` and jump to the group via `fingerprint:`).

## 4. Storage

### 4.1 Schema

```sql
-- events: one typed column on the existing table (same pattern as hot attributes)
ALTER TABLE logs ADD COLUMN IF NOT EXISTS fingerprint VARCHAR;

CREATE TABLE IF NOT EXISTS error_groups (
    fingerprint     VARCHAR PRIMARY KEY,
    service         VARCHAR,
    level           VARCHAR,            -- max severity seen
    title           VARCHAR,            -- "<Type>: <template>"
    exception_type  VARCHAR,
    first_seen      TIMESTAMP,
    last_seen       TIMESTAMP,
    total_count     BIGINT,
    status          VARCHAR DEFAULT 'unresolved',  -- unresolved|resolved|ignored
    resolved_at     TIMESTAMP,
    samples         JSON,               -- ≤3 × {variant_hash, first_seen, stack}
    variant_hashes  VARCHAR[]           -- rolling set backing samples
);

-- per-group 1-minute counts: sparklines + fast threshold evaluation
CREATE TABLE IF NOT EXISTS rollup_error_1m (
    ts          TIMESTAMP,
    fingerprint VARCHAR,
    count       BIGINT
);
```

### 4.2 Stacktrace size discipline (agreed decision)

Full traces live **on the group, not the event** — 10k events in a group
share ~95% of the trace text, so per-event full storage would be pure waste.

- Per-event: `attributes.stack` is written by the ingest route with the
  exception headline + frames (newest first), truncated to
  `stack_max_frames = 100` lines / `stack_max_bytes = 8192` bytes. The
  fingerprint only needs top frames anyway.
- Hot tier cost is bounded by the hot window; the columnar layout means
  queries not touching `stack` never read it.
- Cold tier: existing compaction writes zstd Parquet — near-identical traces
  compress extremely well. No new storage engine.
- Optional zstd-per-event BLOB compression was considered and **not**
  built — group sampling + zstd Parquet already cap the cost.
- Groups survive event retention: `samples` keeps the stack after the
  underlying events age out of the cold tier.

### 4.3 Group maintenance

Read-then-write upsert in the ingest worker (single store mutex ⇒
race-free; same sharded at-least-once semantics as the batch insert):
insert with status `unresolved` on first sight; otherwise bump
`last_seen` (GREATEST) / `total_count`, keep the max severity level,
rotate stack samples per §3.2, and flip `resolved → unresolved`
(regression) when the group was resolved.

At-least-once replay can slightly over-count `total_count`; a reconciliation
job (default hourly) recomputes `total_count`/`last_seen` from the `logs`
table — a zonemap-pruned `GROUP BY fingerprint` once `fingerprint` is a typed
column.

## 5. Notifications

Two paths:

1. **Event-time** — a dedicated notifier task (`main.rs`) POSTs
   `group.created` / `group.regressed` payloads to the
   `[error_tracking.notify] webhooks` list (same JSON conventions as alert
   webhooks).
2. **Threshold** — the `error_group_threshold` condition rides the standard
   alert evaluator: channels, cooldowns, `alert_events` history, approval
   gates.

### 5.1 Event-time (new triggers, async — never on the hot path)

The ingest worker enqueues after the group upsert:

| Trigger | Condition |
|---|---|
| `group.created` | first event of a new fingerprint (gated by `notify.min_level`) |
| `group.regressed` | event lands on a `resolved` group (status flips back + notify) |

Payload: `{kind, service, fingerprint, title, level, count, first_seen,
last_seen}` where `kind` is `group.created` or `group.regressed`.

### 5.2 Threshold (new evaluator condition type)

```jsonc
{
  "type": "error_group_threshold",
  "fingerprint": null,          // null = any group; else one specific group
  "min_level": "error",         // optional severity gate
  "comparator": ">=",
  "value": 50,                  // events over the window
  "window_secs": 300,
  "cooldown_secs": 900
}
```

Evaluated against the hot `logs` table (bounded-parameter COUNT per
fingerprint) inside the standard evaluator loop — severity state machine,
cooldowns, `alert_events` history, and the `pending_approval` human-approval
gate for MCP-created rules all apply unchanged.

## 6. Query/management API

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/error-groups` | List: `window`, `status`, `service`, `q` (title/type search), `sort=recent\|count`, paging |
| GET | `/api/error-groups/{fingerprint}` | One group incl. samples + 24h spark |
| DELETE | `/api/error-groups/{fingerprint}` | Delete the group row (events kept) |
| POST | `/api/error-groups/{fingerprint}/resolve` | Mark resolved (regression re-opens + notifies) |
| POST | `/api/error-groups/{fingerprint}/unresolve` / `/ignore` | Status management |
| POST | `/api/error-groups/{fingerprint}/explain` | AI explanation; returns a config hint when `[llm]` is off |

There is no dedicated `/events` route — underlying events are queried through
`/api/logs?filter=fingerprint:<fp>` (the SPA deep-links into Logs Explorer).

`fingerprint` becomes a whitelisted filter-DSL key so
`fingerprint:abc123 level:error` works in Logs Explorer and MCP `query_logs`.

## 7. SPA — Errors page

- **Group list**: title, service, count, first/last seen, status badge;
  free-text search; status/window filters; sort by recent/count.
- **Group detail**: exception headline + sample stacktraces with an
  in_app-highlighted frame viewer, 24h per-minute sparkline, event
  deep-link into Logs Explorer (`?filter=fingerprint:<fp>`), resolve/
  ignore/re-open, and the **Explain with AI** card (operator context
  optional; graceful hint when no LLM is configured).
- Reuses the existing dark design system; add an "Errors" nav entry.

## 8. Configuration

```toml
[error_tracking]
enabled = true                  # mounts the Sentry-protocol routes
projects = { "7" = "checkout" } # DSN project id → service name (ids numeric)
stack_max_frames = 100
stack_max_bytes = 8192
samples_per_group = 3
reconcile_interval_secs = 3600

[error_tracking.notify]
on_new_group = true
on_regression = true
min_level = "error"             # gate for event-time notifications
webhooks = ["https://hooks.example.com/..."]
```

CLI: `--no-sentry-ingest` disables the routes and group tracking entirely
(`fingerprint` is a built-in column; no hot-attribute setup needed).

## 9. Rollout phases

- **A — Ingest** ✅: envelope + store endpoints, DSN auth, event mapping,
  `fingerprint` column, truncation.
- **B — Grouping** ✅: `error_groups` upserts, variant tracking,
  `rollup_error_1m`, reconciliation job.
- **C — Surface** ✅: error-groups API + Errors SPA page (list/detail/
  resolve/AI explain).
- **D — Notifications** ✅: event-time triggers + `error_group_threshold`
  evaluator condition.
- **E — Polish** (remaining): per-language frame pretty-printing, MCP
  `list_error_groups` tool, minidump→object-storage.

## 10. Test coverage

Implemented in `cargo test --workspace`:

- normalization fixtures (numbers/hex/quoted/whitespace), fingerprint
  stability across message noise, client-fingerprint override, envelope
  parser (length-delimited + length-less items, CRLF, garbage rejection),
  event mapping (client/server fingerprints, level normalization, stack
  serialization + headline, truncation), project-id mapping, group upsert
  lifecycle (create/update/regression, sample rotation), reconciliation,
  list filters, `sentry_key` header/query parsing.

`tests/e2e_sentry_errors.py` (real `sentry-sdk`, against a live server):
DSN auth with a minted insert-scoped key, 3 noisy exceptions → 1 group,
stacktrace sample, DSL `fingerprint:` query, resolve → regression →
`group.regressed` webhook, `error_group_threshold` rule firing through the
evaluator to a webhook channel, AI-explain fallback, list search/filters.

Not yet exercised: `sentry-rust` / `@sentry/node` runs, and a headless
`@sentry/browser` CORS-preflight capture (endpoints are CORS-enabled and
preflight-bypassed in the auth middleware).
