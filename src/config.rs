//! Configuration: CLI flags + TOML/env via figment.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

use crate::ai::LlmConfig;
pub use crate::errors::ErrorTrackingConfig;
use crate::hot::HotAttribute;
use crate::Protocol;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory for WAL segments + redb metadata file.
    pub data_dir: PathBuf,
    /// DuckDB file path (inside data_dir by default).
    pub duckdb_path: PathBuf,
    /// Parquet cold-tier directory (inside data_dir by default).
    pub parquet_dir: PathBuf,

    /// HTTP API bind address (insert endpoint + dashboard + query API).
    pub http_bind: String,
    /// HTTP port (overrides :port in http_bind if set).
    pub http_port: u16,

    /// Enable syslog UDP listener.
    pub syslog_udp_enabled: bool,
    pub syslog_udp_bind: String,

    /// Enable syslog TCP listener.
    pub syslog_tcp_enabled: bool,
    pub syslog_tcp_bind: String,

    /// MCP server mode: off, stdio, or sse (HTTP).
    pub mcp_mode: McpMode,
    /// Bind address for MCP SSE/HTTP server (when mcp_mode = sse).
    pub mcp_http_bind: String,
    /// Optional bearer token required for SSE MCP access. Empty = no auth.
    pub mcp_api_key: String,

    /// Optional bearer token required for ALL HTTP API + insert endpoints
    /// (OWASP A01/A07). Empty (default) = auth disabled for backward compat.
    /// When set, clients must send `Authorization: Bearer <key>` or
    /// `X-API-Key: <key>`. The SPA static assets stay public so the login
    /// page can load; only `/api/*`, `/v1/*`, and `/metrics` are gated.
    pub http_api_key: String,
    /// Hard cap on `/v1/logs` request body size in bytes (OWASP A05 —
    /// prevents unbounded-body DoS). Default 16 MiB.
    pub http_max_body_bytes: usize,
    /// Expose the create_alert_rule MCP tool (gated off by default per §7).
    pub enable_alert_mcp_tool: bool,
    /// How often the alert evaluator re-checks active rules against current
    /// rollup/anomaly data and fires webhook notifications for breaches.
    pub alert_eval_interval_secs: u64,

    /// WAL segment rotation threshold.
    pub wal_segment_max_bytes: u64,
    /// Group-commit batch size.
    pub wal_batch_max_records: usize,
    /// Group-commit time window.
    pub wal_batch_max_delay_ms: u64,

    /// Bounded channel depth between insert endpoints and WAL writer.
    pub insert_channel_depth: usize,
    /// Backpressure timeout for HTTP/TCP inserts when channel is full.
    pub insert_backpressure_timeout_ms: u64,

    /// Number of ingest worker tasks.
    pub ingest_workers: usize,
    /// Ingest batch size (rows per DuckDB Appender flush).
    pub ingest_batch_size: usize,
    /// Ingest flush interval.
    pub ingest_flush_interval_ms: u64,

    /// Rollup cadence.
    pub rollup_interval_secs: u64,
    /// Compaction cadence.
    pub compaction_interval_secs: u64,
    /// Hot tier age cutoff.
    pub hot_tier_hours: u64,
    /// Raw log retention TTL.
    pub retention_days: u64,
    /// Per-service retention overrides (glob pattern → days). First match
    /// wins; `retention_days` is the fallback.
    #[serde(default)]
    pub service_retention: Vec<ServiceRetention>,
    /// Parquet compression codec for the cold tier. Log data is highly
    /// repetitive, so zstd typically shrinks files ~2-3x vs the snappy
    /// default (LESSON_LEARNED: compression was half the cost saving).
    pub parquet_compression: String,

    /// DuckDB engine memory cap, e.g. "512MB", "2GB" (plain bytes also
    /// accepted). Empty = DuckDB default (~80% of system RAM). Bounds the
    /// memory a single heavy query / compaction can claim.
    #[serde(default)]
    pub duckdb_memory_limit: String,
    /// DuckDB worker threads. 0 = DuckDB default (one thread per core).
    /// Lower this when the process shares a box with the ingest path.
    #[serde(default)]
    pub duckdb_threads: usize,

    /// JSON attribute keys stripped at parse time, before persistence.
    /// "Remove fields you don't need early — smaller messages travel
    /// faster" and storage only pays for what you keep.
    #[serde(default)]
    pub drop_attributes: Vec<String>,

    /// When true (default), a JSON object embedded as a string inside
    /// `msg`/`message` is lifted: its fields land in `attributes` (and hot
    /// columns) instead of staying an opaque string. This mirrors the
    /// "message_json" lesson: `{"message":"{\"order_id\":\"ORD-1\",...}"}`
    /// becomes queryable as `order_id:ORD-1`.
    #[serde(default = "default_true")]
    pub unwrap_message_json: bool,

    /// Per-service query-window caps (`service:<glob>` → max window).
    #[serde(default)]
    pub service_query_limits: Vec<ServiceQueryLimit>,

    /// Optional object-storage cold-tier archive (off by default).
    #[serde(default)]
    pub cold_storage: ColdStorageConfig,

    /// Retention bounds: hot-tier age/size caps + the WAL ingest cap
    /// (housekeeping, docs/OPERATIONS.md). All optional — absent = today's
    /// behavior (age via hot_tier_hours, no size caps).
    #[serde(default)]
    pub retention: RetentionConfig,

    /// Backup snapshots (tar.gz of the data dir → local dir + optional
    /// S3/GCS bucket), on a timezone-aware daily schedule or a size trigger.
    #[serde(default)]
    pub backup: BackupConfig,

    /// Forecast cadence (every N rollup cycles).
    pub forecast_every_n_rollups: u32,

    /// Promoted attribute columns (architecture: telemetry optimization).
    /// Each entry becomes a typed top-level `logs` column; the matching JSON
    /// key is popped out of the `attributes` blob at parse time so the column
    /// can be filtered via zonemap-pruned scans instead of JSON parses.
    #[serde(default)]
    pub hot_attributes: Vec<HotAttribute>,

    /// LLM provider for natural-language → filter DSL translation.
    /// Default `Off`; set to Openai or Anthropic with an API key to enable.
    #[serde(default)]
    pub llm: LlmConfig,

    /// SMTP relay for alert email channels. Leave `host` empty to disable
    /// email delivery (channels of type `email` will record a delivery
    /// error instead of sending).
    #[serde(default)]
    pub smtp: SmtpConfig,

    /// Optional MaxMind GeoLite2 Country MMDB path (enables geo enrichment).
    pub geoip_db_path: Option<PathBuf>,

    /// Error tracking (Sentry-SDK-compatible ingest, grouping, notifications).
    /// See `docs/ERROR_TRACKING.md` and `src/errors.rs`.
    #[serde(default)]
    pub error_tracking: ErrorTrackingConfig,

    /// Container/orchestrator push protocols (Docker Engine log drivers +
    /// Kubernetes audit webhook): `[ingest.gelf]`, `[ingest.fluentd]`,
    /// `[ingest.splunk_hec]`, `[ingest.k8s_audit]`. All disabled by default.
    #[serde(default)]
    pub ingest: IngestConfig,

    /// Built-in pull collectors (`[collector.docker]`, `[collector.kubernetes]`):
    /// follow container/pod log streams from the Docker Engine API and the
    /// Kubernetes API and feed them into the normal WAL pipeline. Disabled
    /// by default.
    #[serde(default)]
    pub collector: CollectorConfig,

    /// Tracing filter (RUST_LOG-style).
    pub log_filter: String,
}

/// Per-service retention override (LESSON_LEARNED: "not all logs are equal" —
/// audit streams need 90 days, debug chatty services need 3). First matching
/// rule wins; `retention_days` applies to services with no rule.
///
/// `pattern` is a glob (`*`, `?`) matched against the sanitized service
/// partition directory name on the cold tier (e.g. `payment_*`, `audit_*`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceRetention {
    pub pattern: String,
    pub days: u64,
}

/// Per-service query-window cap (LESSON_LEARNED: per-stream `max_query_range`).
/// Prevents one user's unbounded `service:chatty-svc` scan from squeezing the
/// node. `pattern` is a glob matched against the RAW service value in
/// `service:<value>` equality clauses; only queries that carry such a clause
/// are capped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceQueryLimit {
    pub pattern: String,
    pub max_window_secs: u64,
}

/// SMTP relay settings for alert email channels. TOML:
///
/// ```toml
/// [smtp]
/// host = "smtp.example.com"
/// port = 587
/// username = "alerts@example.com"
/// password = "..."
/// from = "central-logs@example.com"
/// starttls = true          # false = implicit TLS (port 465)
/// ```
///
/// Env equivalents (figment `__` nesting):
/// `CENTRAL_LOGS_SMTP__HOST`, `CENTRAL_LOGS_SMTP__PORT`, …
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SmtpConfig {
    /// Relay hostname. Empty = email delivery disabled.
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// From address (and HELO identity). Required when host is set.
    pub from: String,
    /// true = STARTTLS (typically port 587); false = implicit TLS (465).
    pub starttls: bool,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 587,
            username: String::new(),
            password: String::new(),
            from: String::new(),
            starttls: true,
        }
    }
}

impl SmtpConfig {
    /// Email delivery is usable only when a relay and a from address exist.
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.from.trim().is_empty()
    }
}

/// Optional object-storage cold-tier archive (LESSON_LEARNED: object storage
/// is ~8x cheaper per GB than node disks). Compacted Parquet files are
/// uploaded after compaction and kept in sync with local retention — the
/// remote copy mirrors the local lifecycle. Requires the `object-storage`
/// build feature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColdStorageConfig {
    pub enabled: bool,
    /// `s3` (AWS / MinIO / any S3-compatible endpoint) or `gcs`.
    pub backend: String,
    pub bucket: String,
    /// Key prefix inside the bucket, e.g. `central-logs/parquet`.
    pub prefix: String,
    /// Custom endpoint (MinIO, emulators). `http://` endpoints are allowed.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Keep local Parquet files as the queryable cache (recommended). When
    /// false, local files are deleted after upload — remote data is an
    /// archive only (not queryable through `logs_all` in this version).
    pub keep_local: bool,
}

impl Default for ColdStorageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "s3".to_string(),
            bucket: String::new(),
            prefix: "central-logs/parquet".to_string(),
            endpoint: None,
            region: None,
            keep_local: true,
        }
    }
}

/// Retention bounds (`[retention]`). Durations accept `45s`, `12h`, `30d`,
/// `2mo` (60d), `1y`; sizes accept `10GB`, `500MiB`, or bare byte counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RetentionConfig {
    /// Hot-tier age bound — rows older than this are compacted to Parquet.
    /// Overrides `hot_tier_hours` when set. Default: 2 months ("2mo").
    pub hot_max_age: Option<String>,
    /// Approximate payload-byte cap on the hot table (SUM(raw_len)); when
    /// exceeded, oldest rows are compacted out until under the cap.
    pub hot_max_bytes: Option<String>,
    /// Stop accepting inserts (503) when the WAL directory exceeds this.
    /// None = no cap.
    pub wal_max_bytes: Option<String>,
}

impl RetentionConfig {
    /// Effective hot-tier cutoff duration. Explicit `hot_max_age` wins;
    /// otherwise fall back to the legacy `hot_tier_hours`.
    pub fn hot_max_age_duration(&self, hot_tier_hours: u64) -> chrono::Duration {
        match &self.hot_max_age {
            Some(s) => parse_duration(s).unwrap_or(chrono::Duration::hours(24)),
            None => chrono::Duration::hours(hot_tier_hours.max(1) as i64),
        }
    }

    pub fn hot_max_bytes_value(&self) -> Option<u64> {
        self.hot_max_bytes
            .as_deref()
            .and_then(|s| parse_size(s).ok())
    }

    pub fn wal_max_bytes_value(&self) -> Option<u64> {
        self.wal_max_bytes
            .as_deref()
            .and_then(|s| parse_size(s).ok())
    }
}

/// Backup snapshot configuration (`[backup]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackupConfig {
    /// Master switch. When false, manual `POST /api/ops/backup` still works
    /// (local copy only) but nothing runs on a schedule.
    pub enabled: bool,
    /// `"daily@HH:MM"` (uses `timezone`) or `"size:<N>"` (e.g. `"size:512MiB"`,
    /// snapshot after N MiB of new cold Parquet since the last one).
    pub schedule: String,
    /// IANA timezone for the daily schedule, e.g. "Asia/Jakarta", "UTC".
    pub timezone: String,
    /// Optional remote target: `s3` or `gcs`. Empty = local copies only.
    pub backend: String,
    pub bucket: String,
    /// Key prefix inside the bucket. Per-instance subkeys are appended:
    /// `<prefix>/<instance_id>/<date>/backup.tar.gz`.
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Also keep a tar.gz copy on local disk (recommended).
    pub keep_local_copy: bool,
    /// Directory for local copies. Default: `<data_dir>/backups`.
    pub local_backup_dir: Option<PathBuf>,
    /// Prune local copies beyond the newest N.
    pub keep_last: usize,
    /// Stable identity used in keys/paths. Default: "<hostname>-<http_port>".
    pub instance_id: Option<String>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: "daily@01:30".into(),
            timezone: "UTC".into(),
            backend: String::new(),
            bucket: String::new(),
            prefix: "central-logs/backups".into(),
            endpoint: None,
            region: None,
            keep_local_copy: true,
            local_backup_dir: None,
            keep_last: 14,
            instance_id: None,
        }
    }
}

impl BackupConfig {
    /// Parsed schedule. `Daily { at: NaiveTime }` or `Size(bytes)`.
    pub fn parsed_schedule(&self) -> Result<Schedule, crate::Error> {
        parse_schedule(&self.schedule)
    }

    /// Resolved timezone (defaults to UTC on an unknown name — validated at
    /// startup, so this only fires on hand-edited runtime state).
    pub fn tz(&self) -> chrono_tz::Tz {
        self.timezone.parse().unwrap_or(chrono_tz::UTC)
    }

    /// Stable instance identity for keys/paths.
    pub fn resolved_instance_id(&self, http_port: u16) -> String {
        if let Some(id) = &self.instance_id {
            return id.clone();
        }
        let host = hostname().unwrap_or_else(|| "localhost".into());
        format!("{host}-{http_port}")
    }

    /// Build the remote client (None = local-only backups).
    #[cfg(feature = "object-storage")]
    pub fn remote_store(
        &self,
    ) -> std::result::Result<Option<crate::store::cold::ColdStore>, crate::Error> {
        if self.backend.trim().is_empty() {
            return Ok(None);
        }
        let cfg = ColdStorageConfig {
            enabled: true,
            backend: self.backend.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            keep_local: true,
        };
        Ok(Some(crate::store::cold::ColdStore::from_config(&cfg)?))
    }
}

/// Parsed `[backup].schedule`.
#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// Daily at a fixed local time (interpreted in `backup.timezone`).
    Daily { at: chrono::NaiveTime },
    /// Snapshot once this many bytes of new cold Parquet accumulated.
    Size(u64),
}

// =====================================================================
// Engine/cluster push protocols + pull collectors (docs/ingestion/containers.md)
// =====================================================================

/// Push-protocol listeners: `[ingest.gelf]`, `[ingest.fluentd]`,
/// `[ingest.splunk_hec]`, `[ingest.k8s_audit]`. Each is off by default so
/// existing deployments keep their exact surface area.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IngestConfig {
    /// GELF 1.1 ingest — Docker Engine's `gelf` log driver
    /// (`--log-driver=gelf --log-opt gelf-address=udp://host:12201`) and any
    /// GELF shipper. UDP supports gzip/zlib payloads + GELF chunked
    /// reassembly; TCP accepts null-terminated JSON.
    pub gelf: GelfIngestConfig,
    /// Fluentd forward protocol (MessagePack over TCP) — Docker Engine's
    /// `fluentd` log driver and fluentd/fluent-bit `forward` outputs.
    pub fluentd: FluentdIngestConfig,
    /// Splunk HEC-compatible HTTP endpoint — Docker Engine's `splunk` log
    /// driver (`--log-driver=splunk --log-opt splunk-url=https://…`) posts
    /// to `/services/collector/event/1.0`.
    pub splunk_hec: SplunkHecIngestConfig,
    /// Kubernetes audit webhook — `kube-apiserver`
    /// `--audit-webhook-config-file` POSTs `audit.k8s.io/v1` EventList
    /// batches to `/ingest/kubernetes/audit`.
    pub k8s_audit: K8sAuditIngestConfig,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            gelf: GelfIngestConfig::default(),
            fluentd: FluentdIngestConfig::default(),
            splunk_hec: SplunkHecIngestConfig::default(),
            k8s_audit: K8sAuditIngestConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GelfIngestConfig {
    pub enabled: bool,
    /// UDP bind for GELF datagrams (Docker default driver port 12201).
    /// Empty = UDP disabled.
    pub udp_bind: String,
    /// TCP bind for null-terminated GELF JSON. Empty = TCP disabled.
    pub tcp_bind: String,
    /// GELF chunk reassembly timeout (chunks older than this are dropped).
    pub chunk_timeout_secs: u64,
}

impl Default for GelfIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            udp_bind: "0.0.0.0:12201".to_string(),
            tcp_bind: "0.0.0.0:12201".to_string(),
            chunk_timeout_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FluentdIngestConfig {
    pub enabled: bool,
    /// TCP bind for the MessagePack forward protocol (Docker default 24224).
    pub tcp_bind: String,
    /// Reply `{"ack": <id>}` when the client requests acknowledgement
    /// (`fluentd-request-ack=true`). Keep on for lossless delivery.
    pub ack: bool,
}

impl Default for FluentdIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tcp_bind: "0.0.0.0:24224".to_string(),
            ack: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SplunkHecIngestConfig {
    pub enabled: bool,
    /// Static HEC token accepted in `Authorization: Splunk <token>` in
    /// ADDITION to normal API keys. Empty = API keys only. The Docker
    /// driver requires *some* token (`--log-opt splunk-token=…`), so any
    /// active insert-scoped API key works.
    pub token: String,
}

impl Default for SplunkHecIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct K8sAuditIngestConfig {
    pub enabled: bool,
    /// Audit stages to DROP (kube-apiserver emits one event per configured
    /// stage per request). Common dedup: `["RequestReceived"]` keeps only
    /// the completed/started response events. Empty = keep all stages.
    pub omit_stages: Vec<String>,
}

impl Default for K8sAuditIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            omit_stages: Vec::new(),
        }
    }
}

/// Pull collectors: `[collector.docker]`, `[collector.kubernetes]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CollectorConfig {
    /// Follow container logs from the Docker Engine API (unix socket by
    /// default) — the pull-based alternative for hosts where changing each
    /// container's `--log-driver` is impractical.
    pub docker: DockerCollectorConfig,
    /// Follow pod logs from the Kubernetes API (service-account bearer
    /// token + CA by default) — in-cluster DaemonSet-style collection
    /// without a third-party agent.
    pub kubernetes: K8sCollectorConfig,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            docker: DockerCollectorConfig::default(),
            kubernetes: K8sCollectorConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DockerCollectorConfig {
    pub enabled: bool,
    /// Docker Engine API endpoint. `unix:///var/run/docker.sock` (default)
    /// or `tcp://host:port`.
    pub socket: String,
    /// Engine API version path (e.g. `v1.41`). Empty = unprefixed endpoints.
    pub api_version: String,
    /// Container-name globs to include (e.g. `web-*`). Empty = all.
    pub include: Vec<String>,
    /// Container-name globs to exclude. Wins over `include`.
    pub exclude: Vec<String>,
    /// How often to re-list containers and attach to new ones.
    pub refresh_secs: u64,
    /// Lines of per-container history ingested when the collector first
    /// attaches (`tail` query param). 0 = only new lines.
    pub tail_lines: u64,
}

impl Default for DockerCollectorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: "unix:///var/run/docker.sock".to_string(),
            api_version: "v1.41".to_string(),
            include: Vec::new(),
            exclude: Vec::new(),
            refresh_secs: 30,
            tail_lines: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct K8sCollectorConfig {
    pub enabled: bool,
    /// Kubernetes API server base URL, e.g.
    /// `https://kubernetes.default.svc` (in-cluster) or an explicit URL.
    pub api_url: String,
    /// Literal bearer token. Preferred in-cluster mode is `token_file`.
    pub token: String,
    /// Service-account token file (auto-rotated tokens are re-read each
    /// discovery refresh). Default path works inside a pod.
    pub token_file: String,
    /// Cluster CA bundle for TLS verification.
    pub ca_file: String,
    /// Skip TLS verification (dev clusters only).
    pub insecure_tls: bool,
    /// Namespaces to watch. Empty = all namespaces.
    pub namespaces: Vec<String>,
    /// Optional label selector forwarded to the pods list call
    /// (e.g. `app=web`).
    pub label_selector: String,
    /// How often to re-list pods and attach to new ones.
    pub refresh_secs: u64,
    /// Lines of history ingested when first attaching (`tailLines`).
    pub tail_lines: u64,
}

impl Default for K8sCollectorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_url: "https://kubernetes.default.svc".to_string(),
            token: String::new(),
            token_file: "/var/run/secrets/kubernetes.io/serviceaccount/token".to_string(),
            ca_file: "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt".to_string(),
            insecure_tls: false,
            namespaces: Vec::new(),
            label_selector: String::new(),
            refresh_secs: 30,
            tail_lines: 0,
        }
    }
}

/// Parse a duration like `45s`, `12h`, `30d`, `2mo` (60d), `1y` (365d).
pub fn parse_duration(s: &str) -> Result<chrono::Duration, crate::Error> {
    let t = s.trim().to_ascii_lowercase();
    let (num, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).ok_or_else(|| {
        crate::Error::config(format!("duration '{s}' needs a unit (s/m/h/d/mo/y)"))
    })?);
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| crate::Error::config(format!("bad duration number in '{s}'")))?;
    let secs = match unit {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600.0,
        "d" | "day" | "days" => n * 86400.0,
        "mo" | "month" | "months" => n * 86400.0 * 30.0,
        "y" | "year" | "years" => n * 86400.0 * 365.0,
        other => {
            return Err(crate::Error::config(format!(
                "unknown duration unit '{other}' in '{s}' (use s/m/h/d/mo/y)"
            )))
        }
    };
    if secs <= 0.0 {
        return Err(crate::Error::config(format!(
            "duration '{s}' must be positive"
        )));
    }
    Ok(chrono::Duration::seconds(secs as i64))
}

/// Parse a byte size like `10GB`, `500MiB`, `1TB`, or a bare byte count.
pub fn parse_size(s: &str) -> Result<u64, crate::Error> {
    let t = s.trim().to_ascii_lowercase().replace(' ', "");
    let digits_end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let n: f64 = t[..digits_end]
        .parse()
        .map_err(|_| crate::Error::config(format!("bad size number in '{s}'")))?;
    let mult = match &t[digits_end..] {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(crate::Error::config(format!(
                "unknown size unit '{other}' in '{s}' (use B/KB/MB/GB/TB or KiB/MiB/GiB/TiB)"
            )))
        }
    };
    if n <= 0.0 {
        return Err(crate::Error::config(format!("size '{s}' must be positive")));
    }
    Ok((n * mult) as u64)
}

/// Parse `"daily@HH:MM"` or `"size:<N>"`.
pub fn parse_schedule(s: &str) -> Result<Schedule, crate::Error> {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("daily@") {
        let at = chrono::NaiveTime::parse_from_str(rest.trim(), "%H:%M")
            .map_err(|e| crate::Error::config(format!("bad daily time in '{s}': {e}")))?;
        Ok(Schedule::Daily { at })
    } else if let Some(rest) = t.strip_prefix("size:") {
        Ok(Schedule::Size(parse_size(rest)?))
    } else {
        Err(crate::Error::config(
            "backup.schedule must be 'daily@HH:MM' or 'size:<N>' (e.g. 'size:512MiB')",
        ))
    }
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    Off,
    Stdio,
    /// Streamable-HTTP/SSE transport for remote agents. Not implemented in
    /// this version — `Config::validate` rejects it with a clear error
    /// rather than silently starting no server. Kept as a variant (instead
    /// of removing it) so existing configs fail loudly instead of falling
    /// through to a different mode by accident.
    Sse,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = PathBuf::from("./data");
        Self {
            duckdb_path: data_dir.join("central.duckdb"),
            parquet_dir: data_dir.join("parquet"),
            data_dir,
            http_bind: "0.0.0.0".to_string(),
            http_port: 8080,
            syslog_udp_enabled: true,
            syslog_udp_bind: "0.0.0.0:5140".to_string(),
            syslog_tcp_enabled: true,
            syslog_tcp_bind: "0.0.0.0:5140".to_string(),
            mcp_mode: McpMode::Off,
            mcp_http_bind: "0.0.0.0:8081".to_string(),
            mcp_api_key: String::new(),
            http_api_key: String::new(),
            http_max_body_bytes: 16 * 1024 * 1024,
            enable_alert_mcp_tool: false,
            alert_eval_interval_secs: 30,
            wal_segment_max_bytes: 256 * 1024 * 1024,
            wal_batch_max_records: 4096,
            wal_batch_max_delay_ms: 10,
            insert_channel_depth: 65536,
            insert_backpressure_timeout_ms: 250,
            ingest_workers: 2,
            ingest_batch_size: 2048,
            ingest_flush_interval_ms: 1000,
            rollup_interval_secs: 60,
            compaction_interval_secs: 3600,
            hot_tier_hours: 48,
            retention_days: 30,
            service_retention: Vec::new(),
            parquet_compression: "zstd".to_string(),
            duckdb_memory_limit: String::new(),
            duckdb_threads: 0,
            drop_attributes: Vec::new(),
            unwrap_message_json: true,
            service_query_limits: Vec::new(),
            cold_storage: ColdStorageConfig::default(),
            retention: RetentionConfig {
                hot_max_age: Some("2mo".into()),
                ..Default::default()
            },
            backup: BackupConfig::default(),
            forecast_every_n_rollups: 5,
            hot_attributes: Vec::new(),
            llm: LlmConfig::default(),
            smtp: SmtpConfig::default(),
            geoip_db_path: None,
            error_tracking: ErrorTrackingConfig::default(),
            ingest: IngestConfig::default(),
            collector: CollectorConfig::default(),
            log_filter: "info,central_logs=debug".to_string(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> crate::Result<()> {
        // MCP SSE/HTTP transport isn't implemented (see McpMode::Sse doc) —
        // reject loudly at startup instead of silently running with no MCP
        // server, which is what selecting it used to do.
        if self.mcp_mode == McpMode::Sse {
            return Err(crate::Error::config(
                "mcp_mode = 'sse' is not implemented in this version; use 'stdio' for local \
                 agent integration (Claude Desktop/Code) or 'off'. Remote MCP access over \
                 HTTP is tracked as future work (architecture §7).",
            ));
        }
        if self.wal_batch_max_records == 0 {
            return Err(crate::Error::config("wal_batch_max_records must be > 0"));
        }
        if self.ingest_workers == 0 {
            return Err(crate::Error::config("ingest_workers must be > 0"));
        }
        if self.hot_tier_hours < 1 {
            return Err(crate::Error::config("hot_tier_hours must be >= 1"));
        }
        // Cold-tier Parquet codec whitelist. Values are matched case-
        // insensitively; the uppercase form is interpolated into the COPY
        // statement (values are whitelist-bound, never user-raw).
        const PARQUET_CODECS: &[&str] = &["zstd", "snappy", "gzip", "lz4", "uncompressed"];
        if !PARQUET_CODECS.contains(&self.parquet_compression.to_ascii_lowercase().as_str()) {
            return Err(crate::Error::config(format!(
                "parquet_compression must be one of {PARQUET_CODECS:?} (got '{}')",
                self.parquet_compression
            )));
        }
        // DuckDB engine tuning. memory_limit is interpolated into
        // `SET memory_limit='…'`, so it must match DuckDB's byte-spec format
        // (optional decimal + B/KB/MB/GB/TB, or plain bytes) — this also
        // keeps quotes/semicolons out of the statement. Empty = default.
        let ml = self.duckdb_memory_limit.trim();
        if !ml.is_empty() {
            let num_len = ml.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len();
            let (num, unit) = ml.split_at(num_len);
            let unit = unit.to_ascii_uppercase();
            let ok = !num.is_empty()
                && num.parse::<f64>().map(|v| v > 0.0).unwrap_or(false)
                && matches!(unit.as_str(), "" | "B" | "KB" | "MB" | "GB" | "TB");
            if !ok {
                return Err(crate::Error::config(format!(
                    "duckdb_memory_limit must look like '512MB', '2GB' or plain bytes (got '{}')",
                    self.duckdb_memory_limit
                )));
            }
        }
        if self.duckdb_threads > 4096 {
            return Err(crate::Error::config(
                "duckdb_threads must be between 0 (auto) and 4096",
            ));
        }
        // Per-service retention rules: glob patterns are matched against the
        // sanitized cold-tier partition dir name, so '/' can't appear.
        for rule in &self.service_retention {
            if rule.pattern.trim().is_empty() {
                return Err(crate::Error::config(
                    "service_retention patterns must be non-empty",
                ));
            }
            if rule.pattern.contains('/') {
                return Err(crate::Error::config(format!(
                    "service_retention pattern '{}' must not contain '/'",
                    rule.pattern
                )));
            }
            if rule.days == 0 {
                return Err(crate::Error::config(format!(
                    "service_retention pattern '{}' must have days >= 1",
                    rule.pattern
                )));
            }
        }
        // Per-service query-window caps.
        for limit in &self.service_query_limits {
            if limit.pattern.trim().is_empty() || limit.pattern.contains('/') {
                return Err(crate::Error::config(format!(
                    "service_query_limits pattern '{}' must be non-empty and contain no '/'",
                    limit.pattern
                )));
            }
            if limit.max_window_secs == 0 {
                return Err(crate::Error::config(format!(
                    "service_query_limits pattern '{}' must have max_window_secs >= 1",
                    limit.pattern
                )));
            }
        }
        // Cold-storage archive: only meaningful when compiled in.
        if self.cold_storage.enabled {
            #[cfg(not(feature = "object-storage"))]
            {
                return Err(crate::Error::config(
                    "cold_storage.enabled = true, but this binary was built without the \
                     'object-storage' feature (cargo build --features object-storage)",
                ));
            }
            #[cfg(feature = "object-storage")]
            {
                if !matches!(self.cold_storage.backend.as_str(), "s3" | "gcs") {
                    return Err(crate::Error::config(format!(
                        "cold_storage.backend must be 's3' or 'gcs' (got '{}')",
                        self.cold_storage.backend
                    )));
                }
                if self.cold_storage.bucket.trim().is_empty() {
                    return Err(crate::Error::config(
                        "cold_storage.bucket must be set when cold_storage is enabled",
                    ));
                }
            }
        }
        // Dropped attribute keys: strip-early hygiene. Keys are matched
        // verbatim against top-level JSON attribute names.
        for key in &self.drop_attributes {
            if key.trim().is_empty() {
                return Err(crate::Error::config(
                    "drop_attributes entries must be non-empty",
                ));
            }
            if key.len() > 255 {
                return Err(crate::Error::config(format!(
                    "drop attribute key too long (>255 chars): '{key}'"
                )));
            }
        }
        // Reject hot attributes that would shadow built-in columns: promoting
        // `trace_id` (already a top-level column) would produce a duplicate
        // INSERT and DuckDB would refuse every row. The same applies to all
        // other built-in fields listed in RESERVED_COLUMN_NAMES.
        const RESERVED_COLUMN_NAMES: &[&str] = &[
            "ts",
            "insert_ts",
            "source_host",
            "service",
            "level",
            "message",
            "fingerprint",
            "trace_id",
            "span_id",
            "attributes",
            "geo_country",
            "raw_len",
            "protocol",
        ];
        let mut seen = std::collections::HashSet::new();
        for attr in &self.hot_attributes {
            if RESERVED_COLUMN_NAMES.contains(&attr.name.as_str()) {
                return Err(crate::Error::config(format!(
                    "hot attribute '{}' shadows a built-in column; pick a different name",
                    attr.name
                )));
            }
            if !seen.insert(attr.name.as_str()) {
                return Err(crate::Error::config(format!(
                    "hot attribute '{}' is defined twice",
                    attr.name
                )));
            }
        }
        // A key can't be both promoted (hot attribute) and dropped — the
        // two configs contradict each other.
        for attr in &self.hot_attributes {
            let leaf = attr.json_path.strip_prefix("$.").unwrap_or(&attr.json_path);
            if self.drop_attributes.iter().any(|d| d == leaf) {
                return Err(crate::Error::config(format!(
                    "attribute '{leaf}' is configured both as a hot attribute and a drop attribute"
                )));
            }
        }
        // OWASP A01/A05: surface a loud warning when the HTTP API is being
        // served on a non-loopback interface with no bearer-token auth. Not
        // fatal — single-node loopback dev is the documented default mode —
        // but anyone binding to 0.0.0.0 should set http_api_key.
        if self.http_api_key.trim().is_empty() && !is_loopback_bind(&self.http_bind) {
            tracing::warn!(
                bind = %self.http_bind,
                "HTTP API is bound to a non-loopback interface with http_api_key unset; \
                 any client on the network can read logs, approve alerts, and insert. \
                 Set --http-api-key (OWASP A01/A07)."
            );
        }
        if self.http_max_body_bytes == 0 {
            return Err(crate::Error::config(
                "http_max_body_bytes must be > 0 (use a large value if you really need no limit)",
            ));
        }
        // Retention / backup sanity.
        if let Some(age) = &self.retention.hot_max_age {
            parse_duration(age)
                .map_err(|e| crate::Error::config(format!("retention.hot_max_age: {e}")))?;
        }
        if let Some(sz) = &self.retention.hot_max_bytes {
            parse_size(sz)
                .map_err(|e| crate::Error::config(format!("retention.hot_max_bytes: {e}")))?;
        }
        if let Some(sz) = &self.retention.wal_max_bytes {
            parse_size(sz)
                .map_err(|e| crate::Error::config(format!("retention.wal_max_bytes: {e}")))?;
        }
        if self.backup.enabled {
            self.backup
                .parsed_schedule()
                .map_err(|e| crate::Error::config(format!("backup.{}", e)))?;
            if matches!(self.backup.parsed_schedule(), Ok(Schedule::Daily { .. })) {
                let tz: Result<chrono_tz::Tz, _> = self.backup.timezone.parse();
                if tz.is_err() {
                    return Err(crate::Error::config(format!(
                        "backup.timezone '{}' is not a valid IANA timezone (e.g. 'UTC', 'Asia/Jakarta')",
                        self.backup.timezone
                    )));
                }
            }
            if !self.backup.backend.trim().is_empty() {
                #[cfg(not(feature = "object-storage"))]
                {
                    return Err(crate::Error::config(
                        "backup.backend is set, but this binary was built without the \
                         object-storage feature (rebuild with: cargo build --release --features object-storage)",
                    ));
                }
                if !matches!(self.backup.backend.as_str(), "s3" | "gcs") {
                    return Err(crate::Error::config(format!(
                        "backup.backend must be 's3' or 'gcs' (got '{}')",
                        self.backup.backend
                    )));
                }
                if self.backup.bucket.trim().is_empty() {
                    return Err(crate::Error::config(
                        "backup.bucket must be set when backup.backend is configured",
                    ));
                }
            }
            if self.backup.keep_last == 0 {
                return Err(crate::Error::config("backup.keep_last must be >= 1"));
            }
        }

        // Error tracking sanity.
        if self.error_tracking.enabled && self.error_tracking.samples_per_group == 0 {
            return Err(crate::Error::config(
                "error_tracking.samples_per_group must be >= 1",
            ));
        }
        // Container/orchestrator push + pull integration sanity.
        if self.ingest.gelf.enabled
            && self.ingest.gelf.udp_bind.trim().is_empty()
            && self.ingest.gelf.tcp_bind.trim().is_empty()
        {
            return Err(crate::Error::config(
                "ingest.gelf.enabled = true but both udp_bind and tcp_bind are empty; \
                 set at least one (e.g. '0.0.0.0:12201')",
            ));
        }
        if self.ingest.fluentd.enabled && self.ingest.fluentd.tcp_bind.trim().is_empty() {
            return Err(crate::Error::config(
                "ingest.fluentd.enabled = true but tcp_bind is empty",
            ));
        }
        if self.ingest.k8s_audit.enabled {
            const STAGES: &[&str] = &[
                "RequestReceived",
                "ResponseStarted",
                "ResponseComplete",
                "Panic",
            ];
            for stage in &self.ingest.k8s_audit.omit_stages {
                if !STAGES.contains(&stage.as_str()) {
                    return Err(crate::Error::config(format!(
                        "ingest.k8s_audit.omit_stages entry '{stage}' is not a valid audit \
                         stage (use one of {STAGES:?})"
                    )));
                }
            }
        }
        if self.collector.docker.enabled {
            let s = self.collector.docker.socket.trim();
            if !(s.starts_with("unix://") || s.starts_with("tcp://") || s.starts_with("http://")) {
                return Err(crate::Error::config(format!(
                    "collector.docker.socket must be unix://… or tcp://… (got '{}')",
                    self.collector.docker.socket
                )));
            }
            if self.collector.docker.refresh_secs == 0 {
                return Err(crate::Error::config(
                    "collector.docker.refresh_secs must be >= 1",
                ));
            }
        }
        if self.collector.kubernetes.enabled {
            let u = self.collector.kubernetes.api_url.trim();
            if !u.starts_with("https://") && !u.starts_with("http://") {
                return Err(crate::Error::config(format!(
                    "collector.kubernetes.api_url must be an http(s) URL (got '{}')",
                    self.collector.kubernetes.api_url
                )));
            }
            if self.collector.kubernetes.refresh_secs == 0 {
                return Err(crate::Error::config(
                    "collector.kubernetes.refresh_secs must be >= 1",
                ));
            }
        }
        for url in &self.error_tracking.notify.webhooks {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(crate::Error::config(format!(
                    "error_tracking.notify.webhooks entries must be http(s) URLs (got '{url}')"
                )));
            }
        }
        Ok(())
    }

    pub fn wal_batch_delay(&self) -> Duration {
        Duration::from_millis(self.wal_batch_max_delay_ms)
    }
    pub fn insert_backpressure_timeout(&self) -> Duration {
        Duration::from_millis(self.insert_backpressure_timeout_ms)
    }
    pub fn ingest_flush_interval(&self) -> Duration {
        Duration::from_millis(self.ingest_flush_interval_ms)
    }
    pub fn rollup_interval(&self) -> Duration {
        Duration::from_secs(self.rollup_interval_secs)
    }
    pub fn compaction_interval(&self) -> Duration {
        Duration::from_secs(self.compaction_interval_secs)
    }
    pub fn alert_eval_interval(&self) -> Duration {
        Duration::from_secs(self.alert_eval_interval_secs)
    }
    pub fn http_listen(&self) -> String {
        format!("{}:{}", self.http_bind, self.http_port)
    }
    pub fn protocol_of(&self, p: Protocol) -> &'static str {
        p.as_str()
    }
}

/// True if `bind` is a loopback / wildcard-on-loopback listen address.
/// Conservative: only treats explicit `127.0.0.1`, `::1`, and `localhost`
/// as loopback; `0.0.0.0` and empty are treated as public.
pub fn is_loopback_bind(bind: &str) -> bool {
    let host = bind.split(':').next().unwrap_or("").trim();
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "[::1]")
}

fn default_true() -> bool {
    true
}

/// Parse a duration into seconds: bare integer, or `N`s / `N`m / `N`h / `N`d
/// (case-insensitive).
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let (num, mult): (&str, u64) = if let Some(n) = lower.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = lower.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = lower.strip_suffix('d') {
        (n, 86400)
    } else {
        (lower.as_str(), 1)
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n.saturating_mul(mult))
        .map_err(|_| format!("bad duration '{s}' (expected seconds or Ns/Nm/Nh/Nd)"))
}

/// CLI flags (architecture: clap-based). Most knobs come from config; CLI flags
/// exist only for the things you typically want to override at launch.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "central-logs",
    version,
    about = "Self-hosted centralized logging platform"
)]
pub struct Cli {
    /// Path to TOML config file.
    #[arg(short, long, env = "CENTRAL_LOGS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Override data directory.
    #[arg(long, env = "CENTRAL_LOGS_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Override HTTP port.
    #[arg(long, env = "CENTRAL_LOGS_HTTP_PORT")]
    pub http_port: Option<u16>,

    /// Override MCP mode. `sse` parses but is rejected at config validation
    /// (not implemented — see `McpMode::Sse`); use `stdio` or `off`.
    #[arg(long, env = "CENTRAL_LOGS_MCP_MODE", value_parser = ["off", "stdio", "sse"])]
    pub mcp_mode: Option<String>,

    /// Enable create_alert_rule MCP tool (also requires mcp_mode != off).
    #[arg(long, env = "CENTRAL_LOGS_ENABLE_ALERT_MCP_TOOL")]
    pub enable_alert_mcp_tool: bool,

    /// Disable syslog UDP listener even if enabled in config.
    #[arg(long)]
    pub no_syslog_udp: bool,

    /// Disable syslog TCP listener even if enabled in config.
    #[arg(long)]
    pub no_syslog_tcp: bool,

    /// Set the HTTP API bearer token (OWASP A01/A07). Empty = auth disabled.
    /// Also settable via CENTRAL_LOGS_HTTP_API_KEY.
    #[arg(long, env = "CENTRAL_LOGS_HTTP_API_KEY")]
    pub http_api_key: Option<String>,

    /// Disable the Sentry-SDK-compatible ingest endpoints
    /// (`/api/{project}/envelope/`, `/api/{project}/store/`) and error-group
    /// tracking entirely (on by default; see docs/ERROR_TRACKING.md).
    #[arg(long = "no-sentry-ingest")]
    pub no_sentry_ingest: bool,

    /// Run one backup snapshot right after startup (uses `[backup]` config;
    /// works even when backup.enabled = false — local copy only). The server
    /// keeps serving afterwards.
    #[arg(long = "backup-now")]
    pub backup_now: bool,

    /// Restore a snapshot into --data-dir BEFORE opening the database
    /// (server must be stopped): a local .tar.gz path or an s3://bucket/key
    /// reference (needs [backup] backend config + object-storage build).
    /// The data dir must be empty, or --restore-force-wipe to clear it.
    #[arg(long = "restore-from")]
    pub restore_from: Option<String>,

    /// With --restore-from: wipe a non-empty data dir first.
    #[arg(long = "restore-force-wipe")]
    pub restore_force_wipe: bool,

    /// Print effective config and exit.
    #[arg(long)]
    pub print_config: bool,

    /// Promote a JSON key to a typed top-level column for fast filtering.
    /// Repeatable. Shorthand: `name:type[:json_path]`, e.g.
    ///   --hot-attribute 'user_id:bigint'
    ///   --hot-attribute 'route:varchar:$.request.route'
    /// Supported types: bigint, double, varchar, boolean. Default type varchar.
    #[arg(long = "hot-attribute", value_name = "SPEC")]
    pub hot_attributes: Vec<String>,

    /// Strip a JSON attribute key at parse time, before persistence
    /// (repeatable). Example: --drop-attribute 'debug_payload'
    #[arg(long = "drop-attribute", value_name = "KEY")]
    pub drop_attributes: Vec<String>,

    /// Disable lifting of JSON objects embedded as strings inside
    /// msg/message (on by default).
    #[arg(long = "no-unwrap-message-json")]
    pub no_unwrap_message_json: bool,

    /// Per-service retention override, `glob=days` (repeatable). Example:
    /// --service-retention 'audit_*=90' --service-retention 'debug_*=3'
    /// First matching rule wins; --retention-days is the fallback.
    #[arg(long = "service-retention", value_name = "GLOB=DAYS")]
    pub service_retention: Vec<String>,

    /// Cap the query window for a service, `glob=duration` (repeatable).
    /// Duration: seconds, or Ns/Nm/Nh/Nd. Example:
    /// --service-query-limit 'payment_*=24h'
    #[arg(long = "service-query-limit", value_name = "GLOB=DURATION")]
    pub service_query_limits: Vec<String>,

    /// DuckDB engine memory cap, e.g. '512MB', '2GB' (empty = auto, ~80% RAM).
    #[arg(long, env = "CENTRAL_LOGS_DUCKDB_MEMORY_LIMIT", value_name = "LIMIT")]
    pub duckdb_memory_limit: Option<String>,

    /// DuckDB worker threads (0 = auto, one per core).
    #[arg(long, env = "CENTRAL_LOGS_DUCKDB_THREADS")]
    pub duckdb_threads: Option<usize>,
}

/// Load config from: defaults ← TOML file (if provided) ← env (CENTRAL_LOGS_*) ← CLI overrides.
pub fn load(cli: &Cli) -> crate::Result<Config> {
    let mut fig = Figment::from(Serialized::defaults(Config::default()));

    if let Some(path) = &cli.config {
        fig = fig.merge(Toml::file(path));
    }

    fig = fig.merge(Env::prefixed("CENTRAL_LOGS_").split("__"));

    let mut cfg: Config = fig
        .extract()
        .map_err(|e| crate::Error::config(e.to_string()))?;

    // Flat LLM env contract (.env / systemd EnvironmentFile friendly) —
    // applied only when [llm] was not already configured via TOML:
    //   CENTRAL_LOGS_LLM_PROVIDER = anthropic (default) | openai | 9inference
    //   CENTRAL_LOGS_LLM_API_KEY  = sk-...
    //   CENTRAL_LOGS_LLM_MODEL    = MiniMax-M3 / gpt-4o-mini / ...
    //   CENTRAL_LOGS_LLM_BASE_URL = optional gateway override
    //     (Anthropic-protocol: MiniMax https://api.minimax.io/anthropic, …)
    if matches!(cfg.llm, LlmConfig::Off) {
        if let Ok(api_key) = std::env::var("CENTRAL_LOGS_LLM_API_KEY") {
            let api_key = api_key.trim().to_string();
            if !api_key.is_empty() {
                let provider = std::env::var("CENTRAL_LOGS_LLM_PROVIDER")
                    .unwrap_or_else(|_| "anthropic".into())
                    .trim()
                    .to_ascii_lowercase();
                let model = std::env::var("CENTRAL_LOGS_LLM_MODEL")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let base_url = std::env::var("CENTRAL_LOGS_LLM_BASE_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());
                cfg.llm = match provider.as_str() {
                    "anthropic" => LlmConfig::Anthropic {
                        api_key,
                        model: if model.is_empty() {
                            "claude-3-5-sonnet-20241022".into()
                        } else {
                            model
                        },
                        base_url,
                    },
                    "openai" => LlmConfig::Openai {
                        api_key,
                        model: if model.is_empty() {
                            "gpt-4o-mini".into()
                        } else {
                            model
                        },
                        base_url,
                    },
                    "9inference" => LlmConfig::NineInference {
                        api_key,
                        model: if model.is_empty() {
                            "nemotron-3-ultra".into()
                        } else {
                            model
                        },
                    },
                    other => {
                        return Err(crate::Error::config(format!(
                            "CENTRAL_LOGS_LLM_PROVIDER must be 'anthropic', 'openai', or '9inference' (got '{other}')"
                        )))
                    }
                };
            }
        }
    }

    if let Some(d) = &cli.data_dir {
        cfg.data_dir = d.clone();
        cfg.duckdb_path = d.join("central.duckdb");
        cfg.parquet_dir = d.join("parquet");
    }
    if let Some(p) = cli.http_port {
        cfg.http_port = p;
    }
    if let Some(m) = &cli.mcp_mode {
        cfg.mcp_mode = match m.as_str() {
            "off" => McpMode::Off,
            "stdio" => McpMode::Stdio,
            "sse" => McpMode::Sse,
            _ => return Err(crate::Error::config(format!("unknown mcp_mode: {m}"))),
        };
    }
    if cli.enable_alert_mcp_tool {
        cfg.enable_alert_mcp_tool = true;
    }
    for spec in &cli.hot_attributes {
        match HotAttribute::parse_shorthand(spec) {
            Ok(h) => cfg.hot_attributes.push(h),
            Err(e) => return Err(crate::Error::config(format!("--hot-attribute: {e}"))),
        }
    }
    cfg.drop_attributes
        .extend(cli.drop_attributes.iter().cloned());
    if cli.no_unwrap_message_json {
        cfg.unwrap_message_json = false;
    }
    for spec in &cli.service_retention {
        let (pattern, days) = spec.rsplit_once('=').ok_or_else(|| {
            crate::Error::config(format!(
                "--service-retention expects GLOB=DAYS, got '{spec}'"
            ))
        })?;
        let days: u64 = days.parse().map_err(|_| {
            crate::Error::config(format!(
                "--service-retention days must be a positive integer, got '{days}'"
            ))
        })?;
        cfg.service_retention.push(ServiceRetention {
            pattern: pattern.to_string(),
            days,
        });
    }
    for spec in &cli.service_query_limits {
        let (pattern, dur) = spec.rsplit_once('=').ok_or_else(|| {
            crate::Error::config(format!(
                "--service-query-limit expects GLOB=DURATION, got '{spec}'"
            ))
        })?;
        let secs = parse_duration_secs(dur)
            .map_err(|e| crate::Error::config(format!("--service-query-limit: {e}")))?;
        cfg.service_query_limits.push(ServiceQueryLimit {
            pattern: pattern.to_string(),
            max_window_secs: secs,
        });
    }
    if cli.no_syslog_udp {
        cfg.syslog_udp_enabled = false;
    }
    if cli.no_syslog_tcp {
        cfg.syslog_tcp_enabled = false;
    }
    if cli.no_sentry_ingest {
        cfg.error_tracking.enabled = false;
    }
    if let Some(k) = cli.http_api_key.clone() {
        cfg.http_api_key = k;
    }
    if let Some(v) = &cli.duckdb_memory_limit {
        cfg.duckdb_memory_limit = v.clone();
    }
    if let Some(v) = cli.duckdb_threads {
        cfg.duckdb_threads = v;
    }

    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot::HotAttribute;

    fn base_valid_cfg() -> Config {
        Config::default()
    }

    #[test]
    fn validate_rejects_hot_attr_shadowing_builtin() {
        let mut cfg = base_valid_cfg();
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("trace_id:varchar").unwrap());
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, crate::Error::Config(_)),
            "expected Config error, got {err:?}"
        );
        assert!(
            err.to_string().contains("shadows a built-in"),
            "expected shadowing hint, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_duplicate_hot_attrs() {
        let mut cfg = base_valid_cfg();
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("user_id:bigint").unwrap());
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("user_id:bigint").unwrap());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("defined twice"));
    }

    #[test]
    fn validate_accepts_non_reserved_hot_attrs() {
        let mut cfg = base_valid_cfg();
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("user_id:bigint").unwrap());
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("env:varchar").unwrap());
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_unknown_parquet_codec() {
        let mut cfg = base_valid_cfg();
        cfg.parquet_compression = "brotli99".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("parquet_compression"));
    }

    #[test]
    fn validate_accepts_codec_case_insensitively() {
        let mut cfg = base_valid_cfg();
        cfg.parquet_compression = "ZSTD".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_drop_and_hot_overlap() {
        let mut cfg = base_valid_cfg();
        cfg.hot_attributes
            .push(HotAttribute::parse_shorthand("user_id:bigint").unwrap());
        cfg.drop_attributes.push("user_id".into());
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("both as a hot attribute and a drop attribute"));
    }

    #[test]
    fn validate_rejects_empty_drop_key() {
        let mut cfg = base_valid_cfg();
        cfg.drop_attributes.push("   ".into());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("drop_attributes"));
    }

    #[test]
    fn validate_accepts_service_retention_rules() {
        let mut cfg = base_valid_cfg();
        cfg.service_retention = vec![
            ServiceRetention {
                pattern: "audit_*".into(),
                days: 90,
            },
            ServiceRetention {
                pattern: "debug_?".into(),
                days: 3,
            },
        ];
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_service_retention() {
        let mut cfg = base_valid_cfg();
        cfg.service_retention = vec![ServiceRetention {
            pattern: "a/b".into(),
            days: 7,
        }];
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must not contain '/'"));

        let mut cfg = base_valid_cfg();
        cfg.service_retention = vec![ServiceRetention {
            pattern: "audit".into(),
            days: 0,
        }];
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("days >= 1"));
    }

    #[test]
    fn duration_parser_accepts_units_and_bare_seconds() {
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("10m").unwrap(), 600);
        assert_eq!(parse_duration_secs("24H").unwrap(), 86400);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604800);
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn duckdb_tuning_validation() {
        // Defaults (empty limit, 0 threads) are always valid.
        let mut cfg = base_valid_cfg();
        cfg.validate().unwrap();

        for good in ["512MB", "2GB", "1073741824", "1.5gb"] {
            let mut cfg = base_valid_cfg();
            cfg.duckdb_memory_limit = good.to_string();
            cfg.duckdb_threads = 4;
            cfg.validate()
                .unwrap_or_else(|e| panic!("{good} should be valid: {e}"));
        }

        for bad in ["four gigabytes", "1QB", "-2GB", "'; DROP TABLE logs", "GB"] {
            let mut cfg = base_valid_cfg();
            cfg.duckdb_memory_limit = bad.to_string();
            assert!(cfg.validate().is_err(), "{bad} should be rejected");
        }

        let mut cfg = base_valid_cfg();
        cfg.duckdb_threads = 5000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_query_limits() {
        let mut cfg = base_valid_cfg();
        cfg.service_query_limits.push(ServiceQueryLimit {
            pattern: "a/b".into(),
            max_window_secs: 60,
        });
        assert!(cfg.validate().unwrap_err().to_string().contains("no '/'"));

        let mut cfg = base_valid_cfg();
        cfg.service_query_limits.push(ServiceQueryLimit {
            pattern: "svc".into(),
            max_window_secs: 0,
        });
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_window_secs >= 1"));
    }

    #[test]
    fn validate_rejects_sse_mcp_mode() {
        let mut cfg = base_valid_cfg();
        cfg.mcp_mode = McpMode::Sse;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("not implemented"), "got: {err}");
    }

    #[test]
    fn validate_rejects_cold_storage_without_feature() {
        let mut cfg = base_valid_cfg();
        cfg.cold_storage.enabled = true;
        cfg.cold_storage.bucket = "b".into();
        let res = cfg.validate();
        #[cfg(not(feature = "object-storage"))]
        {
            let err = res.unwrap_err().to_string();
            assert!(err.contains("object-storage"), "got: {err}");
        }
        #[cfg(feature = "object-storage")]
        {
            res.expect("with the feature this config is valid");
        }
    }
}
