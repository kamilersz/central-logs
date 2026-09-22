//! central-logs: a self-hosted, single-node centralized logging platform.
//!
//! See `docs/ARCHITECTURE.md` for the full design. Crate layout follows §10:
//! - [`wal`] — append-only WAL segments + group-commit fsync + redb metadata.
//! - [`ingest`] — background workers: parse → enrich → batch-insert into DuckDB.
//! - [`store`] — DuckDB schema, Appender bulk insert, rollups, compaction.
//! - [`insert`] — HTTP/JSON and syslog ingest endpoints (the synchronous hot path).
//! - [`analytics`] — forecasting (`augurs`) and anomaly detection (MAD/seasonal/ROC).
//! - [`alerts`] — periodic alert-rule evaluation + webhook notification.
//! - [`web`] — axum + askama + htmx + uPlot dashboard.
//! - [`mcp`] — `rmcp` MCP server exposing query/summary/anomaly/forecast/alert tools.

pub mod ai;
pub mod alerts;
pub mod audit;
pub mod config;
pub mod error;
pub mod errors;
pub mod hot;
pub mod ingest;
pub mod insert;
pub mod query;
pub mod store;
pub mod wal;

#[cfg(feature = "dashboard")]
pub mod web;

pub mod analytics;

#[cfg(feature = "mcp")]
pub mod mcp;

pub use error::{Error, Result};

/// A received-but-not-yet-parsed log envelope.
///
/// This is the minimal framed record the insert layer hands to the WAL writer.
/// Parsing of the message body happens later, in the ingest path.
#[derive(Debug, Clone)]
pub struct RawRecord {
    /// When the insert layer received this record (microsecond precision UTC).
    pub receive_ts: chrono::DateTime<chrono::Utc>,
    /// Source IP/port if known (UDP/TCP syslog), empty for HTTP without proxy headers.
    pub source_addr: String,
    /// How the record arrived: `http_json`, `syslog_udp`, `syslog_tcp`.
    pub protocol: Protocol,
    /// Raw bytes of the payload (JSON envelope, syslog line, etc.).
    pub raw: bytes::Bytes,
}

impl RawRecord {
    #[inline]
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }
}

/// How a record arrived at the insert layer. Stored as a string in DuckDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Protocol {
    HttpJson,
    SyslogUdp,
    SyslogTcp,
    /// Sentry-SDK error events (error tracking) — marks rows so they can be
    /// filtered with the DSL: `protocol:sentry` (only) / `-protocol:sentry`
    /// (exclude), independent of regular HTTP/JSON logs.
    Sentry,
    /// OpenTelemetry OTLP log records (`POST /v1/logs`, protobuf or JSON).
    /// Queryable via the DSL: `protocol:otlp_log`.
    OtlpLog,
    /// OpenTelemetry OTLP spans (`POST /v1/traces`). Each row is one span;
    /// `trace_id`/`span_id`/`duration_ms` are populated so traces are
    /// searchable in the existing explorer and latency dashboards.
    OtlpSpan,
    /// OpenTelemetry OTLP metric data points (`POST /v1/metrics`). Each row is
    /// one point; the numeric value lands in `attributes.value`, the metric
    /// name in `message`, and labels in the remaining attributes.
    OtlpMetric,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::HttpJson => "http_json",
            Protocol::SyslogUdp => "syslog_udp",
            Protocol::SyslogTcp => "syslog_tcp",
            Protocol::Sentry => "sentry",
            Protocol::OtlpLog => "otlp_log",
            Protocol::OtlpSpan => "otlp_span",
            Protocol::OtlpMetric => "otlp_metric",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-source insertion-layer counters. Cheapest possible metric source — these
/// don't touch DuckDB at all (architecture §4, dashboard #1).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct InsertCounters {
    pub records_total: u64,
    pub bytes_total: u64,
    pub errors_total: u64,
    pub per_protocol: std::collections::HashMap<&'static str, ProtocolCounters>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ProtocolCounters {
    pub records: u64,
    pub bytes: u64,
}
