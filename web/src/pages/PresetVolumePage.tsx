import { useState } from "react";
import { usePoll, LineChart, AreaChart, toPoints, TimeFilterBar, timeFilterParams, defaultTimeFilter } from "../components";
import { api, type ForecastResponse } from "../api";

export default function PresetVolumePage() {
  const [tf, setTf] = useState(defaultTimeFilter());
  const params = timeFilterParams(tf);

  // History (volume per bucket) + future forecast overlay.
  const { data: fc, error: fcErr } = usePoll<ForecastResponse>(
    () => api.forecast({ ...params, horizon: 60 }),
    30000,
    [JSON.stringify(params)],
  );
  const { data: volumeRows, error: volErr } = usePoll<{ ts: string; n: number }[]>(
    () => api.volume(params),
    10000,
    [JSON.stringify(params)],
  );

  const histPoints = toPoints(volumeRows ?? [], "ts", "n");
  const fcPoints = (fc?.forecast ?? [])
    .map((p) => ({ x: Date.parse(p.ts), y: p.predicted }))
    .filter((p) => !Number.isNaN(p.x));
  const fcUpper = (fc?.forecast ?? [])
    .map((p) => ({ x: Date.parse(p.ts), y: p.upper }))
    .filter((p) => !Number.isNaN(p.x));
  const fcLower = (fc?.forecast ?? [])
    .map((p) => ({ x: Date.parse(p.ts), y: p.lower }))
    .filter((p) => !Number.isNaN(p.x));

  const total = histPoints.reduce((s, p) => s + p.y, 0);
  // Honest stat: average rate per visible bucket (records / bucket).
  const rate = histPoints.length > 0 ? total / histPoints.length : 0;

  return (
    <div>
      <h2 className="text-xl font-semibold mb-4">Volume + Forecast</h2>
      <TimeFilterBar value={tf} onChange={setTf} />

      <div className="grid grid-cols-3 gap-4 mb-4">
        <Stat label="records in range" value={total.toLocaleString()} />
        <Stat label="avg / bucket" value={fmtRate(rate)} />
        <Stat label="forecast model" value={fc?.model ?? "—"} />
      </div>

      {fcErr && <div className="mb-3 text-xs text-amber-600">forecast unavailable: {fcErr}</div>}
      {volErr && <div className="mb-3 text-xs text-red-600">{volErr}</div>}

      <div className="p-4 cl-card mb-4">
        <div className="text-sm font-medium mb-2">History + forecast (dashed, with 95% band)</div>
        <LineChart
          series={[
            { name: "history", color: "#5b8dff", points: histPoints },
            { name: "forecast", color: "#fbbf24", points: fcPoints, dashed: true },
            { name: "upper", color: "#475569", points: fcUpper, dashed: true },
            { name: "lower", color: "#475569", points: fcLower, dashed: true },
          ]}
          height={280}
          yLabel="records"
        />
      </div>

      <div className="p-4 cl-card">
        <div className="text-sm font-medium mb-2">Recent volume</div>
        <AreaChart points={histPoints} color="#5b8dff" height={220} />
      </div>
    </div>
  );
}

function fmtRate(n: number): string {
  if (n >= 100) return n.toFixed(0);
  if (n >= 10) return n.toFixed(1);
  return n.toFixed(2);
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="p-4 cl-card">
      <div className="text-xs text-tremor-content-subtle">{label}</div>
      <div className="text-xl font-semibold mt-1 truncate">{value}</div>
    </div>
  );
}
