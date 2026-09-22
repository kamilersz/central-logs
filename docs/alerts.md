# Alerts

A background evaluator re-checks every `active` rule on a fixed
interval (default 30s) against the same rollup/anomaly tables the
dashboards read, and fires webhook / email / Telegram notifications
when a condition breaches, subject to a per-rule cooldown.

The full rule grammar and HTTP endpoints live in
[HTTP API → Alert rules](API.md#alert-rules-architecture-7). This page
is the operator-facing summary.

## Lifecycle

Rules are created either:

- **From the web UI** — start `active`. The human pressing "save" is
  the approver.
- **By AI agents through the MCP `create_alert_rule` tool** — always
  land in `pending_approval`. A human must approve them via the
  dashboard before they fire. This bounds the blast radius of an agent
  silently wiring up paging loops.

State machine:

```
pending_approval ──approve──▶ active ──fire──▶ (notifies)
       │                       │
       └────reject──▶ rejected │
                               │
                  (cooldown, multi-threshold state machine)
```

| Method | Path | Purpose | Scope |
|---|---|---|---|
| GET | `/api/alert-rules` | List all rules + evaluator state | read |
| POST | `/api/alert-rules` | Create (UI: starts `active`) | admin |
| PUT | `/api/alert-rules/{id}` | Update name/condition/channels | admin |
| DELETE | `/api/alert-rules/{id}` | Delete (history kept) | admin |
| POST | `/api/alert-rules/{id}/approve` | `pending_approval` → `active` | admin |
| POST | `/api/alert-rules/{id}/reject` | → `rejected` | admin |
| GET | `/api/alert-rules/{id}/events` | Firing history (≤200 rows) | read |

## Condition types

```jsonc
// Threshold: fire when the metric's value over the trailing window
// compares to `value`.
{
  "type": "threshold",
  "metric": "volume",         // volume | error_rate | p50_latency | p95_latency | p99_latency
  "comparator": ">",         // > | >= | < | <= | ==
  "value": 100,
  "window_secs": 300,
  "cooldown_secs": 900
}

// Anomaly: fire when an `anomalies` row for this metric at or above
// min_severity landed in the window.
{
  "type": "anomaly",
  "metric": "volume",
  "min_severity": "high",    // low | medium | high | critical
  "window_secs": 300,
  "cooldown_secs": 900
}

// Count: fire when N rows matching a filter-DSL expression land in
// the window ("n matching events per period").
{
  "type": "count",
  "filter": "service:api level:error",
  "comparator": ">=",
  "count": 50,
  "window_secs": 300,
  "cooldown_secs": 900
}

// Multi-threshold (count variant): an escalation ladder. Array order
// is escalation order; the last breached entry wins. The evaluator
// tracks the rule's severity state and notifies ONLY on state changes
// — one notification per transition, never a repeat while the state
// stays the same. A recovery (severity "ok") also notifies once.
{
  "type": "count",
  "filter": "service:api level:error",
  "window_secs": 300,
  "thresholds": [
    { "severity": "warning",  "comparator": ">=", "value": 50 },
    { "severity": "critical", "comparator": ">=", "value": 200 }
  ]
}

// Error-group threshold (from error tracking): fire when N events for
// a fingerprint land in the window, optionally gated by severity.
{
  "type": "error_group_threshold",
  "fingerprint": null,       // null = any group; else one specific group
  "min_level": "error",
  "comparator": ">=",
  "value": 50,
  "window_secs": 300,
  "cooldown_secs": 900
}
```

`metric` for `threshold` and `anomaly` is one of `volume`,
`error_rate`, `p50_latency`, `p95_latency`, `p99_latency` — read from
the `rollup_1m` table. Defaults: `window_secs = 300`,
`cooldown_secs = 900`.

## Notification channels

| Type | When to use | `config` payload |
|---|---|---|
| `email` | On-call mailing lists | `{"recipients": ["ops@example.com", ...]}` (SMTP relay set in `[smtp]`) |
| `telegram` | Group chats, on-call DM | `{"bot_token": "...", "chat_id": "-100..."}` |
| `webhook` | Slack/Discord/Teams incoming webhooks, PagerDuty Events API, custom endpoints | `{"url": "https://hooks.example.com/..."}` |

| Method | Path | Purpose | Scope |
|---|---|---|---|
| GET | `/api/alert-channels` | List (secrets masked) | read |
| POST | `/api/alert-channels` | Create | admin |
| PUT | `/api/alert-channels/{id}` | Update (sending back masked token keeps it) | admin |
| DELETE | `/api/alert-channels/{id}` | Delete (referencing rules skip it) | admin |
| POST | `/api/alert-channels/{id}/test` | Send a sample notification | admin |

Bad filters, unknown channels, and bad configs are rejected at save
time — not silently at eval time.

## What gets written

Every evaluation writes one `alert_events` row per channel attempt
(`notified: false` + `error` when delivery fails; the `channel` column
names the target), and updates the rule's
`last_evaluated_at` / `last_fired_at` / `last_notify_error`. The
**Alerts** page shows rule health without re-deriving it from logs.

## Next

- [HTTP API → alert rules](API.md#alert-rules-architecture-7)
- [Error tracking → error_group_threshold](ERROR_TRACKING.md)
- [Configuration → retention / channels](CONFIGURATION.md)