//! Synchronous hot path: HTTP/JSON and syslog endpoints (architecture §2a).

pub mod counters;
pub mod http;
pub mod otlp;
pub mod syslog;

pub use counters::{InsertCounters, InsertCountersRef, ProtocolCounters};
pub use http::router as http_router;
pub use syslog::spawn_syslog_listeners;
