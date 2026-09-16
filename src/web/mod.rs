//! Dashboard UI: axum routes + askama templates + vendored assets.
//! See architecture §4.

pub mod api;
pub mod auth;
pub mod auth_api;
pub mod dashboard;
pub mod errors_api;
pub mod ops_api;
pub mod sentry_api;
pub mod spa;
pub mod templates;

use std::sync::Arc;

use axum::Router;
use parking_lot::Mutex;

use crate::config::Config;
use crate::insert::counters::InsertCountersRef;
use crate::store::Store;
use crate::wal::meta::WalMeta;
use crate::wal::writer::WalGauges;

#[derive(Clone)]
pub struct WebState {
    pub store: Store,
    pub meta: Arc<WalMeta>,
    pub counters: InsertCountersRef,
    pub cfg: Arc<Mutex<Config>>,
    /// WAL health gauges shared with the writer task (channel fill, bytes).
    pub wal: Arc<WalGauges>,
}

pub fn router(state: WebState) -> Router {
    dashboard::router(state)
}

/// Convenience: build the SPA catch-all router.
pub fn spa_router() -> Router {
    spa::router()
}
