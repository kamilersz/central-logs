// Shared UI helpers + chart components used across pages.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, type ServiceInfo } from "./api";

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
  bars = false,
  bare = false,
  integerY = false,
  timeZone,
  domain,
  bucketMs,
}: {
  series: { name: string; color: string; points: { x: number; y: number }[]; dashed?: boolean }[];
  height?: number;
  yLabel?: string;
  /** Render bars (bucketed counts) instead of polylines. */
  bars?: boolean;
  /** Sparkline mode: no axes, gridlines, labels or hover. */
  bare?: boolean;
  /** Force integer y ticks (counts must never show 0.50 sessions). */
  integerY?: boolean;
  /** IANA zone (or "local") for x-axis labels; default browser time. */
  timeZone?: string;
  /** Explicit [min, max] x-domain (e.g. the selected range incl. empties). */
  domain?: [number, number];
  /** Bucket width in ms for bar sizing; inferred from point spacing if absent. */
  bucketMs?: number;
}) {
  if (series.length === 0 || series.every((s) => s.points.length === 0)) {
    // Sparklines render nothing (their slot is decorative); full charts get
    // the honest empty state.
    if (bare) return null;
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
  const xMin = domain ? domain[0] : Math.min(...xs);
  const xMax = domain ? domain[1] : Math.max(...xs);
  const yMin = Math.min(...ys, 0);
  const yMax = Math.max(...ys, 1);
  const w = 800;
  const h = height;
  const pad = bare
    ? { left: 2, right: 2, top: 2, bottom: 2 }
    : { left: 34, right: 12, top: 12, bottom: 24 };
  const innerW = w - pad.left - pad.right;
  const innerH = h - pad.top - pad.bottom;
  const sx = (x: number) =>
    pad.left + (xMax === xMin ? innerW / 2 : ((x - xMin) / (xMax - xMin)) * innerW);
  const sy = (y: number) =>
    pad.top + innerH - (yMax === yMin ? innerH / 2 : ((y - yMin) / (yMax - yMin)) * innerH);

  // y-axis gridlines (5) on a nice-number scale so labels are integers.
  // Floor the range at 0 for value charts (volume, errors) so the axis
  // doesn't gain spurious space below the data.
  const yticks = integerYTicks(niceTicks(Math.max(0, yMin), Math.max(yMin, yMax), 5), integerY);
  // x-axis ticks (6) — switch format with span: time-of-day under a day,
  // date-only above. Always aligned to day boundaries when the span is
  // ≥ 24h so multi-day windows don't show random mid-day ticks.
  const spanMs = xMax - xMin;
  const spanDays = spanMs / 86_400_000;
  const xTicks = buildXTicks(xMin, xMax, spanDays, 6, timeZone);

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
        onMouseMove={bare ? undefined : onMove}
        onMouseLeave={bare ? undefined : onLeave}
      >
        {/* y gridlines + labels */}
        {!bare && yticks.map((y, i) => (
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
        {!bare && xTicks.map((xt, i) => (
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
        {/* bars (bucketed counts) or lines */}
        {bars
          ? series.map((s) => {
              if (s.points.length === 0) return null;
              const bw = barWidth(s.points, xMin, xMax, innerW, bucketMs);
              const base = sy(Math.max(0, yMin));
              return (
                <g key={s.name} fill={s.color} opacity={0.85}>
                  {s.points.map((p, i) => {
                    const bx = sx(p.x);
                    const by = sy(p.y);
                    return (
                      <rect
                        key={i}
                        x={Math.max(pad.left, bx - bw / 2)}
                        y={Math.min(by, base)}
                        width={Math.max(1, Math.min(bw, innerW + pad.left - Math.max(pad.left, bx - bw / 2)))}
                        height={Math.max(0, Math.abs(base - by))}
                        rx={1}
                      />
                    );
                  })}
                </g>
              );
            })
          : series.map((s) => {
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
        {!bare && hover && (
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
        {!bare && yLabel && (
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

export const TIME_WINDOWS = ["5m", "15m", "1h", "6h", "24h", "7d", "30d", "90d"];

/** Build a deep-link to the Logs Explorer (the `/` route) carrying the
 * current time range + filter. `window` is a relative shorthand, or omit it
 * and pass from/to (ISO) for an explicit range — the explorer understands
 * both via its URL params. */
export function logsExplorerHref(params: {
  window?: string;
  from?: string;
  to?: string;
  filter?: string;
}): string {
  const sp = new URLSearchParams();
  if (params.filter?.trim()) sp.set("filter", params.filter.trim());
  if (params.window) sp.set("window", params.window);
  if (params.from) sp.set("from", params.from);
  if (params.to) sp.set("to", params.to);
  const q = sp.toString();
  return q ? `/?${q}` : "/";
}

/** "open in Logs Explorer →" link styled like Panel.tsx's meta links. */
export function LogsExplorerLink({
  window,
  from,
  to,
  filter,
  label = "open in logs →",
}: {
  window?: string;
  from?: string;
  to?: string;
  filter?: string;
  label?: string;
}) {
  return (
    <a
      href={logsExplorerHref({ window, from, to, filter })}
      className="text-xs text-tremor-brand dark:text-dark-tremor-brand hover:underline whitespace-nowrap"
    >
      {label}
    </a>
  );
}

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
  "px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm min-w-0";

/** Shared toolbar: relative window / custom from-to range + optional filter
 * DSL input. Rendered above every preset dashboard. `defaultOption` adds a
 * value-"" choice (e.g. "panel defaults" on the dashboard viewer) meaning
 * "no override". */
export function TimeFilterBar({
  value,
  onChange,
  showFilter = true,
  defaultOption,
}: {
  value: TimeFilterState;
  onChange: (s: TimeFilterState) => void;
  showFilter?: boolean;
  defaultOption?: string;
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
        {defaultOption !== undefined && <option value="">{defaultOption}</option>}
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
            className={`${inputCls} w-full sm:w-auto`}
            aria-label="from"
          />
          <span className="hidden sm:inline text-tremor-content-subtle text-sm">→</span>
          <input
            type="datetime-local"
            value={value.to}
            onChange={(e) => onChange({ ...value, to: e.target.value })}
            className={`${inputCls} w-full sm:w-auto`}
            aria-label="to"
          />
          {!customRangeValid(value) && (
            <span className="text-xs text-red-600">from must be before to</span>
          )}
        </>
      )}
      {showFilter && (
        <FilterInput
          value={value.filter}
          onChange={(filter) => onChange({ ...value, filter })}
          placeholder='filter, e.g. service:telegram-bot level:error'
          ariaLabel="filter"
          className={`${inputCls} w-full sm:flex-1 sm:min-w-[220px] font-mono`}
        />
      )}
      {/* Jump from the dashboard to the raw logs with the same range+filter. */}
      <LogsExplorerLink {...timeFilterParams(value)} />
    </div>
  );
}

// =====================================================================
// Filter input with `service:` autocomplete
// =====================================================================

/** A filter-DSL text input that completes `service:<name>` from the server's
 *  service registry (`GET /api/services`, maintained by the rollup job).
 *  Plain text otherwise — every other `column:value` clause passes through
 *  untouched. Drop-in replacement for the raw `<input>`s on the logs
 *  explorer, alert-rule editor, dashboard builder, and preset pages.
 *
 *  Completion triggers on the `service:` token immediately before the
 *  cursor (`-service:` negation included). Tab / Enter accepts the
 *  highlighted row, arrows move, Esc closes. Analytics note: choosing a
 *  suggestion does not emit anything special — the resulting filter text
 *  flows through the normal `onChange` like typed text.
 */
export function FilterInput({
  value,
  onChange,
  placeholder,
  className,
  ariaLabel,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  ariaLabel?: string;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [services, setServices] = useState<ServiceInfo[]>([]);
  const [registryTotal, setRegistryTotal] = useState(0);
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(0);
  const [cursor, setCursor] = useState(0);

  // Registry snapshot per mount + slow refresh (services change rarely;
  // the list is small enough to filter client-side).
  useEffect(() => {
    let cancelled = false;
    api
      .listServices(undefined, 1000)
      .then((r) => {
        if (cancelled) return;
        setServices(r.services);
        setRegistryTotal(r.total);
      })
      .catch(() => {
        /* offline / old server without /api/services — plain input */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The `service:` token (with optional `-` negation) right before the cursor.
  const token = useMemo(() => {
    const before = value.slice(0, Math.max(0, Math.min(cursor, value.length)));
    const m = /(?:^|\s)(-?service:)([^\s:]*)$/.exec(before);
    if (!m) return null;
    // m[0] carries at most one leading whitespace char (the `(?:^|\s)`).
    const matchStart = before.length - m[0].length;
    const start = /^\s/.test(m[0]) ? matchStart + 1 : matchStart;
    return { prefix: m[1], partial: m[2], start };
  }, [value, cursor]);

  const matches = useMemo(() => {
    if (!token) return [];
    const p = token.partial.toLowerCase();
    return services
      .filter((s) => s.name.toLowerCase().startsWith(p))
      .slice(0, 8);
  }, [services, token]);

  // Open only while a token is under the cursor and we know services.
  const showOpen = open && token !== null && services.length > 0;

  function syncCursor(el: HTMLInputElement | null) {
    if (el) setCursor(el.selectionStart ?? el.value.length);
  }

  function complete(name: string) {
    if (!token || !inputRef.current) return;
    const before = value.slice(0, token.start);
    const after = value.slice(Math.max(0, Math.min(cursor, value.length)));
    // Preserve a leading `-` for `-service:` negation (it's part of prefix).
    const next = `${before}${token.prefix}${name}${after}`;
    onChange(next);
    const pos = (before + token.prefix + name).length;
    setCursor(pos);
    setOpen(false);
    requestAnimationFrame(() => {
      const el = inputRef.current;
      if (el) {
        el.focus();
        el.setSelectionRange(pos, pos);
      }
    });
  }

  return (
    <div className="relative">
      <input
        ref={inputRef}
        type="text"
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        className={className}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => {
          onChange(e.target.value);
          syncCursor(e.target);
          setHighlight(0);
          setOpen(true);
        }}
        onKeyUp={(e) => {
          if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) {
            syncCursor(e.currentTarget);
          }
        }}
        onClick={(e) => {
          syncCursor(e.currentTarget);
          setOpen(true);
        }}
        onFocus={(e) => {
          syncCursor(e.currentTarget);
          setOpen(true);
        }}
        onBlur={() => {
          // Delayed so a suggestion click (mousedown) lands before close.
          setTimeout(() => setOpen(false), 120);
        }}
        onKeyDown={(e) => {
          if (!showOpen) {
            if (e.key === "Escape") setOpen(false);
            return;
          }
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setHighlight((h) => (h + 1) % Math.max(1, matches.length));
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setHighlight((h) => (h - 1 + matches.length) % Math.max(1, matches.length));
          } else if (e.key === "Enter" || e.key === "Tab") {
            const pick = matches[highlight] ?? matches[0];
            if (pick) {
              e.preventDefault();
              complete(pick.name);
            }
          } else if (e.key === "Escape") {
            setOpen(false);
          }
        }}
      />
      {showOpen && (
        <div className="absolute z-30 mt-1 w-full max-w-md rounded-md border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background shadow-lg overflow-hidden">
          {matches.length === 0 ? (
            <div className="px-3 py-2 text-xs opacity-60 font-mono">
              no service starts with “{token.partial}”
            </div>
          ) : (
            <ul>
              {matches.map((s, i) => (
                <li key={s.name}>
                  <button
                    type="button"
                    // mousedown beats input blur (see the blur timeout above).
                    onMouseDown={(e) => {
                      e.preventDefault();
                      complete(s.name);
                    }}
                    onMouseEnter={() => setHighlight(i)}
                    className={`w-full flex items-baseline justify-between gap-2 px-3 py-1.5 text-left font-mono text-xs ${
                      i === highlight
                        ? "bg-tremor-background-muted dark:bg-dark-tremor-background-muted"
                        : ""
                    }`}
                  >
                    <span className="truncate">
                      <span className="opacity-50">{token.prefix}</span>
                      {s.name}
                    </span>
                    <span className="opacity-50 shrink-0">{s.total.toLocaleString()} rows</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="px-3 py-1 text-[10px] opacity-50 border-t border-tremor-border dark:border-dark-tremor-border">
            {registryTotal} service{registryTotal === 1 ? "" : "s"} known · Tab to complete
          </div>
        </div>
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

/** Force integer ticks for count axes (fractional sessions are nonsense). */
export function integerYTicks(ticks: number[], integerY: boolean): number[] {
  if (!integerY) return ticks;
  const ints = [...new Set(ticks.map((t) => Math.round(t)))].sort((a, b) => a - b);
  return ints.length > 0 ? ints : [0];
}

/** Bar width in viewBox px from bucket spacing (or explicit bucketMs). */
function barWidth(
  points: { x: number; y: number }[],
  xMin: number,
  xMax: number,
  innerW: number,
  bucketMs?: number,
): number {
  let step: number;
  if (bucketMs && bucketMs > 0) {
    step = bucketMs;
  } else {
    const xs = [...new Set(points.map((p) => p.x))].sort((a, b) => a - b);
    let min = Infinity;
    for (let i = 1; i < xs.length; i++) min = Math.min(min, xs[i] - xs[i - 1]);
    step = Number.isFinite(min) ? min : (xMax - xMin) / Math.max(1, xs.length);
  }
  if (!(step > 0)) return 4;
  const span = Math.max(xMax - xMin, step);
  return Math.max(2, (step / span) * innerW * 0.7);
}

/** Canonical bucket key ("1m"|"5m"|"1h"|"1d", as returned by /api/web/stats)
 *  → milliseconds. */
export function intervalMs(key: string | undefined): number {
  switch (key) {
    case "1m": return 60_000;
    case "5m": return 5 * 60_000;
    case "1h": return 3_600_000;
    case "1d": return 86_400_000;
    default: return 3_600_000;
  }
}

/** Zero-fill a sparse bucket series over [fromMs, toMs] so gaps read as
 *  gaps instead of vanishing (and bars space evenly). Points snap down to
 *  their bucket start. */
export function fillBuckets(
  points: { x: number; y: number }[],
  fromMs: number,
  toMs: number,
  stepMs: number,
): { x: number; y: number }[] {
  if (!(stepMs > 0) || !(toMs > fromMs)) return points;
  const byBucket = new Map<number, number>();
  for (const p of points) {
    if (!Number.isFinite(p.x) || !Number.isFinite(p.y)) continue;
    const b = fromMs + Math.floor((p.x - fromMs) / stepMs) * stepMs;
    byBucket.set(b, (byBucket.get(b) ?? 0) + p.y);
  }
  const out: { x: number; y: number }[] = [];
  for (let t = fromMs; t < toMs; t += stepMs) {
    out.push({ x: t, y: byBucket.get(t) ?? 0 });
  }
  return out;
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
/** Offset of `tz` ("UTC" | IANA | "local") at instant `t`, in ms. */
export function tzOffsetMs(tz: string | undefined, t: number): number {
  if (!tz || tz === "local") return -new Date(t).getTimezoneOffset() * 60_000;
  if (tz === "UTC") return 0;
  try {
    const dtf = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      hour12: false,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    const parts = Object.fromEntries(
      dtf.formatToParts(new Date(t)).map((p) => [p.type, p.value]),
    );
    const asUTC = Date.UTC(
      +parts["year"],
      +parts["month"] - 1,
      +parts["day"],
      +parts["hour"] % 24,
      +parts["minute"],
      +parts["second"],
    );
    return asUTC - t;
  } catch {
    return 0;
  }
}

function fmtTick(t: number, opts: Intl.DateTimeFormatOptions, tz?: string): string {
  try {
    return new Intl.DateTimeFormat("en-GB", { ...opts, timeZone: tz === "local" ? undefined : tz }).format(new Date(t));
  } catch {
    return new Date(t).toLocaleString();
  }
}

function buildXTicks(
  xMin: number,
  xMax: number,
  spanDays: number,
  count: number,
  tz?: string,
): { t: number; label: string }[] {
  if (!Number.isFinite(xMin) || !Number.isFinite(xMax) || xMin === xMax) {
    return [{ t: xMin, label: fmtTick(xMin, { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" }, tz) }];
  }
  // Ticks snap to wall-clock boundaries in the selected zone (whole hours,
  // 6-hour marks, midnights) — never odd times like 06:36 PM.
  const off = (t: number) => tzOffsetMs(tz, t);
  if (spanDays >= 2) {
    const stepDays = spanDays >= 14 ? Math.ceil(spanDays / (count - 1)) : 1;
    const step = stepDays * 86_400_000;
    // First wall-midnight at/after xMin.
    const wall = xMin + off(xMin);
    const dayStart = Math.floor(wall / 86_400_000) * 86_400_000;
    let first = dayStart - off(xMin) + (dayStart <= wall ? step : 0);
    const out: { t: number; label: string }[] = [];
    for (let t = first; t <= xMax && out.length < 16; t += step) {
      out.push({ t, label: fmtTick(t, { day: "2-digit", month: "short" }, tz) });
    }
    return out.length > 0 ? out : [{ t: xMin, label: fmtTick(xMin, { day: "2-digit", month: "short" }, tz) }];
  }
  if (spanDays >= 1) {
    // 00 / 06 / 12 / 18 wall marks inside the window.
    const out: { t: number; label: string }[] = [];
    const wall = xMin + off(xMin);
    const dayStart = Math.floor(wall / 86_400_000) * 86_400_000;
    for (let k = 0; k < 4; k++) {
      const t = dayStart + k * 6 * 3_600_000 - off(xMin);
      if (t >= xMin && t <= xMax) {
        out.push({ t, label: fmtTick(t, { hour: "2-digit", minute: "2-digit", hour12: false }, tz) });
      }
    }
    // Also pin both edges so short ranges keep context.
    if (out.length === 0 || out[0].t > xMin) {
      out.unshift({ t: xMin, label: fmtTick(xMin, { hour: "2-digit", minute: "2-digit", hour12: false }, tz) });
    }
    return out;
  }
  // Sub-day: whole wall hours (30-min when the span is under 3h).
  const step = spanMs(xMin, xMax) < 3 * 3_600_000 ? 1_800_000 : 3_600_000;
  const wall = xMin + off(xMin);
  let t = Math.ceil(wall / step) * step - off(xMin);
  const out: { t: number; label: string }[] = [];
  for (; t <= xMax && out.length < 10; t += step) {
    out.push({ t, label: fmtTick(t, { hour: "2-digit", minute: "2-digit", hour12: false }, tz) });
  }
  return out.length > 0 ? out : [{ t: xMin, label: fmtTick(xMin, { hour: "2-digit", minute: "2-digit", hour12: false }, tz) }];
}

function spanMs(xMin: number, xMax: number): number {
  return xMax - xMin;
}

function fmtXLabel(t: number, spanDays: number): string {
  const d = new Date(t);
  if (spanDays >= 7) {
    // Compact: "Sep 09" — drops year noise for week+ windows.
    return d.toLocaleDateString([], { month: "short", day: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "2-digit" });
}
