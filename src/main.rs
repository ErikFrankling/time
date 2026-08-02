mod agent;
mod capture;
mod input;
mod classify;
mod config;
mod db;
mod proto;
mod server;
mod web;

use anyhow::Result;
use std::sync::Arc;

use config::Config;

const USAGE: &str = "time — minute-by-minute activity tracking

  time agent    capture the screen each minute and post it to the server
  time server   classify incoming frames, store them, and serve the UI
  time once     capture and post a single minute, print what came back
  time config   print config and database paths

The agent holds no API key and makes no model calls. Everything that costs
money or needs a secret happens in the server.
";

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    let cfg = Config::load()?;

    match cmd.as_str() {
        "agent" => agent::run(&cfg.agent),
        "server" => server::run(Arc::new(cfg.server)),
        "once" => {
            // Give the input threads a moment to attach before sampling, or a
            // one-shot run always reports zero keys and looks like an empty
            // room.
            let input = input::Monitor::start();
            std::thread::sleep(std::time::Duration::from_millis(300));
            let frame = agent::build_frame(&cfg.agent, &input)?;
            let ack = agent::post(&cfg.agent, &frame)?;
            println!(
                "[{}]{} {} — {}",
                ack.category,
                if ack.classified { "" } else { " (skipped)" },
                ack.project.as_deref().unwrap_or("-"),
                ack.detail.as_deref().unwrap_or("")
            );
            Ok(())
        }
        "config" => {
            println!("config: {}", config::config_path()?.display());
            println!("db:     {}", config::db_path()?.display());
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}
