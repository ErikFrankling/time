//! Pull requests and reviews from GitHub: things that leave no trace in a
//! clone. One GraphQL request per run.

use anyhow::{bail, Context, Result};
use time_core::event::{Envelope, Event, PullRequest, Review};

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
