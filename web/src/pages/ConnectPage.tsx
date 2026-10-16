import { useEffect, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, type WebSitesResponse, type WhoamiResponse } from "../api";
import { trackEvent } from "../track";

/**
 * Connection guide (in-app version of INTEGRATION.md): copy-paste snippets for
 * shipping LOGS from every supported platform, plus the WEBSITE-ANALYTICS
 * setup (tracker + proxy). Snippets embed the host the user is currently
 * browsing and — once created/pasted — a real insert-scoped API key.
 */

// =====================================================================
// Shared bits
// =====================================================================

function currentOrigin(): string {
  const loc = window.location;
  const port = loc.port && loc.port !== "80" && loc.port !== "443" ? `:${loc.port}` : "";
  return `${loc.protocol}//${loc.hostname}${port}`;
}

function currentHostPort(): string {
  return window.location.host;
}

/** Copyable code block. */
function CodeBlock({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="relative">
      <pre className="px-3 py-2 pr-16 rounded bg-tremor-background-muted dark:bg-dark-tremor-background-muted border border-tremor-border text-xs whitespace-pre-wrap break-all font-mono">
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
            /* clipboard unavailable */
          }
        }}
        className="absolute top-1.5 right-1.5 px-2 py-0.5 rounded text-[11px] border border-tremor-border bg-tremor-background hover:bg-tremor-background-muted"
      >
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

function Step({ n, title, children }: { n: number | string; title: string; children?: React.ReactNode }) {
  return (
    <div>
      <div className="flex items-baseline gap-2 mb-1.5">
        <span className="cl-mono text-xs px-1.5 py-0.5 rounded border border-tremor-border text-tremor-content-subtle shrink-0">
          {n}
        </span>
        <h3 className="text-sm font-medium">{title}</h3>
      </div>
      {children && <div className="mt-1.5">{children}</div>}
    </div>
  );
}

/** Platform/dimension tab chips. */
function Chips<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { id: T; label: string }[];
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="flex flex-wrap gap-1 mb-3">
      {options.map((o) => (
        <button
          key={o.id}
          type="button"
          onClick={() => onChange(o.id)}
          className={`px-2.5 py-1 rounded text-xs border ${
            value === o.id
              ? "border-tremor-brand text-tremor-brand"
              : "border-tremor-border text-tremor-content-subtle hover:text-tremor-content"
          }`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

// =====================================================================
// Key plumbing: paste an existing insert key, or mint one (admin only)
// =====================================================================

function KeyBox({
  apiKey,
  setApiKey,
}: {
  apiKey: string;
  setApiKey: (k: string) => void;
}) {
  const [whoami, setWhoami] = useState<WhoamiResponse | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api
      .whoami()
      .then((w) => !cancelled && setWhoami(w))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  const isAdmin = whoami?.scopes.includes("admin") ?? false;

  async function create() {
    setBusy(true);
    setError(null);
    try {
      const r = await api.createApiKey({
        name: `connect:${new Date().toISOString().slice(0, 10)}`,
        scopes: "insert",
      });
      trackEvent("ui.connect.key_create", {});
      setApiKey(r.key);
      setCreated(true);
    } catch (e) {
      setError(
        String(e).includes("403")
          ? "Creating keys requires an admin key — paste an existing insert-scoped key instead, or ask an admin (API Keys page)."
          : String(e),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="p-3 rounded cl-card-plain border border-dashed border-tremor-border mb-4">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="cl-stat-label">insert API key</span>
        <input
          type="text"
          value={apiKey}
          onChange={(e) => setApiKey(e.target.value.trim())}
          placeholder="paste an insert-scoped key (clk_…) — snippets stay templated until you do"
          className="cl-input py-1 flex-1 min-w-[16rem] font-mono text-xs"
          spellCheck={false}
        />
        {isAdmin && (
          <button
            type="button"
            onClick={create}
            disabled={busy}
            className="px-2.5 py-1 rounded text-xs bg-tremor-brand text-white disabled:opacity-50"
          >
            {busy ? "Creating…" : "Create one"}
          </button>
        )}
      </div>
      {created && (
        <div className="mt-2 text-xs text-emerald-700 dark:text-emerald-300">
          Key created — it is shown only here, copy a snippet below now. You can
          revoke it later on the API Keys page.
        </div>
      )}
      {error && <div className="mt-2 text-xs text-amber-600">{error}</div>}
      {!isAdmin && !apiKey && (
        <div className="mt-2 text-xs text-tremor-content-subtle">
          No insert key yet? Admins can create one here or on the{" "}
          <Link to="/api-keys" className="underline">
            API Keys
          </Link>{" "}
          page (scope <code>insert</code> is enough to ship logs).
        </div>
      )}
    </div>
  );
}

// =====================================================================
// Logs snippets — one builder per platform
// =====================================================================

interface Snippet {
  label: string;
  code: string;
}

function logSnippets(origin: string, key: string, syslogHost: string): Record<string, Snippet[]> {
  const K = key || "PASTE_INSERT_KEY";
  const auth = `Authorization: Bearer ${K}`;
  return {
    http: [
      {
        label: "curl — single event",
        code: `curl -X POST ${origin}/v1/logs \\
  -H "${auth}" \\
  -H "Content-Type: application/json" \\
  -d '{"service":"my-app","level":"info","msg":"hello from curl"}'`,
      },
      {
        label: "curl — batch (JSON array / NDJSON)",
        code: `curl -X POST ${origin}/v1/logs \\
  -H "${auth}" \\
  -H "Content-Type: application/x-ndjson" \\
  --data-binary $'{"service":"my-app","level":"info","msg":"first"}\\n{"service":"my-app","level":"error","msg":"second"}'`,
      },
      {
        label: "Node.js (fetch)",
        code: `await fetch("${origin}/v1/logs", {
  method: "POST",
  headers: { "Content-Type": "application/json", Authorization: "Bearer ${K}" },
  body: JSON.stringify({ service: "my-app", level: "info", msg: "hi", order_id: "ORD-1" }),
});`,
      },
      {
        label: "Python (requests)",
        code: `import requests
requests.post(
    "${origin}/v1/logs",
    headers={"Authorization": "Bearer ${K}"},
    json={"service": "my-app", "level": "info", "msg": "hello"},
).raise_for_status()`,
      },
      {
        label: "Go (net/http)",
        code: `body := []byte(\`{"service":"my-app","level":"info","msg":"hello"}\`)
req, _ := http.NewRequest("POST", "${origin}/v1/logs", bytes.NewReader(body))
req.Header.Set("Authorization", "Bearer ${K}")
req.Header.Set("Content-Type", "application/json")
http.DefaultClient.Do(req)`,
      },
    ],
    otlp: [
      {
        label: "OTel env vars (any SDK)",
        code: `# The OpenTelemetry SDK/collector speaks OTLP/HTTP (protobuf + JSON):
export OTEL_EXPORTER_OTLP_ENDPOINT="${origin}"
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer ${K}"
# Traces and metrics land on /v1/traces and /v1/metrics automatically.`,
      },
      {
        label: "Node.js SDK",
        code: `npm i @opentelemetry/sdk-node @opentelemetry/exporter-logs-otlp-http \\
      @opentelemetry/auto-instrumentations-node

# otel.ts
import { NodeSDK } from "@opentelemetry/sdk-node";
import { OTLPLogExporter } from "@opentelemetry/exporter-logs-otlp-http";
const sdk = new NodeSDK({
  logRecordProcessors: [new BatchLogRecordProcessor(new OTLPLogExporter({
    url: "${origin}/v1/logs",
    headers: { Authorization: "Bearer ${K}" },
  }))],
});
sdk.start();`,
      },
      {
        label: "Python SDK",
        code: `pip install opentelemetry-sdk opentelemetry-exporter-otlp-proto-http

# LoggingHandler ships stdlib logging records as OTLP logs:
import logging
from opentelemetry.sdk._logs.export import OTLPLogExporter
from opentelemetry._logs import set_logger_provider
from opentelemetry.sdk._logs import LoggingHandler, LoggerProvider, BatchLogRecordProcessor

provider = LoggerProvider()
provider.add_log_record_processor(BatchLogRecordProcessor(OTLPLogExporter(
    endpoint="${origin}/v1/logs",
    headers={"Authorization": "Bearer ${K}"},
)))
set_logger_provider(provider)
logging.getLogger().addHandler(LoggingHandler())`,
      },
    ],
    syslog: [
      {
        label: "logger (quick test)",
        code: `# UDP (config: syslog_udp_bind, default :5140)
logger -n ${syslogHost.split(":")[0]} -P 5140 -d "hello from syslog"
# TCP
echo "<14>$(date +'%b %e %T') my-host my-app: hello via tcp" | nc ${syslogHost.split(":")[0]} 5140`,
      },
      {
        label: "rsyslog forward",
        code: `# /etc/rsyslog.d/90-central-logs.conf
*.* @${syslogHost.split(":")[0]}:5140   # @ = UDP, @@ = TCP
# then: systemctl restart rsyslog`,
      },
      {
        label: "syslog-ng forward",
        code: `# /etc/syslog-ng/conf.d/central-logs.conf
destination d_cl { syslog("${syslogHost.split(":")[0]}" transport("udp") port(5140)); };
log { source(s_src); destination(d_cl); };`,
      },
    ],
    docker: [
      {
        label: "gelf driver",
        code: `docker run -d --name my-app \\
  --log-driver=gelf \\
  --log-opt gelf-address=udp://${syslogHost.split(":")[0]}:12201 \\
  --log-opt tag=my-app \\
  my-image`,
      },
      {
        label: "fluentd driver",
        code: `docker run -d --name my-app \\
  --log-driver=fluentd \\
  --log-opt fluentd-address=${syslogHost.split(":")[0]}:24224 \\
  --log-opt tag=my-app \\
  my-image`,
      },
      {
        label: "splunk driver (HEC)",
        code: `docker run -d --name my-app \\
  --log-driver=splunk \\
  --log-opt splunk-url=${origin} \\
  --log-opt splunk-token=${K} \\
  --log-opt splunk-verify-connection=false \\
  --log-opt tag=my-app \\
  my-image`,
      },
      {
        label: "compose file",
        code: `services:
  my-app:
    image: my-image
    logging:
      driver: gelf
      options:
        gelf-address: "udp://${syslogHost.split(":")[0]}:12201"
        tag: my-app`,
      },
      {
        label: "built-in collector (no driver change)",
        code: `# central-logs.toml — pull container logs via the Docker Engine API:
[collector.docker]
enabled = true
socket = "unix:///var/run/docker.sock"
include = ["my-app-*"]   # container-name globs, [] = all
tail_lines = 100         # history on first attach`,
      },
    ],
    kubernetes: [
      {
        label: "API-server audit webhook",
        code: `# audit-backend.yaml — kube-apiserver --audit-webhook-config-file
apiVersion: v1
kind: Config
preferences: {}
users:
  - name: central-logs
    user:
      url: ${origin}/ingest/kubernetes/audit
      # extra headers work in in-tree backends:
      # headers: [["Authorization", "Bearer ${K}"]]`,
      },
      {
        label: "built-in pod collector",
        code: `# central-logs.toml — follow pod logs via the Kubernetes API
# (works in-cluster with the default service account):
[collector.kubernetes]
enabled = true
namespaces = []            # [] = all namespaces
label_selector = "app=my-app"
refresh_secs = 30`,
      },
      {
        label: "DaemonSet collectors (fluent-bit)",
        code: `# Any agent that speaks HTTP works — e.g. Fluent Bit tail → central-logs:
[OUTPUT]
    Name          http
    Match         kube.*
    Host          ${window.location.hostname}
    Port          ${window.location.port || "8080"}
    URI           /v1/logs
    Format        json
    # one log line per request body; wrap as needed
    json_date_key ts
    AddHeaders    [{"name":"Authorization","value":"Bearer ${K}"}]`,
      },
    ],
    agents: [
      {
        label: "Vector",
        code: `# vector.toml — send JSON events to /v1/logs
[sources.app]
type = "file"
include = ["/var/log/my-app/*.log"]

[sinks.central_logs]
type = "http"
inputs = ["app"]
uri = "${origin}/v1/logs"
encoding.codec = "json"
compression = "none"
request.headers.Authorization = "Bearer ${K}"`,
      },
      {
        label: "Fluent Bit (http out)",
        code: `[INPUT]
    Name   tail
    Path   /var/log/my-app/*.log
    Tag    my-app

[OUTPUT]
    Name    http
    Match   *
    Host    ${window.location.hostname}
    Port    ${window.location.port || "8080"}
    URI     /v1/logs
    Format  json
    AddHeaders  [{"name":"Authorization","value":"Bearer ${K}"}]`,
      },
      {
        label: "Promtail / Loki → just point HTTP at /v1/logs",
        code: `# Promtail pushes via HTTP too:
clients:
  - url: ${origin}/v1/logs
    headers:
      Authorization: Bearer ${K}
# (NDJSON bodies are accepted line-by-line)`,
      },
    ],
    sentry: [
      {
        label: "Sentry DSN",
        code: `# central-logs is Sentry-SDK compatible — point any SDK here:
SENTRY_DSN="${window.location.protocol}//<insert-key>@${window.location.host}/my-project"

# The Errors page has a DSN wizard that mints the key and renders SDK
# snippets for Node/Python/Browser/Go.`,
      },
    ],
  };
}

// =====================================================================
// Logs tab
// =====================================================================

const LOG_PLATFORMS = [
  { id: "http", label: "HTTP / JSON" },
  { id: "otlp", label: "OpenTelemetry" },
  { id: "syslog", label: "Syslog" },
  { id: "docker", label: "Docker drivers" },
  { id: "kubernetes", label: "Kubernetes" },
  { id: "agents", label: "Agents (Vector/Fluent Bit)" },
  { id: "sentry", label: "Sentry SDK (errors)" },
] as const;

type LogPlatform = (typeof LOG_PLATFORMS)[number]["id"];

function LogsGuide() {
  const origin = useMemo(() => currentOrigin(), []);
  const syslogHost = useMemo(() => currentHostPort(), []);
  const [platform, setPlatform] = useState<LogPlatform>("http");
  const [apiKey, setApiKey] = useState("");
  const snippets = logSnippets(origin, apiKey, syslogHost)[platform] ?? [];

  return (
    <div>
      <KeyBox apiKey={apiKey} setApiKey={setApiKey} />

      <Chips options={LOG_PLATFORMS.map((p) => ({ id: p.id, label: p.label }))} value={platform} onChange={(p) => { setPlatform(p); trackEvent("ui.connect.platform", { platform: p }); }} />

      <div className="space-y-4">
        {snippets.map((s, i) => (
          <div key={s.label}>
            <div className="text-xs font-medium mb-1 text-tremor-content-subtle">{s.label}</div>
            <CodeBlock value={s.code} />
            {i === snippets.length - 1 && platform === "http" && (
              <div className="mt-2 text-xs text-tremor-content-subtle">
                Verify: rows appear in the{" "}
                <Link to="/" className="underline">
                  Logs explorer
                </Link>{" "}
                within a second of the <code>202/200</code>. Rejections and per-source
                counters live on the{" "}
                <Link to="/pipeline" className="underline">
                  Pipeline
                </Link>{" "}
                page.
              </div>
            )}
          </div>
        ))}
      </div>

      <div className="mt-4 p-3 rounded cl-card-plain border border-tremor-border text-xs text-tremor-content-subtle">
        <strong className="text-tremor-content">Every protocol above rides the same pipeline:</strong>{" "}
        insert → WAL (fsync) → ingest → DuckDB → rollups → dashboards &amp; MCP.
        Fields like <code>service</code>, <code>level</code>, <code>msg</code> and any extra
        JSON keys are queryable (<code>level:error</code>, <code>service:my-app</code>, hot
        attributes for hot paths). Full write-up: <code>INTEGRATION.md</code> in the repo.
      </div>
    </div>
  );
}

// =====================================================================
// Web analytics tab
// =====================================================================

function WebGuide() {
  const origin = useMemo(() => currentOrigin(), []);
  const { data: sitesResp, refresh: refreshSites } = useWebSitesPoll();
  const sites = sitesResp?.sites ?? [];
  const [projectId, setProjectId] = useState("");
  const [proxy, setProxy] = useState<"caddy" | "nginx" | "app">("caddy");
  // The id used across snippets: typed input wins, else first project, else hint.
  const activeSite = projectId.trim() || sites[0]?.id || "";
  const idOk = /^[A-Za-z0-9._-]{1,64}$/.test(activeSite);
  const alreadyExists = sites.some((s) => s.id === activeSite);

  const embed = `<script defer data-site="${activeSite || "your-project-id"}" data-endpoint="/_/e" src="/_/s.js"></script>`;

  const configToml = `[web]
enabled = true
trusted_proxies = ["127.0.0.1/32", "::1/128"]  # who may set X-Forwarded-For / X-CL-User
rate_limit_per_ip_per_min = 120
shed_when_wal_fill_pct = 80     # drop analytics before logs feel WAL pressure
honor_dnt = true                # DNT / Sec-GPC → 202, not stored
raw_retention_days = 90
auto_create_projects = true     # first beacon with a new id creates the project
auto_create_max_projects = 100  # cap on total projects (anti-spam)

# Optional seeds below — with auto-create ON, new projects usually need NO
# config at all. Seeds are for infra-managed ids (e.g. self-tracking).
# [[web.site]]
# id = "${activeSite || "your-project-id"}"
# allowed_origins = ["https://your-site.example"]
# user_header = "X-CL-User"                      # optional: authenticated users`;

  const caddy = `# Reverse proxy on YOUR site — neutral paths dodge ad-blockers.
your-site.example {
    # tracker script + collect endpoint → central-logs (same-origin)
    handle_path /_/s.js { reverse_proxy ${origin}/v1/web/t.js }
    handle_path /_/e    { reverse_proxy ${origin}/v1/web/collect }

    # your app on everything else
    handle { reverse_proxy 127.0.0.1:3000 }
}`;

  const nginx = `location = /_/s.js {
    proxy_set_header X-CL-User "";                 # strip any client copy
    proxy_pass ${origin}/v1/web/t.js;
}
location = /_/e {
    proxy_set_header X-Forwarded-For $remote_addr; # rightmost hop is trusted
    proxy_set_header X-CL-User "";
    proxy_pass ${origin}/v1/web/collect;
}`;

  const appRoute = `# No proxy access? Add two routes in YOUR app that forward to central-logs
# and set X-CL-User from the server-side session (never trust the client):
app.get("/_/s.js", async (_req, reply) => {
  const upstream = await fetch("${origin}/v1/web/t.js");
  reply.type("application/javascript").send(await upstream.text());
});
app.post("/_/e", async (req, reply) => {
  await fetch("${origin}/v1/web/collect", {
    method: "POST",
    headers: {
      "Content-Type": "text/plain",
      "X-Forwarded-For": req.ip,          // only trusted if YOUR app is a trusted proxy
      "X-CL-User": req.session?.userId ?? "", // stripped client copies first!
    },
    body: req.body,
  });
  reply.code(202).send();
});`;

  const verify = `# 1) send a synthetic beacon through the proxy (or directly):
curl -s -o /dev/null -w '%{http_code}\\n' \\
  -H 'Origin: https://your-site.example' -H 'Content-Type: text/plain' \\
  --data '{"s":"${activeSite || "your-project-id"}","ev":[{"e":"pageview","u":"/test","dt":0}]}' \\
  ${origin}/v1/web/collect
# → 202 (first beacon for a new id also creates the project)

# 2) the event lands in web_events within a few seconds — watch it on the
#    Web page, or query the API (any read-scoped key):
curl -s -H "Authorization: Bearer PASTE_READ_KEY" \\
  "${origin}/api/web/stats?site=${activeSite || "your-project-id"}&metric=pageviews"`;

  return (
    <div>
      {sitesResp && !sitesResp.enabled && (
        <div className="p-3 rounded border border-amber-500/50 bg-amber-500/10 text-sm mb-4">
          The collector is <strong>off</strong> on this server (<code>web.enabled = false</code>).
          Nothing below will record until an operator enables it — see{" "}
          <strong>One-time server setup</strong> at the bottom.
        </div>
      )}

      <div className="space-y-5">
        <Step n={1} title="Name your project">
          <div className="flex flex-wrap items-center gap-2 mb-2">
            <input
              type="text"
              value={projectId}
              onChange={(e) => setProjectId(e.target.value)}
              placeholder={sites[0]?.id ?? "e.g. ajakan"}
              list="cl-web-projects"
              className="cl-input py-1 font-mono text-xs w-56"
              spellCheck={false}
            />
            <datalist id="cl-web-projects">
              {sites.map((s) => (
                <option key={s.id} value={s.id} />
              ))}
            </datalist>
            {activeSite && (
              <span className="text-xs">
                {alreadyExists ? (
                  <span className="opacity-60">existing project — beacons accepted from its origins</span>
                ) : idOk ? (
                  <span className="text-emerald-700 dark:text-emerald-300">
                    new id — the first beacon creates it automatically ✨ (no server change)
                  </span>
                ) : (
                  <span className="text-amber-600">
                    use 1–64 chars: letters, digits, <code>._-</code>, no spaces
                  </span>
                )}
              </span>
            )}
          </div>
          <p className="text-xs text-tremor-content-subtle mt-2">
            Each project is an isolated analytics scope (own dashboard, own
            allowed origins). The id travels in every beacon{" "}
            <code>(&#123;"s": "…"`)</code>; first beacon wins the id, so
            pre-create on the{" "}
            <Link to="/web" className="underline">
              Web
            </Link>{" "}
            page (admin) to reserve one.
          </p>
        </Step>

        <Step n={2} title="Embed one line in every page">
          <CodeBlock value={embed} />
          <p className="text-xs text-tremor-content-subtle mt-2">
            That's it — the ~2 KB script tracks pageviews (SPA-aware), custom
            events via <code>window.cl.track(name, props)</code>, outbound/download
            clicks, engagement, Web Vitals and JS errors. It sets no cookies.
          </p>
        </Step>

        <Step n={3} title="Same-origin proxy (recommended: neutral /_/ paths)">
          <Chips
            options={[
              { id: "caddy" as const, label: "Caddy" },
              { id: "nginx" as const, label: "nginx" },
              { id: "app" as const, label: "Route in your app" },
            ]}
            value={proxy}
            onChange={(p) => { setProxy(p); trackEvent("ui.connect.proxy", { proxy: p }); }}
          />
          <CodeBlock value={proxy === "caddy" ? caddy : proxy === "nginx" ? nginx : appRoute} />
          <p className="text-xs text-tremor-content-subtle mt-2">
            Beacons are first-party because they are sent to YOUR domain —
            ad-blockers rarely touch them and no CORS preflight is needed. Only
            <code> /v1/web/collect</code> and <code>/v1/web/t.js</code> are public;
            everything else under <code>/api/web/*</code> stays authenticated.
          </p>
        </Step>

        <Step n={4} title="Verify">
          <CodeBlock value={verify} />
          <p className="text-xs text-tremor-content-subtle mt-2">
            Then open the{" "}
            <Link to="/web" className="underline">
              Web
            </Link>{" "}
            page (trends, breakdowns, vitals, errors) and{" "}
            <Link to="/web/realtime" className="underline">
              Web Realtime
            </Link>
            .
          </p>
        </Step>

        <details className="p-3 rounded cl-card-plain border border-tremor-border">
          <summary className="text-sm font-medium cursor-pointer">
            One-time server setup (operator only)
          </summary>
          <div className="mt-2">
            <CodeBlock value={configToml} />
            <p className="text-xs text-tremor-content-subtle mt-2">
              The collector itself must be enabled once (restart). Per-site
              config is optional: with <code>auto_create_projects</code> the
              first beacon creates the project (its origin becomes the first
              allowed origin). Pin origins in <code>[[web.site]]</code> only
              for infra-managed projects.
            </p>
          </div>
        </details>
      </div>

      <div className="mt-4 p-3 rounded cl-card-plain border border-tremor-border text-xs text-tremor-content-subtle">
        <strong className="text-tremor-content">Privacy model:</strong> cookieless;
        visitors are a SHA-256 hash of (daily-rotating salt, site, IP, user-agent) —
        raw IPs are <em>never</em> stored, and hashes change every UTC day (per-day
        uniques are exact; multi-day ranges sum daily uniques). Correlate browser
        sessions with backend logs by having your app render{" "}
        <code>&lt;meta name="traceparent" content="00-…-…-01"&gt;</code> — rows then
        join to <code>logs.trace_id</code> (see the{" "}
        <Link to="/web" className="underline">
          Web
        </Link>{" "}
        page and <code>GET /api/web/trace/&#123;id&#125;</code>).
      </div>
    </div>
  );
}

function useWebSitesPoll() {
  const [data, setData] = useState<WebSitesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    api
      .webSites()
      .then((r) => !cancelled && setData(r))
      .catch((e) => !cancelled && setError(String(e)));
    return () => {
      cancelled = true;
    };
  }, []);
  // Manual refresh hook (the Projects panel on the Web page owns creation;
  // the datalist here refreshes on revisit).
  const refresh = () => {
    api
      .webSites()
      .then(setData)
      .catch((e) => setError(String(e)));
  };
  return { data, error, refresh };
}

function useWebSites() {
  const { data, error } = useWebSitesPoll();
  return { data, error };
}

// =====================================================================
// Page: Logs | Web analytics tabs
// =====================================================================

export default function ConnectPage() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "web" ? "web" : "logs";

  return (
    <div className="max-w-4xl">
      <h2 className="text-xl font-semibold mb-1">Connect</h2>
      <p className="text-sm text-tremor-content-subtle mb-4">
        Ship logs from any platform, or add cookieless web analytics to your
        sites — both land in the same pipeline. Full guides live in{" "}
        <code>INTEGRATION.md</code>; this page renders the same snippets with
        your host filled in.
      </p>

      <Chips
        options={[
          { id: "logs" as const, label: "Logging (all platforms)" },
          { id: "web" as const, label: "Web analytics" },
        ]}
        value={tab}
        onChange={(t) => {
          // Note: tab switches also change the URL (?tab=web), so a pageview
          // fires too — the custom event marks the explicit guide choice.
          trackEvent("ui.connect.tab", { tab: t });
          setParams(t === "web" ? { tab: "web" } : {});
        }}
      />

      {tab === "logs" ? <LogsGuide /> : <WebGuide />}
    </div>
  );
}
