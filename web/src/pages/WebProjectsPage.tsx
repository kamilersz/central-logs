import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { usePoll } from "../components";
import { api, type WebSitesResponse } from "../api";
import { trackEvent } from "../track";

/**
 * Web projects (Manage → Web projects): the analytics project registry —
 * origins, excluded operator IPs, and the donation-funnel goal, per project.
 * Lives here, NOT on the dashboard, so the analytics page stays about data.
 */

interface SiteRow {
  id: string;
  allowed_origins: string[];
  has_user_header: boolean;
  has_exclude_ips: boolean;
  source: "config" | "db";
}

function funnelKey(site: string | undefined): string {
  return `cl-funnel-goal:${site ?? "default"}`;
}

export default function WebProjectsPage() {
  const [searchParams] = useSearchParams();
  const preselect = searchParams.get("project") ?? "";
  const { data, refresh } = usePoll<WebSitesResponse>(() => api.webSites(), 60000, []);
  const sites = data?.sites ?? [];
  const [creating, setCreating] = useState(false);

  return (
    <div className="max-w-4xl">
      <div className="flex items-center justify-between mb-4">
        <h2 className="cl-title">Web projects</h2>
        <button
          type="button"
          className="px-2.5 py-1.5 rounded text-sm bg-tremor-brand text-white"
          onClick={() => setCreating((v) => !v)}
        >
          {creating ? "Cancel" : "New project"}
        </button>
      </div>
      <p className="text-sm opacity-70 mb-4">
        A project is one website: its id goes in the tracker snippet, and only
        the listed origins may send beacons to it. Excluded IPs mark your own
        visits so they don't pollute the stats.
      </p>

      {creating && (
        <ProjectForm
          onDone={() => {
            setCreating(false);
            refresh();
          }}
        />
      )}

      {sites.length === 0 && !creating && (
        <div className="p-3 rounded cl-card-plain border border-dashed border-tremor-border text-sm">
          No projects yet — create one, or enable auto-provisioning in
          central-logs.toml so the first beacon registers it automatically.
        </div>
      )}

      <div className="space-y-4">
        {sites.map((s) => (
          <ProjectCard key={s.id} site={s as SiteRow} onChanged={refresh} expandGoal={s.id === preselect} />
        ))}
      </div>
    </div>
  );
}

/** Shared input styling (matches the rest of the settings pages). */
const inputCls =
  "w-full px-2.5 py-1.5 text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted rounded border border-tremor-border dark:border-dark-tremor-border";

function ProjectForm({
  initial,
  id,
  onDone,
}: {
  initial?: SiteRow;
  id?: string;
  onDone: () => void;
}) {
  const [projectId, setProjectId] = useState(initial?.id ?? "");
  const [origins, setOrigins] = useState((initial?.allowed_origins ?? []).join("\n"));
  const [exclude, setExclude] = useState("");
  const [userHeader, setUserHeader] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    const body = {
      allowed_origins: origins.split("\n").map((o) => o.trim()).filter(Boolean),
      exclude_ips: exclude.split(/[\n,]/).map((o) => o.trim()).filter(Boolean),
      ...(userHeader.trim() ? { user_header: userHeader.trim() } : {}),
    };
    try {
      if (initial || id) {
        await api.webUpdateSite((initial?.id ?? id) as string, body);
      } else {
        await api.webCreateSite({ id: projectId.trim(), ...body });
      }
      trackEvent("ui.projects.saved", { edit: Boolean(initial || id) });
      onDone();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={submit} className="p-4 cl-card space-y-3">
      {!initial && !id && (
        <label className="block">
          <span className="cl-stat-label">project id (used in the tracker snippet)</span>
          <input
            className={inputCls}
            value={projectId}
            onChange={(e) => setProjectId(e.target.value)}
            placeholder="e.g. ajakan"
            spellCheck={false}
          />
        </label>
      )}
      <label className="block">
        <span className="cl-stat-label">allowed origins — one per line, in full</span>
        <textarea
          className={`${inputCls} font-mono`}
          rows={3}
          value={origins}
          onChange={(e) => setOrigins(e.target.value)}
          placeholder={"https://ajakan.or.id\nhttps://www.ajakan.or.id"}
          spellCheck={false}
        />
      </label>
      <label className="block">
        <span className="cl-stat-label">
          excluded IPs / CIDRs — your own visits (comma or newline separated)
        </span>
        <input
          className={`${inputCls} font-mono`}
          value={exclude}
          onChange={(e) => setExclude(e.target.value)}
          placeholder="203.0.113.7, 10.0.0.0/8"
          spellCheck={false}
        />
      </label>
      <label className="block">
        <span className="cl-stat-label">user header (optional — set by your proxy)</span>
        <input
          className={inputCls}
          value={userHeader}
          onChange={(e) => setUserHeader(e.target.value)}
          placeholder="X-CL-User"
          spellCheck={false}
        />
      </label>
      {err && <div className="text-sm text-red-500">{err}</div>}
      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy || (!initial && !id && !projectId.trim())}
          className="px-2.5 py-1.5 rounded text-sm bg-tremor-brand text-white disabled:opacity-50"
        >
          {initial || id ? "Save changes" : "Create project"}
        </button>
        <button type="button" onClick={onDone} className="px-2.5 py-1.5 rounded text-sm border border-tremor-border">
          Cancel
        </button>
      </div>
    </form>
  );
}

function ProjectCard({
  site,
  onChanged,
  expandGoal,
}: {
  site: SiteRow;
  onChanged: () => void;
  expandGoal: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [confirmText, setConfirmText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  async function doDelete() {
    setBusy(true);
    setErr(null);
    try {
      await api.webDeleteSite(site.id);
      trackEvent("ui.projects.deleted", {});
      onChanged();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
      setDeleting(false);
    }
  }

  return (
    <div className="p-4 cl-card">
      <div className="flex flex-wrap items-center gap-2 mb-2">
        <span className="font-medium font-mono">{site.id}</span>
        <span
          className={`text-xs px-1.5 py-0.5 rounded border ${
            site.source === "config"
              ? "border-tremor-border opacity-70"
              : "border-tremor-brand text-tremor-brand"
          }`}
          title={
            site.source === "config"
              ? "Managed by [[web.site]] in central-logs.toml — edit the TOML and restart"
              : "Created at runtime; editable here"
          }
        >
          {site.source === "config" ? "from config" : "runtime"}
        </span>
        {site.has_exclude_ips && (
          <span className="text-xs opacity-60">· own visits excluded</span>
        )}
        {site.has_user_header && (
          <span className="text-xs opacity-60">· user header on</span>
        )}
        <span className="ml-auto flex gap-2">
          {site.source === "db" && (
            <>
              <button
                type="button"
                className="px-2 py-1 rounded text-xs border border-tremor-border"
                onClick={() => setEditing((v) => !v)}
              >
                {editing ? "Close editor" : "Edit"}
              </button>
              {!deleting && (
                <button
                  type="button"
                  className="px-2 py-1 rounded text-xs border border-red-500/40 text-red-600 dark:text-red-400"
                  onClick={() => {
                    setDeleting(true);
                    setConfirmText("");
                  }}
                >
                  Delete
                </button>
              )}
            </>
          )}
        </span>
      </div>

      {/* Origins in full, one per line. */}
      <div className="cl-stat-label mb-1">Allowed origins</div>
      <ul className="text-sm font-mono mb-3 space-y-0.5">
        {site.allowed_origins.length === 0 && <li className="opacity-50">none</li>}
        {site.allowed_origins.map((o) => (
          <li key={o} className="break-all">
            {o}
          </li>
        ))}
      </ul>

      {editing && (
        <div className="mb-3">
          <ProjectForm initial={site} onDone={() => { setEditing(false); onChanged(); }} />
        </div>
      )}

      {deleting && (
        <div className="mb-3 p-3 rounded border border-red-500/40 text-sm space-y-2">
          <p>
            This permanently removes the project <b>{site.id}</b>. <b>All
            collected analytics data for it is deleted</b> (events, sessions,
            rollups; archived files age out per retention). The tracker will
            reject new beacons immediately.
          </p>
          <p className="opacity-80">Type the project id to confirm:</p>
          <div className="flex flex-wrap items-center gap-2">
            <input
              value={confirmText}
              onChange={(e) => setConfirmText(e.target.value)}
              placeholder={site.id}
              aria-label={`Confirm deletion of ${site.id}`}
              className={`${inputCls} font-mono w-56`}
              spellCheck={false}
            />
            <button
              type="button"
              disabled={busy || confirmText.trim() !== site.id}
              onClick={doDelete}
              className="px-2.5 py-1.5 rounded text-xs bg-red-600 text-white disabled:opacity-50"
            >
              Delete project and its data
            </button>
            <button
              type="button"
              onClick={() => setDeleting(false)}
              className="px-2.5 py-1.5 rounded text-xs border border-tremor-border"
            >
              Cancel
            </button>
          </div>
        </div>
      )}
      {err && <div className="text-sm text-red-500 mb-2">{err}</div>}

      <GoalEditor siteId={site.id} expand={expandGoal} />
    </div>
  );
}

/** Donation-funnel goal, per project. A step is a page-path glob (`*`),
 *  a custom event (`event:name`) or an exit-link destination (`outbound:wa.me`).
 *  Saved locally per browser (the dashboard reads the same key). */
function GoalEditor({ siteId, expand }: { siteId: string; expand: boolean }) {
  const key = funnelKey(siteId);
  const [open, setOpen] = useState(expand);
  const [draft, setDraft] = useState("");
  const [saved, setSaved] = useState(false);
  const [savedSteps, setSavedSteps] = useState<string[]>([]);

  useEffect(() => {
    try {
      const raw = localStorage.getItem(key);
      const arr = raw ? (JSON.parse(raw) as unknown) : [];
      const steps = Array.isArray(arr)
        ? arr.filter((s): s is string => typeof s === "string").slice(0, 8)
        : [];
      setSavedSteps(steps);
      setDraft(steps.join("\n"));
    } catch {
      setSavedSteps([]);
      setDraft("");
    }
  }, [key]);

  const dirty = draft.trim() !== savedSteps.join("\n").trim();

  function save(e: React.FormEvent) {
    e.preventDefault();
    const next = draft.split("\n").map((s) => s.trim()).filter(Boolean).slice(0, 8);
    try {
      if (next.length >= 2) localStorage.setItem(key, JSON.stringify(next));
      else localStorage.removeItem(key);
    } catch {
      /* private mode */
    }
    setSavedSteps(next.length >= 2 ? next : []);
    setSaved(true);
    setTimeout(() => setSaved(false), 2500);
    trackEvent("ui.projects.goal_saved", { steps: next.length });
  }

  return (
    <div id="goal" className="border-t border-tremor-border pt-3">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="text-sm font-medium flex items-center gap-2"
      >
        Donation-funnel goal
        <span className="cl-stat-label">
          {savedSteps.length >= 2 ? `· ${savedSteps.length} steps` : "· not set"}
        </span>
        <span className="text-xs underline opacity-60">{open ? "hide" : "edit"}</span>
      </button>
      {open && (
        <form onSubmit={save} className="mt-2 space-y-2">
          <p className="text-xs opacity-70">
            One step per line, top to bottom. Steps are page-path patterns
            (<code>/program/*</code>), custom events (<code>event:donate_click</code>)
            or exit-link destinations (<code>outbound:wa.me</code>) — the real
            conversion is usually the WhatsApp/bank click after the donation
            page.
          </p>
          <textarea
            className={`${inputCls} font-mono`}
            rows={4}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={"/\n/program/*\n/donasi\noutbound:wa.me"}
            spellCheck={false}
          />
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={!dirty}
              className="px-2.5 py-1.5 rounded text-xs bg-tremor-brand text-white disabled:opacity-50"
            >
              Save goal
            </button>
            {saved && <span className="text-xs text-emerald-600 dark:text-emerald-400">Goal saved ✓</span>}
            {!saved && savedSteps.length >= 2 && !dirty && (
              <span className="text-xs opacity-60">Saved — shown on the dashboard</span>
            )}
          </div>
        </form>
      )}
    </div>
  );
}
