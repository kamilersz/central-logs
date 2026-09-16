import { useState } from "react";
import { Button, Card, Select, SelectItem } from "@tremor/react";
import { usePoll, LevelBadge } from "../components";
import {
  api,
  type AlertRule,
  type AlertChannel,
  type AlertChannelType,
  type AlertCondition,
  type CountCondition,
  type ThresholdCondition,
} from "../api";

const METRICS = ["volume", "error_rate", "p50_latency", "p95_latency", "p99_latency"];
const WINDOWS: [string, number][] = [
  ["1m", 60],
  ["5m", 300],
  ["15m", 900],
  ["1h", 3600],
  ["6h", 21600],
  ["24h", 86400],
];
const COMPARATORS = [">=", ">", "<=", "<", "=="];

export default function AlertsPage() {
  const { data: rules, error, loading, refresh } = usePoll<AlertRule[]>(
    () => api.listAlertRules(),
    30000,
  );
  const { data: channels, refresh: refreshChannels } = usePoll<AlertChannel[]>(
    () => api.listAlertChannels(),
    60000,
  );

  async function approve(id: number) {
    await api.approveAlert(id);
    refresh();
  }
  async function reject(id: number) {
    await api.rejectAlert(id);
    refresh();
  }
  async function deleteRule(id: number) {
    if (!window.confirm("Delete this alert rule? Firing history is kept.")) return;
    await api.deleteAlertRule(id);
    refresh();
  }

  const all = rules ?? [];
  const pending = all.filter((r) => r.status === "pending_approval");
  const active = all.filter((r) => r.status === "active");
  const rejected = all.filter((r) => r.status === "rejected");

  return (
    <div>
      <h2 className="cl-title">Alerts</h2>
      <p className="cl-subtitle text-sm mb-5">
        Rules are evaluated every 30s. AI-created rules (via MCP) land in{" "}
        <code>pending_approval</code> and need a human approve; rules you create here start
        active.
      </p>

      <div className="grid grid-cols-3 gap-4 mb-6">
        <Stat label="pending approval" value={String(pending.length)} color="text-amber-400" />
        <Stat label="active" value={String(active.length)} color="text-emerald-400" />
        <Stat label="rejected" value={String(rejected.length)} />
      </div>

      {error && <div className="cl-error-banner mb-4">{error}</div>}
      {loading && <div className="text-tremor-content-subtle text-sm">loading…</div>}

      <div className="grid grid-cols-1 xl:grid-cols-3 gap-4 items-start">
        {/* ── left: rule builder + lists ── */}
        <div className="xl:col-span-2 space-y-4">
          <RuleBuilder channels={channels ?? []} onCreated={refresh} />

          {pending.length > 0 && (
            <section>
              <h3 className="text-sm font-medium text-amber-400 mb-2">
                Pending approval ({pending.length})
              </h3>
              <div className="space-y-2">
                {pending.map((r) => (
                  <RuleRow key={r.id} rule={r} channels={channels ?? []} onApprove={approve} onReject={reject} onDelete={deleteRule} />
                ))}
              </div>
            </section>
          )}

          {active.length > 0 && (
            <section>
              <h3 className="text-sm font-medium text-emerald-400 mb-2">
                Active ({active.length})
              </h3>
              <div className="space-y-2">
                {active.map((r) => (
                  <RuleRow key={r.id} rule={r} channels={channels ?? []} onDelete={deleteRule} />
                ))}
              </div>
            </section>
          )}

          {rejected.length > 0 && (
            <section>
              <h3 className="text-sm font-medium text-tremor-content-subtle mb-2">
                Rejected ({rejected.length})
              </h3>
              <div className="space-y-2">
                {rejected.map((r) => (
                  <RuleRow key={r.id} rule={r} channels={channels ?? []} onDelete={deleteRule} />
                ))}
              </div>
            </section>
          )}

          {!loading && all.length === 0 && !error && (
            <Card>
              <div className="cl-empty">
                <span className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                  No alert rules yet — create one with the form above.
                </span>
              </div>
            </Card>
          )}
        </div>

        {/* ── right: channels ── */}
        <ChannelManager channels={channels ?? []} onChanged={() => { refreshChannels(); refresh(); }} />
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────────────
// Rule builder
// ─────────────────────────────────────────────────────────────────────────────

type ThresholdRow = { severity: string; comparator: string; number: string };

function RuleBuilder({
  channels,
  onCreated,
}: {
  channels: AlertChannel[];
  onCreated: () => void;
}) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"count" | "metric">("count");
  const [filter, setFilter] = useState("service:api level:error");
  const [metric, setMetric] = useState(METRICS[0]);
  const [rows, setRows] = useState<ThresholdRow[]>([
    { severity: "warning", comparator: ">=", number: "50" },
  ]);
  const [windowLabel, setWindowLabel] = useState("5m");
  const [cooldownMin, setCooldownMin] = useState("15");
  const [selected, setSelected] = useState<number[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);

  const windowSecs = WINDOWS.find(([l]) => l === windowLabel)?.[1] ?? 300;

  function toggleChannel(id: number) {
    setSelected((s) => (s.includes(id) ? s.filter((x) => x !== id) : [...s, id]));
  }

  function setRow(i: number, patch: Partial<ThresholdRow>) {
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  }

  async function submit() {
    setBusy(true);
    setErr(null);
    setOk(null);
    try {
      const thresholds = rows
        .filter((r) => r.number.trim() !== "")
        .map((r) => ({
          severity: r.severity.trim() || "alert",
          comparator: r.comparator,
          value: Number(r.number),
        }));
      if (thresholds.length === 0) throw new Error("at least one threshold is required");
      const base = {
        window_secs: windowSecs,
        cooldown_secs: Math.max(0, Number(cooldownMin) * 60),
        thresholds,
      };
      let condition: AlertCondition;
      if (kind === "count") {
        if (!filter.trim()) throw new Error("filter is required");
        condition = {
          type: "count",
          filter: filter.trim(),
          ...base,
        } satisfies CountCondition;
      } else {
        condition = {
          type: "threshold",
          ...base,
        } satisfies ThresholdCondition;
      }
      const resp = await api.createAlertRule({
        name,
        condition,
        metric: kind === "metric" ? metric : undefined,
        channels: selected,
      });
      setOk(`alert #${resp.id} created and active`);
      setName("");
      onCreated();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  }

  const inputCls =
    "mt-1 w-full px-3 py-2 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong focus:border-tremor-brand dark:focus:border-dark-tremor-brand";

  return (
    <Card>
      <div className="text-sm font-medium mb-3">New alert</div>

      <div className="grid grid-cols-2 gap-3">
        <label className="block col-span-2">
          <span className="cl-stat-label">name</span>
          <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="api errors spike" />
        </label>

        <label className="block">
          <span className="cl-stat-label">condition type</span>
          <Select value={kind} onValueChange={(v) => setKind(v as "count" | "metric")} className="mt-1">
            <SelectItem value="count">n matching events per period</SelectItem>
            <SelectItem value="metric">metric threshold</SelectItem>
          </Select>
        </label>

        <label className="block">
          <span className="cl-stat-label">window</span>
          <Select value={windowLabel} onValueChange={setWindowLabel} className="mt-1">
            {WINDOWS.map(([l]) => (
              <SelectItem key={l} value={l}>
                last {l}
              </SelectItem>
            ))}
          </Select>
        </label>

        {kind === "count" ? (
          <label className="block col-span-2">
            <span className="cl-stat-label">filter (DSL)</span>
            <input
              className={`${inputCls} cl-mono`}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="service:api level:error"
            />
          </label>
        ) : (
          <label className="block">
            <span className="cl-stat-label">metric</span>
            <Select value={metric} onValueChange={setMetric} className="mt-1">
              {METRICS.map((m) => (
                <SelectItem key={m} value={m}>
                  {m}
                </SelectItem>
              ))}
            </Select>
          </label>
        )}

        <label className="block">
          <span className="cl-stat-label">cooldown (minutes)</span>
          <input className={inputCls} value={cooldownMin} onChange={(e) => setCooldownMin(e.target.value)} />
        </label>
      </div>

      <div className="mt-3">
        <span className="cl-stat-label">
          {kind === "count" ? "thresholds — rows matching the filter" : "thresholds — metric value"}
        </span>
        <div className="mt-1.5 space-y-1.5">
          {rows.map((row, i) => (
            <div key={i} className="flex gap-1.5 items-center">
              <input
                className="w-28 px-2 py-1.5 text-xs bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong"
                value={row.severity}
                onChange={(e) => setRow(i, { severity: e.target.value })}
                placeholder="warning"
              />
              <select
                value={row.comparator}
                onChange={(e) => setRow(i, { comparator: e.target.value })}
                className="px-1.5 py-1.5 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong"
              >
                {COMPARATORS.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
              <input
                className="flex-1 px-2 py-1.5 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong"
                value={row.number}
                onChange={(e) => setRow(i, { number: e.target.value })}
                placeholder={kind === "count" ? "50" : "100"}
              />
              <button
                type="button"
                onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
                disabled={rows.length === 1}
                className="px-2 py-1 text-xs text-red-400 border border-red-500/25 rounded-md hover:bg-red-500/10 disabled:opacity-30"
                title="remove level"
              >
                ×
              </button>
            </div>
          ))}
        </div>
        <div className="mt-1.5 flex items-center gap-3">
          <button
            type="button"
            onClick={() =>
              setRows((rs) => [
                ...rs,
                { severity: rs.some((r) => r.severity === "critical") ? "alert" : "critical", comparator: ">=", number: "" },
              ])
            }
            className="text-xs text-tremor-brand dark:text-dark-tremor-brand hover:underline"
          >
            + add escalation level
          </button>
          <span className="text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
            notifies once per state change (e.g. warning → critical → recovered) — never repeatedly
          </span>
        </div>
      </div>

      <div className="mt-3">
        <span className="cl-stat-label">notify via</span>
        <div className="mt-1.5 flex flex-wrap gap-2">
          {channels.length === 0 && (
            <span className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
              no channels yet — add one on the right
            </span>
          )}
          {channels.map((c) => (
            <button
              key={c.id}
              type="button"
              onClick={() => toggleChannel(c.id)}
              className={`px-2.5 py-1 rounded-full text-xs border transition-colors ${
                selected.includes(c.id)
                  ? "bg-tremor-brand-faint border-tremor-brand text-tremor-brand dark:bg-dark-tremor-brand-faint dark:border-dark-tremor-brand dark:text-dark-tremor-brand"
                  : "border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:border-tremor-brand dark:hover:border-dark-tremor-brand"
              }`}
            >
              {c.name} ({c.type})
            </button>
          ))}
        </div>
      </div>

      {err && <div className="cl-error-banner mt-3">{err}</div>}
      {ok && (
        <div className="mt-3 rounded-md border border-emerald-500/25 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-300">
          {ok}
        </div>
      )}

      <div className="mt-4 flex justify-end">
        <Button onClick={submit} loading={busy} disabled={!name.trim()}>
          Create alert
        </Button>
      </div>
    </Card>
  );
}

// ─────────────────────────────────────────────────────────────────────────────
// Channels manager
// ─────────────────────────────────────────────────────────────────────────────

function ChannelManager({ channels, onChanged }: { channels: AlertChannel[]; onChanged: () => void }) {
  const [name, setName] = useState("");
  const [type, setType] = useState<AlertChannelType>("email");
  const [recipients, setRecipients] = useState("");
  const [botToken, setBotToken] = useState("");
  const [chatId, setChatId] = useState("");
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [testingId, setTestingId] = useState<number | null>(null);
  const [testResult, setTestResult] = useState<Record<number, { ok: boolean; text: string }>>({});

  async function addChannel() {
    setBusy(true);
    setErr(null);
    try {
      const config =
        type === "email"
          ? { recipients: recipients.split(/[\n,]/).map((s) => s.trim()).filter(Boolean) }
          : type === "telegram"
            ? { bot_token: botToken.trim(), chat_id: chatId.trim() }
            : { url: url.trim() };
      await api.createAlertChannel({ name, type, config });
      setName("");
      setRecipients("");
      setBotToken("");
      setChatId("");
      setUrl("");
      onChanged();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  }

  async function test(id: number) {
    setTestingId(id);
    try {
      const r = await api.testAlertChannel(id);
      setTestResult((t) => ({ ...t, [id]: { ok: r.ok, text: r.ok ? "delivered" : r.error ?? "failed" } }));
    } catch (e) {
      setTestResult((t) => ({ ...t, [id]: { ok: false, text: String(e) } }));
    } finally {
      setTestingId(null);
    }
  }

  async function remove(id: number) {
    if (!window.confirm("Delete this channel? Rules referencing it will skip it.")) return;
    await api.deleteAlertChannel(id);
    onChanged();
  }

  const inputCls =
    "mt-1 w-full px-3 py-2 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border dark:border-dark-tremor-border rounded-md text-tremor-content-strong dark:text-dark-tremor-content-strong";

  return (
    <Card className="xl:sticky xl:top-20">
      <div className="text-sm font-medium mb-3">Notification channels</div>

      {channels.length > 0 && (
        <div className="space-y-2 mb-4">
          {channels.map((c) => (
            <div key={c.id} className="p-2.5 rounded-md border border-tremor-border dark:border-dark-tremor-border">
              <div className="flex items-center justify-between gap-2">
                <span className="text-sm font-medium">{c.name}</span>
                <span className="cl-pill bg-tremor-brand-faint text-tremor-brand dark:bg-dark-tremor-brand-faint dark:text-dark-tremor-brand">
                  {c.type}
                </span>
              </div>
              <div className="cl-mono text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-1 truncate">
                {c.type === "email" && `→ ${(c.config.recipients as string[])?.join(", ")}`}
                {c.type === "telegram" && `→ chat ${c.config.chat_id}`}
                {c.type === "webhook" && `→ ${c.config.url}`}
              </div>
              <div className="flex items-center gap-2 mt-2">
                <button
                  type="button"
                  onClick={() => test(c.id)}
                  disabled={testingId === c.id}
                  className="px-2 py-0.5 rounded text-[11px] border border-tremor-border dark:border-dark-tremor-border hover:border-tremor-brand dark:hover:border-dark-tremor-brand disabled:opacity-50"
                >
                  {testingId === c.id ? "testing…" : "test"}
                </button>
                <button
                  type="button"
                  onClick={() => remove(c.id)}
                  className="px-2 py-0.5 rounded text-[11px] text-red-400 border border-red-500/25 hover:bg-red-500/10"
                >
                  delete
                </button>
                {testResult[c.id] && (
                  <span className={`text-[11px] ${testResult[c.id].ok ? "text-emerald-400" : "text-red-400"}`}>
                    {testResult[c.id].text}
                  </span>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="border-t border-tremor-border dark:border-dark-tremor-border pt-3">
        <div className="text-xs font-medium uppercase tracking-wide text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-2">
          add channel
        </div>
        <label className="block mb-2">
          <span className="cl-stat-label">name</span>
          <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="ops email list" />
        </label>
        <label className="block mb-2">
          <span className="cl-stat-label">type</span>
          <Select value={type} onValueChange={(v) => setType(v as AlertChannelType)} className="mt-1">
            <SelectItem value="email">email (SMTP relay)</SelectItem>
            <SelectItem value="telegram">telegram bot</SelectItem>
            <SelectItem value="webhook">webhook (http POST)</SelectItem>
          </Select>
        </label>

        {type === "email" && (
          <label className="block mb-2">
            <span className="cl-stat-label">recipients (one per line)</span>
            <textarea
              className={inputCls}
              rows={3}
              value={recipients}
              onChange={(e) => setRecipients(e.target.value)}
              placeholder={"ops@example.com\noncall@example.com"}
            />
          </label>
        )}
        {type === "telegram" && (
          <>
            <label className="block mb-2">
              <span className="cl-stat-label">bot token</span>
              <input className={inputCls} type="password" value={botToken} onChange={(e) => setBotToken(e.target.value)} placeholder="123456:ABC-DEF…" />
            </label>
            <label className="block mb-2">
              <span className="cl-stat-label">chat id</span>
              <input className={inputCls} value={chatId} onChange={(e) => setChatId(e.target.value)} placeholder="-1001234567890" />
            </label>
          </>
        )}
        {type === "webhook" && (
          <label className="block mb-2">
            <span className="cl-stat-label">url</span>
            <input className={inputCls} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://hooks.example.com/…" />
          </label>
        )}

        {err && <div className="cl-error-banner mt-2">{err}</div>}

        <div className="mt-3">
          <Button size="xs" variant="secondary" onClick={addChannel} loading={busy} disabled={!name.trim()}>
            Add channel
          </Button>
        </div>
        <p className="text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-3">
          Email delivery needs the server-side SMTP relay configured (<code>[smtp]</code> in
          config / <code>CENTRAL_LOGS_SMTP__*</code> env). Bot tokens are stored server-side and
          never returned to the browser.
        </p>
      </div>
    </Card>
  );
}

// ─────────────────────────────────────────────────────────────────────────────
// Rule row + small bits
// ─────────────────────────────────────────────────────────────────────────────

function describeCondition(rule: AlertRule): string {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const c = rule.condition as any;
  if (!c || typeof c !== "object") return "—";
  const win = c.window_secs ? WINDOWS.find(([l, s]) => s === c.window_secs)?.[0] ?? `${c.window_secs}s` : "?";
  if (c.type === "count") {
    return `${c.comparator ?? ">="} ${c.count} matching "${c.filter}" per ${win}`;
  }
  if (c.type === "threshold") {
    return `${rule.metric} ${c.comparator} ${c.value} per ${win}`;
  }
  if (c.type === "anomaly") {
    return `anomaly ≥ ${c.min_severity ?? "high"} per ${win}`;
  }
  return JSON.stringify(rule.condition);
}

function RuleRow({
  rule,
  channels,
  onApprove,
  onReject,
  onDelete,
}: {
  rule: AlertRule;
  channels: AlertChannel[];
  onApprove?: (id: number) => Promise<void>;
  onReject?: (id: number) => Promise<void>;
  onDelete?: (id: number) => Promise<void>;
}) {
  const condStr = (() => {
    try {
      return JSON.stringify(rule.condition, null, 2);
    } catch {
      return "{}";
    }
  })();
  const channelNames = (rule.channels ?? [])
    .map((id) => channels.find((c) => c.id === id)?.name)
    .filter(Boolean);

  return (
    <div className="p-3 rounded-md border border-tremor-border dark:border-dark-tremor-border">
      <div className="flex justify-between items-start gap-3">
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium">{rule.name}</span>
            <StatusPill status={rule.status} />
            {rule.last_notify_error && (
              <LevelBadge level="warn" />
            )}
          </div>
          <div className="text-xs text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis mt-1">
            {describeCondition(rule)}
          </div>
          <div className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-1">
            channels: <code>{channelNames.length ? channelNames.join(", ") : rule.channel ?? "—"}</code> · created{" "}
            {new Date(rule.created_at).toLocaleString()}
            {rule.last_fired_at && ` · last fired ${new Date(rule.last_fired_at).toLocaleString()}`}
          </div>
          {rule.last_notify_error && (
            <div className="text-xs text-amber-400 mt-1">last error: {rule.last_notify_error}</div>
          )}
          <details className="mt-2">
            <summary className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle cursor-pointer">
              condition
            </summary>
            <pre className="cl-mono mt-1 text-xs p-2 bg-tremor-background-muted dark:bg-dark-tremor-background-muted rounded overflow-auto">
              {condStr}
            </pre>
          </details>
        </div>
        <div className="flex gap-2 shrink-0">
          {rule.status === "pending_approval" && onApprove && onReject && (
            <>
              <button
                onClick={() => onApprove(rule.id)}
                className="px-3 py-1 rounded-md text-xs bg-emerald-600 text-white hover:opacity-90"
              >
                Approve
              </button>
              <button
                onClick={() => onReject(rule.id)}
                className="px-3 py-1 rounded-md text-xs border border-red-500/25 text-red-400 hover:bg-red-500/10"
              >
                Reject
              </button>
            </>
          )}
          {onDelete && (
            <button
              onClick={() => onDelete(rule.id)}
              className="px-2 py-1 rounded-md text-xs text-red-400 border border-red-500/25 hover:bg-red-500/10"
            >
              delete
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

function StatusPill({ status }: { status: string }) {
  const cls =
    status === "active"
      ? "bg-emerald-400/15 text-emerald-300"
      : status === "pending_approval"
      ? "bg-amber-400/15 text-amber-300"
      : "bg-slate-400/15 text-slate-300";
  return <span className={`cl-pill ${cls}`}>{status}</span>;
}

function Stat({ label, value, color = "" }: { label: string; value: string; color?: string }) {
  return (
    <Card>
      <div className="cl-stat-label">{label}</div>
      <div className={`cl-stat-value !text-xl ${color}`}>{value}</div>
    </Card>
  );
}
