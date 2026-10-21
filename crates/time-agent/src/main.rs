use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use time_agent::audio::Mpris;
use time_agent::browser::Watcher;
use time_agent::hyprland::Hyprland;
use time_agent::input::Evdev;
use time_agent::sender::HttpSender;
use time_agent::{Agent, AgentConfig};
use time_core::clock::{Clock, SystemClock, MINUTE_MS};

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TIME_AGENT_CONFIG").map(PathBuf::from))
        .context("usage: time-agent <agent.toml> (or set TIME_AGENT_CONFIG)")?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config = AgentConfig::parse(&text).with_context(|| path.display().to_string())?;
    let token = std::env::var("TIME_OTLP_TOKEN").ok();
    let sender = HttpSender::new(&config.endpoint, token)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let tabs = Watcher::start(&config.device);

    let mut agent = Agent::new(
        config,
        clock.clone(),
        Box::new(Hyprland::from_env()?),
        Box::new(Evdev::start()),
        Box::new(Mpris::default()),
        Box::new(tabs),
        Box::new(sender),
    )?;
    eprintln!(
        "time-agent {} for {}",
        env!("CARGO_PKG_VERSION"),
        agent.config.device
    );
    loop {
        let now = clock.now_ms();
        let next = (now / MINUTE_MS + 1) * MINUTE_MS + 500;
        std::thread::sleep(Duration::from_millis((next - now) as u64));
        match agent.tick() {
            Ok(r) if r.retry.is_some() => {
                eprintln!(
                    "agent: {} pending, will retry: {}",
                    r.pending,
                    r.retry.unwrap_or_default()
                )
            }
            Ok(_) => {}
            Err(e) => eprintln!("agent: tick failed: {e:#}"),
        }
    }
}
