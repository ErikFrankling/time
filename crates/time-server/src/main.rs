use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use time_core::clock::SystemClock;
use time_server::{AppState, Config, Store};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TIME_CONFIG").map(PathBuf::from))
        .context("usage: time-server <config.toml> (or set TIME_CONFIG)")?;
    let config = Config::load(&path)?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let store = Store::open(&config.data_dir.join("time.db"))?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    eprintln!(
        "time-server {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    let state = AppState::new(config, store, Arc::new(SystemClock));
    time_server::serve(listener, state).await
}
