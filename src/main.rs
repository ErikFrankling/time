mod agent;
mod agents;
mod browser;
mod capture;
mod code;
mod input;
mod classify;
mod config;
mod db;
mod proto;
mod report;
mod server;
mod web;

use anyhow::Result;
use std::sync::Arc;

use config::Config;

const USAGE: &str = "time — minute-by-minute activity tracking

  time agent    capture the screen each minute and post it to the server
  time server   classify incoming frames, store them, and serve the UI
  time once     capture and post a single minute, print what came back
                (--wait-for-browser waits out one extension heartbeat first)
  time collect  read commits, diffs and pull requests, post them to the server
  time agents   read what the coding agents did, post it to the server
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
            // The browser extension heartbeats once a minute, so a one-shot run
            // has to wait out most of one to see a tab at all.
            let tabs = browser::Tabs::start(&cfg.agent.device);
            let settle = if std::env::args().any(|a| a == "--wait-for-browser") {
                std::time::Duration::from_secs(62)
            } else {
                std::time::Duration::from_millis(300)
            };
            std::thread::sleep(settle);
            let frame = agent::build_frame(&cfg.agent, &input, &tabs)?;
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
        // Runs where the repositories are, which is never where the server is.
        // Nightly is plenty: a commit timestamp does not change.
        "collect" => {
            let days = std::env::args()
                .nth(2)
                .and_then(|a| a.parse().ok())
                .unwrap_or(cfg.server.code_days);
            let rows = code::collect(&cfg.server, days)?;
            let commits: i64 = rows
                .iter()
                .filter(|r| r.source == code::SOURCE_GIT)
                .map(|r| r.commits)
                .sum();
            let added: i64 = rows.iter().map(|r| r.added).sum();
            let removed: i64 = rows.iter().map(|r| r.removed).sum();
            println!("{days}d: {commits} commits, +{added} -{removed}, {} rows", rows.len());
            // `--dry-run` so the numbers can be checked before a server exists.
            if std::env::args().any(|a| a == "--dry-run") {
                for r in &rows {
                    println!(
                        "  {} {:8} {:24} {:4}c +{:<7} -{:<7} pr{}/{} is{}/{} rv{}",
                        r.day, r.source, r.repo, r.commits, r.added, r.removed,
                        r.prs_opened, r.prs_merged, r.issues_opened, r.issues_closed, r.reviews
                    );
                }
                return Ok(());
            }
            println!("{}", code::post(&cfg.agent, &rows)?.trim());
            Ok(())
        }
        // Same shape as `collect`, and for the same reason: the transcripts are
        // on the machine that ran the agents, never on the server.
        "agents" => {
            let days = std::env::args()
                .nth(2)
                .and_then(|a| a.parse().ok())
                .unwrap_or(cfg.server.agent_days);
            let report = agents::collect(&cfg.server, &cfg.agent.device, days)?;
            let sessions: i64 = report.days.iter().map(|d| d.sessions).sum();
            let prompts: i64 = report.days.iter().map(|d| d.prompts).sum();
            let out: i64 = report.days.iter().map(|d| d.tokens_out).sum();
            let peak = report
                .minutes
                .iter()
                .fold(std::collections::HashMap::<i64, i64>::new(), |mut m, x| {
                    *m.entry(x.ts).or_default() += x.sessions;
                    m
                })
                .into_values()
                .max()
                .unwrap_or(0);
            println!(
                "{days}d: {sessions} sessions, {prompts} prompts, {out} output tokens, \
                 peak {peak} at once, {} day rows, {} minute rows",
                report.days.len(),
                report.minutes.len()
            );
            if std::env::args().any(|a| a == "--dry-run") {
                for d in &report.days {
                    println!(
                        "  {} {:8} {:24} {:3}s {:4}p in{:<10} out{:<10} {:4}m peak{}",
                        d.day, d.tool, d.project, d.sessions, d.prompts, d.tokens_in,
                        d.tokens_out, d.active_minutes, d.peak_parallel
                    );
                }
                return Ok(());
            }
            println!("{}", agents::post(&cfg.agent, &report)?.trim());
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
