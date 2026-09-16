#!/usr/bin/env python3
"""E2E test: real Sentry SDK → central-logs error tracking.

Starts a webhook sink, drives the installed `sentry-sdk` against a running
central-logs server, and verifies grouping, stacktrace samples, the DSL
bridge, resolve/regression, event-time notifications, and the threshold alert
condition. Requires the server already running (auth disabled) on BASE.
"""

import json
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

import sentry_sdk

BASE = "http://127.0.0.1:18114"
SINK_PORT = 18115
ADMIN = "e2e-static-admin"  # static admin key the server was started with
received: list = []


class Sink(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n)
        try:
            received.append(json.loads(body))
        except Exception:
            received.append({"raw": body.decode(errors="replace")})
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *a):
        pass


sink = HTTPServer(("127.0.0.1", SINK_PORT), Sink)
threading.Thread(target=sink.serve_forever, daemon=True).start()


def api(path, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {ADMIN}"},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def wait_for(fn, timeout=40, what="condition"):
    deadline = time.time() + timeout
    last_err = None
    while time.time() < deadline:
        try:
            if fn():
                return True
        except Exception as e:  # noqa: BLE001
            last_err = e
        time.sleep(0.4)
    raise AssertionError(f"timeout waiting for {what} (last error: {last_err})")


def crash_line(value: str):
    """Single source line so identical-shaped errors share frames."""
    raise ValueError(value)


# ── 1. Auth flow: static admin mints an insert-scoped key for the DSN ──
dsn_key = api(
    "/v1/api-keys",
    method="POST",
    body={"name": "sentry-dsn-e2e", "scopes": "insert"},
)["key"]

# Sentry SDK ingest: exceptions + message (numeric project id per SDK rules,
# mapped to `checkout` via [error_tracking.projects]).
sentry_sdk.init(
    dsn=f"http://{dsn_key}@127.0.0.1:{BASE.split(':')[-1]}/7",
    traces_sample_rate=0.0,
    default_integrations=False,
    auto_enabling_integrations=False,
)

for user in ("4811", "9999", "7"):  # same shape, different values → ONE group
    try:
        crash_line(f"user {user} not found")
    except ValueError:
        sentry_sdk.capture_exception()

sentry_sdk.capture_message("disk almost full on /dev/sda1", level="warning")
sentry_sdk.flush(timeout=10)

wait_for(
    lambda: api("/api/error-groups?status=all")["total"] >= 1,
    what="the error group to appear",
)
groups = api("/api/error-groups?status=all")["groups"]
val_groups = [g for g in groups if g["exception_type"] == "ValueError"]
assert val_groups, f"no ValueError group in {groups}"
val_group = val_groups[0]
assert val_group["service"] == "checkout", val_group
assert val_group["total_count"] == 3, f"expected 3 grouped events, got {val_group}"
print("PASS  sentry ingest + grouping by normalized message (3 events → 1 group)")
print("PASS  DSN sentry_key = insert-scoped API key (auth middleware accepted it)")

# capture_message with level=warning stays a plain log row (groups are
# error/fatal only), but the level normalization warning→warn is visible.
warn_rows = api('/api/logs?filter=level:warn&window=1h')["rows"]
assert any("disk almost full" in (r["message"] or "") for r in warn_rows), warn_rows
print("PASS  capture_message ingested; sentry 'warning' normalized to 'warn'")

# ── 2. Detail: stacktrace samples + sparkline ──────────────────────────
fp = val_group["fingerprint"]
detail = api(f"/api/error-groups/{fp}")
assert detail["samples"], "no samples on group"
stacks = [s["stack"] or "" for s in detail["samples"]]
assert any("ValueError" in s and "crash_line" in s for s in stacks), stacks
assert len(detail["samples"]) == 1, "identical shape → exactly one variant"
print("PASS  stacktrace sample captured on group (frames present, deduped)")

# ── 3. Filter-DSL bridge ───────────────────────────────────────────────
rows = api(f"/api/logs?filter=fingerprint:{fp}&window=1h")["rows"]
assert len(rows) == 3, f"expected 3 rows via DSL, got {len(rows)}"
assert rows[0]["attributes"]["exception_type"] == "ValueError"
print("PASS  filter DSL: fingerprint:<fp> returns the grouped events")

# ── 3b. Separation: sentry-format events carry protocol 'sentry' ──────
sentry_rows = api("/api/logs?filter=protocol:sentry&window=1h")["rows"]
assert len(sentry_rows) == 4, f"expected 4 sentry rows, got {len(sentry_rows)}"
assert all(r["protocol"] == "sentry" for r in sentry_rows)
plain_rows = api("/api/logs?filter=-protocol:sentry&window=1h")["rows"]
assert plain_rows and all(r["protocol"] != "sentry" for r in plain_rows)
print("PASS  sentry-format events marked protocol=sentry; regular logs untouched")

# ── 4. Resolve → regression + event-time webhook ───────────────────────
api(f"/api/error-groups/{fp}/resolve", method="POST", body={})
assert api(f"/api/error-groups/{fp}")["status"] == "resolved"
try:
    crash_line("user 1234 not found")
except ValueError:
    sentry_sdk.capture_exception()
sentry_sdk.flush(timeout=10)
wait_for(
    lambda: api(f"/api/error-groups/{fp}")["status"] == "unresolved",
    what="regression to re-open the group",
)
assert api(f"/api/error-groups/{fp}")["total_count"] == 4
wait_for(
    lambda: any(r.get("kind") == "group.created" for r in received),
    what="group.created webhook",
)
wait_for(
    lambda: any(r.get("kind") == "group.regressed" for r in received),
    what="group.regressed webhook",
)
regressed = [r for r in received if r.get("kind") == "group.regressed"][0]
assert regressed["fingerprint"] == fp and regressed["service"] == "checkout"
print("PASS  resolve + regression re-opened the group; created/regressed webhooks fired")

# ── 5. error_group_threshold alert condition ───────────────────────────
channel = api(
    "/api/alert-channels",
    method="POST",
    body={"name": "e2e-sink", "type": "webhook", "config": {"url": f"http://127.0.0.1:{SINK_PORT}/hook"}},
)
rule = api(
    "/api/alert-rules",
    method="POST",
    body={
        "name": "e2e-group-threshold",
        "condition": {
            "type": "error_group_threshold",
            "fingerprint": fp,
            "value": 4,
            "window_secs": 3600,
            "cooldown_secs": 900,
        },
        "channels": [channel["id"]],
    },
)
assert rule["status"] == "active", rule
wait_for(
    lambda: any(
        r.get("kind") is None and "e2e-group-threshold" in json.dumps(r)
        for r in received
    ),
    what="threshold alert webhook",
)
print("PASS  error_group_threshold rule fired through the evaluator to the webhook")

# ── 6. AI explain (LLM off → graceful hint) ────────────────────────────
expl = api(f"/api/error-groups/{fp}/explain", method="POST", body={})
assert expl["provider"] == "off" and expl.get("hint"), expl
print("PASS  AI explain degrades gracefully with LLM off")

# ── 7. List filters ────────────────────────────────────────────────────
only_val = api("/api/error-groups?status=all&q=ValueError")["groups"]
assert only_val and all("ValueError" in (g["title"] or "") or g["exception_type"] == "ValueError" for g in only_val)
resolved = api("/api/error-groups?status=resolved")["groups"]
assert all(g["status"] == "resolved" for g in resolved)
print("PASS  list search (q=) + status filters")

print("\nALL E2E CHECKS PASSED")
