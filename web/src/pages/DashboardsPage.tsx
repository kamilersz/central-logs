import { Link } from "react-router-dom";
import { usePoll } from "../components";
import { api, type DashboardConfig } from "../api";

const presets = [
  { path: "preset/volume", title: "Volume + Forecast", desc: "Records per bucket with a future MSTL projection overlay" },
  { path: "preset/error-rate", title: "Error Rate", desc: "Per-bucket error count + percentage, custom time range, any filter" },
  { path: "preset/latency", title: "Latency Percentiles", desc: "p50/p95/p99 per service, hour or minute buckets, custom range" },
  { path: "preset/anomalies", title: "Anomalies", desc: "Recent flagged anomalies with severity — or detect on demand over any filter" },
];

export default function DashboardsPage() {
  const { data: dashboards, error } = usePoll<DashboardConfig[]>(
    () => api.listDashboards(),
    30000,
  );

  return (
    <div>
      <div className="flex justify-between items-center mb-6">
        <div>
          <h2 className="text-xl font-semibold">Dashboards</h2>
          <p className="text-sm text-tremor-content-subtle">Built-in presets + your saved dashboards</p>
        </div>
        <div className="flex gap-2">
          <Link
            to="/dashboards/ai"
            className="px-3 py-1.5 rounded border border-tremor-border dark:border-dark-tremor-border text-sm hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
          >
            ✦ Build with AI
          </Link>
          <Link
            to="/dashboards/new"
            className="px-3 py-1.5 rounded bg-tremor-brand text-white text-sm hover:opacity-90"
          >
            + New Dashboard
          </Link>
        </div>
      </div>

      <h3 className="text-sm font-medium mb-2 text-tremor-content-subtle">Presets</h3>
      <div className="grid grid-cols-2 gap-3 mb-8">
        {presets.map((p) => (
          <Link
            key={p.path}
            to={`/dashboards/${p.path}`}
            className="block p-4 rounded border border-tremor-border dark:border-dark-tremor-border hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
          >
            <div className="font-medium">{p.title}</div>
            <div className="text-xs text-tremor-content-subtle mt-1">{p.desc}</div>
          </Link>
        ))}
      </div>

      <h3 className="text-sm font-medium mb-2 text-tremor-content-subtle">Your dashboards</h3>
      {error && <div className="text-red-600 text-sm">{error}</div>}
      <div className="grid grid-cols-2 gap-3">
        {(dashboards ?? []).map((d) => (
          <Link
            key={d.id}
            to={`/dashboards/view/${d.id}`}
            className="block p-4 rounded border border-tremor-border dark:border-dark-tremor-border hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
          >
            <div className="font-medium">{d.name}</div>
            {d.description && (
              <div className="text-xs text-tremor-content-subtle mt-1">{d.description}</div>
            )}
            <div className="text-xs text-tremor-content-subtle mt-2">
              {(d.panels as unknown[]).length} panels · updated {new Date(d.updated_at).toLocaleString()}
            </div>
          </Link>
        ))}
        {!error && (dashboards ?? []).length === 0 && (
          <div className="text-sm text-tremor-content-subtle col-span-2">
            No saved dashboards yet. <Link to="/dashboards/new" className="underline">Create one →</Link>
          </div>
        )}
      </div>
    </div>
  );
}
