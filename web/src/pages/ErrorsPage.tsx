import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Button, Card, Select, SelectItem } from "@tremor/react";
import { usePoll, LevelBadge } from "../components";
import DsnWizard from "../components/DsnWizard";
import {
  api,
  type ErrorGroup,
  type ErrorGroupDetail,
  type ErrorGroupSample,
} from "../api";

const WINDOWS = ["1h", "24h", "7d", "30d"];
const STATUS_TABS: { value: "unresolved" | "all" | "resolved" | "ignored"; label: string }[] = [
  { value: "unresolved", label: "Unresolved" },
  { value: "all", label: "All" },
  { value: "resolved", label: "Resolved" },
  { value: "ignored", label: "Ignored" },
];

function relTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  const s = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function shortHash(fp: string, n = 8): string {
  return fp.length > n ? fp.slice(0, n) + "…" : fp;
}

/** Mini bar sparkline from per-minute rollup points. */
function Spark({ points }: { points: { ts: string; count: number }[] }) {
  if (!points.length) {
    return (
      <div className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
        no events in the last 24h
      </div>
    );
  }
  const max = Math.max(...points.map((p) => p.count), 1);
  return (
    <div className="flex items-end gap-[2px] h-12" aria-label="events over the last 24 hours">
      {points.map((p) => (
        <div
          key={p.ts}
          title={`${p.count} @ ${new Date(p.ts).toLocaleTimeString()}`}
          className="flex-1 min-w-[2px] rounded-sm bg-tremor-brand-emphasis dark:bg-dark-tremor-brand-emphasis opacity-80"
          style={{ height: `${Math.max(6, (p.count / max) * 100)}%` }}
        />
      ))}
    </div>
  );
}

/** Stacktrace viewer: lines ending in " #" are in-app frames — highlight. */
function StackView({ stack }: { stack: string }) {
  const lines = stack.split("\n");
  return (
    <pre className="cl-mono text-[11px] leading-relaxed overflow-x-auto p-3 rounded-md bg-tremor-background-subtle dark:bg-dark-tremor-background-subtle border border-tremor-border dark:border-dark-tremor-border">
      {lines.map((line, i) => {
        const inApp = line.trimEnd().endsWith("#");
        const text = inApp ? line.trimEnd().slice(0, -1).trimEnd() : line;
        return (
          <div key={i} className={inApp ? "text-tremor-brand dark:text-dark-tremor-brand font-semibold" : "text-tremor-content-subtle dark:text-dark-tremor-content-subtle"}>
            {text}
            {inApp ? "  ● in-app" : ""}
          </div>
        );
      })}
    </pre>
  );
}

// =====================================================================
// List
// =====================================================================

export default function ErrorsPage() {
  const navigate = useNavigate();
  const [status, setStatus] = useState<"unresolved" | "all" | "resolved" | "ignored">("unresolved");
  const [window_, setWindow] = useState("24h");
  const [sort, setSort] = useState<"recent" | "count">("recent");
  const [search, setSearch] = useState("");
  const [q, setQ] = useState("");
  const [wizardOpen, setWizardOpen] = useState(false);

  // Debounce the search box into the server-side `q` filter.
  useEffect(() => {
    const t = setTimeout(() => setQ(search.trim()), 250);
    return () => clearTimeout(t);
  }, [search]);

  const { data, error, loading, refresh } = usePoll<{ groups: ErrorGroup[]; total: number }>(
    () => api.listErrorGroups({ window: window_, status, sort, q: q || undefined, limit: 200 }),
    15000,
    [status, window_, sort, q],
  );

  const groups = data?.groups ?? [];

  return (
    <div>
      <div className="flex items-start justify-between mb-1 gap-4">
        <div>
          <h2 className="cl-title">Errors</h2>
          <p className="cl-subtitle text-sm">
            Grouped exceptions and error logs. Point any Sentry SDK at this server to capture
            stacktraces — groups update live as events flow through the pipeline.
          </p>
        </div>
        <Button
          size="xs"
          variant="secondary"
          onClick={() => setWizardOpen(true)}
          title="Create an insert-scoped API key and copy a ready-to-paste DSN + SDK snippet."
        >
          ✦ Setup Sentry capture
        </Button>
      </div>

      {wizardOpen && <DsnWizard onClose={() => setWizardOpen(false)} />}

      {/* Controls */}
      <div className="flex flex-wrap items-center gap-3 mb-4 mt-4">
        <input
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder="Search by title or exception type…"
          className="flex-1 min-w-[220px] px-3 py-2 text-sm rounded-md border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background text-tremor-content dark:text-dark-tremor-content focus:outline-none focus:ring-2 focus:ring-tremor-brand"
        />
        <Select value={status} onValueChange={(v) => setStatus(v as typeof status)} className="w-40">
          {STATUS_TABS.map((t) => (
            <SelectItem key={t.value} value={t.value}>
              {t.label}
            </SelectItem>
          ))}
        </Select>
        <Select value={window_} onValueChange={(v) => setWindow(v)} className="w-32">
          {WINDOWS.map((w) => (
            <SelectItem key={w} value={w}>
              {w}
            </SelectItem>
          ))}
        </Select>
        <Select value={sort} onValueChange={(v) => setSort(v as "recent" | "count")} className="w-40">
          <SelectItem value="recent">Newest activity</SelectItem>
          <SelectItem value="count">Most events</SelectItem>
        </Select>
      </div>

      {error && <div className="cl-error-banner mb-4">{error}</div>}
      {loading && <div className="text-tremor-content-subtle text-sm mb-3">loading…</div>}

      {!loading && !error && (
        <p className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-2">
          {data?.total ?? 0} group{(data?.total ?? 0) === 1 ? "" : "s"} active in {window_}
          {q ? ` matching “${q}”` : ""}
        </p>
      )}

      {/* Group table */}
      <div className="space-y-2">
        {groups.map((g) => (
          <button
            key={g.fingerprint}
            type="button"
            onClick={() => navigate(`/errors/${encodeURIComponent(g.fingerprint)}`)}
            className="w-full text-left rounded-lg border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background px-4 py-3 hover:border-tremor-brand dark:hover:border-dark-tremor-brand transition-colors cursor-pointer"
          >
            <div className="flex items-start gap-3">
              <div className="pt-0.5">
                <LevelBadge level={g.level ?? "error"} />
              </div>
              <div className="flex-1 min-w-0">
                <div className="text-sm font-medium text-tremor-content-strong dark:text-dark-tremor-content-strong truncate">
                  {g.title || "(untitled error)"}
                </div>
                <div className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-0.5 flex flex-wrap gap-x-3">
                  <span>{g.service || "unknown"}</span>
                  {g.exception_type && <span className="cl-mono">{g.exception_type}</span>}
                  <span className="cl-mono">{shortHash(g.fingerprint)}</span>
                  {g.status !== "unresolved" && (
                    <span className="uppercase tracking-wide">{g.status}</span>
                  )}
                </div>
              </div>
              <div className="text-right shrink-0">
                <div className="text-sm font-semibold cl-mono text-tremor-content-strong dark:text-dark-tremor-content-strong">
                  {g.total_count.toLocaleString()}
                </div>
                <div className="text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                  events · last {relTime(g.last_seen)}
                </div>
              </div>
            </div>
          </button>
        ))}
      </div>

      {!loading && !error && groups.length === 0 && (
        <Card>
          <div className="cl-empty">
            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" opacity="0.5">
              <path d="M12 9v4m0 4h.01M10.3 3.9L1.8 18a2 2 0 001.7 3h17a2 2 0 001.7-3L13.7 3.9a2 2 0 00-3.4 0z" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            <span className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
              {q
                ? `No groups match “${q}”.`
                : "No error groups yet. Send an error via POST /v1/logs (level: error) or point a Sentry SDK DSN at this server."}
            </span>
          </div>
        </Card>
      )}
    </div>
  );
}

// =====================================================================
// Detail
// =====================================================================

export function ErrorGroupDetailPage() {
  const { fingerprint = "" } = useParams();
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const { data: g, error, loading, refresh } = usePoll<ErrorGroupDetail>(
    () => api.getErrorGroup(fingerprint),
    15000,
    [fingerprint],
  );

  async function act(fn: () => Promise<unknown>) {
    setBusy(true);
    setActionError(null);
    try {
      await fn();
      refresh();
    } catch (e) {
      setActionError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Newest sample first (first-seen sample stays at index 0 server-side).
  const samples: ErrorGroupSample[] = useMemo(() => {
    const s = g?.samples ?? [];
    return s.length > 1 ? [s[s.length - 1], ...s.slice(0, -1)] : s;
  }, [g]);

  return (
    <div>
      <Link
        to="/errors"
        className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:text-tremor-brand dark:hover:text-dark-tremor-brand"
      >
        ← All error groups
      </Link>

      {loading && <div className="text-tremor-content-subtle text-sm mt-4">loading…</div>}
      {error && <div className="cl-error-banner mt-4">{error}</div>}

      {g && (
        <div className="mt-3 space-y-4">
          {/* Header */}
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <LevelBadge level={g.level ?? "error"} />
                {g.status !== "unresolved" && (
                  <span className="cl-pill bg-slate-400/15 text-slate-600 dark:text-slate-300 uppercase tracking-wide">
                    <span className="cl-pill-dot" />
                    {g.status}
                  </span>
                )}
              </div>
              <h2 className="cl-title mt-2 break-all">{g.title || "(untitled error)"}</h2>
              <div className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-1 flex flex-wrap gap-x-4 gap-y-1">
                <span>service: <span className="cl-mono">{g.service || "unknown"}</span></span>
                {g.exception_type && <span>type: <span className="cl-mono">{g.exception_type}</span></span>}
                <span>fingerprint: <span className="cl-mono">{g.fingerprint}</span></span>
                <span>first seen: {relTime(g.first_seen)}</span>
                <span>last seen: {relTime(g.last_seen)}</span>
                <span>{g.total_count.toLocaleString()} retained events</span>
              </div>
            </div>

            {/* Actions */}
            <div className="flex flex-wrap gap-2">
              <a
                href={`/?filter=${encodeURIComponent(`fingerprint:${g.fingerprint}`)}&window=24h`}
                className="px-2.5 py-1 rounded-md text-sm border border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle"
              >
                View events →
              </a>
              {g.status === "unresolved" ? (
                <>
                  <Button size="xs" variant="secondary" loading={busy} onClick={() => act(() => api.resolveErrorGroup(g.fingerprint))}>
                    Resolve
                  </Button>
                  <Button size="xs" variant="light" loading={busy} onClick={() => act(() => api.ignoreErrorGroup(g.fingerprint))}>
                    Ignore
                  </Button>
                </>
              ) : (
                <Button size="xs" variant="secondary" loading={busy} onClick={() => act(() => api.unresolveErrorGroup(g.fingerprint))}>
                  Re-open
                </Button>
              )}
            </div>
          </div>

          {actionError && <div className="cl-error-banner">{actionError}</div>}

          <div className="grid grid-cols-1 xl:grid-cols-3 gap-4 items-start">
            {/* Left: explain + samples */}
            <div className="xl:col-span-2 space-y-4">
              <ExplainCard fingerprint={g.fingerprint} />

              {samples.length === 0 && (
                <Card>
                  <div className="cl-empty text-sm text-tremor-content-subtle">no stacktrace captured</div>
                </Card>
              )}
              {samples.map((s, i) => (
                <Card key={s.variant_hash + i}>
                  <div className="flex items-center justify-between mb-2">
                    <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                      {i === 0 ? "latest stacktrace" : i === samples.length - 1 ? "first seen" : "sample"}
                    </h3>
                    <span className="text-[10px] cl-mono text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                      {relTime(s.first_seen)} · variant {shortHash(s.variant_hash, 6)}
                    </span>
                  </div>
                  {s.stack ? (
                    <StackView stack={s.stack} />
                  ) : (
                    <p className="text-xs text-tremor-content-subtle">message-only event (no stacktrace)</p>
                  )}
                </Card>
              ))}
            </div>

            {/* Right: trend */}
            <Card>
              <h3 className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-3">
                events · last 24h
              </h3>
              <Spark points={g.spark} />
            </Card>
          </div>
        </div>
      )}
    </div>
  );
}

// =====================================================================
// AI explain
// =====================================================================

function ExplainCard({ fingerprint }: { fingerprint: string }) {
  const [context, setContext] = useState("");
  const [showContext, setShowContext] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ provider: string; explanation?: string | null; hint?: string; error?: string } | null>(null);

  async function explain() {
    setBusy(true);
    try {
      setResult(await api.explainErrorGroup(fingerprint, context));
    } catch (e) {
      setResult({ provider: "error", error: String(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <div className="flex items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-medium text-tremor-content-strong dark:text-dark-tremor-content-strong">
            Explain this error
          </h3>
          <p className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-0.5">
            Sends this group's stacktrace + metadata to the configured LLM for a plain-language
            diagnosis. Raw log data never leaves the server.
          </p>
        </div>
        <Button size="xs" loading={busy} onClick={explain}>
          Explain with AI
        </Button>
      </div>

      <button
        type="button"
        onClick={() => setShowContext((s) => !s)}
        className="mt-2 text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle underline"
      >
        {showContext ? "hide context" : "+ add operator context (optional)"}
      </button>
      {showContext && (
        <textarea
          value={context}
          onChange={(e) => setContext(e.target.value)}
          rows={2}
          placeholder="e.g. started after the 3.2 deploy, only affects EU traffic…"
          className="mt-2 w-full px-3 py-2 text-sm rounded-md border border-tremor-border dark:border-dark-tremor-border bg-tremor-background dark:bg-dark-tremor-background text-tremor-content dark:text-dark-tremor-content focus:outline-none focus:ring-2 focus:ring-tremor-brand"
        />
      )}

      {result && (
        <div className="mt-3 rounded-md border border-tremor-border dark:border-dark-tremor-border p-3">
          <div className="text-[10px] uppercase tracking-wide cl-mono text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-1">
            provider: {result.provider}
          </div>
          {result.error && <div className="cl-error-banner text-xs">{result.error}</div>}
          {result.hint && (
            <p className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle">{result.hint}</p>
          )}
          {result.explanation && (
            <p className="text-sm whitespace-pre-wrap text-tremor-content-strong dark:text-dark-tremor-content-strong">
              {result.explanation}
            </p>
          )}
        </div>
      )}
    </Card>
  );
}
