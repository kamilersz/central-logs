import { useState } from "react";
import { usePoll, SeverityBadge, TimeFilterBar, timeFilterParams, defaultTimeFilter } from "../components";
import { api, type AnomalyRow } from "../api";

export default function PresetAnomaliesPage() {
  const [tf, setTf] = useState(defaultTimeFilter());
  const params = timeFilterParams(tf);
  const { data: rows, error } = usePoll<AnomalyRow[]>(
    () => api.anomalies(params),
    30000,
    [JSON.stringify(params)],
  );

  const anomalies = rows ?? [];
  const byMethod = new Map<string, number>();
  for (const a of anomalies) {
    byMethod.set(a.method, (byMethod.get(a.method) ?? 0) + 1);
  }
  const byMetric = new Map<string, number>();
  for (const a of anomalies) {
    byMetric.set(a.metric, (byMetric.get(a.metric) ?? 0) + 1);
  }

  return (
    <div>
      <h2 className="text-xl font-semibold mb-4">Anomalies</h2>
      <TimeFilterBar value={tf} onChange={setTf} />
      {tf.filter.trim() && (
        <div className="mb-3 text-xs text-tremor-content-subtle">
          Filter set — anomalies are detected on demand over the filtered series.
        </div>
      )}

      <div className="grid grid-cols-3 gap-4 mb-4">
        <Stat label="total" value={String(anomalies.length)} />
        <Stat
          label="by method"
          value={Array.from(byMethod.entries()).map(([k, v]) => `${k}: ${v}`).join("  ·  ") || "—"}
        />
        <Stat
          label="by metric"
          value={Array.from(byMetric.entries()).map(([k, v]) => `${k}: ${v}`).join("  ·  ") || "—"}
        />
      </div>

      {error && <div className="mb-3 text-red-600 text-sm">{error}</div>}

      <div className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border">
        <table className="w-full text-sm">
          <thead>
            <tr className="text-tremor-content-subtle text-xs">
              <th className="text-left py-1">time</th>
              <th className="text-left py-1">metric</th>
              <th className="text-right py-1">score</th>
              <th className="text-left py-1">method</th>
              <th className="text-left py-1">severity</th>
            </tr>
          </thead>
          <tbody>
            {anomalies.map((a, i) => (
              <tr key={i} className="border-t border-tremor-border dark:border-dark-tremor-border">
                <td className="py-1.5 text-xs text-tremor-content-subtle">
                  {new Date(a.ts).toLocaleString()}
                </td>
                <td className="py-1.5">{a.metric}</td>
                <td className="py-1.5 text-right font-mono">{a.score.toFixed(2)}</td>
                <td className="py-1.5 font-mono text-xs">{a.method}</td>
                <td className="py-1.5">
                  <SeverityBadge severity={a.severity} />
                </td>
              </tr>
            ))}
            {anomalies.length === 0 && (
              <tr>
                <td colSpan={5} className="py-4 text-center text-tremor-content-subtle">
                  No anomalies detected in this window.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border">
      <div className="text-xs text-tremor-content-subtle">{label}</div>
      <div className="text-base font-semibold mt-1 truncate" title={value}>{value}</div>
    </div>
  );
}
