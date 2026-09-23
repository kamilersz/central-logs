//! Synchronous hot path: HTTP/JSON, syslog, and engine/cluster push
//! protocol endpoints (architecture §2a).

pub mod counters;
pub mod fluentd;
pub mod gelf;
pub mod http;
pub mod k8s_audit;
pub mod msgpack;
pub mod otlp;
pub mod splunk_hec;
pub mod syslog;

pub use counters::{InsertCounters, InsertCountersRef, ProtocolCounters};
pub use fluentd::{spawn_fluentd_listener, FluentdState};
pub use gelf::{spawn_gelf_listeners, GelfListeners, GelfState};
pub use http::router as http_router;
pub use splunk_hec::router as splunk_hec_router;
pub use syslog::spawn_syslog_listeners;
