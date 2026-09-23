import { usePoll } from "../components";
import { api, type PipelineStatus } from "../api";

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
}

/**
 * Pipeline health: insert-channel fill, WAL throughput, ingest lag, and
 * drop counters. The local equivalent of a Kafka consumer-lag dashboard —
 * backpressure should be visible BEFORE logs start dropping.
 */
export default function PipelinePage() {
  const { data, error } = usePoll<PipelineStatus>(() => api.pipeline(), 5000, []);

  const fillPct = data ? Math.min(100, data.channel_fill_ratio * 100) : 0;
  const fillColor =
    fillPct > 80 ? "bg-red-500" : fillPct > 50 ? "bg-yellow-500" : "bg-emerald-500";
  const lagHigh = (data?.ingest_lag_bytes ?? 0) > 256 * 1024 * 1024;
  const drops = data?.audit_dropped_total ?? 0;

  return (
    <div>
      <div className="flex justify-between items-center mb-4">
        <h2 className="text-xl font-semibold">Pipeline</h2>
        <span className="text-xs text-tremor-content-subtle">refreshes every 5s</span>
      </div>

      {error && <div className="mb-3 text-red-600 text-sm">{error}</div>}

      <div className="grid grid-cols-2 lg:grid-cols-4 gap-4 mb-4">
        <Stat
          label="insert channel"
          value={data ? `${data.channel_depth} / ${data.channel_capacity}` : "—"}
          sub={data ? `${fillPct.toFixed(1)}% full` : undefined}
        />
        <Stat
          label="ingest lag"
          value={data ? fmtBytes(data.ingest_lag_bytes) : "—"}
          sub={lagHigh ? "falling behind" : "healthy"}
          warn={lagHigh}
        />
        <Stat
          label="WAL written (total)"
          value={data ? fmtBytes(data.wal_bytes_written_total) : "—"}
        />
        <Stat
          label="records received (total)"
          value={data ? data.records_total.toLocaleString() : "—"}
        />
      </div>

      <div className="p-4 cl-card mb-4">
        <div className="text-xs text-tremor-content-subtle mb-2">
          insert → WAL channel fill (sustained 100% means fsync can't keep up; inserts start timing out)
        </div>
        <div className="h-3 rounded-md bg-tremor-background-muted dark:bg-dark-tremor-background-muted overflow-hidden">
          <div
            className={`h-full ${fillColor} transition-all`}
            style={{ width: `${Math.max(fillPct, 1.5)}%` }}
          />
        </div>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
        <div
          className={`p-4 rounded-md border ${
            drops > 0
              ? "border-red-400 dark:border-red-600"
              : "border-tremor-border dark:border-dark-tremor-border"
          }`}
        >
          <div className="text-xs text-tremor-content-subtle">audit events dropped</div>
          <div className={`text-base font-semibold mt-1 ${drops > 0 ? "text-red-600" : ""}`}>
            {drops.toLocaleString()}
          </div>
          <div className="text-xs text-tremor-content-subtle mt-1">
            {drops > 0
              ? "WAL channel saturated — self-audit events were lost. Raise insert_channel_depth or investigate disk fsync latency."
              : "no audit loss — WAL buffering held"}
          </div>
        </div>
        <div className="p-4 cl-card">
          <div className="text-xs text-tremor-content-subtle">insert errors (total)</div>
          <div className="text-base font-semibold mt-1">
            {(data?.errors_total ?? 0).toLocaleString()}
          </div>
          <div className="text-xs text-tremor-content-subtle mt-1">
            rejected inserts: malformed payloads or backpressure timeouts
          </div>
        </div>
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  sub,
  warn,
}: {
  label: string;
  value: string;
  sub?: string;
  warn?: boolean;
}) {
  return (
    <div className="p-4 cl-card">
      <div className="text-xs text-tremor-content-subtle">{label}</div>
      <div className={`text-base font-semibold mt-1 ${warn ? "text-yellow-600" : ""}`}>{value}</div>
      {sub && <div className="text-xs text-tremor-content-subtle mt-0.5">{sub}</div>}
    </div>
  );
}
