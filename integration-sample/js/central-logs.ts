/**
 * central-logs integration — typed logger that batches NDJSON into
 * `POST {BASE_URL}/v1/logs` and survives transient ingest failures.
 *
 * ─── Setup (one per app) ─────────────────────────────────────────────────
 *   1. Set an admin key on the server, pinned in the server's `.env`:
 *
 *        CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -hex 24)
 *
 *      and restart the server. This is the long-lived admin credential.
 *
 *   2. Mint an insert-only key for THIS app:
 *
 *        curl -sf -X POST http://localhost:8080/v1/api-keys \
 *          -H "Authorization: Bearer $ADMIN_KEY" \
 *          -H 'Content-Type: application/json' \
 *          -d '{"name":"my-app","scopes":"insert"}'
 *
 *      The raw `clk_...` value is shown exactly once. Persist it next to
 *      your app's other secrets — `.env`, secrets manager, etc.
 *
 *   3. Set these env vars for this app:
 *
 *        CENTRAL_LOGS_URL=http://localhost:8080        # default
 *        CENTRAL_LOGS_API_KEY=clk_...                  # from step 2
 *        CENTRAL_LOGS_SERVICE=my-app                   # filter key, default "default-apps"
 *
 * ─── Usage ────────────────────────────────────────────────────────────────
 *
 *      import { log, logApi, withCentralLogging, flush } from "./central-logs.js";
 *
 *      log("info", "service started", { version: "1.2.3" });
 *      log("warn", "queue lag 12s",   { queue: "events", duration_ms: 12000 });
 *
 *      // Bun/Hono/Express-style wrapped route:
 *      const routes = withCentralLogging({
 *        "/api/health": async () => new Response("ok"),
 *        "/api/users":  async (req) => fetch(...),
 *      });
 *
 *      // Force a final flush on shutdown:
 *      process.on("SIGTERM", () => { void flush(); });
 *
 * Records are batched every FLUSH_MS (default 1000ms) or once MAX_BATCH
 * lines accumulate, whichever comes first. If the server is unreachable the
 * batch is logged to stderr and dropped (the queue is bounded by MAX_QUEUE,
 * default 5000, to avoid unbounded memory growth in a long outage).
 *
 * ─── Field schema sent to central-logs ───────────────────────────────────
 *   service      string (required) — CENTRAL_LOGS_SERVICE
 *   level        "info" | "warn" | "error"
 *   msg          string — short free-text summary
 *   ts           RFC3339 string — set automatically
 *   duration_ms  number  — auto-added by withCentralLogging
 *   status       number  — auto-added by withCentralLogging
 *   ...          anything you pass in `fields` lands in the JSON `attributes`
 *                column and becomes queryable via the filter DSL.
 *                Common typed fields (promote via --hot-attribute for
 *                zonemap-pruned scans): user_id:bigint, trace_id:varchar,
 *                request_route:varchar, status:bigint.
 */

// ── config ─────────────────────────────────────────────────────────────────
const BASE_URL = (process.env.CENTRAL_LOGS_URL ?? "http://localhost:8080").replace(/\/+$/, "");
const API_KEY = process.env.CENTRAL_LOGS_API_KEY ?? "";
const SERVICE = process.env.CENTRAL_LOGS_SERVICE ?? "default-apps";
const FLUSH_MS = Number(process.env.CENTRAL_LOGS_FLUSH_MS ?? 1000);
const MAX_BATCH = Number(process.env.CENTRAL_LOGS_MAX_BATCH ?? 100);
const MAX_QUEUE = Number(process.env.CENTRAL_LOGS_MAX_QUEUE ?? 5000);

export type Level = "info" | "warn" | "error";

export interface LogRecord {
  service: string;
  level: Level;
  msg: string;
  ts: string;
  [key: string]: unknown;
}

// ── queue + flush ──────────────────────────────────────────────────────────
// Config is re-read on every flush so callers can mutate process.env after
// importing this module (e.g. dotenv-loaded apps, test harnesses).
function readApiKey(): string {
  return process.env.CENTRAL_LOGS_API_KEY ?? "";
}

const queue: LogRecord[] = [];
let flushing = false;
let dropped = 0;

function enqueue(record: LogRecord) {
  if (!readApiKey()) {
    // Misconfiguration is loud and visible — fall back to stderr so
    // developers notice, but don't crash the app.
    console.error(JSON.stringify(record));
    return;
  }
  if (queue.length >= MAX_QUEUE) queue.shift(); // bounded: drop oldest
  queue.push(record);
  if (queue.length >= MAX_BATCH) void flush();
}

export function log(level: Level, msg: string, fields: Record<string, unknown> = {}) {
  enqueue({ service: SERVICE, level, msg, ts: new Date().toISOString(), ...fields });
}

/** Convenience for logging a single Request/Response pair (Bun, Hono, Workers).
 *  Auto-derives level from status (<400 = info, <500 = warn, else error). */
export function logApi(req: Request, fields: Record<string, unknown> = {}) {
  const status = typeof fields.status === "number" ? fields.status : 0;
  const level: Level = status >= 500 ? "error" : status >= 400 ? "warn" : "info";
  log(level, `${req.method} ${new URL(req.url).pathname}`, {
    status: status || undefined,
    ...fields,
  });
}

/** Wrap a route map to log every call with timing + status + query string. */
export function withCentralLogging(routes: Record<string, (req: Request) => Promise<Response>>) {
  for (const [path, handler] of Object.entries(routes)) {
    if (typeof handler !== "function") continue;
    routes[path] = async (req: Request) => {
      const started = Date.now();
      const query = new URL(req.url).search || undefined;
      try {
        const res = await handler(req);
        log(res.status < 400 ? "info" : "warn", `${req.method} ${path}`, {
          status: res.status,
          duration_ms: Date.now() - started,
          query,
        });
        return res;
      } catch (e: unknown) {
        const message = e instanceof Error ? e.message : String(e);
        log("error", `${req.method} ${path} failed: ${message}`, {
          status: 500,
          duration_ms: Date.now() - started,
          query,
        });
        throw e;
      }
    };
  }
  return routes;
}

/** Flush the queue. Safe to await on shutdown. */
export async function flush(): Promise<void> {
  if (flushing || queue.length === 0) return;
  const key = readApiKey();
  if (!key) {
    // Misconfigured at flush time — drain to stderr so the records aren't
    // silently held in memory.
    for (const r of queue) console.error(JSON.stringify(r));
    queue.splice(0, queue.length);
    return;
  }
  flushing = true;
  const batch = queue.splice(0, queue.length);
  const body = batch.map((r) => JSON.stringify(r)).join("\n");
  try {
    const res = await fetch(`${BASE_URL}/v1/logs`, {
      method: "POST",
      headers: {
        "Content-Type": "application/x-ndjson",
        Authorization: `Bearer ${key}`,
      },
      body,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}: ${(await res.text()).slice(0, 200)}`);
    const out = (await res.json().catch(() => ({}))) as { rejected?: number };
    if (typeof out.rejected === "number" && out.rejected > 0) {
      console.error(`[central-logs] rejected ${out.rejected}/${batch.length} records`);
    }
  } catch (e: unknown) {
    // Bounded backpressure: drop the batch on persistent failure rather
    // than letting memory grow unbounded. The original lines still go to
    // stderr so nothing is silently lost.
    dropped += batch.length;
    const message = e instanceof Error ? e.message : String(e);
    console.error(`[central-logs] flush failed (${dropped} dropped total): ${message}`);
    for (const r of batch) console.error(JSON.stringify(r));
  } finally {
    flushing = false;
  }
}

// ── plumbing ───────────────────────────────────────────────────────────────
type Timer = { unref?: () => void; [Symbol.dispose]?: () => void };
const flushTimer = setInterval(() => void flush(), FLUSH_MS) as unknown as Timer;
flushTimer.unref?.(); // don't keep the event loop alive solely for flushing
for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.once(sig, () => {
    clearInterval(flushTimer as unknown as number);
    void flush();
  });
}
