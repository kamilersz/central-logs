import { useEffect, useState } from "react";
import { api, ApiKeyOut, CreateApiKeyResponse } from "../api";

const ALL_SCOPES = ["insert", "read", "write", "admin"] as const;
type ScopeName = (typeof ALL_SCOPES)[number];

/**
 * Admin-only page: list / create / revoke API keys.
 *
 * Raw keys are shown EXACTLY ONCE at creation time (the backend doesn't keep
 * them — only their SHA-256 hash). The UI displays a copy button + warning
 * banner; once dismissed, the key cannot be recovered.
 */
export default function ApiKeysPage() {
  const [keys, setKeys] = useState<ApiKeyOut[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<CreateApiKeyResponse | null>(null);

  // Create-form state.
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<Set<ScopeName>>(new Set(["read"]));

  async function refresh() {
    setLoading(true);
    try {
      setKeys(await api.listApiKeys());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  function toggleScope(s: ScopeName) {
    setScopes((prev) => {
      const next = new Set(prev);
      if (next.has(s)) next.delete(s);
      else next.add(s);
      return next;
    });
  }

  async function onCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim()) return;
    if (scopes.size === 0) {
      setError("pick at least one scope");
      return;
    }
    // `admin` implies everything else; the backend will expand it, but we
    // also expand client-side so the displayed scope list matches what was
    // actually requested.
    const scopeStr = Array.from(scopes).join(",");
    try {
      const resp = await api.createApiKey({ name: name.trim(), scopes: scopeStr });
      setCreated(resp);
      setName("");
      setScopes(new Set(["read"]));
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  }

  async function onRevoke(id: number, label: string) {
    if (!window.confirm(`Revoke key "${label}"? Active clients using it will be disconnected.`)) return;
    try {
      await api.revokeApiKey(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-xl font-semibold text-tremor-content-strong dark:text-dark-tremor-content-strong">
          API Keys
        </h1>
        <p className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-1">
          Issue scoped keys to insert clients, dashboards, and agents. Keys are
          stored as SHA-256 hashes; the raw value is shown once at creation.
        </p>
      </div>

      {error && (
        <div className="p-3 rounded border border-red-500/40 bg-red-500/10 text-sm text-red-500">
          {error}
        </div>
      )}

      {created && (
        <div className="p-4 rounded border border-amber-500/50 bg-amber-500/10">
          <div className="font-semibold text-amber-600 dark:text-amber-400 mb-1">
            Copy this key now — it will not be shown again.
          </div>
          <code className="block font-mono text-sm bg-tremor-background-muted dark:bg-dark-tremor-background-muted p-2 rounded break-all">
            {created.key}
          </code>
          <div className="text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mt-2">
            {created.name} · prefix {created.key_prefix}… · scopes: {created.scopes.join(",")}
          </div>
          <button
            type="button"
            onClick={() => navigator.clipboard.writeText(created.key)}
            className="mt-2 px-2.5 py-1 rounded text-sm border border-tremor-border dark:border-dark-tremor-border hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
          >
            Copy
          </button>
          <button
            type="button"
            onClick={() => setCreated(null)}
            className="ml-2 mt-2 px-2.5 py-1 rounded text-sm border border-tremor-border dark:border-dark-tremor-border hover:bg-tremor-background-muted dark:hover:bg-dark-tremor-background-muted"
          >
            Dismiss
          </button>
        </div>
      )}

      <form onSubmit={onCreate} className="p-4 rounded border border-tremor-border dark:border-dark-tremor-border space-y-3">
        <div className="text-sm font-medium">Create new key</div>
        <div className="flex flex-wrap gap-3 items-end">
          <label className="flex-1 min-w-[200px]">
            <span className="block text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-1">
              Name
            </span>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. api-shipper-prod"
              maxLength={128}
              className="w-full px-2.5 py-1.5 rounded text-sm bg-transparent border border-tremor-border dark:border-dark-tremor-border"
            />
          </label>
          <div>
            <span className="block text-xs text-tremor-content-subtle dark:text-dark-tremor-content-subtle mb-1">
              Scopes
            </span>
            <div className="flex gap-2">
              {ALL_SCOPES.map((s) => (
                <label key={s} className="flex items-center gap-1 text-sm">
                  <input
                    type="checkbox"
                    checked={scopes.has(s)}
                    onChange={() => toggleScope(s)}
                  />
                  {s}
                </label>
              ))}
            </div>
          </div>
          <button
            type="submit"
            className="px-3 py-1.5 rounded text-sm bg-tremor-brand dark:bg-dark-tremor-brand text-white"
          >
            Generate
          </button>
        </div>
      </form>

      <div>
        <div className="text-sm font-medium mb-2">Active keys ({keys.length})</div>
        {loading ? (
          <div className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
            loading…
          </div>
        ) : keys.length === 0 ? (
          <div className="text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
            no active keys
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-xs uppercase text-tremor-content-subtle dark:text-dark-tremor-content-subtle border-b border-tremor-border dark:border-dark-tremor-border">
                <th className="py-2 pr-3">Name</th>
                <th className="py-2 pr-3">Prefix</th>
                <th className="py-2 pr-3">Scopes</th>
                <th className="py-2 pr-3">Created</th>
                <th className="py-2 pr-3">Last used</th>
                <th className="py-2 pr-3"></th>
              </tr>
            </thead>
            <tbody>
              {keys.map((k) => (
                <tr key={k.id} className="border-b border-tremor-border dark:border-dark-tremor-border">
                  <td className="py-2 pr-3">{k.name}</td>
                  <td className="py-2 pr-3 font-mono">{k.key_prefix}…</td>
                  <td className="py-2 pr-3">{k.scopes.join(",")}</td>
                  <td className="py-2 pr-3 text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                    {new Date(k.created_at).toLocaleString()}
                  </td>
                  <td className="py-2 pr-3 text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
                    {k.last_used_at ? new Date(k.last_used_at).toLocaleString() : "—"}
                  </td>
                  <td className="py-2 pr-3 text-right">
                    <button
                      type="button"
                      onClick={() => onRevoke(k.id, k.name)}
                      className="px-2 py-0.5 rounded text-xs border border-red-500/40 text-red-500 hover:bg-red-500/10"
                    >
                      Revoke
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
