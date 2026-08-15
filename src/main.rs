mod agent;
mod agents;
mod apk;
mod browser;
mod capture;
mod classifier;
mod classify;
mod code;
mod config;
mod db;
mod input;
mod proto;
mod reclassify;
mod report;
mod server;
mod spool;
mod web;

use anyhow::{Context, Result};
use std::sync::Arc;

use config::Config;

const USAGE: &str = "time — minute-by-minute activity tracking

  time agent    capture the screen each minute and post it to the server
  time server   classify incoming frames, store them, and serve the UI
  time once     capture and post a single minute, print what came back
                (--wait-for-browser waits out one extension heartbeat first)
  time collect  read commits, diffs and pull requests, post them to the server
  time agents   read what the coding agents did, post it to the server
  time reclassify <days> [--pending] [--device NAME] [--model NAME] [--endpoint URL]
                [--run-id ID] [--limit N] [--apply]
                re-judge stored minutes from the kept screenshots; results go
                to the minute_trial table for comparison unless --apply, which
                rewrites the live labels. Runs where the server data is.
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
                agent::status(&ack),
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
            let (rows, cache) = code::collect(&cfg.server, days)?;
            let commits: i64 = rows
                .iter()
                .filter(|r| r.source == code::SOURCE_GIT)
                .map(|r| r.commits)
                .sum();
            let added: i64 = rows.iter().map(|r| r.added).sum();
            let removed: i64 = rows.iter().map(|r| r.removed).sum();
            println!(
                "{days}d: {commits} commits, +{added} -{removed}, {} rows",
                rows.len()
            );
            // `--dry-run` so the numbers can be checked before a server exists.
            if std::env::args().any(|a| a == "--dry-run") {
                for r in &rows {
                    println!(
                        "  {} {:8} {:24} {:4}c +{:<7} -{:<7} pr{}/{} is{}/{} rv{}",
                        r.day,
                        r.source,
                        r.repo,
                        r.commits,
                        r.added,
                        r.removed,
                        r.prs_opened,
                        r.prs_merged,
                        r.issues_opened,
                        r.issues_closed,
                        r.reviews
                    );
                }
                return Ok(());
            }
            println!("{}", code::post(&cfg.agent, &rows)?.trim());
            // Only now: a scan whose rows never reached the server must be
            // repeated, not remembered as done.
            cache.commit();
            Ok(())
        }
        // Same shape as `collect`, and for the same reason: the transcripts are
        // on the machine that ran the agents, never on the server.
        "agents" => {
            let days = std::env::args()
                .nth(2)
                .and_then(|a| a.parse().ok())
                .unwrap_or(cfg.server.agent_days);
            let (report, cache) = agents::collect(&cfg.server, &cfg.agent.device, days)?;
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
                        d.day,
                        d.tool,
                        d.project,
                        d.sessions,
                        d.prompts,
                        d.tokens_in,
                        d.tokens_out,
                        d.active_minutes,
                        d.peak_parallel
                    );
                }
                return Ok(());
            }
            println!("{}", agents::post(&cfg.agent, &report)?.trim());
            // Only after the server accepted the report, for the same reason
            // as `collect`: a failed post must be rescanned, not remembered.
            cache.commit();
            Ok(())
        }
        // Server-side, unlike collect/agents: it reads the database and the
        // kept frames, both of which live where the server runs.
        "reclassify" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let mut opts = reclassify::Opts::default();
            let mut days = None;
            let mut i = 0;
            while i < args.len() {
                let flag = args[i].as_str();
                match flag {
                    "--apply" => opts.apply = true,
                    "--pending" => opts.pending_only = true,
                    "--device" | "--model" | "--endpoint" | "--run-id" | "--limit" => {
                        i += 1;
                        let v = args
                            .get(i)
                            .cloned()
                            .with_context(|| format!("{flag} needs a value"))?;
                        match flag {
                            "--device" => opts.device = Some(v),
                            "--model" => opts.model = Some(v),
                            "--endpoint" => opts.endpoint = Some(v),
                            "--run-id" => opts.run_id = Some(v),
                            _ => opts.limit = Some(v.parse().context("--limit takes a number")?),
                        }
                    }
                    a => {
                        days = Some(
                            a.parse::<i64>()
                                .with_context(|| format!("unexpected argument {a}"))?,
                        )
                    }
                }
                i += 1;
            }
            opts.days = days.context(
                "usage: time reclassify <days> [--device NAME] [--model NAME] \
                 [--endpoint URL] [--run-id ID] [--limit N] [--apply]",
            )?;
            reclassify::run(cfg.server, opts)
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
