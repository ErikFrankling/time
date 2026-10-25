//! The Time server: event store, classifier and website.
//!
//! Everything arrives as OTel log records on `/v1/logs` and is stored as
//! typed events, deduplicated by `event.id`. Nothing derived (minutes,
//! reports, the status screen) is ever the only copy of a fact.

pub mod apk;
pub mod classify;
pub mod config;
pub mod day;
pub mod digest;
pub mod http;
pub mod import_v1;
pub mod inbox;
pub mod legacy;
pub mod listening;
pub mod mcp;
pub mod nudge;
pub mod status;
pub mod store;
pub mod views;
pub mod web;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use time_core::clock::{Clock, Millis};

pub use classify::Classifier;
pub use config::Config;
pub use store::Store;

/// Shared by every request handler.
pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub classifier: Classifier,
    /// Required on `/v1/logs` when set (`TIME_INGEST_TOKEN`). The homelab
    /// gateway authenticates devices; this guards the server itself.
    pub ingest_token: Option<String>,
    /// Required on `/mcp` when set (`TIME_MCP_TOKEN`).
    pub mcp_token: Option<String>,
    pub tz: chrono_tz::Tz,
    /// The Android APK served at `/app`.
    pub apk: apk::Shared,
    pub nudger: nudge::Nudger,
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
    /// Load the rules and, if they changed since the stored labels were
    /// made, queue all of history for relabelling.
    pub fn new(config: Config, store: Store, clock: Arc<dyn Clock>) -> anyhow::Result<Arc<Self>> {
        let classifier = Classifier::from_config(&config)?;
        if let Some(n) = classifier.requeue_if_rules_changed(&store)? {
            eprintln!(
                "classifier: rules are new ({}), {n} minute(s) queued",
                classifier.version()
            );
        }
        let tz = config.tz()?;
        let apk = apk::Apk::open(
            config
                .apk_path
                .clone()
                .unwrap_or_else(|| config.data_dir.join("time.apk")),
            config.apk_path.is_some(),
            config.apk_repo.clone(),
        );
        let ingest_token = std::env::var("TIME_INGEST_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let mcp_token = std::env::var("TIME_MCP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let nudger = nudge::Nudger::from_env(config.nudge.clone(), &config.public_url);
        let started_ms = clock.now_ms();
        Ok(Arc::new(Self {
            ingest_token,
            mcp_token,
            tz,
            apk,
            nudger,
            config,
            store,
            clock,
            classifier,
            started_ms,
            counters: Counters::default(),
        }))
    }
}

/// Serve on an already-bound listener until the future is dropped. Tests bind
/// port 0 and read the address back; `main` binds the configured port and
/// also starts [`classify::run_forever`].
pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> anyhow::Result<()> {
    axum::serve(listener, http::router(state)).await?;
    Ok(())
}
