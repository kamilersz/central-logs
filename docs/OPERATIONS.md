# Operations

Reference deployment: a `central-logs.service` user-level systemd unit, HTTP
on `:8080` and syslog UDP+TCP on `:5140` by default. The admin credential is
a single env var (`CENTRAL_LOGS_HTTP_API_KEY`) loaded via `EnvironmentFile=`
(see [SECURITY.md](SECURITY.md)).

## Running as a systemd service

A user-level systemd unit is included as a reference (edit the hot-attribute
list and `WorkingDirectory` to taste):

```bash
# Build the SPA (rust-embed bakes it into the release binary)
cd web && npm install && npm run build && cd ..
cargo build --release

# Install the unit
mkdir -p ~/.config/systemd/user
cp deploy/central-logs.service ~/.config/systemd/user/

# Generate and persist the admin credential
echo "CENTRAL_LOGS_HTTP_API_KEY=clk_$(openssl rand -base64 36 | tr '+/' '-_' | tr -d =)" > .env
chmod 600 .env

systemctl --user daemon-reload
systemctl --user enable --now central-logs
systemctl --user status central-logs
```

The unit uses `./.env` as `EnvironmentFile` and runs as a lingering user
unit, so it survives logout/reboot.

Add or remove `--hot-attribute` flags in the unit to match your own services.

## Day-to-day

```bash
# Status / restart
systemctl --user status central-logs
systemctl --user restart central-logs

# Tail logs
journalctl --user -u central-logs -f

# Verify health + store state
curl http://localhost:8080/health
curl -s -H "Authorization: Bearer $(grep ^CENTRAL_LOGS_HTTP_API_KEY= .env | cut -d= -f2-)" \
  http://localhost:8080/api/store | jq .

# Spawn a scoped key for an external inserter (returns the raw value once)
ADMIN=$(grep ^CENTRAL_LOGS_HTTP_API_KEY= .env | cut -d= -f2-)
curl -s -X POST http://localhost:8080/v1/api-keys \
  -H "Authorization: Bearer $ADMIN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","scopes":"insert"}'
```

DuckDB data lives in `./data/` (WAL segments, `central.duckdb` + `.wal`,
`meta.redb`, `parquet/` for the cold tier). Snapshots are simply `tar` of
that directory when the service is stopped. **Don't `rm` `central.duckdb.wal`
while the process is running** — but since the service now checkpoints on
shutdown, you should never need to.

## Housekeeping: backups, restore, storage

### Backups

Automatic (configure `[backup]` in the TOML — timezone-aware daily schedule
or a size trigger), or on demand:

```bash
# UI: Storage page → "Run backup now" (admin)
curl -X POST -H "Authorization: Bearer $ADMIN" http://localhost:8080/api/ops/backup
curl -H "Authorization: Bearer $ADMIN" http://localhost:8080/api/ops/backups
```

Each snapshot is a tar.gz of `data/` (DuckDB CHECKPOINTed first) plus a
`manifest.json` sibling carrying the archive's sha256. Remote copies land at
`s3://<bucket>/<prefix>/<instance_id>/<date>/backup.tar.gz`.

CLI one-shot (works even with `backup.enabled = false`):

```bash
./target/release/central-logs --backup-now ...   # snapshot at startup, then serve
```

### Restore

Restore targets a FRESH directory — never the live data dir:

```bash
# while the server is stopped:
./target/release/central-logs --data-dir ./data-restored \
  --restore-from backups/gitar-p720-8084-20260915.tar.gz
# remote:
./target/release/central-logs --data-dir ./data-restored \
  --restore-from s3://cl-backups/central-logs/backups/gitar-p720-8084/2026-09-15/backup.tar.gz
# then point the unit at the restored dir (or copy it into place)
```

Or from the Storage page (admin): fill in the archive path + target dir,
tick the confirmation, Restore. The checksum is verified against the
manifest before anything is written.

### WAL cap

Set `retention.wal_max_bytes = "20GB"`: a monitor pauses ingest (503 on
insert routes) when the WAL directory crosses the cap, and resumes when
compaction shrinks it. Prevents a burst from filling the disk.

## Shipper

`central-logs` ingests anything speaking HTTP NDJSON to `POST /v1/logs`.
Anything that can read `docker logs --follow` / `journalctl -f` / a log file
can ship in.

Minimal Node-sidecar pattern (zero dependencies, autodiscover every 20s,
batch every 2s, bounded backpressure):

```javascript
// central-logs-shipper.mjs — generic tailer → NDJSON ingest
import { spawn } from "node:child_process";

const BASE = process.env.CENTRAL_LOGS_URL ?? "http://localhost:8084";
const KEY  = process.env.CENTRAL_LOGS_API_KEY;
const PATTERN = new RegExp(process.env.CONTAINER_PATTERN ?? ".");

const queue = [];
const flush = async () => {
  if (!queue.length) return;
  const batch = queue.splice(0, queue.length);
  try {
    await fetch(`${BASE}/v1/logs`, {
      method: "POST",
      headers: { "Content-Type": "application/x-ndjson", Authorization: `Bearer ${KEY}` },
      body: batch.map((r) => JSON.stringify(r)).join("\n"),
    });
  } catch (e) { queue.unshift(...batch); console.error("flush failed", e); }
};
setInterval(flush, 2000);

const list = () => new Promise((res) => {
  const p = spawn("docker", ["ps", "--format", "{{.Names}}"]);
  let out = ""; p.stdout.on("data", (c) => (out += c));
  p.on("exit", () => res(out.split("\n").map((s) => s.trim()).filter(PATTERN.test.bind(PATTERN))));
});

const tails = new Map();
const startTail = (name) => {
  if (tails.has(name)) return;
  const c = spawn("docker", ["logs", "--follow", "--timestamps", name]);
  tails.set(name, c);
  let buf = "";
  c.stdout.on("data", (chunk) => {
    buf += chunk;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      const m = line.match(/^(\S+) /);                       // docker timestamp
      const ts = m && !Number.isNaN(Date.parse(m[1])) ? m[1] : new Date().toISOString();
      queue.push({ service: name, level: "info", msg: m ? line.slice(m[0].length) : line, ts });
    }
  });
};
setInterval(async () => {
  for (const n of await list()) startTail(n);
}, 20000);
```

Generalize the `record` shape — host-attached services should set
`service`/`level`/`msg`/`ts`, raw text is wrapped automatically. Run the
script as a sibling user systemd unit
(`~/.config/systemd/user/central-logs-shipper.service`) for the same
auto-restart you give the server.
