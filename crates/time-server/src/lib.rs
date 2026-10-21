//! The Time server: event store, classifier and website.
//!
//! Everything arrives as OTel log records on `/v1/logs` and is stored as
//! typed events, deduplicated by `event.id`. Nothing derived (minutes,
//! reports, the status screen) is ever the only copy of a fact.

pub mod config;
pub mod http;
pub mod status;
pub mod store;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use time_core::clock::{Clock, Millis};

pub use config::Config;
pub use store::Store;

/// Shared by every request handler.
pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub started_ms: Millis,
    pub counters: Counters,
}

/// Process-lifetime counters exported on `/metrics`.
#[derive(Default)]
pub struct Counters {
    pub ingest_requests: AtomicU64,
    pub records_accepted: AtomicU64,
    pub records_duplicate: AtomicU64,
    pub records_rejected: AtomicU64,
}

impl AppState {
    pub fn new(config: Config, store: Store, clock: Arc<dyn Clock>) -> Arc<Self> {
        let started_ms = clock.now_ms();
        Arc::new(Self {
            config,
            store,
            clock,
            started_ms,
            counters: Counters::default(),
        })
    }
}

/// Serve on an already-bound listener until the future is dropped. Tests bind
/// port 0 and read the address back; `main` binds the configured port.
pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> anyhow::Result<()> {
    axum::serve(listener, http::router(state)).await?;
    Ok(())
}
