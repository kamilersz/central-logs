import { useState } from "react";
import { Button, Card } from "@tremor/react";
import { usePoll } from "../components";
import { api, type BackupRun, type ColdDateRow } from "../api";

function fmtBytes(n: number | null | undefined): string {
  const v = n ?? 0;
  if (v < 1024) return `${v} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let x = v / 1024;
  let i = 0;
  while (x >= 1024 && i < units.length - 1) {
    x /= 1024;
    i++;
  }
  return `${x.toFixed(1)} ${units[i]}`;
}

function fmtNum(n: number | null | undefined): string {
  return (n ?? 0).toLocaleString();
}

function relTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  const s = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <Card>
      <div className="cl-stat-label">{label}</div>
      <div className="text-lg font-semibold cl-mono text-tremor-content-strong dark:text-dark-tremor-content-strong mt-1">
        {value}
      </div>
      {sub && (
        <div className="text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-0.5">
          {sub}
        </div>
      )}
    </Card>
  );
}

function StatusPill({ status }: { status: string | null }) {
  const cls =
    status === "ok"
      ? "bg-emerald-400/15 text-emerald-600 dark:text-emerald-300"
      : status === "error"
        ? "bg-red-400/15 text-red-600 dark:text-red-300"
        : "bg-amber-400/15 text-amber-600 dark:text-amber-300";
  return (
    <span className={`cl-pill ${cls}`}>
      <span className="cl-pill-dot" />
      {status ?? "running"}
    </span>
  );
}

export default function StoragePage() {
  const isAdmin = document.cookie.includes("cl_session");
  const { data, error, loading, refresh } = usePoll(
    () => api.storageOverview(),
    30000,
  );
  const { data: cold, refresh: refreshCold } = usePoll(
    () => api.storageCold(),
    60000,
  );
  const [busy, setBusy] = useState(false);
  const [actionMsg, setActionMsg] = useState<string | null>(null);
  const [restoreFrom, setRestoreFrom] = useState("");
  const [restoreInto, setRestoreInto] = useState("");
  const [confirmed, setConfirmed] = useState(false);

  async function runBackup() {
    setBusy(true);
    setActionMsg(null);
    try {
      await api.runBackup();
      setActionMsg("Backup started — refresh in a few seconds.");
      setTimeout(() => {
        refresh();
        refreshCold();
      }, 3000);
    } catch (e) {
      setActionMsg(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function runRestore() {
    if (!confirmed) return;
    setBusy(true);
    setActionMsg(null);
    try {
      const r = await api.restore(restoreFrom.trim(), restoreInto.trim());
      setActionMsg(`Restored into ${r.restored_into} — ${r.next}`);
    } catch (e) {
      setActionMsg(String(e));
    } finally {
      setBusy(false);
      setConfirmed(false);
    }
  }

  const dates: ColdDateRow[] = cold?.dates ?? [];
  const runs: BackupRun[] = data?.backups ?? [];

  return (
    <div>
      <h2 className="cl-title">Storage & backups</h2>
      <p className="cl-subtitle text-sm mb-4">
        Hot/cold footprint, retention bounds, and snapshot housekeeping.
      </p>

      {error && <div className="cl-error-banner mb-4">{error}</div>}
      {actionMsg && (
        <div className="mb-4 rounded-md border border-tremor-border dark:border-dark-tremor-border p-3 text-sm text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis">
          {actionMsg}
        </div>
      )}
      {loading && <div className="text-tremor-content-subtle text-sm mb-3">loading…</div>}

      {/* Header stats */}
      <div className="grid grid-cols-2 xl:grid-cols-4 gap-4 mb-6">
        <Stat label="hot rows" value={fmtNum(data?.hot_rows)} sub={`oldest ${relTime(data?.oldest_hot_ts)}`} />
        <Stat label="cold parquet" value={fmtBytes(data?.cold_parquet_bytes)} sub={`${fmtNum(data?.cold_parquet_files)} files`} />
        <Stat label="wal directory" value={fmtBytes(data?.wal_bytes)} sub={data?.retention.wal_max_bytes ? `cap ${fmtBytes(data.retention.wal_max_bytes)}` : "no cap"} />
        <Stat label="error groups" value={fmtNum(data?.error_groups)} sub={`${fmtNum(data?.alerts_active)} active alert rules`} />
      </div>

      <div className="grid grid-cols-1 xl:grid-cols-3 gap-4 items-start">
        {/* Left: cold breakdown + backups */}
        <div className="xl:col-span-2 space-y-4">
          <Card>
            <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
              cold partitions by day
            </h3>
            {dates.length === 0 ? (
              <div className="cl-empty text-sm text-tremor-content-subtle">
                nothing compacted yet — the hot tier is young
              </div>
            ) : (
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-left text-[11px] uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                    <th className="pb-2">day</th>
                    <th className="pb-2 text-right">rows</th>
                    <th className="pb-2 text-right">files</th>
                    <th className="pb-2 text-right">on disk</th>
                  </tr>
                </thead>
                <tbody>
                  {dates.map((d) => (
                    <tr key={d.date ?? "?"} className="border-t border-tremor-border dark:border-dark-tremor-border">
                      <td className="py-1.5 cl-mono text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis">{d.date}</td>
                      <td className="py-1.5 text-right cl-mono">{fmtNum(d.rows)}</td>
                      <td className="py-1.5 text-right cl-mono">{fmtNum(d.files)}</td>
                      <td className="py-1.5 text-right cl-mono">{fmtBytes(d.bytes)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </Card>

          <Card>
            <div className="flex items-center justify-between mb-3">
              <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                backup runs
              </h3>
              {isAdmin && (
                <Button size="xs" loading={busy} onClick={runBackup}>
                  Run backup now
                </Button>
              )}
            </div>
            {runs.length === 0 ? (
              <div className="cl-empty text-sm text-tremor-content-subtle">
                no snapshots yet — schedule one in central-logs.toml ([backup]) or run one now
              </div>
            ) : (
              <div className="space-y-1.5">
                {runs.map((r) => (
                  <div
                    key={r.id}
                    className="flex items-center gap-3 text-sm border border-tremor-border dark:border-dark-tremor-border rounded-md px-3 py-2"
                  >
                    <StatusPill status={r.status} />
                    <span className="cl-mono text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle w-28">
                      {relTime(r.started_at)}
                    </span>
                    <span className="cl-mono text-xs w-20">{r.trigger}</span>
                    <span className="cl-mono text-xs">{fmtBytes(r.bytes)}</span>
                    <span className="flex-1 truncate text-xs cl-mono text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                      {r.error ?? r.remote_key ?? r.local_path ?? ""}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </Card>
        </div>

        {/* Right: retention + restore */}
        <div className="space-y-4">
          <Card>
            <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
              retention policy
            </h3>
            <dl className="text-sm space-y-2">
              <div className="flex justify-between">
                <dt className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle">hot max age</dt>
                <dd className="cl-mono">
                  {data ? `${Math.round((data.retention.hot_max_age_secs ?? 0) / 86400)}d` : "—"}
                </dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle">hot max size</dt>
                <dd className="cl-mono">{data?.retention.hot_max_bytes ? fmtBytes(data.retention.hot_max_bytes) : "off"}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle">cold retention</dt>
                <dd className="cl-mono">{data ? `${data.retention.cold_retention_days}d` : "—"}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle">service rules</dt>
                <dd className="cl-mono">{fmtNum(data?.retention.service_rules)}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle">wal cap</dt>
                <dd className="cl-mono">{data?.retention.wal_max_bytes ? fmtBytes(data.retention.wal_max_bytes) : "off"}</dd>
              </div>
            </dl>
          </Card>

          {isAdmin && (
            <Card>
              <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
                restore a snapshot
              </h3>
              <p className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
                Extracts a snapshot into a fresh directory (never the live one). Point the
                instance at it with <code>--data-dir</code> to serve.
              </p>
              <input
                value={restoreFrom}
                onChange={(e) => setRestoreFrom(e.target.value)}
                placeholder="/path/backup.tar.gz or s3://bucket/key"
                className="w-full mb-2 px-3 py-2 text-sm rounded-md border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background text-tremor-content dark:text-dark-tremor-content focus:outline-none focus:ring-2 focus:ring-tremor-brand"
              />
              <input
                value={restoreInto}
                onChange={(e) => setRestoreInto(e.target.value)}
                placeholder="target dir (must be empty / new)"
                className="w-full mb-2 px-3 py-2 text-sm rounded-md border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background text-tremor-content dark:text-dark-tremor-content focus:outline-none focus:ring-2 focus:ring-tremor-brand"
              />
              <label className="flex items-center gap-2 text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
                <input type="checkbox" checked={confirmed} onChange={(e) => setConfirmed(e.target.checked)} />
                I understand this will write the snapshot into the target directory.
              </label>
              <Button
                size="xs"
                variant="secondary"
                loading={busy}
                disabled={!confirmed || !restoreFrom.trim() || !restoreInto.trim()}
                onClick={runRestore}
              >
                Restore
              </Button>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}
