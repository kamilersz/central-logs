#!/usr/bin/env python3
"""Load generator for central-logs stress testing.

Modes:
  ramp  - closed-loop concurrent sources; workers ramp up over time,
          each sending small JSON batches to POST /v1/logs as fast as the
          server acks (WAL fsync).
  bulk  - large NDJSON batches (few big requests).

Outputs a per-second CSV: ts, active_workers, requests, records, bytes,
latency samples are aggregated into p50/p95/p99 per second.

Examples:
  ./loadgen.py ramp --url http://127.0.0.1:8088 --key clk_... \
      --workers 64 --ramp-step 8 --ramp-interval 20 --duration 180 --batch-size 5
  ./loadgen.py bulk --url http://127.0.0.1:8088 --key clk_... \
      --bulk-requests 20 --bulk-rows 5000
"""

import argparse
import csv
import http.client
import json
import random
import string
import sys
import threading
import time
from urllib.parse import urlparse

SERVICES = ["api-gateway", "auth", "payments", "search", "worker-mail",
            "worker-media", "billing", "front-proxy"]
LEVELS = ["info"] * 7 + ["warn"] * 2 + ["error"]
MESSAGES = [
    "request completed route=/v1/items status=200 dur_ms={d}",
    "db query slow table=orders rows={n} dur_ms={d}",
    "cache miss key=user:{n} tier=lru",
    "payment authorized txn=tx_{n} amount={d}.{n}",
    "retry attempt={n} op=enqueue queue=emails",
    "upstream timeout host=svc-{n} dur_ms={d}",
    "session refreshed user_id={n}",
    "invoice generated id=inv_{n} total={d}",
]


def make_record(rnd, ts_ms):
    ts_ms = int(ts_ms)
    msg = rnd.choice(MESSAGES).format(n=rnd.randint(1, 999999), d=rnd.randint(1, 900))
    return {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(ts_ms / 1000))
              + f".{ts_ms % 1000:03d}Z",
        "service": rnd.choice(SERVICES),
        "level": rnd.choice(LEVELS),
        "msg": msg,
        "request_id": "req_" + "".join(rnd.choices(string.hexdigits[:16], k=12)),
        "user_id": rnd.randint(1, 100000),
        "method": rnd.choice(["GET", "POST", "PUT"]),
        "status": rnd.choice([200] * 8 + [400, 404, 500]),
        "dur_ms": round(rnd.random() * 500, 2),
        "host": f"node-{rnd.randint(1, 40)}",
    }


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = 0
        self.records = 0
        self.bytes_sent = 0
        self.errors = 0
        self.latencies = []
        self.active_workers = 0
        self.stop = False

    def add(self, reqs, recs, nbytes, lat, errs):
        with self.lock:
            self.requests += reqs
            self.records += recs
            self.bytes_sent += nbytes
            self.errors += errs
            if lat:
                self.latencies.append(lat)


def worker_loop(args, stats, rnd_seed):
    rnd = random.Random(rnd_seed)
    parsed = urlparse(args.url)
    host, port, path = parsed.hostname, parsed.port, parsed.path or "/v1/logs"
    conn = None
    while not stats.stop:
        try:
            if conn is None:
                conn = http.client.HTTPConnection(host, port, timeout=30)
                if args.auth_header:  # keep-alive warm-up against /health
                    conn.request("GET", "/health", headers={})
                    conn.getresponse().read()
            batch = [make_record(rnd, time.time() * 1000) for _ in range(args.batch_size)]
            body = json.dumps(batch).encode()
            headers = {
                "Content-Type": "application/json",
                "Content-Length": str(len(body)),
            }
            if args.key:
                headers["Authorization"] = f"Bearer {args.key}"
            t0 = time.perf_counter()
            conn.request("POST", path, body=body, headers=headers)
            resp = conn.getresponse()
            payload = resp.read()
            lat_ms = (time.perf_counter() - t0) * 1000
            ok = resp.status == 200
            accepted = 0
            if ok:
                try:
                    accepted = json.loads(payload).get("accepted", 0)
                except Exception:
                    accepted = args.batch_size
            stats.add(1, accepted if ok else 0, len(body), lat_ms, 0 if ok else 1)
            if not ok:
                conn.close()
                conn = None
                time.sleep(0.2)
        except Exception:
            stats.add(1, 0, 0, None, 1)
            try:
                if conn:
                    conn.close()
            except Exception:
                pass
            conn = None
            time.sleep(0.2)


def run_ramp(args):
    stats = Stats()
    threads = []
    active = min(args.ramp_start, args.workers)
    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["ts", "active_workers", "requests", "records",
                    "bytes", "errors", "p50_ms", "p95_ms", "p99_ms"])
        t_start = time.time()
        next_ramp = t_start + args.ramp_interval
        last = {"req": 0, "rec": 0, "b": 0, "err": 0, "t": t_start}
        for i in range(active):
            th = threading.Thread(target=worker_loop, args=(args, stats, i), daemon=True)
            th.start()
            threads.append(th)
        stats.active_workers = active
        print(f"ramp: started {active} workers -> target {args.workers}, "
              f"+{args.ramp_step}/ {args.ramp_interval}s, duration {args.duration}s", flush=True)
        while time.time() - t_start < args.duration:
            time.sleep(1)
            now = time.time()
            if (active < args.workers and now >= next_ramp):
                for i in range(active, min(active + args.ramp_step, args.workers)):
                    th = threading.Thread(target=worker_loop, args=(args, stats, i), daemon=True)
                    th.start()
                    threads.append(th)
                active = min(active + args.ramp_step, args.workers)
                stats.active_workers = active
                next_ramp = now + args.ramp_interval
                print(f"ramp: active workers = {active}", flush=True)
            with stats.lock:
                recs = stats.records
                reqs = stats.requests
                byts = stats.bytes_sent
                errs = stats.errors
                lats = sorted(stats.latencies)
                stats.latencies = []
            dt = now - last["t"]
            p50 = p95 = p99 = ""
            if lats:
                p50 = round(lats[int(len(lats) * 0.50)], 1)
                p95 = round(lats[int(len(lats) * 0.95)], 1)
                p99 = round(lats[min(int(len(lats) * 0.99), len(lats) - 1)], 1)
            w.writerow([round(now, 1), active, reqs - last["req"], recs - last["rec"],
                        byts - last["b"], errs - last["err"], p50, p95, p99])
            f.flush()
            rate = (recs - last["rec"]) / dt if dt else 0
            print(f"  t={now - t_start:6.0f}s w={active:3d} "
                  f"rate={rate:9.0f} rec/s lat p50/p95/p99={p50}/{p95}/{p99} ms "
                  f"errs={errs - last['err']}", flush=True)
            last = {"req": reqs, "rec": recs, "b": byts, "err": errs, "t": now}
        stats.stop = True
        with stats.lock:
            total = stats.records
            errs = stats.errors
    print(f"ramp done: {total} records accepted, {errs} failed requests", flush=True)
    return total


def bulk_sender(args, stats, wid, out):
    rnd = random.Random(1000 + wid)
    parsed = urlparse(args.url)
    host, port, path = parsed.hostname, parsed.port, parsed.path or "/v1/logs"
    lines_per_req = args.bulk_rows // args.bulk_concurrency
    for _ in range(args.bulk_requests):
        ts = time.time() * 1000
        body = b"".join(
            json.dumps(make_record(rnd, ts + i)).encode() + b"\n"
            for i in range(lines_per_req)
        )
        headers = {"Content-Type": "application/x-ndjson",
                   "Content-Length": str(len(body))}
        if args.key:
            headers["Authorization"] = f"Bearer {args.key}"
        conn = http.client.HTTPConnection(host, port, timeout=120)
        t0 = time.perf_counter()
        try:
            conn.request("POST", path, body=body, headers=headers)
            resp = conn.getresponse()
            payload = resp.read()
            lat = (time.perf_counter() - t0) * 1000
            accepted = 0
            if resp.status == 200:
                try:
                    accepted = json.loads(payload).get("accepted", 0)
                except Exception:
                    accepted = lines_per_req
            stats.add(1, accepted, len(body), lat, 0 if resp.status == 200 else 1)
            out.write(f"  bulk w{wid}: {accepted}/{lines_per_req} rows in {lat:.0f} ms "
                      f"({accepted / lat * 1000:.0f} rec/s)\n")
        except Exception as e:
            stats.add(1, 0, len(body), None, 1)
            out.write(f"  bulk w{wid}: FAILED {e}\n")
        finally:
            conn.close()


def run_bulk(args):
    stats = Stats()
    import io
    out_buf = io.StringIO()
    threads = []
    t0 = time.time()
    for i in range(args.bulk_concurrency):
        th = threading.Thread(target=bulk_sender, args=(args, stats, i, out_buf), daemon=True)
        th.start()
        threads.append(th)
    for th in threads:
        th.join()
    dur = time.time() - t0
    out_buf.seek(0)
    print(out_buf.read(), flush=True)
    print(f"bulk done: {stats.records} rows in {dur:.1f}s = "
          f"{stats.records / dur:.0f} rec/s overall", flush=True)
    return stats.records


def main():
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["ramp", "bulk"])
    p.add_argument("--url", default="http://127.0.0.1:8088")
    p.add_argument("--key", default="")
    p.add_argument("--out", default="stress_ramp.csv")
    p.add_argument("--duration", type=int, default=180)
    p.add_argument("--workers", type=int, default=64)
    p.add_argument("--ramp-start", type=int, default=8)
    p.add_argument("--ramp-step", type=int, default=8)
    p.add_argument("--ramp-interval", type=int, default=20)
    p.add_argument("--batch-size", type=int, default=5)
    p.add_argument("--bulk-requests", type=int, default=20)
    p.add_argument("--bulk-rows", type=int, default=5000)
    p.add_argument("--bulk-concurrency", type=int, default=4)
    p.add_argument("--no-warmup", dest="auth_header", action="store_false")
    a = p.parse_args()
    if a.mode == "ramp":
        run_ramp(a)
    else:
        run_bulk(a)


if __name__ == "__main__":
    sys.exit(main())
