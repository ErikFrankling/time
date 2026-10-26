use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context;
use time_agent::sender::{Delivery, HttpSender, Sender};
use time_collect::pricing::RateTable;
use time_collect::usage::{self, Activity, Places};
use time_collect::{git, github, speak, t3, CollectConfig};

/// One run: scan, send, exit. A systemd timer runs it every few minutes;
/// stable event ids make every rerun harmless. `--days N` looks further back
/// than the config's window, for a backfill; `--dry` prints the events
/// instead of sending them.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut days = None;
    let mut dry = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--days" => days = Some(args.next().context("--days N")?.parse::<i64>()?),
            "--dry" => dry = true,
            _ => path = Some(PathBuf::from(a)),
        }
    }
    let path = path.context("usage: time-collect <collect.toml> [--days N] [--dry]")?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: CollectConfig = toml::from_str(&text).with_context(|| path.display().to_string())?;
    let now = chrono::Utc::now().timestamp_millis();
    let t0 = std::time::Instant::now();
    let lap = |what: &str| {
        let hwm = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmHWM"))
                    .map(|l| l.split_whitespace().nth(1).unwrap_or("").to_owned())
            })
            .unwrap_or_default();
        eprintln!(
            "collect: {what} at {:.1}s, peak {hwm} kB",
            t0.elapsed().as_secs_f64()
        )
    };
    let since = now - days.unwrap_or(cfg.days) * 86_400_000;

    // libgit2 maps pack files and caches objects by the gigabyte by default;
    // a background tally keeps to a few hundred megabytes, so the pc's lag
    // guard never has a reason to stop it.
    // SAFETY: set once, before any repository is opened.
    unsafe {
        let _ = git2::opts::set_mwindow_size(32 << 20);
        let _ = git2::opts::set_mwindow_mapped_limit(128 << 20);
        let _ = git2::opts::set_mwindow_file_limit(64);
        let _ = git2::opts::set_cache_max_size(64 << 20);
    }
    let mut events = Vec::new();
    let mut repos = Vec::new();
    for root in &cfg.roots {
        let root = expand(root);
        git::find_repos(&root, 6, &mut repos);
    }

    // Agents' calls: tokens per hour, and which repository each agent was
    // working in when, so commits can be credited.
    let t3_dir = expand(&cfg.t3);
    let (sessions, t3_projects) = usage::t3_sessions(&t3_dir).unwrap_or_else(|e| {
        eprintln!("collect: t3 sessions: {e:#}");
        Default::default()
    });
    let mut calls = usage::t3_calls(&t3_dir, &expand("~/")).unwrap_or_else(|e| {
        eprintln!("collect: t3 usage: {e:#}");
        Vec::new()
    });
    match usage::opencode_calls(&expand(&cfg.opencode)) {
        Ok(mut c) => calls.append(&mut c),
        Err(e) => eprintln!("collect: opencode: {e:#}"),
    }
    lap("usage calls read");
    let home = expand("~/");
    let places = Places::build(&home, &repos, &t3_projects);
    let activity = Activity::build(&calls, &places);
    lap("usage read");
    let rates = std::fs::read_to_string(t3_dir.join("usage-model-rates.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| RateTable::parse(&v["document"]))
        .unwrap_or_default();
    let mut hours = usage::hours(&calls, &rates, &places, &sessions, &cfg.device, since);
    eprintln!(
        "collect: {} model call(s), {} hour bucket(s)",
        calls.len(),
        hours.len()
    );
    events.append(&mut hours);

    let mut seen = HashSet::new();
    for repo in &repos {
        match git::scan(
            repo,
            &cfg.device,
            &cfg.authors,
            since / 1000,
            &mut seen,
            &activity,
        ) {
            Ok(mut e) => events.append(&mut e),
            Err(e) => eprintln!("collect: {}: {e:#}", repo.display()),
        }
    }
    lap("git scanned");
    if cfg.github {
        // GitHub answers for a year at most.
        match github::fetch(&cfg.device, since.max(now - 365 * 86_400_000), now) {
            Ok(mut e) => events.append(&mut e),
            Err(e) => eprintln!("collect: github: {e:#}"),
        }
        if cfg.github_commits > 0 {
            match github::commits(
                &cfg.device,
                since,
                now,
                &seen,
                &activity,
                cfg.github_commits,
            ) {
                Ok(mut e) => {
                    eprintln!("collect: {} commit(s) only on GitHub", e.len());
                    events.append(&mut e);
                }
                Err(e) => eprintln!("collect: github commits: {e:#}"),
            }
        }
    }
    lap("github fetched");
    if t3_dir.join("statev2.sqlite").exists() {
        match t3::collect(&t3_dir, &cfg.device, since) {
            Ok(mut e) => {
                eprintln!("collect: {} event(s) from T3 Code", e.len());
                events.append(&mut e);
            }
            Err(e) => eprintln!("collect: t3: {e:#}"),
        }
    }
    let speak_dir = expand(&cfg.speak);
    if speak_dir.is_dir() {
        let mut e = speak::collect(&speak_dir, &cfg.device, since);
        eprintln!("collect: {} event(s) from speak", e.len());
        events.append(&mut e);
    }
    eprintln!(
        "collect: {} event(s) from {} repositories",
        events.len(),
        repos.len()
    );
    if dry {
        for e in &events {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({"id": e.id, "start": e.start_ms, "name": e.event.name(), "body": e.event})
                )?
            );
        }
        return Ok(());
    }
    let mut sender = HttpSender::new(&cfg.endpoint, std::env::var("TIME_OTLP_TOKEN").ok())?;
    for chunk in events.chunks(500) {
        match sender.send(chunk) {
            Delivery::Accepted => {}
            Delivery::Retry(why) | Delivery::Refused(why) => anyhow::bail!("sending: {why}"),
        }
    }
    Ok(())
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(p),
    }
}
