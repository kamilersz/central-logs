import { useState } from "react";
import { usePoll, LineChart, AreaChart, toPoints, TimeFilterBar, timeFilterParams, defaultTimeFilter } from "../components";
import { api, type ErrorRateRow } from "../api";

export default function PresetErrorRatePage() {
  const [tf, setTf] = useState(defaultTimeFilter());
  const params = timeFilterParams(tf);
  const { data: rows, error } = usePoll<ErrorRateRow[]>(
    () => api.errorRate(params),
    10000,
    [JSON.stringify(params)],
  );

  const errPoints = toPoints(rows ?? [], "ts", "errors");
  const ratePoints = toPoints(rows ?? [], "ts", "rate").map((p) => ({ ...p, y: p.y * 100 }));
  const total = (rows ?? []).reduce((s, r) => s + r.total, 0);
  const errors = (rows ?? []).reduce((s, r) => s + r.errors, 0);
  const overall = total > 0 ? (errors / total) * 100 : 0;

  return (
    <div>
      <h2 className="text-xl font-semibold mb-4">Error Rate</h2>
      <TimeFilterBar value={tf} onChange={setTf} />

      <div className="grid grid-cols-3 gap-4 mb-4">
        <Stat label="total errors" value={errors.toLocaleString()} color="text-red-600" />
        <Stat label="total records" value={total.toLocaleString()} />
        <Stat label="overall error rate" value={`${overall.toFixed(2)}%`} color={overall > 5 ? "text-red-600" : ""} />
      </div>

      {error && <div className="mb-3 text-red-600 text-sm">{error}</div>}

      <div className="p-4 cl-card mb-4">
        <div className="text-sm font-medium mb-2">Error count per bucket</div>
        <AreaChart points={errPoints} color="#ef4444" height={240} />
      </div>

      <div className="p-4 cl-card">
        <div className="text-sm font-medium mb-2">Error rate (%) per bucket</div>
        <LineChart
          series={[{ name: "error %", color: "#f59e0b", points: ratePoints }]}
          height={240}
          yLabel="% errors"
        />
      </div>
    </div>
  );
}

function Stat({ label, value, color = "" }: { label: string; value: string; color?: string }) {
  return (
    <div className="p-4 cl-card">
      <div className="text-xs text-tremor-content-subtle">{label}</div>
      <div className={`text-xl font-semibold mt-1 ${color}`}>{value}</div>
    </div>
  );
}
