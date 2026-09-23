import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type AiDashboardQuestion, type AiDashboardProposal } from "../api";
import { PanelView } from "../Panel";

/**
 * AI dashboard builder: describe what you want → the model asks clarifying
 * questions (choices when enumerable, free text otherwise) → a preview you
 * can save. Powered by POST /api/ai/dashboard.
 */
export default function AiBuilderPage() {
  const navigate = useNavigate();
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [provider, setProvider] = useState<string | null>(null);
  const [questions, setQuestions] = useState<AiDashboardQuestion[]>([]);
  const [answers, setAnswers] = useState<Record<number, string>>({});
  const [proposal, setProposal] = useState<AiDashboardProposal | null>(null);
  // Questions raised by the most recent refine call — when set, the refine
  // input acts as the answer box too and the user re-submits with answers.
  const [refineQuestions, setRefineQuestions] = useState<AiDashboardQuestion[]>([]);
  const [refineAnswers, setRefineAnswers] = useState<Record<number, string>>({});
  const [saving, setSaving] = useState(false);
  // Refinement loop: extra instructions applied to the latest preview.
  const [refine, setRefine] = useState("");
  const [refineBusy, setRefineBusy] = useState(false);

  async function generate() {
    setBusy(true);
    setError(null);
    try {
      const answerList =
        questions.length > 0
          ? questions.map((q) => ({ question: q.question, answer: answers[q.id] ?? "" })).filter((a) => a.answer.trim())
          : undefined;
      const resp = await api.aiDashboard({
        description,
        answers: answerList,
      });
      setProvider(resp.provider);
      if (resp.stage === "clarify") {
        setQuestions(resp.questions);
        setAnswers({});
        setProposal(null);
      } else if (resp.dashboard) {
        setProposal(resp.dashboard);
        setQuestions([]);
      } else {
        setError("model returned no dashboard and no questions");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  /** Send a follow-up instruction; the server revises the current preview. */
  async function applyRefine() {
    if (!proposal) return;
    setRefineBusy(true);
    setError(null);
    try {
      // Carry any answers from a previous clarify round so the model can
      // actually act on them.
      const answers =
        refineQuestions.length > 0
          ? refineQuestions
              .map((q) => ({
                question: q.question,
                answer: refineAnswers[q.id] ?? "",
              }))
              .filter((a) => a.answer.trim())
          : undefined;
      const resp = await api.aiDashboard({
        description,
        current: proposal,
        revision: refine,
        answers,
      });
      if (resp.stage === "build" && resp.dashboard) {
        setProposal(resp.dashboard);
        setRefine("");
        setRefineQuestions([]);
        setRefineAnswers({});
      } else if (resp.stage === "clarify") {
        setRefineQuestions(resp.questions);
        setRefineAnswers({});
      } else {
        setError("the assistant didn't return a dashboard — try rephrasing");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setRefineBusy(false);
    }
  }

  async function save() {
    if (!proposal) return;
    setSaving(true);
    try {
      const r = await api.saveDashboard({
        name: proposal.name,
        description: proposal.description,
        panels: proposal.panels,
      });
      navigate(`/dashboards/view/${r.id}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  const canGenerate =
    description.trim().length > 0 &&
    !busy &&
    (questions.length === 0 || questions.every((q) => (answers[q.id] ?? "").trim().length > 0));

  /** Refine can submit when there's a non-empty instruction AND every open
   *  clarify question has been answered. Without this guard the user could
   *  send a partial answer set, which the model can't act on. */
  function canRefine(): boolean {
    if (refineBusy) return false;
    if (!refine.trim()) return false;
    if (refineQuestions.length === 0) return true;
    return refineQuestions.every((q) => (refineAnswers[q.id] ?? "").trim().length > 0);
  }

  return (
    <div>
      <div className="flex justify-between items-center mb-6">
        <div>
          <h2 className="text-xl font-semibold">Build with AI</h2>
          <p className="text-sm text-tremor-content-subtle">
            Describe the dashboard you want — the assistant asks clarifying questions, then
            assembles it.
          </p>
        </div>
      </div>

      {error && <div className="mb-4 p-3 bg-red-100 dark:bg-red-900/30 text-red-800 rounded-md text-sm">{error}</div>}

      <div className="p-4 cl-card mb-4">
        <label className="text-xs text-tremor-content-subtle">What do you want to see?</label>
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={3}
          placeholder='e.g. "Ops view for the telegram-bot: traffic, error rate over the last day, and p95 latency"'
          className="mt-1 w-full px-3 py-2 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm"
        />
        <div className="flex items-center gap-3 mt-3">
          <button
            onClick={generate}
            disabled={!canGenerate}
            className="px-3 py-1.5 rounded-md text-sm bg-tremor-brand text-white disabled:opacity-50"
          >
            {busy ? "Thinking…" : questions.length > 0 ? "Generate dashboard" : "Start"}
          </button>
          {provider && (
            <span className="text-xs text-tremor-content-subtle">
              provider: {provider}
              {provider === "off" && " (no LLM configured — keyword-based starter dashboard)"}
            </span>
          )}
        </div>
      </div>

      {questions.length > 0 && (
        <div className="p-4 cl-card mb-4 space-y-4">
          <div className="text-sm font-medium">A few questions to sharpen the design:</div>
          {questions.map((q) => (
            <div key={q.id}>
              <label className="text-sm">{q.question}</label>
              {q.choices && q.choices.length > 0 ? (
                <div className="flex flex-wrap gap-2 mt-1">
                  {q.choices.map((c) => (
                    <button
                      key={c}
                      onClick={() => setAnswers((a) => ({ ...a, [q.id]: c }))}
                      className={`px-3 py-1.5 rounded-md text-sm border ${
                        answers[q.id] === c
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
                    value={answers[q.id] && !q.choices.includes(answers[q.id]) ? answers[q.id] : ""}
                    onChange={(e) => setAnswers((a) => ({ ...a, [q.id]: e.target.value }))}
                    className="px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm min-w-[180px]"
                  />
                </div>
              ) : (
                <input
                  type="text"
                  value={answers[q.id] ?? ""}
                  onChange={(e) => setAnswers((a) => ({ ...a, [q.id]: e.target.value }))}
                  placeholder="your answer…"
                  className="mt-1 w-full px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm"
                />
              )}
            </div>
          ))}
        </div>
      )}

      {proposal && (
        <div>
          <div className="flex justify-between items-center mb-4">
            <div>
              <h3 className="font-medium">{proposal.name}</h3>
              {proposal.description && (
                <p className="text-sm text-tremor-content-subtle">{proposal.description}</p>
              )}
            </div>
            <div className="flex gap-2">
              <button
                onClick={() => {
                  setProposal(null);
                  setQuestions([]);
                  setAnswers({});
                }}
                className="px-3 py-1.5 rounded-md text-sm border border-tremor-border dark:border-dark-tremor-border"
                disabled={saving}
              >
                Start over
              </button>
              <button
                onClick={save}
                disabled={saving}
                className="px-3 py-1.5 rounded-md text-sm bg-tremor-brand text-white disabled:opacity-50"
              >
                {saving ? "Saving…" : "Save dashboard"}
              </button>
            </div>
          </div>
          <div className="grid grid-cols-12 gap-4">
            {proposal.panels.map((p, i) => (
              <PanelView key={i} panel={p} />
            ))}
          </div>

          {/* Refinement loop: ask for the missing details, or answer the
              clarifying questions the assistant raised on the last refine. */}
          <div className="mt-4 p-4 cl-card bg-tremor-background-muted dark:bg-dark-tremor-background-muted">
            <label className="text-xs text-tremor-content-subtle">
              {refineQuestions.length > 0
                ? "Answer the questions, then refine"
                : "Not quite right? Describe the change — the preview updates in place."}
            </label>
            <div className="flex gap-2 mt-1">
              <input
                type="text"
                value={refine}
                onChange={(e) => setRefine(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && canRefine()) applyRefine();
                }}
                placeholder='e.g. "add a big number for failed checkouts", "make latency 12 wide and move it first"'
                className="flex-1 px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm"
              />
              <button
                onClick={applyRefine}
                disabled={!canRefine()}
                className="px-3 py-1.5 rounded-md text-sm border border-tremor-brand text-tremor-brand dark:text-dark-tremor-brand disabled:opacity-50 whitespace-nowrap"
              >
                {refineBusy ? "Refining…" : "Refine"}
              </button>
            </div>
            {refineQuestions.length > 0 && (
              <div className="mt-3 space-y-3">
                {refineQuestions.map((q) => (
                  <div key={q.id}>
                    <label className="text-sm">{q.question}</label>
                    {q.choices && q.choices.length > 0 ? (
                      <div className="flex flex-wrap gap-2 mt-1">
                        {q.choices.map((c) => (
                          <button
                            key={c}
                            type="button"
                            onClick={() =>
                              setRefineAnswers((a) => ({ ...a, [q.id]: c }))
                            }
                            className={`px-2.5 py-1 rounded-md text-xs border ${
                              refineAnswers[q.id] === c
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
                            refineAnswers[q.id] && !q.choices.includes(refineAnswers[q.id])
                              ? refineAnswers[q.id]
                              : ""
                          }
                          onChange={(e) =>
                            setRefineAnswers((a) => ({ ...a, [q.id]: e.target.value }))
                          }
                          className="px-2 py-1 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm min-w-[160px]"
                        />
                      </div>
                    ) : (
                      <input
                        type="text"
                        value={refineAnswers[q.id] ?? ""}
                        onChange={(e) =>
                          setRefineAnswers((a) => ({ ...a, [q.id]: e.target.value }))
                        }
                        placeholder="your answer…"
                        className="mt-1 w-full px-2 py-1.5 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded-md text-sm"
                      />
                    )}
                  </div>
                ))}
              </div>
            )}
            {provider === "off" && (
              <div className="mt-2 text-xs text-tremor-content-subtle">
                no LLM configured — refinements only understand simple add/remove requests
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
