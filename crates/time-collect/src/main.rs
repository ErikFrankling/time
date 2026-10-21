use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context;
use time_agent::sender::{Delivery, HttpSender, Sender};
use time_collect::{git, github, CollectConfig};

/// One run: scan, send, exit. A systemd timer runs it every few minutes;
/// stable event ids make every rerun harmless.
fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: time-collect <collect.toml>")?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: CollectConfig = toml::from_str(&text).with_context(|| path.display().to_string())?;
    let now = chrono::Utc::now().timestamp_millis();
    let since = now - cfg.days * 86_400_000;

    let mut events = Vec::new();
    let mut repos = Vec::new();
    for root in &cfg.roots {
        let root = expand(root);
        git::find_repos(&root, 6, &mut repos);
    }
    let mut seen = HashSet::new();
    for repo in &repos {
        match git::scan(repo, &cfg.device, &cfg.authors, since / 1000, &mut seen) {
            Ok(mut e) => events.append(&mut e),
            Err(e) => eprintln!("collect: {}: {e:#}", repo.display()),
        }
    }
    if cfg.github {
        match github::fetch(&cfg.device, since, now) {
            Ok(mut e) => events.append(&mut e),
            Err(e) => eprintln!("collect: github: {e:#}"),
        }
    }
    eprintln!(
        "collect: {} event(s) from {} repositories",
        events.len(),
        repos.len()
    );
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
