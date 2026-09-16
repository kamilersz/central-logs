// Shared UI helpers + chart components used across pages.

import { useEffect, useMemo, useRef, useState } from "react";

/** Polls an async function on an interval; returns {data, error, loading}. */
export function usePoll<T>(
  fn: () => Promise<T>,
  intervalMs: number,
  deps: unknown[] = [],
): { data: T | null; error: string | null; loading: boolean; refresh: () => void } {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    fn()
      .then((r) => {
        if (cancelled) return;
        setData(r);
        setError(null);
      })
      .catch((e) => {
        if (cancelled) return;
        setError(String(e));
      })
      .finally(() => {
        if (cancelled) return;
        setLoading(false);
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick, ...deps]);

  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), intervalMs);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [intervalMs]);

  return { data, error, loading, refresh: () => setTick((t) => t + 1) };
}

/** A small "badge" pill for severity levels. Light-friendly + dark: pairs so
 * every theme stays readable (the `dark` class follows the active theme). */
const SEVERITY_STYLES: Record<string, string> = {
  low: "bg-slate-400/15 text-slate-600 dark:text-slate-300",
  medium: "bg-amber-400/15 text-amber-600 dark:text-amber-300",
  high: "bg-orange-400/15 text-orange-600 dark:text-orange-300",
  critical: "bg-red-400/15 text-red-600 dark:text-red-300",
};

export function SeverityBadge({ severity }: { severity: string }) {
  const cls = SEVERITY_STYLES[severity.toLowerCase()] ?? SEVERITY_STYLES.low;
  return (
    <span className={`cl-pill ${cls}`}>
      <span className="cl-pill-dot" />
      {severity}
    </span>
  );
}

const LEVEL_STYLES: Record<string, string> = {
  debug: "bg-slate-400/15 text-slate-600 dark:text-slate-300",
  info: "bg-emerald-400/15 text-emerald-600 dark:text-emerald-300",
  warn: "bg-amber-400/15 text-amber-600 dark:text-amber-300",
  error: "bg-red-400/15 text-red-600 dark:text-red-300",
  fatal: "bg-rose-400/15 text-rose-600 dark:text-rose-300",
};

export function LevelBadge({ level }: { level: string }) {
  const cls = LEVEL_STYLES[level] ?? LEVEL_STYLES.debug;
  return (
    <span className={`cl-pill ${cls}`}>
      <span className="cl-pill-dot" />
      {level}
    </span>
  );
}

/** A simple SVG line chart for time-series. Avoids pulling in a heavy chart
 * dependency — we only need a few hundred points on an x-axis. */
export function LineChart({
  series,
  height = 240,
  yLabel,
}: {
  series: { name: string; color: string; points: { x: number; y: number }[]; dashed?: boolean }[];
  height?: number;
  yLabel?: string;
}) {
  if (series.length === 0 || series.every((s) => s.points.length === 0)) {
    return (
      <div
        className="cl-empty text-tremor-content-subtle dark:text-dark-tremor-content-subtle"
        style={{ height }}
      >
        <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" opacity="0.5">
          <path d="M3 17l5-6 4 3 5-7 4 4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        <span className="text-sm">no data in this window</span>
      </div>
    );
  }
  const allPts = series.flatMap((s) => s.points);
  const xs = allPts.map((p) => p.x);
  const ys = allPts.map((p) => p.y);
  const xMin = Math.min(...xs);
  const xMax = Math.max(...xs);
  const yMin = Math.min(...ys, 0);
  const yMax = Math.max(...ys, 1);
  const w = 800;
  const h = height;
  const pad = { left: 50, right: 20, top: 20, bottom: 30 };
  const innerW = w - pad.left - pad.right;
  const innerH = h - pad.top - pad.bottom;
  const sx = (x: number) =>
    pad.left + (xMax === xMin ? innerW / 2 : ((x - xMin) / (xMax - xMin)) * innerW);
  const sy = (y: number) =>
    pad.top + innerH - (yMax === yMin ? innerH / 2 : ((y - yMin) / (yMax - yMin)) * innerH);

  // y-axis gridlines (5) on a nice-number scale so labels are integers.
  // Floor the range at 0 for value charts (volume, errors) so the axis
  // doesn't gain spurious space below the data.
  const yticks = niceTicks(Math.max(0, yMin), Math.max(yMin, yMax), 5);
  // x-axis ticks (6) — switch format with span: time-of-day under a day,
  // date-only above. Always aligned to day boundaries when the span is
  // ≥ 24h so multi-day windows don't show random mid-day ticks.
  const spanMs = xMax - xMin;
  const spanDays = spanMs / 86_400_000;
  const xTicks = buildXTicks(xMin, xMax, spanDays, 6);

  // Hover state — nearest point per series at the cursor's x.
  const [hover, setHover] = useState<{ px: number; rows: { name: string; color: string; x: number; y: number }[] } | null>(null);
  const svgRef = useRef<SVGSVGElement | null>(null);

  /** Find the closest point on each series to the cursor's x coordinate. */
  function nearest(cursorX: number) {
    const rows: { name: string; color: string; x: number; y: number }[] = [];
    for (const s of series) {
      if (s.points.length === 0) continue;
      let best = s.points[0];
      let bestD = Math.abs(best.x - cursorX);
      for (let i = 1; i < s.points.length; i++) {
        const d = Math.abs(s.points[i].x - cursorX);
        if (d < bestD) {
          best = s.points[i];
          bestD = d;
        }
      }
      rows.push({ name: s.name, color: s.color, x: best.x, y: best.y });
    }
    return rows;
  }

  function onMove(e: React.MouseEvent<SVGSVGElement>) {
    const svg = svgRef.current;
    if (!svg) return;
    const rect = svg.getBoundingClientRect();
    // Convert client px → SVG viewBox px.
    const vx = ((e.clientX - rect.left) / rect.width) * w;
    if (vx < pad.left || vx > w - pad.right) {
      setHover(null);
      return;
    }
    // Convert viewBox px back to data x, then snap nearest.
    const dataX =
      xMax === xMin ? xMin : xMin + ((vx - pad.left) / innerW) * (xMax - xMin);
    setHover({ px: vx, rows: nearest(dataX) });
  }
  function onLeave() {
    setHover(null);
  }

  return (
    <div className="relative">
      <svg
        ref={svgRef}
        viewBox={`0 0 ${w} ${h}`}
        className="w-full block"
        style={{ height }}
        onMouseMove={onMove}
        onMouseLeave={onLeave}
      >
        {/* y gridlines + labels */}
        {yticks.map((y, i) => (
          <g key={i}>
            <line
              x1={pad.left}
              x2={w - pad.right}
              y1={sy(y)}
              y2={sy(y)}
              style={{ stroke: "var(--cl-chart-grid)" }}
              strokeOpacity={i === 0 ? 1 : 0.7}
              strokeDasharray={i === 0 ? undefined : "3 5"}
            />
            <text
              x={pad.left - 8}
              y={sy(y) + 4}
              textAnchor="end"
              fontSize={11}
              style={{ fill: "var(--cl-content-subtle)", fontFamily: "var(--cl-font-mono)" }}
            >
              {fmtNum(y)}
            </text>
          </g>
        ))}
        {/* x labels */}
        {xTicks.map((xt, i) => (
          <text
            key={i}
            x={sx(xt.t)}
            y={h - 8}
            textAnchor={i === 0 ? "start" : i === xTicks.length - 1 ? "end" : "middle"}
            fontSize={11}
            style={{ fill: "var(--cl-content-subtle)", fontFamily: "var(--cl-font-mono)" }}
          >
            {xt.label}
          </text>
        ))}
        {/* lines */}
        {series.map((s) => {
          if (s.points.length === 0) return null;
          const path = s.points
            .map((p, i) => `${i === 0 ? "M" : "L"} ${sx(p.x)} ${sy(p.y)}`)
            .join(" ");
          return (
            <path
              key={s.name}
              d={path}
              fill="none"
              stroke={s.color}
              strokeWidth={2}
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeDasharray={s.dashed ? "6 4" : undefined}
            />
          );
        })}
        {/* hover crosshair + per-series dots */}
        {hover && (
          <g pointerEvents="none">
            <line
              x1={hover.px}
              x2={hover.px}
              y1={pad.top}
              y2={h - pad.bottom}
              style={{ stroke: "var(--cl-content-subtle)" }}
              strokeOpacity={0.45}
              strokeDasharray="2 3"
            />
            {hover.rows.map((r, i) => (
              <circle
                key={i}
                cx={sx(r.x)}
                cy={sy(r.y)}
                r={4}
                fill={r.color}
                stroke="var(--cl-bg)"
                strokeWidth={2}
              />
            ))}
          </g>
        )}
        {yLabel && (
          <text
            x={12}
            y={pad.top + innerH / 2}
            textAnchor="middle"
            fontSize={11}
            style={{ fill: "var(--cl-content-subtle)" }}
            transform={`rotate(-90, 12, ${pad.top + innerH / 2})`}
          >
            {yLabel}
          </text>
        )}
      </svg>
      {/* HTML legend — SVG text at the viewBox edge used to clip and
          collide with the x-axis labels */}
      {/* Hover tooltip — absolute overlay anchored to the chart so it
          floats above SVG without being clipped. */}
      {hover && (
        <div
          className="absolute pointer-events-none px-2.5 py-1.5 rounded text-xs shadow-lg"
          style={{
            // Position near the cursor's x; flip to the left when near the
            // right edge so it doesn't overflow.
            left: `${(hover.px / w) * 100}%`,
            transform: `translate(${hover.px > w * 0.7 ? "-100%" : "8px"}, -100%) translateY(-8px)`,
            background: "var(--cl-bg-emphasis)",
            color: "var(--cl-content-strong)",
            fontFamily: "var(--cl-font-mono)",
            whiteSpace: "nowrap",
            zIndex: 1,
          }}
        >
          <div className="text-tremor-content-subtle" style={{ fontSize: 10 }}>
            {new Date(hover.rows[0]?.x ?? 0).toLocaleString()}
          </div>
          {hover.rows.map((r, i) => (
            <div key={i} className="flex items-center gap-2">
              <span
                className="inline-block w-2 h-2 rounded-full"
                style={{ background: r.color }}
              />
              <span className="text-tremor-content-subtle">{r.name || "value"}:</span>
              <span>{fmtNum(r.y)}</span>
            </div>
          ))}
        </div>
      )}
      <div className="mt-1 flex flex-wrap items-center gap-x-5 gap-y-1">
        {series
          .filter((s) => s.points.length > 0 && s.name !== "")
          .map((s) => (
            <span key={s.name} className="flex items-center gap-2">
              <span
                className="inline-block h-[3px] w-4 rounded-full"
                style={{ background: s.color }}
              />
              <span className="text-[11px] text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis">
                {s.name}
              </span>
            </span>
          ))}
      </div>
    </div>
  );
}

function fmtNum(n: number): string {
  if (Math.abs(n) >= 1e9) return (n / 1e9).toFixed(1) + "G";
  if (Math.abs(n) >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (Math.abs(n) >= 1e3) return (n / 1e3).toFixed(1) + "k";
  if (Number.isInteger(n)) return String(n);
  return n.toFixed(2);
}

/** An area chart (filled region under a single line). Same shape as LineChart. */
export function AreaChart({
  points,
  color = "#2f81f7",
  height = 240,
}: {
  points: { x: number; y: number }[];
  color?: string;
  height?: number;
}) {
  return (
    <LineChart
      series={[{ name: "", color, points }]}
      height={height}
    />
  );
}

/** Parse `{ts: "...", n: 123}` rows into chart points. Tolerates non-array
 * payloads (e.g. an `{"error": ...}` body) instead of throwing in render. */
export function toPoints<T>(
  rows: T[],
  tsKey: keyof T,
  valueKey: keyof T,
): { x: number; y: number }[] {
  const out: { x: number; y: number }[] = [];
  if (!Array.isArray(rows)) return out;
  for (const r of rows) {
    const ts = r[tsKey] as unknown;
    const v = r[valueKey] as unknown;
    if (typeof ts !== "string" || typeof v !== "number") continue;
    const t = Date.parse(ts);
    if (Number.isNaN(t)) continue;
    out.push({ x: t, y: v });
  }
  return out;
}

// =====================================================================
// Time + filter controls, shared by the preset dashboards and panels
// =====================================================================

export const TIME_WINDOWS = ["5m", "15m", "1h", "6h", "24h", "7d"];

/** Selector state for a dashboard time range + filter. `window: "custom"`
 * switches to the explicit from/to datetime inputs. */
export interface TimeFilterState {
  window: string;
  /** datetime-local values ("YYYY-MM-DDTHH:MM"), used when window==="custom". */
  from: string;
  to: string;
  filter: string;
}

export function defaultTimeFilter(): TimeFilterState {
  const to = new Date();
  const from = new Date(to.getTime() - 3600_000);
  return {
    window: "1h",
    from: toLocalInput(from),
    to: toLocalInput(to),
    filter: "",
  };
}

/** Format a Date as a datetime-local input value (local time, minutes). */
export function toLocalInput(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** True when the state carries a usable custom range. */
export function customRangeValid(s: TimeFilterState): boolean {
  if (s.window !== "custom") return true;
  const f = Date.parse(s.from);
  const t = Date.parse(s.to);
  return !Number.isNaN(f) && !Number.isNaN(t) && t > f;
}

/** Convert selector state into the query params the dashboard endpoints
 * accept: either {window} or {from,to} (ISO), plus the filter. */
export function timeFilterParams(s: TimeFilterState): {
  window?: string;
  from?: string;
  to?: string;
  filter?: string;
} {
  const base: { window?: string; from?: string; to?: string; filter?: string } = {};
  if (s.window === "custom" && customRangeValid(s)) {
    base.from = new Date(s.from).toISOString();
    base.to = new Date(s.to).toISOString();
  } else {
    base.window = s.window === "custom" ? "1h" : s.window;
  }
  if (s.filter.trim()) base.filter = s.filter.trim();
  return base;
}

const inputCls =
  "px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm";

/** Shared toolbar: relative window / custom from-to range + optional filter
 * DSL input. Rendered above every preset dashboard. */
export function TimeFilterBar({
  value,
  onChange,
  showFilter = true,
}: {
  value: TimeFilterState;
  onChange: (s: TimeFilterState) => void;
  showFilter?: boolean;
}) {
  const custom = value.window === "custom";
  return (
    <div className="flex flex-wrap items-center gap-2 mb-4">
      <select
        value={value.window}
        onChange={(e) => onChange({ ...value, window: e.target.value })}
        className={inputCls}
        aria-label="time range"
      >
        {TIME_WINDOWS.map((w) => (
          <option key={w} value={w}>
            last {w}
          </option>
        ))}
        <option value="custom">custom range…</option>
      </select>
      {custom && (
        <>
          <input
            type="datetime-local"
            value={value.from}
            onChange={(e) => onChange({ ...value, from: e.target.value })}
            className={inputCls}
            aria-label="from"
          />
          <span className="text-tremor-content-subtle text-sm">→</span>
          <input
            type="datetime-local"
            value={value.to}
            onChange={(e) => onChange({ ...value, to: e.target.value })}
            className={inputCls}
            aria-label="to"
          />
          {!customRangeValid(value) && (
            <span className="text-xs text-red-600">from must be before to</span>
          )}
        </>
      )}
      {showFilter && (
        <input
          type="text"
          value={value.filter}
          onChange={(e) => onChange({ ...value, filter: e.target.value })}
          placeholder='filter, e.g. service:telegram-bot level:error'
          className={`${inputCls} flex-1 min-w-[220px] font-mono`}
          aria-label="filter"
        />
      )}
    </div>
  );
}

// =====================================================================
// Chart axis helpers
// =====================================================================

/** "Nice number" ticks between [min, max] — classic algorithm (Heckbert
 * 1990): pick a step from {1, 2, 5} × 10^n that yields ~`count` divisions.
 *  Result is always an integer multiple of a base (5/10/50/100/500…). */
export function niceTicks(min: number, max: number, count: number): number[] {
  if (!Number.isFinite(min) || !Number.isFinite(max) || min === max) {
    return [min || 0];
  }
  if (max < min) [min, max] = [max, min];
  const range = niceNumber(max - min, false);
  const step = niceNumber(range / Math.max(1, count - 1), true);
  const niceMin = Math.floor(min / step) * step;
  const niceMax = Math.ceil(max / step) * step;
  const ticks: number[] = [];
  // Guard against pathological ranges that produce runaway loops.
  const maxTicks = 64;
  for (let v = niceMin; v <= niceMax + step * 0.5 && ticks.length < maxTicks; v += step) {
    ticks.push(Number(v.toFixed(10)));
  }
  return ticks;
}

function niceNumber(x: number, round: boolean): number {
  if (x <= 0 || !Number.isFinite(x)) return 1;
  const exp = Math.floor(Math.log10(x));
  const frac = x / Math.pow(10, exp);
  let nice: number;
  if (round) {
    if (frac < 1.5) nice = 1;
    else if (frac < 3) nice = 2;
    else if (frac < 7) nice = 5;
    else nice = 10;
  } else {
    if (frac <= 1) nice = 1;
    else if (frac <= 2) nice = 2;
    else if (frac <= 5) nice = 5;
    else nice = 10;
  }
  return nice * Math.pow(10, exp);
}

/** Build x-axis ticks. For spans ≥24h we step in whole days and label
 *  with the local date — no more "10:00 AM · 7:24 AM · 4:48 AM" randomness
 *  on week-long charts. Sub-day spans fall back to hour-of-day. */
function buildXTicks(
  xMin: number,
  xMax: number,
  spanDays: number,
  count: number,
): { t: number; label: string }[] {
  if (!Number.isFinite(xMin) || !Number.isFinite(xMax) || xMin === xMax) {
    return [{ t: xMin, label: new Date(xMin).toLocaleString() }];
  }
  if (spanDays >= 2) {
    // Step in whole days, snapped to local midnight. The first label may
    // be xMin itself if it's already at midnight, otherwise the next one.
    const out: { t: number; label: string }[] = [];
    const stepDays = spanDays >= 14 ? Math.ceil(spanDays / (count - 1)) : 1;
    const step = stepDays * 86_400_000;
    const firstMidnight = nextMidnight(xMin);
    // Always start at xMin so the left edge is anchored.
    out.push({ t: xMin, label: fmtXLabel(xMin, spanDays) });
    for (let t = firstMidnight; t <= xMax && out.length < 16; t += step) {
      out.push({ t, label: fmtXLabel(t, spanDays) });
    }
    if (out[out.length - 1].t !== xMax) {
      out.push({ t: xMax, label: fmtXLabel(xMax, spanDays) });
    }
    return out;
  }
  if (spanDays >= 1) {
    // 24h-ish: a few hour ticks aligned to local midnight/6/12/18.
    return Array.from({ length: count }, (_, i) => {
      const t = xMin + ((xMax - xMin) * i) / (count - 1);
      return { t, label: new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) };
    });
  }
  // Sub-day: hour:minute ticks.
  return Array.from({ length: count }, (_, i) => {
    const t = xMin + ((xMax - xMin) * i) / (count - 1);
    return { t, label: new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) };
  });
}

function nextMidnight(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  if (d.getTime() <= ms) d.setDate(d.getDate() + 1);
  return d.getTime();
}

function fmtXLabel(t: number, spanDays: number): string {
  const d = new Date(t);
  if (spanDays >= 7) {
    // Compact: "Sep 09" — drops year noise for week+ windows.
    return d.toLocaleDateString([], { month: "short", day: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "2-digit" });
}
