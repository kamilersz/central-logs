import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Button,
  Card,
  Flex,
  Select,
  SelectItem,
  Table,
  TableHead,
  TableHeaderCell,
  TableBody,
  TableRow,
  TableCell,
} from "@tremor/react";
import {
  ArrowDownTrayIcon,
  ArrowPathIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  SparklesIcon,
} from "@heroicons/react/24/outline";
import { api, type LogRow, type LogsParams, type SchemaResponse } from "../api";
import { LevelBadge, toLocalInput } from "../components";

const WINDOWS = ["5m", "15m", "1h", "6h", "24h", "7d", "30d", "90d"] as const;

/** A relative window shorthand the API accepts (e.g. "5d", "12h"). */
const SHORTHAND_RE = /^\d+[smhdw]$/;

/** Export page size — the server clamps this to its own max. */
const EXPORT_LIMIT = 10000;

// Default filter hides central-logs' own audit/internal traffic so the
// explorer opens on *user* services.
const DEFAULT_FILTER = "-service:central-logs";
const DEFAULT_WINDOW = "1h";

/** True when the custom from/to pair forms a usable range. */
function customRangeValid(from: string, to: string): boolean {
  const f = Date.parse(from);
  const t = Date.parse(to);
  return !Number.isNaN(f) && !Number.isNaN(t) && t > f;
}

// Small text helpers — keeps JSX readable without depending on Tremor prop
// quirks that differ across versions.
function Muted({ children, className = "" }: { children: React.ReactNode; className?: string }) {
  return (
    <span className={`text-tremor-content-subtle dark:text-dark-tremor-content-subtle ${className}`}>
      {children}
    </span>
  );
}

// Custom-range datetime inputs, styled like the filter input above.
const customRangeCls =
  "px-2 py-1 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong";

export default function LogsPage() {
  // ?filter=…&window=… (or &from=…&to=…) pre-fills the explorer (used by the
  // deep-links on dashboard panels). A window shorthand that isn't a preset
  // (e.g. ?window=5d) is kept and added to the dropdown so the backend gets
  // exactly the linked range; an explicit from/to pair switches to custom.
  const [searchParams] = useSearchParams();
  const paramWindow = searchParams.get("window");
  const paramFrom = searchParams.get("from");
  const paramTo = searchParams.get("to");
  const [filter, setFilter] = useState(searchParams.get("filter") ?? DEFAULT_FILTER);
  const [extraWindows, setExtraWindows] = useState<string[]>(() =>
    paramWindow && SHORTHAND_RE.test(paramWindow) && !(WINDOWS as readonly string[]).includes(paramWindow)
      ? [paramWindow]
      : [],
  );
  const windowOptions = [...WINDOWS, ...extraWindows, "custom"];
  const [windowSel, setWindowSel] = useState<string>(
    paramFrom && paramTo
      ? "custom"
      : paramWindow && (SHORTHAND_RE.test(paramWindow) || paramWindow === "custom")
        ? paramWindow
        : DEFAULT_WINDOW,
  );
  const [customFrom, setCustomFrom] = useState(
    toLocalInput(paramFrom ? new Date(paramFrom) : new Date(Date.now() - 3600_000)),
  );
  const [customTo, setCustomTo] = useState(
    toLocalInput(paramTo ? new Date(paramTo) : new Date()),
  );
  const [rows, setRows] = useState<LogRow[]>([]);
  const [total, setTotal] = useState(0);
  const [truncated, setTruncated] = useState(false);
  const [filterError, setFilterError] = useState<string | null>(null);
  const [schema, setSchema] = useState<SchemaResponse | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<string | null>(null);
  const [aiBusy, setAiBusy] = useState(false);
  const [aiError, setAiError] = useState<string | null>(null);
  const [aiRaw, setAiRaw] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<string>("");

  useEffect(() => {
    api.schema().then(setSchema).catch(() => {});
  }, []);

  // Query params for the current selection: relative window, or explicit
  // ISO from/to when "custom" is picked.
  const rangeParams: LogsParams = useMemo(
    () =>
      windowSel === "custom"
        ? { from: new Date(customFrom).toISOString(), to: new Date(customTo).toISOString() }
        : { window: windowSel },
    [windowSel, customFrom, customTo],
  );
  const customInvalid = windowSel === "custom" && !customRangeValid(customFrom, customTo);

  async function refresh() {
    if (customInvalid) return;
    setLoading(true);
    try {
      const resp = await api.logs({ filter, ...rangeParams, limit: 200 });
      setRows(resp.rows);
      setTotal(resp.total_matched);
      setTruncated(resp.truncated);
      setFilterError(resp.filter_error);
      setLastUpdated(new Date().toLocaleTimeString());
    } catch (e) {
      setFilterError(String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    const t = setTimeout(refresh, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter, rangeParams]);

  async function exportXlsx() {
    if (customInvalid) return;
    setExporting(true);
    setExportError(null);
    try {
      await api.logsExport({ filter, ...rangeParams, limit: EXPORT_LIMIT });
    } catch (e) {
      setExportError(String(e));
    } finally {
      setExporting(false);
    }
  }

  async function askAi() {
    setAiBusy(true);
    setAiError(null);
    setAiRaw(null);
    try {
      const nl = window.prompt(
        "Describe what you want to see (e.g. 'errors for user 42 in the last hour'):",
      );
      if (!nl) return;
      const resp = await api.aiQuery(
        nl,
        windowSel === "custom" ? undefined : windowSel,
      );
      setFilter(resp.filter);
      setAiRaw(`[${resp.provider}] ${resp.raw}`);
    } catch (e) {
      setAiError(String(e));
    } finally {
      setAiBusy(false);
    }
  }

  const toggleExpand = (id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const suggestions = useMemo(() => schema?.filterable_columns ?? [], [schema]);

  return (
    <div className="min-h-screen p-6 max-w-7xl mx-auto">
      <Flex className="mb-5">
        <div>
          <h1 className="cl-title">Logs Explorer</h1>
          <p className="cl-subtitle text-sm">search, filter and inspect everything flowing through the pipeline</p>
        </div>
        <Flex className="gap-3 justify-end">
          <Muted className="text-xs">updated {lastUpdated}</Muted>
          <Button
            icon={ArrowDownTrayIcon}
            variant="secondary"
            onClick={exportXlsx}
            loading={exporting}
            disabled={customInvalid}
          >
            Export Excel
          </Button>
          <Button
            icon={ArrowPathIcon}
            variant="secondary"
            onClick={refresh}
            loading={loading}
            disabled={customInvalid}
          >
            Refresh
          </Button>
        </Flex>
      </Flex>
      {exportError && (
        <div className="mb-4 cl-error-banner">Export failed: {exportError}</div>
      )}

      {/* Filter bar */}
      <Card className="mb-4">
        <div className="grid grid-cols-3 gap-3 items-end">
          <div className="col-span-2">
            <Muted className="text-xs">Filter DSL — AND / OR / (grouping) / -exclude</Muted>
            <input
              type="text"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="service:api OR service:web level:error -service:internal"
              className="cl-mono mt-1 w-full px-3 py-2 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong placeholder:text-tremor-content-subtle dark:placeholder:text-dark-tremor-content-subtle focus:border-tremor-brand dark:focus:border-dark-tremor-brand"
            />
            <div className="mt-1.5 flex flex-wrap gap-1">
              {suggestions.slice(0, 12).map((s: string) => (
                <button
                  key={s}
                  onClick={() => setFilter((f) => `${f} ${s}:`.trim())}
                  className="cl-mono text-[11px] px-2 py-0.5 bg-tremor-background-muted dark:bg-dark-tremor-background-muted rounded border border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:border-tremor-brand dark:hover:border-dark-tremor-brand hover:text-tremor-brand dark:hover:text-dark-tremor-brand transition-colors"
                >
                  {s}
                </button>
              ))}
            </div>
          </div>
          <div>
            <Muted className="text-xs">Window</Muted>
            <Select value={windowSel} onValueChange={(v) => setWindowSel(v)} className="mt-1">
              {windowOptions.map((w) => (
                <SelectItem key={w} value={w}>
                  {w === "custom" ? "custom range…" : `last ${w}`}
                </SelectItem>
              ))}
            </Select>
            {windowSel === "custom" && (
              <div className="mt-2 flex items-center gap-2">
                <input
                  type="datetime-local"
                  value={customFrom}
                  onChange={(e) => setCustomFrom(e.target.value)}
                  className={customRangeCls}
                  aria-label="from"
                />
                <span className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle text-sm">→</span>
                <input
                  type="datetime-local"
                  value={customTo}
                  onChange={(e) => setCustomTo(e.target.value)}
                  className={customRangeCls}
                  aria-label="to"
                />
              </div>
            )}
            {customInvalid && (
              <div className="mt-1 text-xs text-red-600 dark:text-red-400">from must be before to</div>
            )}
          </div>
        </div>
        <Flex className="mt-3 justify-start gap-2">
          <Button
            icon={SparklesIcon}
            variant="light"
            color="indigo"
            onClick={askAi}
            loading={aiBusy}
          >
            Ask AI
          </Button>
          {aiError && (
            <div className="flex-1 cl-error-banner">
              {aiError}
            </div>
          )}
          {aiRaw && (
            <Muted className="text-xs truncate max-w-md cl-mono">{aiRaw}</Muted>
          )}
        </Flex>
        {filterError && (
          <div className="mt-3 cl-error-banner">
            Filter error: {filterError}
          </div>
        )}
      </Card>

      {/* Summary cards */}
      <div className="grid grid-cols-3 gap-4 mb-4">
        <Card>
          <div className="cl-stat-label">matched</div>
          <div className="cl-stat-value">{total.toLocaleString()}</div>
        </Card>
        <Card>
          <div className="cl-stat-label">window</div>
          <div className="cl-stat-value">
            {windowSel === "custom"
              ? `${customFrom.replace("T", " ")} → ${customTo.replace("T", " ")}`
              : windowSel}
          </div>
        </Card>
        <Card>
          <div className="cl-stat-label">shown</div>
          <div className="cl-stat-value">{rows.length}</div>
          {truncated && <span className="text-amber-400 text-sm">truncated</span>}
        </Card>
      </div>

      {/* Results */}
      <Card>
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell className="w-8" />
              <TableHeaderCell className="w-44">time</TableHeaderCell>
              <TableHeaderCell className="w-32">service</TableHeaderCell>
              <TableHeaderCell className="w-24">level</TableHeaderCell>
              <TableHeaderCell>message</TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map((r, i) => {
              const id = `${r.ts}-${i}`;
              const isOpen = expanded.has(id);
              return (
                <>
                  <TableRow
                    key={id}
                    className="cursor-pointer hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
                    onClick={() => toggleExpand(id)}
                  >
                    <TableCell>
                      {isOpen ? (
                        <ChevronDownIcon className="w-4 h-4 text-tremor-content-subtle" />
                      ) : (
                        <ChevronRightIcon className="w-4 h-4 text-tremor-content-subtle" />
                      )}
                    </TableCell>
                    <TableCell>
                      <span className="cl-ts">
                        {new Date(r.ts).toLocaleString()}
                      </span>
                    </TableCell>
                    <TableCell>
                      <span className="cl-mono text-[13px] text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis">
                        {r.service ?? "—"}
                      </span>
                    </TableCell>
                    <TableCell>
                      <LevelBadge level={r.level ?? "—"} />
                    </TableCell>
                    <TableCell>
                      <div className="cl-msg truncate max-w-xl">
                        {r.message ?? ""}
                        {r.hot?.user_name != null && (
                          <span className="ml-2 text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                            @{String(r.hot.user_name)}
                          </span>
                        )}
                        {r.hot?.user_id != null && (
                          <span className="ml-1 text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                            ({r.hot.user_id})
                          </span>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
                  {isOpen && (
                    <TableRow key={`${id}-detail`} className="bg-tremor-background-muted dark:bg-dark-tremor-background-muted">
                      <TableCell colSpan={5}>
                        <LogDetail row={r} />
                      </TableCell>
                    </TableRow>
                  )}
                </>
              );
            })}
          </TableBody>
        </Table>
        {rows.length === 0 && !loading && (
          <div className="cl-empty">
            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" className="text-tremor-content-subtle dark:text-dark-tremor-content-subtle" opacity="0.5">
              <circle cx="11" cy="11" r="7" />
              <path d="m21 21-4.3-4.3" strokeLinecap="round" />
            </svg>
            <Muted>No rows matched this filter and window.</Muted>
          </div>
        )}
      </Card>
    </div>
  );
}

function LogDetail({ row }: { row: LogRow }) {
  // Hot attributes first (user attribution), then built-ins.
  const hotEntries = Object.entries(row.hot ?? {}).map(
    ([k, v]): [string, string | null] => [k, v == null ? null : String(v)],
  );
  const fields: Array<[string, string | null]> = [
    ...hotEntries,
    ["source_host", row.source_host],
    ["trace_id", row.trace_id],
    ["span_id", row.span_id],
    ["geo_country", row.geo_country],
    ["protocol", row.protocol],
    ["raw_len", row.raw_len != null ? String(row.raw_len) : null],
    ["insert_ts", row.insert_ts],
  ];
  return (
    <div className="grid grid-cols-2 gap-4 p-3">
      <div>
        <Muted className="text-xs">fields</Muted>
        <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1">
          {fields.map(([k, v]) =>
            v ? (
              <div key={k} className="text-xs">
                <Muted>{k}</Muted>: <span>{v}</span>
              </div>
            ) : null,
          )}
        </div>
      </div>
      <div>
        <Muted className="text-xs">attributes (JSON residue)</Muted>
        <pre className="mt-2 text-xs overflow-auto max-h-48 bg-tremor-background dark:bg-dark-tremor-background p-2 rounded">
          {JSON.stringify(row.attributes ?? {}, null, 2)}
        </pre>
      </div>
    </div>
  );
}
