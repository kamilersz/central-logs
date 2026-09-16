// Typed client for the central-logs HTTP API. All calls return parsed JSON or
// throw on non-2xx responses.
//
// Auth model (OWASP A01/A07): the browser authenticates via a `cl_session`
// cookie set by POST /api/auth/login. The cookie is HttpOnly + SameSite=Strict
// and travels automatically on every same-origin fetch — so most of this file
// doesn't need to touch it. On a 401 from any call we hard-redirect to the
// server-rendered /login page, since that means the session expired or was
// revoked.

export interface HotAttr {
  name: string;
  duckdb_type: string;
  json_path: string;
}

export interface SchemaResponse {
  hot: HotAttr[];
  filterable_columns: string[];
}

export interface LogRow {
  ts: string;
  insert_ts: string;
  source_host: string | null;
  service: string | null;
  level: string | null;
  message: string | null;
  trace_id: string | null;
  span_id: string | null;
  attributes: Record<string, unknown> | null;
  geo_country: string | null;
  raw_len: number | null;
  protocol: string | null;
  /** Promoted hot-attribute columns (user_id, user_name, ...). */
  hot: Record<string, string | number | boolean> | null;
}

export interface LogsResponse {
  rows: LogRow[];
  total_matched: number;
  truncated: boolean;
  filter_error: string | null;
}

export interface LogsParams {
  filter?: string;
  window?: string; // "5m" | "1h" | "24h" | "7d"
  from?: string;
  to?: string;
  limit?: number;
  offset?: number;
}

export interface CountersSnapshot {
  records_total: number;
  bytes_total: number;
  errors_total: number;
  per_protocol: Record<string, { records: number; bytes: number; errors: number }>;
}

export interface AiQueryResponse {
  filter: string;
  provider: string;
  raw: string;
}

export interface DashboardConfig {
  id: number;
  name: string;
  description: string | null;
  panels: unknown;
  created_at: string;
  updated_at: string;
}

export interface AlertRule {
  id: number;
  name: string;
  metric: string;
  condition: unknown;
  channel: string | null;
  /** Web-managed notification channel ids (see AlertChannel). */
  channels: number[];
  status: string; // "pending_approval" | "active" | "rejected"
  created_at: string;
  activated_at: string | null;
  last_evaluated_at?: string | null;
  last_fired_at?: string | null;
  last_notify_error?: string | null;
}

/** One escalation level. Array order = escalation order. Notifications fire
 * only when the rule's severity label CHANGES between evaluations. */
export interface ThresholdEntry {
  severity: string; // "warning", "critical", …
  comparator: string; // ">=", ">", "<=", "<", "=="
  value: number;
}

/** Count-per-period condition ("n matching events per period"). */
export interface CountCondition {
  type: "count";
  filter: string;
  comparator?: string; // default ">=" (legacy single-threshold)
  count?: number; // legacy single-threshold
  window_secs?: number;
  cooldown_secs?: number;
  thresholds?: ThresholdEntry[];
}

/** Rollup-metric threshold condition. */
export interface ThresholdCondition {
  type: "threshold";
  comparator?: string;
  value?: number;
  window_secs?: number;
  cooldown_secs?: number;
  thresholds?: ThresholdEntry[];
}

export interface AnomalyCondition {
  type: "anomaly";
  min_severity?: string;
  window_secs?: number;
  cooldown_secs?: number;
}

export type AlertCondition = CountCondition | ThresholdCondition | AnomalyCondition;

export type AlertChannelType = "email" | "telegram" | "webhook";

export interface AlertChannel {
  id: number;
  name: string;
  type: AlertChannelType;
  /** email: {recipients}, telegram: {bot_token_masked, chat_id}, webhook: {url} */
  config: Record<string, unknown>;
  created_at: string;
  updated_at: string | null;
}

export interface AlertRuleBody {
  name: string;
  condition: AlertCondition;
  metric?: string;
  channels: number[];
  channel?: string;
}

export interface WhoamiResponse {
  key_id: number | null;
  name: string;
  scopes: string[];
  via: string;
}

export interface ApiKeyOut {
  id: number;
  name: string;
  key_prefix: string;
  scopes: string[];
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
}

export interface CreateApiKeyResponse {
  /** Raw token — shown ONLY at creation. Never retrievable again. */
  key: string;
  id: number;
  name: string;
  key_prefix: string;
  scopes: string[];
}

/** Params every preset dashboard endpoint accepts: a relative window OR an
 * explicit from/to range, plus an optional filter-DSL string. */
export interface DashboardParams {
  window?: string;
  bucket?: string;
  from?: string; // ISO-8601
  to?: string; // ISO-8601
  filter?: string;
  horizon?: number; // forecast only
}

export interface VolumeRow {
  ts: string;
  n: number;
}

export interface AiDashboardQuestion {
  id: number;
  question: string;
  choices?: string[];
}

/** Panel shape shared by the builder, the viewer, and the AI proposal. */
export interface Panel {
  title: string;
  type: "volume" | "error-rate" | "latency" | "top-services" | "log-count" | "anomalies";
  window: string; // "5m".."7d" or "custom" (with from/to set)
  filter: string;
  viz?: "chart" | "number";
  /** Width on a 12-column grid (1..12). Default 6. */
  w?: number;
  from?: string; // datetime-local string, used when window === "custom"
  to?: string;
}

export interface AiDashboardProposal {
  name: string;
  description: string;
  panels: Panel[];
}

export interface AiDashboardResponse {
  stage: "clarify" | "build";
  questions: AiDashboardQuestion[];
  dashboard?: AiDashboardProposal;
  provider: string;
  raw: string;
}

// On any 401, bounce to the server-rendered login page. We avoid an infinite
// redirect loop by letting /login itself handle the form post.
function redirectToLoginIfUnauthorized(status: number): void {
  if (status === 401 && !window.location.pathname.startsWith("/login")) {
    window.location.href = "/login";
  }
}

async function getJson<T>(url: string): Promise<T> {
  const resp = await fetch(url, { credentials: "same-origin" });
  redirectToLoginIfUnauthorized(resp.status);
  if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}: ${await resp.text()}`);
  return (await resp.json()) as T;
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const resp = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  redirectToLoginIfUnauthorized(resp.status);
  if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}: ${await resp.text()}`);
  return (await resp.json()) as T;
}

async function delJson<T>(url: string): Promise<T> {
  const resp = await fetch(url, {
    method: "DELETE",
    credentials: "same-origin",
  });
  redirectToLoginIfUnauthorized(resp.status);
  if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}: ${await resp.text()}`);
  return (await resp.json()) as T;
}

async function putJson<T>(url: string, body: unknown): Promise<T> {
  const resp = await fetch(url, {
    method: "PUT",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  redirectToLoginIfUnauthorized(resp.status);
  if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}: ${await resp.text()}`);
  return (await resp.json()) as T;
}

function buildQuery(params: Record<string, string | number | undefined>): string {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") sp.set(k, String(v));
  }
  const s = sp.toString();
  return s ? `?${s}` : "";
}

export interface ErrorRateRow {
  ts: string;
  errors: number;
  total: number;
  rate: number;
}
export interface LatencyRow {
  ts: string;
  service: string;
  p50: number;
  p95: number;
  p99: number;
}
export interface AnomalyRow {
  ts: string;
  metric: string;
  score: number;
  method: string;
  severity: string;
}
export interface ForecastPoint {
  ts: string;
  predicted: number;
  lower: number;
  upper: number;
}
export interface ForecastResponse {
  history?: string[];
  forecast?: ForecastPoint[];
  model?: string;
  error?: string;
}

export interface PipelineStatus {
  channel_depth: number;
  channel_capacity: number;
  channel_fill_ratio: number;
  wal_bytes_written_total: number;
  ingest_lag_bytes: number;
  audit_dropped_total: number;
  records_total: number;
  errors_total: number;
}

// ─── Error tracking (Sentry-SDK-compatible; see docs/ERROR_TRACKING.md) ───

export interface ErrorGroup {
  fingerprint: string;
  service: string | null;
  level: string | null;
  title: string | null;
  exception_type: string | null;
  first_seen: string | null;
  last_seen: string | null;
  total_count: number;
  status: string; // "unresolved" | "resolved" | "ignored"
  resolved_at: string | null;
}

/** A sampled stacktrace kept on the group (first-seen + latest variants). */
export interface ErrorGroupSample {
  variant_hash: string;
  first_seen: string;
  stack: string | null;
}

export interface ErrorSparkPoint {
  ts: string;
  count: number;
}

export interface ErrorGroupDetail extends ErrorGroup {
  samples: ErrorGroupSample[];
  /** Per-minute event counts over the trailing 24h. */
  spark: ErrorSparkPoint[];
}

export interface ErrorGroupsResponse {
  groups: ErrorGroup[];
  total: number;
}

export interface ErrorGroupParams {
  window?: string; // "1h" | "24h" | "7d" | "30d"
  status?: "unresolved" | "resolved" | "ignored" | "all";
  service?: string;
  q?: string;
  sort?: "recent" | "count";
  limit?: number;
  offset?: number;
}

export interface ErrorExplainResponse {
  provider: string;
  explanation?: string | null;
  hint?: string;
  error?: string;
}

// ─── Housekeeping: storage overview + backups ───

export interface BackupRun {
  id: number;
  started_at: string | null;
  finished_at: string | null;
  trigger: string | null;
  status: string | null;
  local_path: string | null;
  remote_key: string | null;
  bytes: number | null;
  checksum: string | null;
  error: string | null;
}

export interface StorageOverview {
  hot_rows: number;
  cold_parquet_bytes: number;
  cold_parquet_files: number;
  oldest_hot_ts: string | null;
  newest_hot_ts: string | null;
  wal_bytes: number;
  error_groups: number;
  alerts_active: number;
  retention: {
    hot_max_age_secs: number;
    hot_max_bytes: number | null;
    wal_max_bytes: number | null;
    cold_retention_days: number;
    service_rules: number;
  };
  backups: BackupRun[];
}

export interface ColdDateRow {
  date: string | null;
  files: number | null;
  rows: number | null;
  bytes: number | null;
}

export interface BackupsResponse {
  runs: BackupRun[];
  local_files: { file: string; bytes: number; modified: string | null }[];
}

export interface RestoreResponse {
  restored_into: string;
  checksum: string;
  manifest: Record<string, unknown>;
  next: string;
}

export const api = {
  // --- auth ---
  whoami: () => getJson<WhoamiResponse>("/api/auth/whoami"),
  logout: () => postJson<{ status: string }>("/api/auth/logout", {}),

  // --- api keys (admin only) ---
  listApiKeys: () => getJson<ApiKeyOut[]>("/v1/api-keys"),
  createApiKey: (body: { name: string; scopes?: string }) =>
    postJson<CreateApiKeyResponse>("/v1/api-keys", body),
  revokeApiKey: (id: number) => delJson<{ id: number; status: string }>(`/v1/api-keys/${id}`),

  // --- schema / logs / dashboards ---
  schema: () => getJson<SchemaResponse>("/api/schema/hot"),
  counters: () => getJson<CountersSnapshot>("/api/counters"),
  pipeline: () => getJson<PipelineStatus>("/api/pipeline"),
  logs: (params: LogsParams) =>
    getJson<LogsResponse>(`/api/logs${buildQuery(params as Record<string, string | number | undefined>)}`),
  volume: (params: DashboardParams) =>
    getJson<VolumeRow[]>(`/api/dashboard/volume${buildQuery(params as Record<string, string | number | undefined>)}`),
  errorRate: (params: DashboardParams) =>
    getJson<ErrorRateRow[]>(`/api/dashboard/error-rate${buildQuery(params as Record<string, string | number | undefined>)}`),
  latency: (params: DashboardParams) =>
    getJson<LatencyRow[]>(`/api/dashboard/latency${buildQuery(params as Record<string, string | number | undefined>)}`),
  anomalies: (params: DashboardParams) =>
    getJson<AnomalyRow[]>(`/api/dashboard/anomalies${buildQuery(params as Record<string, string | number | undefined>)}`),
  forecast: (params: DashboardParams) =>
    getJson<ForecastResponse>(`/api/dashboard/forecast${buildQuery(params as Record<string, string | number | undefined>)}`),
  listDashboards: () => getJson<DashboardConfig[]>("/api/dashboard/configs"),
  getDashboard: (id: number) => getJson<DashboardConfig>(`/api/dashboard/configs/${id}`),
  saveDashboard: (body: { name: string; description?: string; panels: unknown }) =>
    postJson<{ id: number; status: string }>("/api/dashboard/configs", body),
  updateDashboard: (id: number, body: { name?: string; description?: string; panels?: unknown }) =>
    putJson<{ id: number; status: string }>(`/api/dashboard/configs/${id}`, body),
  listAlertRules: () => getJson<AlertRule[]>("/api/alert-rules"),
  createAlertRule: (body: AlertRuleBody) =>
    postJson<{ id: number; status: string }>("/api/alert-rules", body),
  updateAlertRule: (id: number, body: AlertRuleBody) =>
    putJson<{ id: number; updated_rows: number }>(`/api/alert-rules/${id}`, body),
  deleteAlertRule: (id: number) =>
    delJson<{ id: number; deleted_rows: number }>(`/api/alert-rules/${id}`),
  approveAlert: (id: number) => postJson<unknown>(`/api/alert-rules/${id}/approve`, {}),
  rejectAlert: (id: number) => postJson<unknown>(`/api/alert-rules/${id}/reject`, {}),
  listAlertChannels: () => getJson<AlertChannel[]>("/api/alert-channels"),
  createAlertChannel: (body: { name: string; type: AlertChannelType; config: Record<string, unknown> }) =>
    postJson<{ id: number; name: string }>("/api/alert-channels", body),
  updateAlertChannel: (
    id: number,
    body: { name: string; type: AlertChannelType; config: Record<string, unknown> },
  ) => putJson<{ id: number; updated_rows: number }>(`/api/alert-channels/${id}`, body),
  deleteAlertChannel: (id: number) =>
    delJson<{ id: number; deleted_rows: number }>(`/api/alert-channels/${id}`),
  testAlertChannel: (id: number) =>
    postJson<{ ok: boolean; error: string | null }>(`/api/alert-channels/${id}/test`, {}),
  aiQuery: (query: string, timeHint?: string) =>
    postJson<AiQueryResponse>("/api/ai/query", { query, time_hint: timeHint }),
  aiDashboard: (body: {
    description: string;
    answers?: { question: string; answer: string }[];
    /** Revision mode: the previous proposal to modify. */
    current?: AiDashboardProposal;
    /** Revision mode: what to change. */
    revision?: string;
  }) => postJson<AiDashboardResponse>("/api/ai/dashboard", body),

  // --- error tracking ---
  listErrorGroups: (params: ErrorGroupParams) =>
    getJson<ErrorGroupsResponse>(
      `/api/error-groups${buildQuery(params as Record<string, string | number | undefined>)}`,
    ),
  getErrorGroup: (fingerprint: string) =>
    getJson<ErrorGroupDetail>(`/api/error-groups/${encodeURIComponent(fingerprint)}`),
  resolveErrorGroup: (fingerprint: string) =>
    postJson<{ fingerprint: string; status: string }>(
      `/api/error-groups/${encodeURIComponent(fingerprint)}/resolve`,
      {},
    ),
  unresolveErrorGroup: (fingerprint: string) =>
    postJson<{ fingerprint: string; status: string }>(
      `/api/error-groups/${encodeURIComponent(fingerprint)}/unresolve`,
      {},
    ),
  ignoreErrorGroup: (fingerprint: string) =>
    postJson<{ fingerprint: string; status: string }>(
      `/api/error-groups/${encodeURIComponent(fingerprint)}/ignore`,
      {},
    ),
  deleteErrorGroup: (fingerprint: string) =>
    delJson<{ fingerprint: string; deleted: boolean }>(
      `/api/error-groups/${encodeURIComponent(fingerprint)}`,
    ),
  explainErrorGroup: (fingerprint: string, context?: string) =>
    postJson<ErrorExplainResponse>(
      `/api/error-groups/${encodeURIComponent(fingerprint)}/explain`,
      { context: context || undefined },
    ),

  // --- housekeeping (storage + backups) ---
  storageOverview: () => getJson<StorageOverview>("/api/storage/overview"),
  storageCold: (from?: string, to?: string) =>
    getJson<{ dates: ColdDateRow[] }>(
      `/api/storage/cold${buildQuery({ from, to } as Record<string, string | number | undefined>)}`,
    ),
  listBackups: () => getJson<BackupsResponse>("/api/ops/backups"),
  runBackup: () => postJson<{ started: boolean }>("/api/ops/backup", {}),
  restore: (from: string, into: string) =>
    postJson<RestoreResponse>("/api/ops/restore", { from, into }),
};
