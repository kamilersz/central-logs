#!/usr/bin/env python3
"""central-logs integration client — batched NDJSON inserts.

Setup (see INTEGRATION.md):
    1. Admin key pinned in the server's .env (CENTRAL_LOGS_HTTP_API_KEY).
    2. Mint an insert-only key:
         curl -sf -X POST http://localhost:8080/v1/api-keys \\
           -H "Authorization: Bearer $ADMIN_KEY" \\
           -H 'Content-Type: application/json' \\
           -d '{"name":"my-app","scopes":"insert"}'
    3. Export for this app:
         CENTRAL_LOGS_URL      (default http://localhost:8080)
         CENTRAL_LOGS_API_KEY  (the raw clk_... value — required)
         CENTRAL_LOGS_SERVICE  (default "default-app")

Usage:
    from central_logs import log          # shared instance
    log("info", "app started", version="1.2.3")
    log("warn", "queue lag 12s", queue="events", duration_ms=12000)

    # or an explicitly configured instance
    from central_logs import CentralLogs
    cl = CentralLogs(service="payments")
    cl.log("error", "gateway timeout", duration_ms=3000)
    cl.close()                            # final flush

Env vars are re-read lazily so dotenv-style imports work. On persistent
ingest failure batches go to stderr (nothing silently vanishes); the queue
is bounded so a long outage cannot exhaust memory.
"""

from __future__ import annotations

import atexit
import json
import os
import sys
import threading
from datetime import datetime, timezone
from typing import Any
from urllib import error as urlerror
from urllib import request as urlrequest

_LEVELS = ("debug", "info", "warn", "error", "fatal")


class CentralLogs:
    def __init__(
        self,
        url: str | None = None,
        api_key: str | None = None,
        service: str | None = None,
        flush_ms: int | None = None,
        max_batch: int | None = None,
        max_queue: int | None = None,
    ) -> None:
        self._url = (url or os.environ.get("CENTRAL_LOGS_URL") or "http://localhost:8080").rstrip("/")
        self._api_key = api_key if api_key is not None else os.environ.get("CENTRAL_LOGS_API_KEY", "")
        self._service = service or os.environ.get("CENTRAL_LOGS_SERVICE", "default-app")
        self._flush_ms = flush_ms or int(os.environ.get("CENTRAL_LOGS_FLUSH_MS", "1000"))
        self._max_batch = max_batch or int(os.environ.get("CENTRAL_LOGS_MAX_BATCH", "100"))
        self._max_queue = max_queue or int(os.environ.get("CENTRAL_LOGS_MAX_QUEUE", "5000"))

        self._queue: list[dict[str, Any]] = []
        self._lock = threading.Lock()
        self._dropped = 0
        self._closed = False

        self._wake = threading.Event()
        self._worker = threading.Thread(target=self._run, name="central-logs", daemon=True)
        self._worker.start()
        atexit.register(self.close)

    # ── public API ─────────────────────────────────────────────────────────
    def log(self, level: str, msg: str, **fields: Any) -> None:
        """Queue one record. `fields` become queryable attributes."""
        level = level.lower()
        if level not in _LEVELS:
            level = "info"
        record = {
            "service": self._service,
            "level": level,
            "msg": msg,
            "ts": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            **fields,
        }
        with self._lock:
            if len(self._queue) >= self._max_queue:
                self._queue.pop(0)  # bounded: drop oldest
            self._queue.append(record)
            full = len(self._queue) >= self._max_batch
        if full:
            self.flush()

    def flush(self, timeout: float = 5.0) -> None:
        """Send the queued batch now. Safe to call concurrently."""
        batch: list[dict[str, Any]] = []
        with self._lock:
            if self._queue:
                batch, self._queue = self._queue[:], []
        if not batch:
            return
        if not self._api_key:
            self._drop(batch, "CENTRAL_LOGS_API_KEY not set")
            return
        body = "\n".join(json.dumps(r, default=str) for r in batch)
        try:
            req = urlrequest.Request(
                f"{self._url}/v1/logs",
                data=body.encode("utf-8"),
                method="POST",
                headers={
                    "Content-Type": "application/x-ndjson",
                    "Authorization": f"Bearer {self._api_key}",
                },
            )
            with urlrequest.urlopen(req, timeout=timeout) as resp:
                out = json.loads(resp.read().decode("utf-8") or "{}")
                rejected = out.get("rejected", 0)
                if rejected:
                    print(f"[central-logs] rejected {rejected}/{len(batch)} records", file=sys.stderr)
        except urlerror.HTTPError as e:
            self._drop(batch, f"HTTP {e.code}: {e.read()[:200]!r}")
        except Exception as e:  # noqa: BLE001 — network layer, best-effort
            self._drop(batch, str(e))

    def close(self) -> None:
        """Stop the worker and flush whatever is left."""
        if self._closed:
            return
        self._closed = True
        self._wake.set()
        self._worker.join(timeout=3)
        self.flush()

    # ── internals ──────────────────────────────────────────────────────────
    def _drop(self, batch: list[dict[str, Any]], reason: str) -> None:
        self._dropped += len(batch)
        print(f"[central-logs] flush failed ({self._dropped} dropped total): {reason}", file=sys.stderr)
        for r in batch:
            print(json.dumps(r, default=str), file=sys.stderr)

    def _run(self) -> None:
        while not self._closed:
            self._wake.wait(self._flush_ms / 1000.0)
            self._wake.clear()
            if self._closed:
                break
            self.flush()


# Module-level shared instance + convenience function.
_default = CentralLogs()


def log(level: str, msg: str, **fields: Any) -> None:
    _default.log(level, msg, **fields)


if __name__ == "__main__":
    # Smoke test:  CENTRAL_LOGS_API_KEY=... python3 central_logs.py
    log("info", "central-logs python client smoke test", env="test")
    _default.flush()
    print("flushed; verify with: curl '.../api/logs?filter=service:<service>&window=5m'")
