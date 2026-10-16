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
    /// Website-analytics beacons refused, grouped by reason (PLAN-WEB-ANALYTICS
    /// W1): `dnt`, `gpc`, `rate_limited`, `shed`, `bad_payload`, `unknown_site`,
    /// `origin_denied`, `too_many_events`, `paused`. Analytics is shed before
    /// logs ever are, so its losses must be visible.
    pub web_rejected_total: HashMap<&'static str, u64>,
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
        self.record_n(proto, 1, bytes as u64);
    }

    /// Batch form: count `n` records totalling `bytes` bytes in one lock.
    pub fn record_n(&self, proto: Protocol, n: usize, bytes: u64) {
        let mut c = self.inner.lock();
        c.records_total += n as u64;
        c.bytes_total += bytes;
        let entry = c.per_protocol.entry(proto.as_str()).or_default();
        entry.records += n as u64;
        entry.bytes += bytes;
    }

    pub fn record_error(&self, proto: Protocol) {
        self.record_error_n(proto, 1);
    }

    pub fn record_error_n(&self, proto: Protocol, n: usize) {
        let mut c = self.inner.lock();
        c.errors_total += n as u64;
        let entry = c.per_protocol.entry(proto.as_str()).or_default();
        entry.errors += n as u64;
    }

    /// Count a dropped self-audit event (WAL channel saturated).
    pub fn record_audit_drop(&self) {
        self.inner.lock().audit_dropped_total += 1;
    }

    /// Count a refused web-analytics beacon by reason (dnt, rate_limited,
    /// shed, bad_payload, …).
    pub fn record_web_rejected(&self, reason: &'static str) {
        let mut c = self.inner.lock();
        c.errors_total += 1;
        *c.web_rejected_total.entry(reason).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> InsertCounters {
        self.inner.lock().clone()
    }
}
