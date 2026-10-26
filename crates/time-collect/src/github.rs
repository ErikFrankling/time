//! From GitHub: pull requests and reviews (one GraphQL request per run), and
//! commits no local clone has.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;

use time_core::event::{Commit, Envelope, Event, PullRequest, Review};

use crate::usage::Activity;
use crate::{stable_id, SERVICE};

const QUERY: &str = r#"
query($from: DateTime!, $to: DateTime!) {
  viewer {
    contributionsCollection(from: $from, to: $to) {
      pullRequestContributions(first: 100) {
        nodes { occurredAt pullRequest { number merged mergedAt repository { nameWithOwner } } }
      }
      pullRequestReviewContributions(first: 100) {
        nodes { occurredAt pullRequest { number } repository { nameWithOwner } }
      }
    }
  }
}
"#;

/// From TIME_GITHUB_TOKEN / GITHUB_TOKEN / GH_TOKEN, else the `gh` CLI's own
/// login. Never from a config file.
pub fn token() -> Option<String> {
    for var in ["TIME_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(v) = std::env::var(var)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
        {
            return Some(v);
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !t.is_empty()).then_some(t)
}

pub fn fetch(device: &str, from_ms: i64, to_ms: i64) -> Result<Vec<Envelope>> {
    let Some(token) = token() else {
        bail!("no GitHub token (set TIME_GITHUB_TOKEN, or run `gh auth login`)")
    };
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap_or_default()
            .to_rfc3339()
    };
    let body = serde_json::json!({ "query": QUERY, "variables": { "from": iso(from_ms), "to": iso(to_ms) } });
    let resp = reqwest::blocking::Client::new()
        .post("https://api.github.com/graphql")
        .bearer_auth(token)
        .header("User-Agent", "time-collect")
        .json(&body)
        .send()
        .context("calling the GitHub GraphQL API")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().context("parsing the GitHub response")?;
    if !status.is_success() || v.get("errors").is_some() {
        bail!("github {status}: {v}");
    }
    Ok(parse(device, &v))
}

fn nodes<'a>(v: &'a serde_json::Value, path: &[&str]) -> &'a [serde_json::Value] {
    let mut cur = v;
    for k in path {
        cur = &cur[k];
    }
    cur.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn repo_of(v: &serde_json::Value) -> String {
    v["nameWithOwner"]
        .as_str()
        .unwrap_or("unknown")
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
        .to_owned()
}

fn at(s: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The GraphQL response as events, separate from the network for testing.
pub fn parse(device: &str, v: &serde_json::Value) -> Vec<Envelope> {
    let c = &v["data"]["viewer"]["contributionsCollection"];
    let mut out = Vec::new();
    let mut push = |key: String, ms: i64, event: Event| {
        out.push(Envelope {
            id: stable_id(&key),
            device: device.to_owned(),
            service: SERVICE.to_owned(),
            start_ms: ms,
            end_ms: ms,
            event,
        });
    };
    for n in nodes(c, &["pullRequestContributions", "nodes"]) {
        let pr = &n["pullRequest"];
        let repo = repo_of(&pr["repository"]);
        let number = pr["number"].as_u64().unwrap_or(0) as u32;
        if let Some(ms) = at(n["occurredAt"].as_str()) {
            push(
                format!("pr:{repo}:{number}:opened"),
                ms,
                Event::PullRequest(PullRequest {
                    repo: repo.clone(),
                    number,
                    action: "opened".into(),
                }),
            );
        }
        // Merged counts on the day it merged, not the day it was opened.
        if pr["merged"].as_bool() == Some(true) {
            if let Some(ms) = at(pr["mergedAt"].as_str()) {
                push(
                    format!("pr:{repo}:{number}:merged"),
                    ms,
                    Event::PullRequest(PullRequest {
                        repo: repo.clone(),
                        number,
                        action: "merged".into(),
                    }),
                );
            }
        }
    }
    for n in nodes(c, &["pullRequestReviewContributions", "nodes"]) {
        let repo = repo_of(&n["repository"]);
        let number = n["pullRequest"]["number"].as_u64().unwrap_or(0) as u32;
        if let Some(ms) = at(n["occurredAt"].as_str()) {
            push(
                format!("review:{repo}:{number}:{ms}"),
                ms,
                Event::Review(Review { repo, number }),
            );
        }
    }
    out
}

/// Erik's commits that only GitHub has (no clone on this machine has their
/// sha): found with the commit search, counted from each commit's file list
/// with the same generated-file rules as local ones. At most `max_detail`
/// commits are looked up one by one per run.
pub fn commits(
    device: &str,
    from_ms: i64,
    to_ms: i64,
    local: &HashSet<String>,
    activity: &Activity,
    max_detail: usize,
) -> Result<Vec<Envelope>> {
    let Some(token) = token() else {
        bail!("no GitHub token (set TIME_GITHUB_TOKEN, or run `gh auth login`)")
    };
    let client = reqwest::blocking::Client::new();
    // Rate limited (403/429): wait as GitHub asks (at most a few minutes
    // in all), then try again.
    let get = |url: &str| -> Result<serde_json::Value> {
        let mut waits = 0;
        loop {
            let resp = client
                .get(url)
                .bearer_auth(&token)
                .header("User-Agent", "time-collect")
                .header("Accept", "application/vnd.github+json")
                .send()
                .with_context(|| url.to_owned())?;
            let status = resp.status();
            let retry = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let v: serde_json::Value = resp.json().with_context(|| url.to_owned())?;
            if status.is_success() {
                return Ok(v);
            }
            if (status.as_u16() == 403 || status.as_u16() == 429) && waits < 4 {
                waits += 1;
                std::thread::sleep(std::time::Duration::from_secs(retry.unwrap_or(60).min(120)));
                continue;
            }
            bail!("github {status}: {url}: {v}");
        }
    };
    let login = get("https://api.github.com/user")?["login"]
        .as_str()
        .context("no login")?
        .to_owned();
    let day = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap_or_default()
            .format("%Y-%m-%d")
            .to_string()
    };
    // The search returns at most 1000 hits per query: a month at a time,
    // halved while a window is fuller than that.
    let mut windows = Vec::new();
    let mut t = from_ms;
    while t < to_ms {
        windows.push((t, (t + 30 * 86_400_000).min(to_ms)));
        t += 30 * 86_400_000;
    }
    let mut found: Vec<serde_json::Value> = Vec::new();
    let mut searches = 0;
    while let Some((a, b)) = windows.pop() {
        let mut page = 1;
        loop {
            // 30 searches a minute.
            if searches > 0 {
                std::thread::sleep(std::time::Duration::from_millis(2_100));
            }
            searches += 1;
            let url = format!(
                "https://api.github.com/search/commits?q=author:{login}+author-date:{}..{}&sort=author-date&per_page=100&page={page}",
                day(a),
                day(b)
            );
            let v = match get(&url) {
                Ok(v) => v,
                // Keep what was found; the next run looks again.
                Err(e) => {
                    eprintln!("collect: github commit search: {e:#}");
                    break;
                }
            };
            let total = v["total_count"].as_u64().unwrap_or(0);
            if total > 1000 && b - a > 86_400_000 {
                let mid = a + (b - a) / 2;
                windows.push((a, mid));
                windows.push((mid, b));
                break;
            }
            let items = v["items"].as_array().cloned().unwrap_or_default();
            let n = items.len();
            found.extend(items);
            if n < 100 || page * 100 >= total as usize {
                break;
            }
            page += 1;
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in found {
        let Some(sha) = item["sha"].as_str().map(str::to_owned) else {
            continue;
        };
        if item["parents"].as_array().is_some_and(|p| p.len() > 1) {
            continue;
        }
        let Some(ms) = at(item["commit"]["author"]["date"].as_str()) else {
            continue;
        };
        if ms < from_ms || ms >= to_ms {
            continue;
        }
        let full = item["repository"]["full_name"].as_str().unwrap_or_default();
        let repo = full.rsplit('/').next().unwrap_or(full).to_owned();
        let message = item["commit"]["message"].as_str().unwrap_or_default();
        let key = crate::git::identity(
            &repo,
            ms / 1000,
            item["commit"]["author"]["email"]
                .as_str()
                .unwrap_or_default(),
            message,
        );
        // Known from a clone (by hash, or rebased there), or found twice.
        if local.contains(&sha) || local.contains(&key) || !seen.insert(key.clone()) {
            continue;
        }
        if out.len() >= max_detail {
            break;
        }
        let Ok(detail) = get(&format!(
            "https://api.github.com/repos/{full}/commits/{sha}"
        ))
        .inspect_err(|e| eprintln!("collect: github commit: {e:#}")) else {
            break;
        };
        let (mut added, mut removed, mut files) = (0u32, 0u32, 0u32);
        for f in detail["files"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let a = f["additions"].as_u64().unwrap_or(0) as u32;
            let r = f["deletions"].as_u64().unwrap_or(0) as u32;
            if crate::git::is_generated(
                f["filename"].as_str().unwrap_or_default(),
                (a + r) as usize,
            ) {
                continue;
            }
            added += a;
            removed += r;
            files += 1;
        }
        if added + removed > crate::git::BULK_COMMIT_LINES {
            (added, removed) = (0, 0);
        }
        let (agent, agent_evidence) = crate::git::agent_of(message, &repo, ms, activity);
        out.push(Envelope {
            id: stable_id(&key),
            device: device.to_owned(),
            service: SERVICE.to_owned(),
            start_ms: ms,
            end_ms: ms,
            event: Event::Commit(Commit {
                repo,
                sha,
                path: None,
                lines_added: added,
                lines_removed: removed,
                files,
                agent,
                agent_evidence,
                source: Some("github".into()),
            }),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_and_merged_are_separate_events_with_stable_ids() {
        let v = serde_json::json!({"data": {"viewer": {"contributionsCollection": {
            "pullRequestContributions": {"nodes": [
                {"occurredAt": "2026-10-20T10:00:00Z", "pullRequest": {"number": 7, "merged": true, "mergedAt": "2026-10-21T09:00:00Z", "repository": {"nameWithOwner": "ErikFrankling/time"}}},
                {"occurredAt": "2026-10-20T11:00:00Z", "pullRequest": {"number": 8, "merged": false, "mergedAt": null, "repository": {"nameWithOwner": "ErikFrankling/time"}}}
            ]},
            "pullRequestReviewContributions": {"nodes": [
                {"occurredAt": "2026-10-20T12:00:00Z", "pullRequest": {"number": 3}, "repository": {"nameWithOwner": "neptune/naiaclaw"}}
            ]}
        }}}});
        let events = parse("pc", &v);
        assert_eq!(events.len(), 4);
        assert_eq!(
            parse("pc", &v)
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            events.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
        );
        assert!(
            matches!(&events[1].event, Event::PullRequest(p) if p.action == "merged" && p.number == 7)
        );
        assert!(
            matches!(&events[3].event, Event::Review(r) if r.repo == "naiaclaw" && r.number == 3)
        );
    }
}
