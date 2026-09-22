# Sentry SDK ingest

Unmodified Sentry SDKs (Python, Rust, JS, Java, PHP, Go, …) can point
their existing DSN at central-logs and get fingerprint grouping, sampled
stacktraces, an **Errors** SPA page, and event-time notifications — with
**zero client-side code changes**.

Full design rationale lives in [Error tracking](../ERROR_TRACKING.md).
This page is the short operator/agent recipe.

## DSN mapping

A Sentry DSN is `{scheme}://{public_key}@{host}:{port}/{project_id}`. Map it onto existing credentials:

```
http://clk_YOUR_INSERT_KEY@logs.example.com:8080/7
      └── insert-scoped API key ──┘       └─ numeric project id ─┘
```

- `sentry_key` (the DSN public key) **is** an existing `clk_...` API
  key with `insert` scope. Mint via `POST /v1/api-keys`.
- `project_id` maps to the `service` column. SDKs treat it as an opaque
  string, so `my-service` is fine (sentry.io uses numbers; that's not
  a protocol requirement).

Map numeric project ids to friendly service names in config:

```toml
[error_tracking.projects]
"7" = "checkout"
```

Unmapped ids become `project-<id>`.

## Endpoints

| Route | Purpose |
|---|---|
| `POST /api/{project}/envelope/` | Primary — all modern SDKs (sentry-python ≥2, sentry-rust, JS, java, PHP) |
| `POST /api/{project}/store/` | Legacy JSON event + `X-Sentry-Auth` header or `?sentry_key=` query |
| `OPTIONS /api/{project}/(envelope\|store)/` | CORS preflight — required for `@sentry/browser` |

`sentry_key` is accepted from (in order): `X-Sentry-Auth` header,
`?sentry_key=` query param, `Authorization: Bearer`, `X-API-Key`.
Browser SDKs cannot set custom headers on cross-origin requests, so
query-param auth must work.

## Configuration

```toml
[error_tracking]
enabled = true                  # mount the Sentry-protocol routes (default on)
projects = { "7" = "checkout" } # DSN project id → service name
stack_max_frames = 100
stack_max_bytes = 8192
samples_per_group = 3
reconcile_interval_secs = 3600

[error_tracking.notify]
on_new_group = true             # webhook on first event of a new group
on_regression = true            # webhook when a resolved group re-opens
min_level = "error"             # "fatal" restricts event-time notifications
webhooks = ["https://hooks.example.com/..."]
```

Disable entirely with `--no-sentry-ingest`.

## What it gives you

- **Fingerprint grouping** — exceptions group on stable fingerprints
  (top-5 in-app frames + normalized message, with a client-fingerprint
  override).
- **Sampled stacktraces** — first + latest distinct variants per group,
  rotated automatically. Bounded storage regardless of event count.
- **Errors SPA page** — list with status / window / service / search
  filters; per-group detail with the sampled stacks, a 24h sparkline,
  resolve/ignore/reopen buttons, and an **Explain with AI** card.
- **Event-time notifications** — `group.created` on first event of a new
  group, `group.regressed` when a resolved group re-opens.
- **Threshold alerts** — `error_group_threshold` evaluator condition
  rides the standard alert system (cooldowns, channels, approval).
- **DSL separation** — Sentry events carry `protocol = sentry` so
  `protocol:sentry` (only) and `-protocol:sentry` (exclude) work in the
  Logs Explorer. Underlying events for one group:
  `/api/logs?filter=fingerprint:<fp>`.

## Pointing an existing SDK at central-logs

Just swap the DSN. Example for `sentry-python`:

```python
import sentry_sdk
sentry_sdk.init(dsn="http://clk_xxx@logs.example.com:8080/7")
```

That's it. Throws, exceptions, and `capture_message(...)` calls now
land in central-logs's `error_groups`.

## Next

- [Error tracking (full design) →](../ERROR_TRACKING.md)
- [Alerts →](../alerts.md) — `error_group_threshold` conditions
- [Query →](../query.md)