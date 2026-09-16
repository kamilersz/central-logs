// DSN setup wizard: collects a project name, creates an insert-scoped API key,
// then renders copy-paste DSN + SDK snippets. Lives in /errors because the
// DSN it produces is specifically for shipping Sentry-compatible events.

import { useEffect, useMemo, useState } from "react";
import { api } from "../api";

interface Props {
  /** Pre-fill the project name (e.g. when the wizard was opened from a hint). */
  initialProject?: string;
  onClose: () => void;
}

interface Snippet {
  id: string;
  label: string;
  code: string;
}

/** Build the host portion of the DSN — the same host the user is currently
 *  looking at, so the SDK ships to the right place without env juggling. */
function currentHost(): string {
  const loc = window.location;
  const proto = loc.protocol.replace(":", "");
  const port = loc.port && loc.port !== "80" && loc.port !== "443" ? `:${loc.port}` : "";
  return `${proto}://${loc.hostname}${port}`;
}

/** Slugify a project name into a URL-safe service identifier (matches the
 *  /api/{project}/envelope route shape and the `service` column on rows). */
function slugify(s: string): string {
  return s
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
}

/** Build Sentry SDK snippets for the most common platforms. Each assumes
 *  the user will paste the DSN into the project's env (SENTRY_DSN) — but we
 *  also inline it so the snippet is truly copy-paste. */
function snippetsFor(project: string, dsn: string): Snippet[] {
  return [
    {
      id: "node",
      label: "Node.js",
      code:
        `npm i @sentry/node\n` +
        `// app.js\n` +
        `const Sentry = require("@sentry/node");\n` +
        `Sentry.init({ dsn: "${dsn}", tracesSampleRate: 0.1 });`,
    },
    {
      id: "python",
      label: "Python",
      code:
        `pip install sentry-sdk\n` +
        `# app.py\n` +
        `import sentry_sdk\n` +
        `sentry_sdk.init(dsn="${dsn}", traces_sample_rate=0.1)`,
    },
    {
      id: "browser",
      label: "Browser (JS)",
      code:
        `npm i @sentry/browser\n` +
        `import * as Sentry from "@sentry/browser";\n` +
        `Sentry.init({ dsn: "${dsn}", tracesSampleRate: 0.1 });`,
    },
    {
      id: "go",
      label: "Go",
      code:
        `go get github.com/getsentry/sentry-go\n` +
        `import "github.com/getsentry/sentry-go"\n` +
        `sentry.Init(sentry.ClientOptions{Dsn: "${dsn}", TracesSampleRate: 0.1})`,
    },
    {
      id: "curl",
      label: "curl (test)",
      code: [
        `# Send a synthetic event — useful to confirm the ingest path works`,
        `# before wiring up a real SDK. The key is just the public part of`,
        `# the DSN (the bit before @).`,
        `KEY=$(echo "${dsn}" | sed -E 's|^.*/([^/]+)@.*$|\\1|')`,
        `URL=$(echo "${dsn}" | sed -E 's|^([^/]+://[^/]+)/.*$|\\1|')`,
        `curl -X POST "$URL/api/${project}/envelope/" \\`,
        `  -H "Content-Type: application/x-sentry-envelope" \\`,
        `  -H "X-Sentry-Auth: Sentry sentry_version=7, sentry_key=$KEY" \\`,
        `  --data-binary @envelope.bin`,
      ].join("\n"),
    },
  ];
}

export default function DsnWizard({ initialProject = "", onClose }: Props) {
  const [project, setProject] = useState(initialProject);
  const [keyName, setKeyName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [createdKey, setCreatedKey] = useState<string | null>(null);
  const [keyId, setKeyId] = useState<number | null>(null);
  const [tab, setTab] = useState("node");

  const host = useMemo(() => currentHost(), []);
  const slug = slugify(project);
  const dsn = createdKey && slug ? `${host}/${slug}?sentry_key=${createdKey}` : null;
  const snippets = dsn ? snippetsFor(slug, dsn) : [];
  const active = snippets.find((s) => s.id === tab) ?? snippets[0];

  // Keep the key-name input sane — defaults to the project name and updates
  // as the user types. We allow the user to override so two captures against
  // the same project (e.g. web + worker) get distinct keys.
  useEffect(() => {
    if (!keyName) setKeyName(project ? `sentry:${project.trim()}` : "");
  }, [project, keyName]);

  async function generate() {
    if (!slug) {
      setError("project name must contain at least one letter or digit");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const r = await api.createApiKey({
        name: keyName.trim() || `sentry:${slug}`,
        scopes: "insert",
      });
      setCreatedKey(r.key);
      setKeyId(r.id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function reset() {
    setCreatedKey(null);
    setKeyId(null);
    setError(null);
  }

  return (
    <div
      className="fixed inset-0 z-20 flex items-center justify-center bg-black/40 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-2xl max-h-[90vh] overflow-auto rounded-lg border border-tremor-border bg-tremor-background dark:bg-dark-tremor-background shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-tremor-border px-5 py-3">
          <div>
            <h2 className="text-base font-semibold">DSN setup wizard</h2>
            <p className="text-xs text-tremor-content-subtle">
              Creates a dedicated insert-scoped key so a new Sentry SDK can ship events here.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="px-2 py-1 text-sm text-tremor-content-subtle hover:text-tremor-content"
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        <div className="p-5 space-y-4">
          {error && (
            <div className="p-3 rounded bg-red-100 dark:bg-red-900/30 text-red-800 text-sm">
              {error}
            </div>
          )}

          {!createdKey ? (
            <>
              <Step n={1} title="Pick a project name">
                <p className="text-xs text-tremor-content-subtle mb-2">
                  This becomes the <code>service</code> column on incoming errors and the
                  path in the DSN. Use lowercase, hyphens or dots — no spaces.
                </p>
                <input
                  type="text"
                  value={project}
                  onChange={(e) => setProject(e.target.value)}
                  placeholder="my-app"
                  className="w-full px-3 py-2 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm font-mono"
                  autoFocus
                />
                {project && slug !== project.trim() && (
                  <div className="text-xs text-tremor-content-subtle mt-1">
                    DSN slug: <code>{slug || "(empty)"}</code>
                  </div>
                )}
              </Step>

              <Step n={2} title="Name the API key">
                <p className="text-xs text-tremor-content-subtle mb-2">
                  Anything you can recognise in the API Keys list later.
                </p>
                <input
                  type="text"
                  value={keyName}
                  onChange={(e) => setKeyName(e.target.value)}
                  placeholder="sentry:my-app"
                  className="w-full px-3 py-2 bg-tremor-background dark:bg-dark-tremor-background border border-tremor-border dark:border-dark-tremor-border rounded text-sm font-mono"
                />
                <div className="text-xs text-tremor-content-subtle mt-1">
                  Scope will be <code>insert</code> — the minimum needed to send events.
                </div>
              </Step>

              <div className="flex justify-end gap-2 pt-2">
                <button
                  type="button"
                  onClick={onClose}
                  className="px-3 py-1.5 rounded text-sm border border-tremor-border"
                >
                  Cancel
                </button>
                <button
                  type="button"
                  onClick={generate}
                  disabled={busy || !slug}
                  className="px-3 py-1.5 rounded text-sm bg-tremor-brand text-white disabled:opacity-50"
                >
                  {busy ? "Creating key…" : "Create DSN"}
                </button>
              </div>
            </>
          ) : (
            <>
              <div className="p-3 rounded bg-emerald-100 dark:bg-emerald-900/30 text-emerald-800 dark:text-emerald-200 text-sm">
                Created insert key #{keyId}. Copy the DSN below — it is shown only once.
              </div>

              <Step n={3} title="Your DSN">
                <CopyBlock value={dsn ?? ""} />
                <p className="text-xs text-tremor-content-subtle mt-2">
                  Set <code>SENTRY_DSN</code> in your service's environment, or paste it
                  into <code>Sentry.init({"{"}dsn: …{"}"})</code>. Errors will land in the
                  Errors page within seconds.
                </p>
              </Step>

              <Step n={4} title="Install snippet">
                <div className="flex flex-wrap gap-1 mb-2">
                  {snippets.map((s) => (
                    <button
                      key={s.id}
                      type="button"
                      onClick={() => setTab(s.id)}
                      className={`px-2.5 py-1 rounded text-xs border ${
                        tab === s.id
                          ? "border-tremor-brand text-tremor-brand"
                          : "border-tremor-border text-tremor-content-subtle"
                      }`}
                    >
                      {s.label}
                    </button>
                  ))}
                </div>
                {active && <CopyBlock value={active.code} language="shell" />}
              </Step>

              <Step n={5} title="Verify">
                <p className="text-xs text-tremor-content-subtle">
                  The <strong>curl</strong> tab above sends a synthetic event end-to-end.
                  If it returns <code>200</code> and a new event appears on this page in
                  a few seconds, the SDK install will too.
                </p>
              </Step>

              <div className="flex justify-between gap-2 pt-2">
                <button
                  type="button"
                  onClick={reset}
                  className="px-3 py-1.5 rounded text-sm border border-tremor-border"
                >
                  Create another
                </button>
                <button
                  type="button"
                  onClick={onClose}
                  className="px-3 py-1.5 rounded text-sm bg-tremor-brand text-white"
                >
                  Done
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function Step({ n, title, children }: { n: number; title: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="flex items-baseline gap-2 mb-1.5">
        <span className="cl-mono text-xs px-1.5 py-0.5 rounded border border-tremor-border text-tremor-content-subtle">
          {n}
        </span>
        <h3 className="text-sm font-medium">{title}</h3>
      </div>
      {children}
    </div>
  );
}

function CopyBlock({ value, language }: { value: string; language?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="relative">
      <pre className="px-3 py-2 rounded bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border text-xs whitespace-pre-wrap break-all font-mono">
        {value}
      </pre>
      <button
        type="button"
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(value);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          } catch {
            /* clipboard not available */
          }
        }}
        className="absolute top-1.5 right-1.5 px-2 py-0.5 rounded text-[11px] border border-tremor-border bg-tremor-background hover:bg-tremor-background-muted"
      >
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}
