import { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { api, type AiDashboardQuestion, type Panel } from "../api";
import { TIME_WINDOWS, toLocalInput } from "../components";

type PanelType = Panel["type"];
type PanelViz = NonNullable<Panel["viz"]>;

const PANEL_TYPES: { value: PanelType; label: string }[] = [
  { value: "volume", label: "Volume (records over time)" },
  { value: "error-rate", label: "Error rate" },
  { value: "latency", label: "Latency percentiles" },
  { value: "top-services", label: "Top services" },
  { value: "log-count", label: "Total log count" },
  { value: "anomalies", label: "Anomalies" },
];

const VIZ_TYPES: { value: PanelViz; label: string }[] = [
  { value: "chart", label: "Timeline chart" },
  { value: "number", label: "Big number" },
];

/** Widths offered on the 12-column grid. */
const WIDTHS = [2, 3, 4, 6, 8, 12];

const inputCls =
  "mt-1 w-full px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm";

type EditablePanel = Panel & { id: string };

function withId(p: Panel): EditablePanel {
  return { ...p, id: Math.random().toString(36).slice(2, 9) };
}

function newPanel(): EditablePanel {
  const now = new Date();
  return {
    id: Math.random().toString(36).slice(2, 9),
    type: "volume",
    title: "New panel",
    window: "1h",
    filter: "",
    viz: "chart",
    w: 6,
    from: toLocalInput(new Date(now.getTime() - 3600_000)),
    to: toLocalInput(now),
  };
}

export default function DashboardBuilderPage() {
  const { id } = useParams<{ id?: string }>();
  const isEdit = Boolean(id);
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [panels, setPanels] = useState<EditablePanel[]>([newPanel()]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // --- Modify with AI ---
  const [aiPrompt, setAiPrompt] = useState("");
  const [aiBusy, setAiBusy] = useState(false);
  const [aiNote, setAiNote] = useState<string | null>(null);
  const [aiQuestions, setAiQuestions] = useState<AiDashboardQuestion[]>([]);
  const [aiAnswers, setAiAnswers] = useState<Record<number, string>>({});

  useEffect(() => {
    if (!isEdit || !id) return;
    api
      .getDashboard(Number(id))
      .then((d) => {
        setName(d.name);
        setDescription(d.description ?? "");
        const restored = Array.isArray(d.panels)
          ? (d.panels as Panel[]).map(withId)
          : [newPanel()];
        setPanels(restored);
      })
      .catch((e) => setError(String(e)));
  }, [id, isEdit]);

  function update(panelId: string, patch: Partial<Panel>) {
    setPanels((ps) => ps.map((p) => (p.id === panelId ? { ...p, ...patch } : p)));
  }
  function remove(panelId: string) {
    setPanels((ps) => ps.filter((p) => p.id !== panelId));
  }
  function addPanel() {
    setPanels((ps) => [...ps, newPanel()]);
  }
  function move(panelId: string, dir: -1 | 1) {
    setPanels((ps) => {
      const i = ps.findIndex((p) => p.id === panelId);
      const j = i + dir;
      if (i < 0 || j < 0 || j >= ps.length) return ps;
      const out = [...ps];
      [out[i], out[j]] = [out[j], out[i]];
      return out;
    });
  }

  async function modifyWithAi() {
    setAiBusy(true);
    setAiNote(null);
    setError(null);
    try {
      const current = {
        name,
        description,
        panels: panels.map(({ id: _id, ...rest }) => rest),
      };
      const answers =
        aiQuestions.length > 0
          ? aiQuestions
              .map((q) => ({ question: q.question, answer: aiAnswers[q.id] ?? "" }))
              .filter((a) => a.answer.trim())
          : undefined;
      const resp = await api.aiDashboard({
        description: description || name || "dashboard",
        current,
        revision: aiPrompt,
        answers,
      });
      if (resp.stage === "build" && resp.dashboard) {
        const d = resp.dashboard;
        setName(d.name);
        setDescription(d.description);
        setPanels(d.panels.map(withId));
        setAiNote(
          resp.provider === "off"
            ? `applied (${resp.raw})`
            : `applied via ${resp.provider}`,
        );
        setAiQuestions([]);
        setAiAnswers({});
      } else if (resp.stage === "clarify") {
        // Surface the questions inline so the user can answer and re-submit.
        setAiQuestions(resp.questions);
        setAiAnswers({});
        setAiNote("answer the questions below and click Apply again");
      } else {
        setAiNote("the assistant didn't return a dashboard — try rephrasing");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setAiBusy(false);
    }
  }

  async function save() {
    setSaving(true);
    setError(null);
    try {
      // Strip the local-only `id` field before persisting.
      const panelsOut = panels.map(({ id: _id, ...rest }) => rest);
      if (isEdit && id) {
        await api.updateDashboard(Number(id), {
          name,
          panels: panelsOut,
        });
        navigate(`/dashboards/view/${id}`);
      } else {
        const r = await api.saveDashboard({ name, description, panels: panelsOut });
        navigate(`/dashboards/view/${r.id}`);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div>
      <div className="flex justify-between items-center mb-6">
        <h2 className="text-xl font-semibold">{isEdit ? "Edit dashboard" : "New dashboard"}</h2>
        <div className="flex gap-2">
          <button
            onClick={() => navigate(-1)}
            className="px-3 py-1.5 rounded text-sm border border-tremor-border dark:border-dark-tremor-border"
          >
            Cancel
          </button>
          <button
            onClick={save}
            disabled={saving || !name}
            className="px-3 py-1.5 rounded text-sm bg-tremor-brand text-white disabled:opacity-50"
          >
            {saving ? "Saving..." : "Save"}
          </button>
        </div>
      </div>

      {error && <div className="mb-4 p-3 bg-red-100 dark:bg-red-900/30 text-red-800 rounded text-sm">{error}</div>}

      <div className="grid grid-cols-2 gap-4 mb-4">
        <div>
          <label className="text-xs text-tremor-content-subtle">Name</label>
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="My dashboard"
            className={inputCls}
          />
        </div>
        <div>
          <label className="text-xs text-tremor-content-subtle">Description</label>
          <input
            type="text"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="optional"
            className={inputCls}
          />
        </div>
      </div>

      {/* Modify with AI — works on the current panel list, in place.
          When the assistant asks clarifying questions, they appear here so
          the user can answer + re-submit without leaving the editor. */}
      <div className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border mb-6 bg-tremor-background-muted dark:bg-dark-tremor-background-muted">
        <label className="text-xs text-tremor-content-subtle">Modify with AI</label>
        <div className="flex gap-2 mt-1">
          <input
            type="text"
            value={aiPrompt}
            onChange={(e) => setAiPrompt(e.target.value)}
            placeholder='e.g. "add a p95 latency panel for telegram-bot, make the error panels big numbers, reorder latency first"'
            className="flex-1 px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm"
          />
          <button
            onClick={modifyWithAi}
            disabled={
              aiBusy ||
              !aiPrompt.trim() ||
              (aiQuestions.length > 0 &&
                aiQuestions.some((q) => !(aiAnswers[q.id] ?? "").trim()))
            }
            className="px-3 py-1.5 rounded text-sm border border-tremor-brand text-tremor-brand dark:text-dark-tremor-brand disabled:opacity-50 whitespace-nowrap"
          >
            {aiBusy ? "Applying…" : aiQuestions.length > 0 ? "Apply with answers" : "Apply"}
          </button>
        </div>
        {aiNote && <div className="mt-2 text-xs text-tremor-content-subtle">{aiNote}</div>}
        {aiQuestions.length > 0 && (
          <div className="mt-3 space-y-3">
            {aiQuestions.map((q) => (
              <div key={q.id}>
                <label className="text-sm">{q.question}</label>
                {q.choices && q.choices.length > 0 ? (
                  <div className="flex flex-wrap gap-2 mt-1">
                    {q.choices.map((c) => (
                      <button
                        key={c}
                        type="button"
                        onClick={() => setAiAnswers((a) => ({ ...a, [q.id]: c }))}
                        className={`px-2.5 py-1 rounded text-xs border ${
                          aiAnswers[q.id] === c
                            ? "border-tremor-brand bg-tremor-brand-faint dark:bg-dark-tremor-brand-faint text-tremor-brand dark:text-dark-tremor-brand"
                            : "border-tremor-border dark:border-dark-tremor-border"
                        }`}
                      >
                        {c}
                      </button>
                    ))}
                    <input
                      type="text"
                      placeholder="or type your own…"
                      value={
                        aiAnswers[q.id] && !q.choices.includes(aiAnswers[q.id])
                          ? aiAnswers[q.id]
                          : ""
                      }
                      onChange={(e) =>
                        setAiAnswers((a) => ({ ...a, [q.id]: e.target.value }))
                      }
                      className="px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm min-w-[160px]"
                    />
                  </div>
                ) : (
                  <input
                    type="text"
                    value={aiAnswers[q.id] ?? ""}
                    onChange={(e) =>
                      setAiAnswers((a) => ({ ...a, [q.id]: e.target.value }))
                    }
                    placeholder="your answer…"
                    className="mt-1 w-full px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm"
                  />
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="space-y-3">
        {panels.map((p, i) => (
          <div
            key={p.id}
            className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border"
          >
            <div className="grid grid-cols-12 gap-3 items-end">
              <div className="col-span-1 flex flex-col gap-1">
                <button
                  onClick={() => move(p.id, -1)}
                  disabled={i === 0}
                  className="px-2 py-0.5 rounded text-xs border border-tremor-border dark:border-dark-tremor-border disabled:opacity-30"
                  title="Move up"
                >
                  ↑
                </button>
                <button
                  onClick={() => move(p.id, 1)}
                  disabled={i === panels.length - 1}
                  className="px-2 py-0.5 rounded text-xs border border-tremor-border dark:border-dark-tremor-border disabled:opacity-30"
                  title="Move down"
                >
                  ↓
                </button>
              </div>
              <div className="col-span-3">
                <label className="text-xs text-tremor-content-subtle">Title</label>
                <input
                  type="text"
                  value={p.title}
                  onChange={(e) => update(p.id, { title: e.target.value })}
                  className={inputCls}
                />
              </div>
              <div className="col-span-2">
                <label className="text-xs text-tremor-content-subtle">Type</label>
                <select
                  value={p.type}
                  onChange={(e) => update(p.id, { type: e.target.value as PanelType })}
                  className={inputCls}
                >
                  {PANEL_TYPES.map((t) => (
                    <option key={t.value} value={t.value}>
                      {t.label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="col-span-2">
                <label className="text-xs text-tremor-content-subtle">Visualization</label>
                <select
                  value={p.viz ?? "chart"}
                  onChange={(e) => update(p.id, { viz: e.target.value as PanelViz })}
                  className={inputCls}
                >
                  {VIZ_TYPES.map((t) => (
                    <option key={t.value} value={t.value}>
                      {t.label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="col-span-2">
                <label className="text-xs text-tremor-content-subtle">Width</label>
                <select
                  value={p.w ?? 6}
                  onChange={(e) => update(p.id, { w: Number(e.target.value) })}
                  className={inputCls}
                >
                  {WIDTHS.map((w) => (
                    <option key={w} value={w}>
                      {w}/12
                    </option>
                  ))}
                </select>
              </div>
              <div className="col-span-2">
                <label className="text-xs text-tremor-content-subtle">Period</label>
                <select
                  value={p.window}
                  onChange={(e) => update(p.id, { window: e.target.value })}
                  className={inputCls}
                >
                  {TIME_WINDOWS.map((w) => (
                    <option key={w} value={w}>
                      last {w}
                    </option>
                  ))}
                  <option value="custom">custom…</option>
                </select>
              </div>
            </div>
            {p.window === "custom" && (
              <div className="grid grid-cols-2 gap-3 mt-3">
                <div>
                  <label className="text-xs text-tremor-content-subtle">From</label>
                  <input
                    type="datetime-local"
                    value={p.from ?? ""}
                    onChange={(e) => update(p.id, { from: e.target.value })}
                    className={inputCls}
                  />
                </div>
                <div>
                  <label className="text-xs text-tremor-content-subtle">To</label>
                  <input
                    type="datetime-local"
                    value={p.to ?? ""}
                    onChange={(e) => update(p.id, { to: e.target.value })}
                    className={inputCls}
                  />
                </div>
              </div>
            )}
            <div className="grid grid-cols-12 gap-3 mt-3 items-end">
              <div className="col-span-11">
                <label className="text-xs text-tremor-content-subtle">Filter DSL (optional)</label>
                <input
                  type="text"
                  value={p.filter}
                  onChange={(e) => update(p.id, { filter: e.target.value })}
                  placeholder="service:api level:error"
                  className={`${inputCls} font-mono`}
                />
              </div>
              <div className="col-span-1">
                <button
                  onClick={() => remove(p.id)}
                  disabled={panels.length === 1}
                  className="mt-1 px-2 py-1.5 rounded text-xs border border-tremor-border dark:border-dark-tremor-border w-full disabled:opacity-40"
                  title="Remove panel"
                >
                  ×
                </button>
              </div>
            </div>
          </div>
        ))}
      </div>

      <button
        onClick={addPanel}
        className="mt-3 px-3 py-1.5 rounded text-sm border border-dashed border-tremor-border dark:border-dark-tremor-border w-full hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
      >
        + Add panel
      </button>
    </div>
  );
}
