// Shared panel renderer used by the dashboard viewer and the AI builder
// preview. Every panel type runs against the filter-aware dashboard
// endpoints, so a panel's filter is always applied server-side.

import { useMemo } from "react";
import { Link } from "react-router-dom";
import { usePoll, LineChart, toPoints, SeverityBadge } from "./components";
import {
  api,
  type Panel,
  type VolumeRow,
  type ErrorRateRow,
  type LatencyRow,
  type AnomalyRow,
  type ForecastResponse,
} from "./api";

const COLORS = ["#5b8dff", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899"];

/** Resolve a panel's period (relative window or custom from/to) into query
 * params for the dashboard endpoints. */
function panelPeriod(p: Panel): { window?: string; from?: string; to?: string } {
  if (p.window === "custom" && p.from && p.to) {
    const f = new Date(p.from);
    const t = new Date(p.to);
    if (!Number.isNaN(f.getTime()) && !Number.isNaN(t.getTime()) && t > f) {
      return { from: f.toISOString(), to: t.toISOString() };
    }
    return { window: "1h" };
  }
  return { window: p.window || "1h" };
}

function panelParams(p: Panel) {
  return { ...panelPeriod(p), filter: p.filter || undefined };
}

export function PanelView({ panel }: { panel: Panel }) {
  const viz = panel.viz === "number" ? "number" : "chart";
  const title = panel.title || panel.type;
  // 12-column grid width, clamped defensively (AI proposals are validated
  // server-side but hand-edited dashboards may carry anything).
  const w = Math.min(12, Math.max(1, Math.round(panel.w ?? 6)));

  return (
    <div
      className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border"
      style={{ gridColumn: `span ${w}` }}
    >
      {/* Title on its own line so it never wraps; meta (window · filter)
          goes below, truncated with hover tooltip when long. */}
      <h3 className="font-medium truncate" title={title}>
        {title}
      </h3>
      {(() => {
        const period =
          panel.window === "custom" && panel.from && panel.to
            ? `${new Date(panel.from).toLocaleString()} → ${new Date(panel.to).toLocaleString()}`
            : panel.window;
        const meta = panel.filter ? `${period} · ${panel.filter}` : period;
        const metaTitle = `${period}${panel.filter ? `\n${panel.filter}` : ""}`;
        return (
          <div className="flex items-center gap-2 mt-1">
            <span
              className="text-xs text-tremor-content-subtle truncate min-w-0"
              title={metaTitle}
            >
              {meta}
            </span>
            {panel.filter && (
              <Link
                to={`/?filter=${encodeURIComponent(panel.filter)}&window=${encodeURIComponent(panel.window)}`}
                className="text-xs text-tremor-brand dark:text-dark-tremor-brand hover:underline whitespace-nowrap"
              >
                open in logs →
              </Link>
            )}
          </div>
        );
      })()}
      <div className="mt-2">
        <PanelBody panel={panel} viz={viz} />
      </div>
    </div>
  );
}

function PanelBody({ panel, viz }: { panel: Panel; viz: "chart" | "number" }) {
  switch (panel.type) {
    case "volume":
      return <VolumePanel panel={panel} viz={viz} />;
    case "error-rate":
      return <ErrorRatePanel panel={panel} viz={viz} />;
    case "latency":
      return <LatencyPanel panel={panel} viz={viz} />;
    case "log-count":
      return <LogCountPanel panel={panel} />;
    case "top-services":
      return <TopServicesPanel panel={panel} />;
    case "anomalies":
      return <AnomaliesPanel panel={panel} viz={viz} />;
    default:
      return <div className="text-xs text-tremor-content-subtle">unknown panel type</div>;
  }
}

function Loading({ error }: { error: string | null }) {
  if (error) return <div className="text-red-600 text-sm">{error}</div>;
  return <div className="text-tremor-content-subtle text-sm">loading…</div>;
}

function Big({ value, sub }: { value: string; sub?: string }) {
  return (
    <div className="py-2">
      <div className="text-3xl font-semibold">{value}</div>
      {sub && <div className="text-xs text-tremor-content-subtle mt-1">{sub}</div>}
    </div>
  );
}

function VolumePanel({ panel, viz }: { panel: Panel; viz: "chart" | "number" }) {
  const params = panelParams(panel);
  const vol = usePoll<VolumeRow[]>(() => api.volume(params), 30000, [JSON.stringify(params)]);
  const fc = usePoll<ForecastResponse | null>(
    async () => {
      if (viz !== "chart") return null;
      try {
        return await api.forecast({ ...params, horizon: 30 });
      } catch {
        return null; // forecast is a nice-to-have overlay
      }
    },
    60000,
    [JSON.stringify(params), viz],
  );

  if (vol.error) return <Loading error={vol.error} />;
  if (!vol.data) return <Loading error={null} />;

  const rows = vol.data;
  const total = rows.reduce((s, r) => s + r.n, 0);
  if (viz === "number") {
    return <Big value={total.toLocaleString()} sub={`${rows.length} buckets`} />;
  }
  const hist = toPoints(rows, "ts", "n");
  const fcPts = (fc.data?.forecast ?? [])
    .map((p) => ({ x: Date.parse(p.ts), y: p.predicted }))
    .filter((p) => !Number.isNaN(p.x));
  return (
    <LineChart
      series={[
        { name: "records", color: "#5b8dff", points: hist },
        { name: "forecast", color: "#fbbf24", points: fcPts },
      ]}
      height={220}
    />
  );
}

function ErrorRatePanel({ panel, viz }: { panel: Panel; viz: "chart" | "number" }) {
  const params = panelParams(panel);
  const { data, error, loading } = usePoll<ErrorRateRow[]>(() => api.errorRate(params), 30000, [
    JSON.stringify(params),
  ]);
  if (error) return <Loading error={error} />;
  if (loading || !data) return <Loading error={null} />;

  const total = data.reduce((s, r) => s + r.total, 0);
  const errors = data.reduce((s, r) => s + r.errors, 0);
  const pct = total > 0 ? (errors / total) * 100 : 0;
  if (viz === "number") {
    return (
      <Big
        value={`${pct.toFixed(2)}%`}
        sub={`${errors.toLocaleString()} errors of ${total.toLocaleString()} records`}
      />
    );
  }
  const errPts = toPoints(data, "ts", "errors");
  const ratePts = toPoints(data, "ts", "rate").map((p) => ({ ...p, y: p.y * 100 }));
  return (
    <LineChart
      series={[
        { name: "errors", color: "#ef4444", points: errPts },
        { name: "error %", color: "#f59e0b", points: ratePts },
      ]}
      height={220}
    />
  );
}

const LAT_METRICS = ["p50", "p95", "p99"] as const;

function LatencyPanel({ panel, viz }: { panel: Panel; viz: "chart" | "number" }) {
  const params = panelParams(panel);
  const { data, error, loading } = usePoll<LatencyRow[]>(() => api.latency(params), 30000, [
    JSON.stringify(params),
  ]);
  const metric: (typeof LAT_METRICS)[number] = "p95";
  const series = useMemo(() => {
    const byService = new Map<string, { x: number; y: number }[]>();
    for (const r of data ?? []) {
      const list = byService.get(r.service) ?? [];
      list.push({ x: Date.parse(r.ts), y: r[metric] ?? 0 });
      byService.set(r.service, list);
    }
    return Array.from(byService.entries()).map(([name, points], i) => ({
      name,
      color: COLORS[i % COLORS.length],
      points: points.filter((p) => !Number.isNaN(p.x)),
    }));
  }, [data, metric]);

  if (error) return <Loading error={error} />;
  if (loading || !data) return <Loading error={null} />;

  const max = series.reduce(
    (m, s) => Math.max(m, ...s.points.map((p) => p.y)),
    0,
  );
  if (viz === "number") {
    return <Big value={`${max.toFixed(0)} ms`} sub={`max ${metric} across services`} />;
  }
  return <LineChart series={series} height={220} yLabel="ms" />;
}

function LogCountPanel({ panel }: { panel: Panel }) {
  const { data, error, loading } = usePoll(
    async () => {
      const params = panelParams(panel);
      const r = await api.logs({
        filter: params.filter,
        window: params.window,
        from: params.from,
        to: params.to,
        limit: 1,
      });
      return { count: r.total_matched };
    },
    30000,
    [JSON.stringify(panelParams(panel))],
  );
  if (error) return <Loading error={error} />;
  if (loading || !data) return <Loading error={null} />;
  return <Big value={data.count.toLocaleString()} />;
}

interface TopServiceRow {
  service: string;
  n: number;
  errors: number;
}

function TopServicesPanel({ panel }: { panel: Panel }) {
  // /api/topn only takes hours; derive from the panel period.
  const { data, error, loading } = usePoll<TopServiceRow[]>(
    async () => {
      const sp = new URLSearchParams();
      sp.set("hours", String(panelHours(panel)));
      if (panel.filter.trim()) sp.set("filter", panel.filter.trim());
      const resp = await fetch(`/api/topn?${sp.toString()}`, { credentials: "same-origin" });
      if (resp.status === 401) window.location.href = "/login";
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      return (await resp.json()) as TopServiceRow[];
    },
    30000,
    [panel.window, panel.from, panel.to, panel.filter],
  );
  if (error) return <Loading error={error} />;
  if (loading || !data) return <Loading error={null} />;
  return (
    <table className="w-full text-sm">
      <thead>
        <tr className="text-tremor-content-subtle text-xs">
          <th className="text-left py-1">service</th>
          <th className="text-right py-1">n</th>
          <th className="text-right py-1">errors</th>
        </tr>
      </thead>
      <tbody>
        {data.map((r, i) => (
          <tr key={i} className="border-t border-tremor-border dark:border-dark-tremor-border">
            <td className="py-1">{r.service ?? "—"}</td>
            <td className="py-1 text-right">{(r.n ?? 0).toLocaleString()}</td>
            <td className="py-1 text-right">{(r.errors ?? 0).toLocaleString()}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function AnomaliesPanel({ panel, viz }: { panel: Panel; viz: "chart" | "number" }) {
  const params = panelParams(panel);
  const { data, error, loading } = usePoll<AnomalyRow[]>(() => api.anomalies(params), 30000, [
    JSON.stringify(params),
  ]);
  if (error) return <Loading error={error} />;
  if (loading || !data) return <Loading error={null} />;
  if (viz === "number") {
    return <Big value={String(data.length)} sub="flagged anomalies in range" />;
  }
  return (
    <table className="w-full text-sm">
      <thead>
        <tr className="text-tremor-content-subtle text-xs">
          <th className="text-left py-1">time</th>
          <th className="text-right py-1">score</th>
          <th className="text-left py-1">method</th>
          <th className="text-left py-1">severity</th>
        </tr>
      </thead>
      <tbody>
        {data.slice(0, 20).map((a, i) => (
          <tr key={i} className="border-t border-tremor-border dark:border-dark-tremor-border">
            <td className="py-1 text-xs text-tremor-content-subtle">
              {new Date(a.ts).toLocaleString()}
            </td>
            <td className="py-1 text-right font-mono">{a.score.toFixed(2)}</td>
            <td className="py-1 font-mono text-xs">{a.method}</td>
            <td className="py-1">
              <SeverityBadge severity={a.severity} />
            </td>
          </tr>
        ))}
        {data.length === 0 && (
          <tr>
            <td colSpan={4} className="py-4 text-center text-tremor-content-subtle">
              No anomalies detected in this range.
            </td>
          </tr>
        )}
      </tbody>
    </table>
  );
}

/** Approximate a panel period in whole hours (for the /api/topn endpoint). */
function panelHours(p: Panel): number {
  if (p.window === "custom" && p.from && p.to) {
    const f = new Date(p.from).getTime();
    const t = new Date(p.to).getTime();
    if (!Number.isNaN(f) && !Number.isNaN(t) && t > f) {
      return Math.max(1, Math.round((t - f) / 3600_000));
    }
    return 1;
  }
  const m = (p.window || "1h").match(/^(\d+)([smhdw])$/);
  if (!m) return 1;
  const n = Number(m[1]);
  switch (m[2]) {
    case "m":
      return Math.max(1, Math.round(n / 60));
    case "h":
      return n;
    case "d":
      return n * 24;
    case "w":
      return n * 24 * 7;
    default:
      return 1;
  }
}
