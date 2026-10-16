import { useEffect, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import {
  usePoll,
  LineChart,
  toPoints,
  TimeFilterBar,
  timeFilterParams,
  defaultTimeFilter,
  customRangeValid,
  intervalMs,
  fillBuckets,
} from "../components";
import {
  api,
  type WebBreakdownRow,
  type WebErrorsRow,
  type WebRangeParams,
  type WebSitesResponse,
  type WebSourceRow,
  type WebVitalsRow,
} from "../api";
import { trackEvent } from "../track";
import {
  TZ_OPTIONS,
  loadTz,
  saveTz,
  type TzId,
  fmtTs,
  fmtDuration,
  countryName,
  regionName,
  screenLabel,
  parseWindowMs,
  largestRemainder,
  cap,
} from "../webformat";

/**
 * Website analytics. Privacy note (kept as a tooltip, not a footnote):
 * unique visitors are exact per day bucket and SUMMED across buckets —
 * the daily-rotating visitor hash cannot recognize visitors across days.
 */

type ViewMetric = "visitors" | "pageviews" | "sessions";

const FILTER_KEYS = [
  "path",
  "country",
  "region",
  "city",
  "device",
  "browser",
  "os",
  "source",
  "medium",
] as const;
type FilterKey = (typeof FILTER_KEYS)[number];

const FILTER_LABELS: Record<FilterKey, string> = {
  path: "page",
  country: "country",
  region: "region",
  city: "city",
  device: "device",
  browser: "browser",
  os: "os",
  source: "source",
  medium: "source type",
};

const FILTER_HINTS: Record<FilterKey, string> = {
  path: "Filter everything to this page",
  country: "Filter everything to this country",
  region: "Filter everything to this region",
  city: "Filter everything to this city",
  device: "Filter everything to this device",
  browser: "Filter everything to this browser",
  os: "Filter everything to this OS",
  source: "Filter everything to this source",
  medium: "Filter everything to this source type",
};

type Qp = WebRangeParams & Record<string, string | boolean | undefined>;

/** Chip value shown to humans (country codes → names, devices capitalized). */
function chipLabel(key: FilterKey, value: string): string {
  switch (key) {
    case "country":
      return countryName(value);
    case "region":
      return regionName(value);
    case "device":
      return cap(value);
    case "medium":
      return mediumLabel(value);
    default:
      return value;
  }
}

const MEDIUM_LABELS: Record<string, string> = {
  direct: "Direct traffic",
  organic: "Search engines",
  ai: "AI assistants",
  social: "Social media",
  referral: "Referrals",
  email: "Email",
  paid: "Paid ads",
  campaign: "Campaigns",
};

function mediumLabel(m: string): string {
  return MEDIUM_LABELS[m] ?? m;
}

export default function WebAnalyticsPage() {
  const [searchParams, setSearchParams] = useSearchParams();

  // ---- URL-backed state: project + filter chips survive a reload ----
  const site = searchParams.get("site") ?? "";
  const tz = (searchParams.get("tz") as TzId) || loadTz();
  const filters = useMemo(() => {
    const out: Partial<Record<FilterKey, string>> = {};
    for (const k of FILTER_KEYS) {
      const v = searchParams.get(k);
      if (v) out[k] = v;
    }
    return out;
  }, [searchParams]);

  function updateParams(fn: (sp: URLSearchParams) => void) {
    const next = new URLSearchParams(searchParams);
    fn(next);
    setSearchParams(next, { replace: true });
  }
  const setSite = (id: string) =>
    updateParams((sp) => (id ? sp.set("site", id) : sp.delete("site")));
  const setChip = (key: FilterKey, value: string | null) => {
    trackEvent("ui.web.chip", { key, value: value ?? "" });
    updateParams((sp) => (value ? sp.set(key, value) : sp.delete(key)));
  };
  const clearChips = () =>
    updateParams((sp) => FILTER_KEYS.forEach((k) => sp.delete(k)));
  const setTz = (t: TzId) => {
    saveTz(t);
    updateParams((sp) => (t === "Asia/Jakarta" ? sp.delete("tz") : sp.set("tz", t)));
  };

  // ---- range (component state; compare overlay) ----
  const [tf, setTf] = useState(defaultTimeFilter);
  const [compare, setCompare] = useState(true);
  const [view, setView] = useState<ViewMetric>("visitors");
  const [excludeInternal, setExcludeInternal] = useState(true);
  const [exportView, setExportView] = useState<"events" | "sessions">("events");

  // Project list comes from the registry (config seeds + runtime projects).
  const { data: sites, refresh: refreshSites } = usePoll<WebSitesResponse>(
    () => api.webSites(),
    60000,
    [],
  );
  refreshSites; // polled
  const siteOptions = (sites?.sites ?? []).map((s) => s.id);
  const activeSite = site && siteOptions.includes(site) ? site : siteOptions[0] ?? undefined;
  const siteOrigin =
    sites?.sites.find((s) => s.id === activeSite)?.allowed_origins?.[0] ?? "";

  const qp: Qp = useMemo(
    () => ({
      ...timeFilterParams(tf),
      ...(activeSite ? { site: activeSite } : {}),
      ...filters,
      ...(excludeInternal ? {} : { include_internal: true }),
    }),
    [JSON.stringify(timeFilterParams(tf)), activeSite, JSON.stringify(filters), excludeInternal],
  );

  // The full selected range, INCLUDING when the toolbar uses a window
  // shorthand (no from/to params): to≈now, from=to−window. This anchors the
  // chart domain and the previous-period overlay.
  const rangeMs = useMemo(() => {
    if (tf.window === "custom" && customRangeValid(tf)) {
      const from = Date.parse(tf.from);
      const to = Date.parse(tf.to);
      return { from, to, span: Math.max(to - from, 60_000) };
    }
    const to = Date.now();
    const from = to - parseWindowMs(tf.window);
    return { from, to, span: to - from };
  }, [tf.window, tf.from, tf.to]);
  const prevQp: Qp = useMemo(
    () => ({
      ...qp,
      from: new Date(rangeMs.from - rangeMs.span).toISOString(),
      to: new Date(rangeMs.from).toISOString(),
      window: undefined,
    }),
    [JSON.stringify(qp), rangeMs.from, rangeMs.span],
  );

  // Exact headline totals, current + previous (KPI cards + changes).
  const { data: summary } = usePoll(
    () => api.webSummary(qp as WebRangeParams),
    15000,
    [JSON.stringify(qp)],
  );
  const { data: prevSummary } = usePoll(
    () => (compare ? api.webSummary(prevQp as WebRangeParams) : Promise.resolve(null)),
    15000,
    [JSON.stringify(prevQp), compare],
  );
  // Per-metric series double as KPI sparklines; visitors+pageviews feed the
  // main trend (current + previous-period dashed overlay).
  const { data: sparkVisitors } = usePoll(
    () => api.webStats({ ...qp, metric: "visitors", interval: "auto" }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: sparkSessions } = usePoll(
    () => api.webStats({ ...qp, metric: "sessions", interval: "auto" }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: sparkPageviews } = usePoll(
    () => api.webStats({ ...qp, metric: "pageviews", interval: "auto" }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: sparkBounce } = usePoll(
    () => api.webStats({ ...qp, metric: "bounce_rate", interval: "auto" }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: sparkDuration } = usePoll(
    () => api.webStats({ ...qp, metric: "avg_duration", interval: "auto" }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: prevVisitors } = usePoll(
    () =>
      compare
        ? api.webStats({ ...prevQp, metric: "visitors", interval: "auto" })
        : Promise.resolve(null),
    30000,
    [JSON.stringify(prevQp), compare],
  );
  const { data: prevPageviews } = usePoll(
    () =>
      compare
        ? api.webStats({ ...prevQp, metric: "pageviews", interval: "auto" })
        : Promise.resolve(null),
    30000,
    [JSON.stringify(prevQp), compare],
  );
  const { data: realtime } = usePoll(() => api.webRealtime(qp), 10000, [JSON.stringify(qp)]);
  const { data: errors } = usePoll<WebErrorsRow[]>(
    () => api.webErrors({ ...qp } as WebRangeParams).then((r) => r.rows),
    30000,
    [JSON.stringify(qp)],
  );

  // ---- trend shaping: zero-filled buckets over the full selected range ----
  const stepMs = intervalMs(sparkVisitors?.interval);
  const zeroFill = (
    points: { t: string; v: number }[],
    shiftMs = 0,
  ): { x: number; y: number }[] =>
    fillBuckets(toPoints(points, "t", "v"), rangeMs.from + shiftMs, rangeMs.to + shiftMs, stepMs);
  const trendSeries: {
    name: string;
    color: string;
    points: { x: number; y: number }[];
    dashed?: boolean;
  }[] = [
    {
      name: "Visitors",
      color: "#5b8dff",
      points: zeroFill(sparkVisitors?.points ?? []),
    },
    {
      name: "Pageviews",
      color: "#34d399",
      points: zeroFill(sparkPageviews?.points ?? []),
    },
  ];
  if (compare) {
    // Previous period, shifted onto the current axis, dashed, same colour.
    const pv = zeroFill(prevVisitors?.points ?? []).map((p) => ({
      x: p.x + rangeMs.span,
      y: p.y,
    }));
    const pp = zeroFill(prevPageviews?.points ?? []).map((p) => ({
      x: p.x + rangeMs.span,
      y: p.y,
    }));
    trendSeries.push({ name: "Visitors (prev)", color: "#5b8dff", points: pv, dashed: true });
    trendSeries.push({ name: "Pageviews (prev)", color: "#34d399", points: pp, dashed: true });
  }
  const useBars = rangeMs.span / stepMs <= 48;

  function changeLabel(
    cur: number | undefined,
    prev: number | undefined,
    fmt: (n: number) => string,
  ): string {
    if (!compare) return "";
    if (cur === undefined || prev === undefined) return "";
    if (prev === 0) return cur > 0 ? `${fmt(cur)} · new in this period` : "no previous data";
    const pct = ((cur - prev) / prev) * 100;
    const sign = pct > 0.05 ? "+" : pct < -0.05 ? "−" : "±";
    const delta = fmt(Math.abs(cur - prev));
    return `${sign}${delta} (${sign}${Math.abs(pct).toFixed(1)}%) vs prev. period`;
  }

  const onlineNow = realtime?.visitors_5m ?? 0;

  return (
    <div>
      {/* Sticky toolbar: sits BELOW the app header (which owns the avatar
          menu) and aligns with the card grid (no negative margins). */}
      <div className="sticky top-14 z-[1] bg-tremor-background dark:bg-dark-tremor-background border-b border-tremor-border dark:border-dark-tremor-border py-2 mb-4">
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <span className="text-lg font-semibold mr-1">Web analytics</span>
          {siteOptions.length > 0 && (
            <select
              className="cl-input py-1 w-auto"
              value={activeSite ?? ""}
              aria-label="project"
              onChange={(e) => {
                if (e.target.value === "__manage") {
                  window.location.href = `/web/projects${activeSite ? `?project=${encodeURIComponent(activeSite)}` : ""}`;
                  return;
                }
                setSite(e.target.value);
                trackEvent("ui.web.project", { project: e.target.value });
              }}
            >
              {siteOptions.map((id) => (
                <option key={id} value={id}>
                  {id}
                </option>
              ))}
              <option value="__manage">Manage projects…</option>
            </select>
          )}
          <div className="[&>div]:mb-0">
            <TimeFilterBar value={tf} onChange={setTf} showFilter={false} />
          </div>
          <label
            className="flex items-center gap-1 text-xs opacity-80"
            title="Overlay the previous period of the same length (dashed) and show changes on the KPI cards"
          >
            <input
              type="checkbox"
              checked={compare}
              onChange={(e) => setCompare(e.target.checked)}
            />
            Compare
          </label>
          <label
            className="flex items-center gap-1 text-xs opacity-80"
            title="Hides visits from your own IPs/CIDRs. Mark them once per project under Manage → Web projects (exclude_ips); they are excluded by default and this toggle re-includes them."
          >
            <input
              type="checkbox"
              checked={excludeInternal}
              onChange={(e) => setExcludeInternal(e.target.checked)}
            />
            Exclude my visits
          </label>
          <select
            className="cl-input py-1 text-xs w-auto"
            value={tz}
            aria-label="timezone"
            onChange={(e) => setTz(e.target.value as TzId)}
          >
            {TZ_OPTIONS.map((o) => (
              <option key={o.id} value={o.id}>
                {o.label}
              </option>
            ))}
          </select>
          <span className="flex items-center gap-1 text-xs">
            <span className="cl-stat-label">Export:</span>
            <select
              className="cl-input py-1 text-xs w-auto"
              value={exportView}
              aria-label="what to export"
              onChange={(e) => setExportView(e.target.value as "events" | "sessions")}
            >
              <option value="events">page events</option>
              <option value="sessions">sessions</option>
            </select>
            <button
              type="button"
              className="px-2 py-1 rounded text-xs border border-tremor-border"
              onClick={() => {
                trackEvent("ui.web.export", { view: exportView });
                api.webExportCsv({ ...qp, view: exportView } as WebRangeParams & {
                  view: "events" | "sessions";
                }).catch(() => {});
              }}
            >
              Download CSV
            </button>
          </span>
          <span
            className="text-xs tabular-nums opacity-80"
            title="Distinct visitors with activity in the last 5 minutes"
          >
            <span
              className={`inline-block h-2 w-2 rounded-full mr-1 align-middle ${
                onlineNow > 0 ? "bg-emerald-500" : "bg-tremor-content-subtle opacity-40"
              }`}
            />
            {onlineNow} visitors online now
          </span>
          <Link
            to={`/web/projects${activeSite ? `?project=${encodeURIComponent(activeSite)}` : ""}`}
            className="text-xs underline opacity-70 ml-auto"
          >
            Project settings ›
          </Link>
        </div>
        {FILTER_KEYS.some((k) => filters[k]) && (
          <div className="flex flex-wrap items-center gap-1.5 mt-2 text-xs">
            <span className="cl-stat-label">Filters:</span>
            {FILTER_KEYS.filter((k) => filters[k]).map((k) => (
              <button
                key={k}
                type="button"
                onClick={() => setChip(k, null)}
                title="Remove this filter"
                className="px-2 py-0.5 rounded-full border border-tremor-brand text-tremor-brand"
              >
                {FILTER_LABELS[k]}: {chipLabel(k, filters[k] ?? "")} ×
              </button>
            ))}
            <button
              type="button"
              onClick={clearChips}
              className="underline opacity-70"
            >
              Clear all
            </button>
          </div>
        )}
      </div>

      {sites && sites.sites.length === 0 && (
        <div className="p-3 rounded cl-card-plain border border-dashed border-tremor-border mb-4 text-sm">
          No web analytics projects configured yet. Follow the{" "}
          <Link to="/connect?tab=web" className="underline">
            connection guide
          </Link>{" "}
          to enable the collector, set up a same-origin proxy, and embed the
          tracker — or create your first project in{" "}
          <Link to="/web/projects" className="underline">
            Web projects
          </Link>
          .
        </div>
      )}

      {/* One metric everywhere: the toggle re-points every table + share bar. */}
      <div className="flex flex-wrap items-center gap-2 mb-3 text-sm">
        <span className="cl-stat-label">Show:</span>
        {(
          [
            ["visitors", "Visitors"],
            ["pageviews", "Pageviews"],
            ["sessions", "Sessions"],
          ] as [ViewMetric, string][]
        ).map(([k, label]) => (
          <button
            key={k}
            type="button"
            onClick={() => {
              setView(k);
              trackEvent("ui.web.view", { view: k });
            }}
            className={`px-2 py-0.5 rounded text-xs border ${
              view === k
                ? "border-tremor-brand text-tremor-brand"
                : "border-tremor-border opacity-70"
            }`}
          >
            {label}
          </button>
        ))}
      </div>

      <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-4 mb-4">
        <Kpi
          label="Visitors"
          title="Distinct people per day, summed across days (cookieless: the daily hash cannot link days together)"
          value={(summary?.visitors ?? 0).toLocaleString()}
          change={changeLabel(summary?.visitors, prevSummary?.visitors, (n) =>
            n.toLocaleString(),
          )}
          points={toPoints(
            (sparkVisitors?.points ?? []).map((p) => ({ t: p.t, v: p.v })),
            "t",
            "v",
          )}
        />
        <Kpi
          label="Sessions"
          title="Visits: they start on a pageview and close after 30 minutes of inactivity"
          value={(summary?.sessions ?? 0).toLocaleString()}
          change={changeLabel(summary?.sessions, prevSummary?.sessions, (n) =>
            n.toLocaleString(),
          )}
          points={toPoints(
            (sparkSessions?.points ?? []).map((p) => ({ t: p.t, v: p.v })),
            "t",
            "v",
          )}
        />
        <Kpi
          label="Pageviews"
          title="Total pages viewed in range"
          value={(summary?.pageviews ?? 0).toLocaleString()}
          change={changeLabel(summary?.pageviews, prevSummary?.pageviews, (n) =>
            n.toLocaleString(),
          )}
          points={toPoints(
            (sparkPageviews?.points ?? []).map((p) => ({ t: p.t, v: p.v })),
            "t",
            "v",
          )}
        />
        <Kpi
          label="Bounce rate"
          title="Share of sessions that viewed exactly one page and took no deliberate action (no outbound click, download or custom tag)"
          value={`${((summary?.bounce_rate ?? 0) * 100).toFixed(1)}%`}
          invert
          change={changeLabel(summary?.bounce_rate, prevSummary?.bounce_rate, (n) =>
            `${(n * 100).toFixed(1)}%`,
          )}
          points={toPoints(
            (sparkBounce?.points ?? []).map((p) => ({ t: p.t, v: p.v })),
            "t",
            "v",
          )}
        />
        <Kpi
          label="Avg. session duration"
          title="Average engaged time per session, capped at the session's real length"
          value={fmtDuration(summary?.avg_duration_ms ?? 0)}
          change={changeLabel(summary?.avg_duration_ms, prevSummary?.avg_duration_ms, fmtDuration)}
          points={toPoints(
            (sparkDuration?.points ?? []).map((p) => ({ t: p.t, v: p.v })),
            "t",
            "v",
          )}
        />
      </div>

      <div className="p-4 cl-card mb-4">
        <div className="flex flex-wrap items-center justify-between gap-2 mb-2">
          <span className="text-sm font-medium">
            Visitors and pageviews · {intervalLabel(sparkVisitors?.interval)}
          </span>
          <span className="flex items-center gap-3 text-xs opacity-70">
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full bg-[#5b8dff]" /> Visitors
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full bg-[#34d399]" /> Pageviews
            </span>
            {compare && <span>— dashed = previous period</span>}
            <span>{TZ_OPTIONS.find((o) => o.id === tz)?.label}</span>
          </span>
        </div>
        <LineChart
          series={trendSeries}
          height={260}
          yLabel="count"
          bars={useBars}
          integerY
          timeZone={tz === "local" ? undefined : tz}
          domain={[rangeMs.from, rangeMs.to]}
          bucketMs={stepMs}
        />
      </div>

      <PageTabs qp={qp} view={view} siteOrigin={siteOrigin} onPick={(v) => setChip("path", v)} />
      <Sources qp={qp} view={view} onPick={(v) => setChip("medium", v)} />
      <Geography qp={qp} view={view} onPick={(k, v) => setChip(k, v)} />
      <Devices qp={qp} view={view} onPick={(k, v) => setChip(k, v)} />
      <Vitals qp={qp} />
      <Funnel qp={qp} site={activeSite} />
      <Sessions qp={qp} tz={tz} />
      <Links qp={qp} />
      <Utm qp={qp} />
      <CustomEvents qp={qp} />
      <EmptyStrip qp={qp} />
      {(errors ?? []).length > 0 && (
        <div className="p-4 cl-card mb-4">
          <div className="text-sm font-medium mb-2">JS Errors</div>
          <Table
            head={["error", "page", "hits"]}
            rows={(errors ?? []).map((e) => [e.name, e.path, String(e.hits)])}
          />
        </div>
      )}
    </div>
  );
}

function intervalLabel(key: string | undefined): string {
  switch (key) {
    case "1m":
      return "per minute";
    case "5m":
      return "per 5 minutes";
    case "1h":
      return "hourly buckets";
    case "1d":
      return "daily buckets";
    default:
      return "";
  }
}

function Kpi({
  label,
  title,
  value,
  change,
  points,
  invert,
}: {
  label: string;
  title?: string;
  value: string;
  change: string;
  points: { x: number; y: number }[];
  invert?: boolean;
}) {
  const pct = /^([+−±])([\d.]+)%/.exec(change)?.[2];
  const negative = change.startsWith("−");
  const good =
    pct === undefined ? "" : negative !== !!invert ? "text-emerald-600 dark:text-emerald-400" : "text-red-500";
  return (
    <div className="p-4 cl-card" title={title}>
      <div className="cl-stat-label mb-1.5">{label}</div>
      <div className="text-2xl font-semibold tabular-nums">{value}</div>
      <div className="mt-1 h-8 overflow-hidden pointer-events-none">
        <LineChart
          series={[{ name: "spark", color: "#5b8dff", points }]}
          height={32}
          bare
        />
      </div>
      <div className={`mt-1 text-xs tabular-nums ${good || "opacity-70"}`}>{change}</div>
    </div>
  );
}

function midTruncate(s: string, max = 34): string {
  if (s.length <= max) return s;
  return `${s.slice(0, Math.ceil((max - 1) / 2))}…${s.slice(-Math.floor((max - 1) / 2))}`;
}

function isZeroCell(c: string): boolean {
  const n = Number(c.replace(/[%a-zA-Z×\s·]/g, ""));
  return c.trim() !== "" && Number.isFinite(n) && n === 0;
}

/** Value tables: first column pickable (adds a filter chip), numeric
 *  columns right-aligned next to the label, optional share bar + "Show all". */
function Table({
  head,
  rows,
  shareCol,
  hideZero,
  onPick,
  pickHint,
  limit = 10,
  externalHref,
  cell0Text,
}: {
  head: string[];
  rows: string[][];
  /** Column index getting the % share bar (default 1). */
  shareCol?: number;
  hideZero?: boolean;
  onPick?: (value: string) => void;
  pickHint?: string;
  limit?: number;
  /** When set, rows get an ↗ "open" link built as href + first-column value. */
  externalHref?: (value: string) => string;
  /** Display text for column 0 when it differs from the pick value. */
  cell0Text?: (r: string[]) => string;
}) {
  const [showAll, setShowAll] = useState(false);
  const visible0 = hideZero ? rows.filter((r) => !r.slice(1).every(isZeroCell)) : rows;
  if (visible0.length === 0) {
    return <div className="text-sm opacity-50">no data in range</div>;
  }
  const sc = shareCol ?? 1;
  const maxShare = Math.max(
    1,
    ...visible0.map((r) => Number(r[sc].replace(/[^0-9.\-]/g, "")) || 0),
  );
  const shown = showAll ? visible0 : visible0.slice(0, limit);
  return (
    <div>
      <table className="w-full text-sm tabular-nums">
        <thead>
          <tr className="cl-stat-label text-left">
            {head.map((h, i) => (
              <th key={h + i} className={`py-1 pr-3 font-normal ${i > 0 ? "text-right" : ""}`}>
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {shown.map((r, i) => (
            <tr key={i} className="border-t border-tremor-border/40">
              {r.map((c, j) => {
                const share =
                  j === sc
                    ? (Math.max(0, Number(c.replace(/[^0-9.\-]/g, "")) || 0) / maxShare) * 100
                    : 0;
                const c0 = j === 0 && cell0Text ? cell0Text(r) : c;
                const content =
                  j === 0 ? (
                    <span className="inline-flex items-center gap-1 max-w-[20rem]">
                      <span
                        title={c0}
                        className={onPick ? "underline decoration-dotted underline-offset-2" : ""}
                      >
                        {midTruncate(c0)}
                      </span>
                      {externalHref && (
                        <a
                          href={externalHref(c)}
                          target="_blank"
                          rel="noreferrer"
                          title="Open this page"
                          className="opacity-50 hover:opacity-100"
                          onClick={(e) => e.stopPropagation()}
                        >
                          ↗
                        </a>
                      )}
                    </span>
                  ) : (
                    <span title={c}>{c}</span>
                  );
                return (
                  <td
                    key={j}
                    className={`py-1 pr-3 relative max-w-[20rem] ${j > 0 ? "text-right" : ""}`}
                  >
                    {j === sc && (
                      <span
                        className="absolute inset-y-0.5 left-0 bg-tremor-brand/10 rounded-sm pointer-events-none"
                        style={{ width: `${Math.min(100, share)}%` }}
                      />
                    )}
                    <span className="relative">
                      {j === 0 && onPick ? (
                        <button
                          type="button"
                          onClick={() => onPick(r[0])}
                          title={pickHint ?? `Filter by ${r[0]}`}
                          className="text-left"
                        >
                          {content}
                        </button>
                      ) : (
                        content
                      )}
                    </span>
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      {visible0.length > limit && (
        <button
          type="button"
          onClick={() => setShowAll((v) => !v)}
          className="mt-1 text-xs underline opacity-70"
        >
          {showAll ? "Show top only" : `Show all ${visible0.length} rows`}
        </button>
      )}
    </div>
  );
}

/** Top / Entry / Exit pages. Visitors first (the page-wide toggle picks the
 *  primary metric), pageviews second, share bars behind the primary column. */
function PageTabs({
  qp,
  view,
  siteOrigin,
  onPick,
}: {
  qp: Qp;
  view: ViewMetric;
  siteOrigin: string;
  onPick: (v: string) => void;
}) {
  const [tab, setTab] = useState<"top" | "entry" | "exit">("top");
  const { data } = usePoll(
    () => api.webPages({ ...qp, limit: 50 } as WebRangeParams & { limit: number }),
    30000,
    [JSON.stringify(qp)],
  );
  const rows =
    tab === "top" ? data?.top ?? [] : tab === "entry" ? data?.entry ?? [] : data?.exit ?? [];
  const primary = (r: (typeof rows)[number]) =>
    view === "sessions" ? r.sessions : view === "pageviews" ? r.pageviews ?? 0 : r.visitors ?? r.sessions;
  const head =
    tab === "top"
      ? ["page", view, "pageviews"]
      : ["page", view, "bounce %"];
  return (
    <div className="p-4 cl-card mb-4">
      <div className="flex items-center gap-2 mb-2">
        <span className="text-sm font-medium">Pages</span>
        {(["top", "entry", "exit"] as const).map((t) => (
          <button
            key={t}
            type="button"
            onClick={() => setTab(t)}
            className={`px-2 py-0.5 rounded text-xs border ${
              tab === t
                ? "border-tremor-brand text-tremor-brand"
                : "border-tremor-border opacity-70"
            }`}
          >
            {t === "top" ? "Top" : t === "entry" ? "Entry" : "Exit"}
          </button>
        ))}
        <span className="cl-stat-label ml-auto hidden sm:inline">Entry → Exit = first → last page of each visit</span>
      </div>
      <Table
        head={head}
        shareCol={1}
        hideZero
        onPick={onPick}
        pickHint="Filter everything to this page"
        limit={10}
        externalHref={
          siteOrigin
            ? (p) => `${siteOrigin}${p.startsWith("/") ? p : `/${p}`}`
            : undefined
        }
        rows={rows.map((r) =>
          tab === "top"
            ? [
                r.page,
                String(primary(r)),
                String(r.pageviews ?? 0),
              ]
            : [
                r.page,
                String(primary(r)),
                `${((r.bounce_rate ?? 0) * 100).toFixed(1)}%`,
              ],
        )}
      />
    </div>
  );
}

/** Traffic sources: visitors + bounce % per medium (share bars on visitors). */
function Sources({
  qp,
  view,
  onPick,
}: {
  qp: Qp;
  view: ViewMetric;
  onPick: (v: string) => void;
}) {
  const { data } = usePoll(
    () => api.webSources({ ...qp, limit: 12 } as WebRangeParams & { limit: number }),
    30000,
    [JSON.stringify(qp)],
  );
  const sources = [...(data?.sources ?? [])].sort((a, b) => {
    const va = view === "sessions" ? a.sessions : view === "pageviews" ? a.sessions : a.visitors;
    const vb = view === "sessions" ? b.sessions : view === "pageviews" ? b.sessions : b.visitors;
    return vb - va;
  });
  return (
    <div className="p-4 cl-card mb-4">
      <div className="flex items-center justify-between mb-2">
        <span className="text-sm font-medium">Traffic sources</span>
        <Link to="/alerts" className="text-xs underline opacity-70">
          Alert on traffic drop ›
        </Link>
      </div>
      <Table
        head={["source", view, "bounce %"]}
        shareCol={1}
        hideZero
        onPick={onPick}
        pickHint="Filter everything to this source type"
        cell0Text={(r) => mediumLabel(r[0])}
        rows={sources.map((s: WebSourceRow) => [
          s.medium,
          String(view === "sessions" ? s.sessions : s.visitors),
          `${(s.bounce_rate * 100).toFixed(1)}%`,
        ])}
      />
    </div>
  );
}

/** Geography: country / region / city, visitors share bars, names in full. */
function Geography({
  qp,
  view,
  onPick,
}: {
  qp: Qp;
  view: ViewMetric;
  onPick: (k: FilterKey, v: string) => void;
}) {
  const { data: countries } = usePoll(
    () => api.webBreakdown({ ...qp, by: "country", limit: 8 }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: regions } = usePoll(
    () => api.webBreakdown({ ...qp, by: "region", limit: 8 }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: cities } = usePoll(
    () => api.webBreakdown({ ...qp, by: "city", limit: 8 }),
    30000,
    [JSON.stringify(qp)],
  );
  const num = (r: WebBreakdownRow) =>
    view === "sessions" ? r.sessions : view === "pageviews" ? r.pageviews : r.visitors;
  const cols: {
    title: string;
    titleHint?: string;
    key: FilterKey;
    rows: WebBreakdownRow[] | undefined;
    fmt: (v: string) => string;
  }[] = [
    { title: "Country", key: "country", rows: countries?.rows, fmt: (v) => countryName(v) },
    {
      title: "Region",
      titleHint: "Regions come from a GeoIP database (GeoLite2-City) — see the docs for setup",
      key: "region",
      rows: regions?.rows,
      fmt: (v) => regionName(v),
    },
    {
      title: "City",
      titleHint: "Cities come from a GeoIP database (GeoLite2-City) — see the docs for setup",
      key: "city",
      rows: cities?.rows,
      fmt: (v) => (v === "unknown" || v === "(none)" ? "Unknown" : v),
    },
  ];
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Geography ({view})</div>
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        {cols.map((c) => (
          <div key={c.title} title={c.titleHint}>
            <div className="cl-stat-label mb-1">{c.title}</div>
            <Table
              head={[c.title.toLowerCase(), view]}
              shareCol={1}
              hideZero
              onPick={(v) => onPick(c.key, v)}
              pickHint={FILTER_HINTS[c.key]}
              rows={(c.rows ?? []).map((r) => [c.fmt(r.value), String(num(r))])}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

/** Devices: share-of-{view} bars (largest-remainder %, always 100), plus
 *  browser / OS / screen detail. All values clickable as filter chips. */
function Devices({
  qp,
  view,
  onPick,
}: {
  qp: Qp;
  view: ViewMetric;
  onPick: (k: FilterKey, v: string) => void;
}) {
  const { data: devices } = usePoll(
    () => api.webBreakdown({ ...qp, by: "device", limit: 6 }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: browsers } = usePoll(
    () => api.webBreakdown({ ...qp, by: "browser", limit: 6 }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: os } = usePoll(
    () => api.webBreakdown({ ...qp, by: "os", limit: 6 }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: screens } = usePoll(
    () => api.webBreakdown({ ...qp, by: "screen", limit: 6 }),
    30000,
    [JSON.stringify(qp)],
  );
  const num = (r: WebBreakdownRow) =>
    view === "sessions" ? r.sessions : view === "pageviews" ? r.pageviews : r.visitors;
  const devRows = (devices?.rows ?? []).map((r) => ({
    label: cap(r.value),
    n: num(r),
  }));
  const shares = largestRemainder(devRows.map((d) => d.n));
  const mini: { title: string; key: FilterKey; rows: WebBreakdownRow[] | undefined; fmt: (v: string) => string }[] = [
    { title: "Browser", key: "browser", rows: browsers?.rows, fmt: (v) => v },
    { title: "Operating system", key: "os", rows: os?.rows, fmt: (v) => v },
    { title: "Screen width", key: "device" as FilterKey, rows: screens?.rows, fmt: (v) => screenLabel(v) },
  ];
  // screen chips filter on nothing server-side; map screen → os? No: screen
  // has no dedicated chip key — use title hint only (no chip).
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Devices</div>
      <div className="cl-stat-label mb-1.5">share of {view}</div>
      <div className="space-y-1.5 mb-3">
        {devRows.length === 0 && <div className="text-sm opacity-50">no data in range</div>}
        {devRows.map((d, i) => (
          <button
            key={d.label}
            type="button"
            onClick={() => onPick("device", d.label.toLowerCase())}
            title={FILTER_HINTS.device}
            className="w-full flex items-center gap-2 text-sm"
          >
            <span className="w-20 shrink-0 text-left">{d.label}</span>
            <span className="flex-1 h-4 rounded bg-tremor-background-muted dark:bg-dark-tremor-background-muted overflow-hidden block">
              <span
                className="h-full bg-tremor-brand rounded block"
                style={{ width: `${(d.n / Math.max(1, devRows.reduce((a, b) => a + b.n, 0))) * 100}%` }}
              />
            </span>
            <span className="w-10 text-right tabular-nums opacity-80">{shares[i]}%</span>
          </button>
        ))}
      </div>
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        {mini.map((c, idx) => (
          <div key={c.title}>
            <div className="cl-stat-label mb-1">{c.title}</div>
            <Table
              head={[c.title.toLowerCase(), view]}
              shareCol={1}
              hideZero
              onPick={idx < 2 ? (v) => onPick(c.key, v) : undefined}
              pickHint={idx < 2 ? FILTER_HINTS[c.key] : undefined}
              rows={(c.rows ?? []).slice(0, 5).map((r) => [c.fmt(r.value), String(num(r))])}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

/** Web Vitals, renamed for humans: what it measures first, acronym second.
 *  Status is a word, units are consistent, low samples are flagged. */
const VITAL_META: Record<
  string,
  { name: string; full: string; good: number; poor: number; goodLabel: string; fmt: (v: number) => string }
> = {
  LCP: {
    name: "Loading speed",
    full: "Largest Contentful Paint — time until the main content appears",
    good: 2500,
    poor: 4000,
    goodLabel: "≤ 2.5 s",
    fmt: (v) => `${(v / 1000).toFixed(1)} s`,
  },
  INP: {
    name: "Responsiveness",
    full: "Interaction to Next Paint — delay after a tap or click",
    good: 200,
    poor: 500,
    goodLabel: "≤ 200 ms",
    fmt: (v) => `${Math.round(v)} ms`,
  },
  CLS: {
    name: "Visual stability",
    full: "Cumulative Layout Shift — how much the page jumps while loading",
    good: 0.1,
    poor: 0.25,
    goodLabel: "≤ 0.1",
    fmt: (v) => v.toFixed(2),
  },
};

function Vitals({ qp }: { qp: Qp }) {
  const { data } = usePoll(
    () => api.webVitals({ ...qp } as WebRangeParams),
    30000,
    [JSON.stringify(qp)],
  );
  const rows = data?.rows ?? [];
  const byMetric = new Map<string, WebVitalsRow[]>();
  for (const r of rows) {
    const list = byMetric.get(r.metric) ?? [];
    list.push(r);
    byMetric.set(r.metric, list);
  }
  const overall = [...byMetric.entries()].map(([metric, rs]) => {
    const samples = rs.reduce((s, r) => s + r.samples, 0);
    const p75 = samples > 0 ? rs.reduce((s, r) => s + r.p75 * r.samples, 0) / samples : 0;
    const good = samples > 0 ? rs.reduce((s, r) => s + r.good_pct * r.samples, 0) / samples : 0;
    return { metric, p75, good, samples };
  });
  const order = ["LCP", "INP", "CLS"];
  overall.sort((a, b) => order.indexOf(a.metric) - order.indexOf(b.metric));
  const status = (metric: string, p75: number): { word: string; cls: string } => {
    const m = VITAL_META[metric];
    if (!m) return { word: "Good", cls: "text-emerald-600 dark:text-emerald-400" };
    if (p75 <= m.good) return { word: "Good", cls: "text-emerald-600 dark:text-emerald-400" };
    if (p75 <= m.poor)
      return { word: "Needs work", cls: "text-amber-500" };
    return { word: "Poor", cls: "text-red-500" };
  };
  const byPath = (metric: string) =>
    (byMetric.get(metric) ?? [])
      .filter((r) => r.path !== "(none)")
      .sort((a, b) => b.samples - a.samples)
      .slice(0, 5);
  const byDevice = (metric: string) => {
    const map = new Map<string, { samples: number; p75: number }>();
    for (const r of byMetric.get(metric) ?? []) {
      const d = map.get(r.device) ?? { samples: 0, p75: 0 };
      d.samples += r.samples;
      d.p75 += r.p75 * r.samples;
      map.set(r.device, d);
    }
    return [...map.entries()].map(([device, d]) => ({
      device: cap(device),
      p75: d.samples > 0 ? d.p75 / d.samples : 0,
      samples: d.samples,
    }));
  };
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Page experience (Web Vitals, p75)</div>
      {overall.length === 0 ? (
        <div className="text-sm opacity-50">
          No page-experience data in range — vitals are collected by the tracker
          on real page loads.
        </div>
      ) : (
        <table className="w-full text-sm tabular-nums">
          <thead>
            <tr className="cl-stat-label text-left">
              <th className="py-1 pr-3 font-normal">What it measures</th>
              <th className="py-1 pr-3 font-normal text-right">p75</th>
              <th className="py-1 pr-3 font-normal text-right">Good</th>
              <th className="py-1 pr-3 font-normal text-right">% good</th>
              <th className="py-1 pr-3 font-normal text-right">Status</th>
            </tr>
          </thead>
          <tbody>
            {overall.map((o) => {
              const m = VITAL_META[o.metric];
              if (!m) return null;
              const st = status(o.metric, o.p75);
              const low = o.samples < 50;
              return (
                <tr key={o.metric} className="border-t border-tremor-border/40">
                  <td className="py-1.5 pr-3">
                    <span title={m.full} className="cursor-help">
                      {m.name}
                    </span>
                    <span className="block text-xs opacity-60">
                      {o.metric} ({m.full.split("—")[0].trim()}) · {o.samples} samples
                      {low ? " · low data, may change" : ""}
                    </span>
                  </td>
                  <td className="py-1.5 pr-3 text-right">{m.fmt(o.p75)}</td>
                  <td className="py-1.5 pr-3 text-right opacity-70">{m.goodLabel}</td>
                  <td className="py-1.5 pr-3 text-right">{(o.good * 100).toFixed(0)}%</td>
                  <td className={`py-1.5 pr-3 text-right ${st.cls}`}>{st.word}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      {overall.map((o) => {
        const m = VITAL_META[o.metric];
        if (!m) return null;
        return (
          <details key={o.metric} className="mt-2 text-sm">
            <summary className="cursor-pointer opacity-70">
              {m.name} ({o.metric}) by page / device
            </summary>
            <div className="cl-stat-label mt-1">by page</div>
            <Table
              head={["page", "p75", "samples"]}
              rows={byPath(o.metric).map((r) => [r.path, m.fmt(r.p75), String(r.samples)])}
            />
            <div className="cl-stat-label mt-1">by device</div>
            <Table
              head={["device", "p75", "samples"]}
              rows={byDevice(o.metric).map((r) => [r.device, m.fmt(r.p75), String(r.samples)])}
            />
          </details>
        );
      })}
    </div>
  );
}

/** Donation funnel: display-only. The goal itself is edited per project on
 *  the Web projects page (steps can be paths, `event:name` custom events or
 *  `outbound:host` exit links). */
function funnelKey(site: string | undefined): string {
  return `cl-funnel-goal:${site ?? "default"}`;
}

function Funnel({ qp, site }: { qp: Qp; site: string | undefined }) {
  const key = funnelKey(site);
  const [steps, setSteps] = useState<string[]>(() => {
    try {
      const raw = localStorage.getItem(key);
      const arr = raw ? (JSON.parse(raw) as unknown) : [];
      return Array.isArray(arr)
        ? arr.filter((s): s is string => typeof s === "string").slice(0, 8)
        : [];
    } catch {
      return [];
    }
  });
  useEffect(() => {
    try {
      const raw = localStorage.getItem(key);
      const arr = raw ? (JSON.parse(raw) as unknown) : [];
      setSteps(
        Array.isArray(arr)
          ? arr.filter((s): s is string => typeof s === "string").slice(0, 8)
          : [],
      );
    } catch {
      setSteps([]);
    }
  }, [key]);
  const { data } = usePoll(
    () =>
      steps.length >= 2
        ? api.webFunnel({ ...qp, steps: steps.join(",") } as WebRangeParams & { steps: string })
        : Promise.resolve(null),
    30000,
    [JSON.stringify(qp), steps.join("|")],
  );
  const max = Math.max(1, ...(data?.steps ?? []).map((s) => s.sessions));
  return (
    <div className="p-4 cl-card mb-4">
      <div className="flex items-center justify-between mb-2">
        <span className="text-sm font-medium">Donation funnel</span>
        <Link
          to={`/web/projects${site ? `?project=${encodeURIComponent(site)}` : ""}#goal`}
          className="text-xs underline opacity-70"
        >
          {steps.length >= 2 ? "Edit goal" : "Set a goal"} ›
        </Link>
      </div>
      {steps.length < 2 ? (
        <p className="text-sm opacity-70">
          Counts appear once you define a goal — e.g. landing page → program
          page → donation page → WhatsApp click. Goals are managed per project
          under <Link to="/web/projects" className="underline">Web projects</Link>.
        </p>
      ) : (
        <div className="space-y-1.5">
          {(data?.steps ?? []).map((s) => (
            <div key={s.step}>
              <div className="flex justify-between text-xs mb-0.5">
                <span className="font-mono truncate max-w-[70%]" title={s.pattern}>
                  {s.step}. {s.pattern}
                </span>
                <span className="tabular-nums opacity-80">
                  {s.sessions} · {s.step > 0 ? `${(s.drop_off * 100).toFixed(1)}% drop-off` : ""}
                </span>
              </div>
              <div className="h-2 rounded bg-tremor-background-muted dark:bg-dark-tremor-background-muted overflow-hidden">
                <div
                  className="h-full bg-tremor-brand rounded"
                  style={{ width: `${(s.sessions / max) * 100}%` }}
                />
              </div>
            </div>
          ))}
          <div className="pt-1 text-xs opacity-70">
            Overall conversion: {((data?.overall_conversion ?? 0) * 100).toFixed(1)}% reach the
            final step
          </div>
        </div>
      )}
    </div>
  );
}

/** Session explorer: readable rows (no hashes), context per row, headers,
 *  expandable full page sequence. */
function Sessions({ qp, tz }: { qp: Qp; tz: TzId }) {
  const { data } = usePoll(
    () => api.webSessions({ ...qp, limit: 30 } as WebRangeParams & { limit: number }),
    30000,
    [JSON.stringify(qp)],
  );
  const [openId, setOpenId] = useState<string | null>(null);
  const { data: detail } = usePoll(
    () => (openId ? api.webSessionDetail(openId) : Promise.resolve(null)),
    60000,
    [openId ?? ""],
  );
  const rows = data?.sessions ?? [];
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Session explorer</div>
      {rows.length === 0 ? (
        <div className="text-sm opacity-50">no sessions in range</div>
      ) : (
        <table className="w-full text-sm tabular-nums">
          <thead>
            <tr className="cl-stat-label text-left">
              <th className="py-1 pr-3 font-normal">#</th>
              <th className="py-1 pr-3 font-normal">Time</th>
              <th className="py-1 pr-3 font-normal">Entry → Exit</th>
              <th className="py-1 pr-3 font-normal text-right">Pages</th>
              <th className="py-1 pr-3 font-normal text-right">Duration</th>
              <th className="py-1 pr-3 font-normal">Status</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((s, i) => (
              <tr key={s.session_id} className="border-t border-tremor-border/40 align-top">
                <td className="py-1.5 pr-3 opacity-50" title={`session …${s.session_id.slice(-8)}`}>
                  {i + 1}
                </td>
                <td className="py-1.5 pr-3 whitespace-nowrap">
                  <button
                    type="button"
                    className="text-left underline decoration-dotted underline-offset-2"
                    title="Show the full page sequence"
                    onClick={() => {
                      setOpenId(openId === s.session_id ? null : s.session_id);
                      trackEvent("ui.web.session_open", {});
                    }}
                  >
                    {fmtTs(s.start_ts, tz)}
                  </button>
                  <span className="block text-xs opacity-60">
                    {[cap(s.device ?? ""), s.country ? countryName(s.country) : "", s.medium ? mediumLabel(s.medium) : ""]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </td>
                <td className="py-1.5 pr-3">
                  <span className="max-w-[16rem] truncate block" title={`${s.entry_path ?? "—"} → ${s.exit_path ?? "—"}`}>
                    {s.entry_path ?? "—"} → {s.exit_path ?? "—"}
                  </span>
                  <span className="block text-xs opacity-60">{s.source ? s.source : ""}</span>
                </td>
                <td className="py-1.5 pr-3 text-right">{s.pageviews}</td>
                <td className="py-1.5 pr-3 text-right">{fmtDuration(s.engaged_ms)}</td>
                <td className="py-1.5 pr-3">
                  {s.is_bounce ? (
                    <span className="text-xs text-amber-600 dark:text-amber-400">Bounce</span>
                  ) : (
                    <span className="text-xs opacity-60">Engaged</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {openId && (
        <div className="mt-2 pl-3 border-l border-tremor-border text-xs space-y-0.5">
          {(detail?.events ?? []).map((e, i) => (
            <div key={i} className="flex flex-wrap gap-x-2 tabular-nums">
              <span className="opacity-60">{fmtTs(e.ts, tz)}</span>
              <span className="font-medium">
                {e.event === "pageview" ? "viewed" : e.event}
              </span>
              {e.name ? <span className="font-mono">{e.name}</span> : null}
              {e.path ? (
                <span className="truncate max-w-[16rem] opacity-80" title={e.path}>
                  {e.path}
                </span>
              ) : null}
              {e.trace_id ? (
                <Link
                  to={`/?filter=${encodeURIComponent(`trace_id:${e.trace_id}`)}&window=24h`}
                  className="underline opacity-70"
                >
                  backend logs →
                </Link>
              ) : null}
            </div>
          ))}
          {detail && detail.events.length === 0 && <div className="opacity-50">loading…</div>}
        </div>
      )}
    </div>
  );
}

/** Exit links + downloads; the whole panel hides while nothing is tracked
 *  (the empty strip says so instead). */
function Links({ qp }: { qp: Qp }) {
  const { data: outbound } = usePoll(
    () => api.webLinks({ ...qp, kind: "outbound", limit: 10 } as WebRangeParams & { kind?: string; limit?: number }),
    30000,
    [JSON.stringify(qp)],
  );
  const { data: downloads } = usePoll(
    () => api.webLinks({ ...qp, kind: "download", limit: 10 } as WebRangeParams & { kind?: string; limit?: number }),
    30000,
    [JSON.stringify(qp)],
  );
  if ((outbound?.rows ?? []).length === 0 && (downloads?.rows ?? []).length === 0) return null;
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Exit links & downloads</div>
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
        {(outbound?.rows ?? []).length > 0 && (
          <div>
            <div className="cl-stat-label mb-1">Exit links</div>
            <Table
              head={["destination", "clicks"]}
              rows={(outbound?.rows ?? []).map((r) => [`${r.host ?? ""}${r.path}`, String(r.hits)])}
            />
          </div>
        )}
        {(downloads?.rows ?? []).length > 0 && (
          <div>
            <div className="cl-stat-label mb-1">Downloads</div>
            <Table
              head={["file", "hits"]}
              rows={(downloads?.rows ?? []).map((r) => [r.path, String(r.hits)])}
            />
          </div>
        )}
      </div>
    </div>
  );
}

/** UTM breakdown; hides entirely while no campaign links are in use. */
function Utm({ qp }: { qp: Qp }) {
  const { data: src } = usePoll(
    () => api.webBreakdown({ ...qp, by: "utm_source", limit: 5 }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: med } = usePoll(
    () => api.webBreakdown({ ...qp, by: "utm_medium", limit: 5 }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: camp } = usePoll(
    () => api.webBreakdown({ ...qp, by: "utm_campaign", limit: 5 }),
    60000,
    [JSON.stringify(qp)],
  );
  const cols = [
    { title: "UTM source", rows: src?.rows },
    { title: "UTM medium", rows: med?.rows },
    { title: "UTM campaign", rows: camp?.rows },
  ];
  const anyTagged = cols.some((c) => (c.rows ?? []).some((r) => r.value !== "(none)"));
  if (!anyTagged) return null;
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">UTM tracking</div>
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        {cols.map((c) => (
          <div key={c.title}>
            <div className="cl-stat-label mb-1">{c.title}</div>
            <Table
              head={["value", "pageviews", "visitors"]}
              hideZero
              rows={(c.rows ?? []).map((r) => [r.value, String(r.pageviews), String(r.visitors)])}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

/** Custom event tags; hides while none exist (strip covers it). */
function CustomEvents({ qp }: { qp: Qp }) {
  const { data } = usePoll(
    () => api.webCustom({ ...qp, limit: 20 } as WebRangeParams & { limit: number }),
    30000,
    [JSON.stringify(qp)],
  );
  if ((data?.rows ?? []).length === 0) return null;
  return (
    <div className="p-4 cl-card mb-4">
      <div className="text-sm font-medium mb-2">Custom tags</div>
      <Table
        head={["tag", "hits", "visitors"]}
        rows={(data?.rows ?? []).map((r) => [r.name, String(r.hits), String(r.visitors)])}
      />
    </div>
  );
}

/** One honest strip instead of a page of empty panels. */
function EmptyStrip({ qp }: { qp: Qp }) {
  const { data: outbound } = usePoll(
    () => api.webLinks({ ...qp, kind: "outbound", limit: 1 } as WebRangeParams & { kind?: string; limit?: number }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: downloads } = usePoll(
    () => api.webLinks({ ...qp, kind: "download", limit: 1 } as WebRangeParams & { kind?: string; limit?: number }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: utm } = usePoll(
    () => api.webBreakdown({ ...qp, by: "utm_source", limit: 5 }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: custom } = usePoll(
    () => api.webCustom({ ...qp, limit: 1 } as WebRangeParams & { limit: number }),
    60000,
    [JSON.stringify(qp)],
  );
  const { data: errors } = usePoll<WebErrorsRow[]>(
    () => api.webErrors({ ...qp } as WebRangeParams).then((r) => r.rows),
    60000,
    [JSON.stringify(qp)],
  );
  const missing: string[] = [];
  if ((outbound?.rows ?? []).length === 0) missing.push("exit links");
  if ((downloads?.rows ?? []).length === 0) missing.push("downloads");
  if (!(utm?.rows ?? []).some((r) => r.value !== "(none)")) missing.push("UTM tags");
  if ((custom?.rows ?? []).length === 0) missing.push("custom tags");
  if ((errors ?? []).length === 0) missing.push("JS errors");
  if (missing.length === 0) return null;
  return (
    <div className="p-3 cl-card mb-4 text-sm flex flex-wrap items-center gap-x-2 gap-y-1">
      <span className="opacity-70">Not yet tracked: {missing.join(" · ")}</span>
      <Link to="/connect?tab=web" className="text-xs underline opacity-70 ml-auto">
        Setup guide ›
      </Link>
    </div>
  );
}
