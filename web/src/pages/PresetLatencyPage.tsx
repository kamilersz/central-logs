import { useState, useMemo } from "react";
import { usePoll, LineChart, TimeFilterBar, timeFilterParams, defaultTimeFilter } from "../components";
import { api, type LatencyRow } from "../api";

const COLORS = ["#5b8dff", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899"];

export default function PresetLatencyPage() {
  const [tf, setTf] = useState(defaultTimeFilter());
  const params = timeFilterParams(tf);
  const [metric, setMetric] = useState<"p50" | "p95" | "p99">("p95");
  const { data: rows, error } = usePoll<LatencyRow[]>(
    () => api.latency(params),
    10000,
    [JSON.stringify(params)],
  );

  // Group by service → one line per service.
  const series = useMemo(() => {
    const byService = new Map<string, { x: number; y: number }[]>();
    for (const r of rows ?? []) {
      const list = byService.get(r.service) ?? [];
      list.push({ x: Date.parse(r.ts), y: (r[metric] as number) ?? 0 });
      byService.set(r.service, list);
    }
    return Array.from(byService.entries()).map(([name, points], i) => ({
      name,
      color: COLORS[i % COLORS.length],
      points: points.filter((p) => !Number.isNaN(p.x)),
    }));
  }, [rows, metric]);

  return (
    <div>
      <div className="flex justify-between items-center mb-4">
        <h2 className="text-xl font-semibold">Latency Percentiles</h2>
        <select
          value={metric}
          onChange={(e) => setMetric(e.target.value as "p50" | "p95" | "p99")}
          className="px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm"
        >
          <option value="p50">p50</option>
          <option value="p95">p95</option>
          <option value="p99">p99</option>
        </select>
      </div>
      <TimeFilterBar value={tf} onChange={setTf} />

      {error && <div className="mb-3 text-red-600 text-sm">{error}</div>}

      <div className="p-4 cl-card">
        <div className="text-sm font-medium mb-2">{metric} per service (ms)</div>
        <LineChart series={series} height={280} yLabel="ms" />
      </div>

      <div className="mt-4 p-4 cl-card">
        <div className="text-sm font-medium mb-2">Per-service summary</div>
        <table className="w-full text-sm">
          <thead>
            <tr className="text-tremor-content-subtle text-xs">
              <th className="text-left py-1">service</th>
              <th className="text-right py-1">latest p50</th>
              <th className="text-right py-1">latest p95</th>
              <th className="text-right py-1">latest p99</th>
            </tr>
          </thead>
          <tbody>
            {series.map((s) => {
              // Most recent (last) bucket per service — what the user
              // actually wants from a summary table, not the all-time max.
              const myRows = (rows ?? []).filter((r) => r.service === s.name);
              const latest = myRows.length > 0 ? myRows[myRows.length - 1] : null;
              return (
                <tr key={s.name} className="border-t border-tremor-border dark:border-dark-tremor-border">
                  <td className="py-1">
                    <span className="inline-block w-3 h-3 mr-2 align-middle" style={{ background: s.color }} />
                    {s.name}
                  </td>
                  <td className="py-1 text-right">{latest ? latest.p50.toFixed(0) : "—"}</td>
                  <td className="py-1 text-right">{latest ? latest.p95.toFixed(0) : "—"}</td>
                  <td className="py-1 text-right">{latest ? latest.p99.toFixed(0) : "—"}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
