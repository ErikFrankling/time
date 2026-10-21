use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use time_agent::audio::Mpris;
use time_agent::browser::Watcher;
use time_agent::hyprland::Hyprland;
use time_agent::input::Evdev;
use time_agent::probe::{Audio, Desktop, Input, Tabs};
use time_agent::sender::HttpSender;
use time_agent::{Agent, AgentConfig};
use time_core::clock::{Clock, SystemClock, MINUTE_MS};

const USAGE: &str = "usage:
  time-agent <agent.toml>          observe every minute and send
  time-agent --probe <agent.toml>  print what the agent can see right now, then exit";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (probe, path) = match args.as_slice() {
        [flag, p] if flag == "--probe" => (true, PathBuf::from(p)),
        [p] => (false, PathBuf::from(p)),
        [] => (
            false,
            std::env::var_os("TIME_AGENT_CONFIG")
                .map(PathBuf::from)
                .context(USAGE)?,
        ),
        _ => anyhow::bail!("{USAGE}"),
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config = AgentConfig::parse(&text).with_context(|| path.display().to_string())?;
    if probe {
        return run_probe(&config);
    }

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
    )?
    .with_system(Box::new(time_agent::system::Linux));
    eprintln!(
        "time-agent {} for {}",
        env!("CARGO_PKG_VERSION"),
        agent.config.device
    );

    // Sending on its own thread: a slow or unreachable receiver must never
    // delay the next observation (it once cost a minute that way).
    let mut flusher = agent.take_flusher().context("flusher")?;
    std::thread::spawn(move || loop {
        match flusher.flush() {
            Ok(r) if r.retry.is_some() => {
                eprintln!("agent: will retry: {}", r.retry.unwrap_or_default())
            }
            Ok(_) => {}
            Err(e) => eprintln!("agent: send failed: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(10));
    });

    loop {
        let now = clock.now_ms();
        let next = (now / MINUTE_MS + 1) * MINUTE_MS + 500;
        std::thread::sleep(Duration::from_millis((next - now) as u64));
        if let Err(e) = agent.tick() {
            eprintln!("agent: tick failed: {e:#}");
        }
    }
}

/// What each probe sees now, so a quiet channel can be diagnosed in seconds.
fn run_probe(config: &AgentConfig) -> anyhow::Result<()> {
    match Hyprland::from_env().and_then(|mut h| h.active_window()) {
        Ok(Some(w)) => println!(
            "window:  class={:?} fullscreen={} title={} chars",
            w.class,
            w.fullscreen,
            w.title.chars().count()
        ),
        Ok(None) => println!("window:  nothing has focus"),
        Err(e) => println!("window:  ERROR {e:#}"),
    }
    let mut input = Evdev::start();
    std::thread::sleep(Duration::from_secs(3));
    let snap = input.take();
    match input.problem() {
        Some(p) => println!("input:   ERROR {p}"),
        None => println!(
            "input:   {} devices; in 3 s: {} keys, {} pointer events; idle {:?} s",
            input.devices(),
            snap.keys,
            snap.mouse,
            snap.idle_seconds
        ),
    }
    let mut audio = Mpris::default();
    let playing = audio.playing();
    match audio.problem() {
        Some(p) => println!("audio:   ERROR {p}"),
        None => println!("audio:   playing={playing}"),
    }
    let s = time_agent::system::System::sample(&mut time_agent::system::Linux);
    println!(
        "system:  locked={:?} lid_closed={:?} charging={:?} battery={:?}% wifi={:?} gateway={:?} domain={:?}",
        s.locked, s.lid_closed, s.charging, s.battery_percent, s.network.ssid, s.network.gateway, s.network.domain
    );
    let tabs = Watcher::start(&config.device);
    match tabs.problem() {
        Some(p) => println!("tabs:    ERROR {p}"),
        None => println!("tabs:    listening on 127.0.0.1:5600"),
    }
    println!("sends to {}", config.endpoint);
    Ok(())
}
