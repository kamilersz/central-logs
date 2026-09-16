//! Per-source insertion-layer counters. The cheapest metric source (architecture §4 #1).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::Protocol;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ProtocolCounters {
    pub records: u64,
    pub bytes: u64,
    pub errors: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct InsertCounters {
    pub records_total: u64,
    pub bytes_total: u64,
    pub errors_total: u64,
    /// Self-audit events dropped because the WAL channel was saturated
    /// (LESSON_LEARNED: "we cannot afford to lose any audit data" — at
    /// least make the loss visible). Surfaced in /api/counters + /metrics.
    pub audit_dropped_total: u64,
    pub per_protocol: HashMap<&'static str, ProtocolCounters>,
}

#[derive(Clone, Default)]
pub struct InsertCountersRef {
    inner: Arc<Mutex<InsertCounters>>,
}

impl InsertCountersRef {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, proto: Protocol, bytes: usize) {
        let mut c = self.inner.lock();
        c.records_total += 1;
        c.bytes_total += bytes as u64;
        let entry = c.per_protocol.entry(proto.as_str()).or_default();
        entry.records += 1;
        entry.bytes += bytes as u64;
    }

    pub fn record_error(&self, proto: Protocol) {
        let mut c = self.inner.lock();
        c.errors_total += 1;
        let entry = c.per_protocol.entry(proto.as_str()).or_default();
        entry.errors += 1;
    }

    /// Count a dropped self-audit event (WAL channel saturated).
    pub fn record_audit_drop(&self) {
        self.inner.lock().audit_dropped_total += 1;
    }

    pub fn snapshot(&self) -> InsertCounters {
        self.inner.lock().clone()
    }
}
