//! Code and AI: commits and lines (Erik's own and his agents'), and the
//! tokens every agent used with what they would cost at API list prices, per
//! hour of a day or per day of a week or month.
//!
//! Sources: `github.commit` events (local clones and GitHub, by the
//! collector) and `llm.usage.hour` events (the agents' own transcripts, read
//! through T3 Code's usage scan and OpenCode's database).

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};
use time_core::clock::Millis;
use time_core::event::{Envelope, Event};

pub const EVENTS: [&str; 2] = ["github.commit", "llm.usage.hour"];

/// Series drawn separately before the rest is "other".
const TOP: usize = 7;

#[derive(Debug, Clone)]
struct CommitRow {
    bucket: usize,
    repo: String,
    agent: Option<String>,
    added: u64,
    removed: u64,
}

#[derive(Debug, Clone)]
struct UsageRow {
    bucket: usize,
    provider: String,
    account: String,
    model: String,
    project: String,
    thread: Option<(String, String)>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost: f64,
    requests: u64,
}

impl UsageRow {
    fn tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

/// Per repository: commits, lines added, removed; then the same for Erik.
type RepoSums = (u64, u64, u64, u64, u64, u64);

/// One period's code and AI usage, bucketed.
#[derive(Debug, Clone, Default)]
pub struct Work {
    pub buckets: usize,
    commits: Vec<CommitRow>,
    usage: Vec<UsageRow>,
}

/// The headline numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub commits: u64,
    pub added: u64,
    pub removed: u64,
    pub erik_commits: u64,
    pub erik_added: u64,
    pub erik_removed: u64,
    pub tokens: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
    pub requests: u64,
}

/// A model's provider family, for colours and grouping.
fn family(provider: &str) -> &str {
    match provider {
        "claude" | "claudeAgent" => "claude",
        p => p,
    }
}

/// "claude-opus-5-5" → "Opus 5.5"; other names as they are.
pub fn model_name(model: &str) -> String {
    let m = model.split('[').next().unwrap_or(model);
    if let Some(rest) = m.strip_prefix("claude-") {
        let mut parts = rest.split('-');
        let fam = parts.next().unwrap_or("");
        let ver: Vec<&str> = parts
            .take_while(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit()))
            .collect();
        let mut fam = fam.to_owned();
        if let Some(f) = fam.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        return if ver.is_empty() {
            fam
        } else {
            format!("{fam} {}", ver.join("."))
        };
    }
    m.to_owned()
}

impl Work {
    /// From events, `bucket` placing a time in a bucket (or outside them all).
    pub fn collect(
        events: &[Envelope],
        buckets: usize,
        bucket: impl Fn(Millis) -> Option<usize>,
    ) -> Self {
        let mut w = Self {
            buckets,
            ..Default::default()
        };
        for e in events {
            let Some(b) = bucket(e.start_ms).filter(|b| *b < buckets) else {
                continue;
            };
            match &e.event {
                Event::Commit(c) => w.commits.push(CommitRow {
                    bucket: b,
                    repo: c.repo.rsplit('/').next().unwrap_or(&c.repo).to_owned(),
                    agent: c.agent.clone(),
                    added: u64::from(c.lines_added),
                    removed: u64::from(c.lines_removed),
                }),
                Event::UsageHour(u) => w.usage.push(UsageRow {
                    bucket: b,
                    provider: family(&u.provider).to_owned(),
                    account: if u.account.is_empty() {
                        family(&u.provider).to_owned()
                    } else {
                        u.account.clone()
                    },
                    model: u.model.split('[').next().unwrap_or(&u.model).to_owned(),
                    project: u.project.clone().unwrap_or_else(|| "(no project)".into()),
                    thread: u.thread_id.clone().map(|id| {
                        (
                            id,
                            u.thread_title
                                .clone()
                                .unwrap_or_else(|| "(untitled thread)".into()),
                        )
                    }),
                    input: u.input_tokens,
                    cache_read: u.cache_read_tokens,
                    cache_write: u.cache_write_tokens,
                    output: u.output_tokens,
                    cost: u.cost_usd,
                    requests: u64::from(u.requests),
                }),
                _ => {}
            }
        }
        w
    }

    /// The events of `[from, to)` placed in buckets `width` ms wide (an
    /// hour, or a day when the days are given as `starts`).
    pub fn between(events: &[Envelope], starts: &[Millis], to: Millis) -> Self {
        let n = starts.len();
        Self::collect(events, n, |t| {
            if t >= to || starts.first().is_none_or(|s| t < *s) {
                return None;
            }
            Some(starts.partition_point(|s| *s <= t).saturating_sub(1))
        })
    }

    pub fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for c in &self.commits {
            t.commits += 1;
            t.added += c.added;
            t.removed += c.removed;
            if c.agent.is_none() {
                t.erik_commits += 1;
                t.erik_added += c.added;
                t.erik_removed += c.removed;
            }
        }
        for u in &self.usage {
            t.tokens += u.tokens();
            t.input += u.input;
            t.cache_read += u.cache_read;
            t.cache_write += u.cache_write;
            t.output += u.output;
            t.cost += u.cost;
            t.requests += u.requests;
        }
        t
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty() && self.usage.is_empty()
    }

    /// Everything the Code and AI charts draw.
    pub fn json(&self, prev: &Totals) -> Value {
        let n = self.buckets;
        let t = self.totals();

        // Commits per bucket by repository (top ones, then "other"); Erik's
        // own and the agents' apart.
        let mut repo_totals: HashMap<&str, RepoSums> = HashMap::new();
        for c in &self.commits {
            let r = repo_totals.entry(&c.repo).or_default();
            r.0 += 1;
            r.1 += c.added;
            r.2 += c.removed;
            if c.agent.is_none() {
                r.3 += 1;
                r.4 += c.added;
                r.5 += c.removed;
            }
        }
        let mut repos: Vec<(&str, RepoSums)> = repo_totals.into_iter().collect();
        repos.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
        let top: Vec<&str> = repos.iter().take(TOP).map(|(r, _)| *r).collect();
        let mut by_repo: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
        let mut erik = vec![0u64; n];
        let mut agents = vec![0u64; n];
        let (mut add_e, mut del_e, mut add_a, mut del_a) =
            (vec![0u64; n], vec![0u64; n], vec![0u64; n], vec![0u64; n]);
        let mut by_agent: HashMap<String, u64> = HashMap::new();
        for c in &self.commits {
            let i = top.iter().position(|r| *r == c.repo).unwrap_or(top.len());
            by_repo.entry(i).or_insert_with(|| vec![0; n])[c.bucket] += 1;
            if c.agent.is_none() {
                erik[c.bucket] += 1;
                add_e[c.bucket] += c.added;
                del_e[c.bucket] += c.removed;
            } else {
                agents[c.bucket] += 1;
                add_a[c.bucket] += c.added;
                del_a[c.bucket] += c.removed;
            }
            let who = c.agent.as_deref().map_or("Erik".to_owned(), model_name);
            *by_agent.entry(who).or_default() += 1;
        }
        let repo_series: Vec<Value> = by_repo
            .into_iter()
            .map(|(i, data)| json!({"name": top.get(i).copied().unwrap_or("other"), "data": data}))
            .collect();
        let mut by_agent: Vec<(String, u64)> = by_agent.into_iter().collect();
        by_agent.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        // Tokens and cost per bucket by model.
        let mut model_totals: HashMap<(&str, &str), [f64; 7]> = HashMap::new();
        for u in &self.usage {
            let m = model_totals.entry((&u.provider, &u.model)).or_default();
            m[0] += u.tokens() as f64;
            m[1] += u.input as f64;
            m[2] += u.cache_read as f64;
            m[3] += u.cache_write as f64;
            m[4] += u.output as f64;
            m[5] += u.cost;
            m[6] += u.requests as f64;
        }
        let mut models: Vec<((&str, &str), [f64; 7])> = model_totals.into_iter().collect();
        models.sort_by(|a, b| b.1[5].total_cmp(&a.1[5]).then(b.1[0].total_cmp(&a.1[0])));
        let top_models: Vec<(&str, &str)> = models.iter().take(TOP).map(|(k, _)| *k).collect();
        // Per series and bucket: [tokens, input, cache read, cache write, output, cost].
        let mut by_model: BTreeMap<usize, Vec<[f64; 6]>> = BTreeMap::new();
        for u in &self.usage {
            let i = top_models
                .iter()
                .position(|k| *k == (u.provider.as_str(), u.model.as_str()))
                .unwrap_or(top_models.len());
            let s = &mut by_model.entry(i).or_insert_with(|| vec![[0.0; 6]; n])[u.bucket];
            s[0] += u.tokens() as f64;
            s[1] += u.input as f64;
            s[2] += u.cache_read as f64;
            s[3] += u.cache_write as f64;
            s[4] += u.output as f64;
            s[5] += u.cost;
        }
        let model_series: Vec<Value> = by_model
            .into_iter()
            .map(|(i, data)| {
                let (provider, model) = top_models.get(i).copied().unwrap_or(("other", "other"));
                json!({
                    "name": if provider == "other" { "other models".to_owned() } else { model_name(model) },
                    "provider": provider,
                    "tokens": data.iter().map(|d| d[0]).collect::<Vec<_>>(),
                    "cost": data.iter().map(|d| (d[5] * 100.0).round() / 100.0).collect::<Vec<_>>(),
                    "parts": data.iter().map(|d| [d[1], d[2], d[3], d[4]]).collect::<Vec<_>>(),
                })
            })
            .collect();

        let group = |key: &dyn Fn(&UsageRow) -> String, k: usize| -> Vec<Value> {
            let mut m: HashMap<String, (f64, f64)> = HashMap::new();
            for u in &self.usage {
                let e = m.entry(key(u)).or_default();
                e.0 += u.tokens() as f64;
                e.1 += u.cost;
            }
            let mut v: Vec<(String, (f64, f64))> = m.into_iter().collect();
            v.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1).then(a.0.cmp(&b.0)));
            v.into_iter()
                .take(k)
                .map(|(name, (tokens, cost))| {
                    json!({"name": name, "tokens": tokens, "cost": (cost * 100.0).round() / 100.0})
                })
                .collect()
        };
        let mut threads: HashMap<&str, (&str, &str, f64, f64)> = HashMap::new();
        for u in &self.usage {
            if let Some((id, title)) = &u.thread {
                let e = threads.entry(id).or_insert((title, &u.project, 0.0, 0.0));
                e.2 += u.tokens() as f64;
                e.3 += u.cost;
            }
        }
        let mut threads: Vec<(&str, &str, f64, f64)> = threads.into_values().collect();
        threads.sort_by(|a, b| b.3.total_cmp(&a.3));

        json!({
            "any": !self.is_empty(),
            "totals": totals_json(&t),
            "prev": totals_json(prev),
            "line": line(&t),
            "commits": {"repos": repo_series, "erik": erik, "agents": agents,
                "byAgent": by_agent.into_iter().take(8).map(|(name, n)| json!({"name": name, "n": n})).collect::<Vec<_>>()},
            "lines": {"erikAdd": add_e, "erikDel": del_e, "agentAdd": add_a, "agentDel": del_a},
            "repos": repos.iter().take(10).map(|(name, r)| json!({"name": name, "commits": r.0,
                "added": r.1, "removed": r.2, "erik": r.3, "erikAdded": r.4, "erikRemoved": r.5})).collect::<Vec<_>>(),
            "models": model_series,
            "modelTotals": models.iter().take(12).map(|((p, m), v)| json!({"name": model_name(m), "provider": p,
                "tokens": v[0], "input": v[1], "cacheRead": v[2], "cacheWrite": v[3], "output": v[4],
                "cost": (v[5] * 100.0).round() / 100.0, "requests": v[6]})).collect::<Vec<_>>(),
            "accounts": group(&|u| u.account.clone(), 8),
            "projects": group(&|u| u.project.clone(), 10),
            "threads": threads.into_iter().take(10).map(|(title, project, tokens, cost)| json!({"name": title,
                "project": project, "tokens": tokens, "cost": (cost * 100.0).round() / 100.0})).collect::<Vec<_>>(),
        })
    }
}

fn totals_json(t: &Totals) -> Value {
    json!({
        "commits": t.commits, "added": t.added, "removed": t.removed,
        "erikCommits": t.erik_commits, "erikAdded": t.erik_added, "erikRemoved": t.erik_removed,
        "tokens": t.tokens, "input": t.input, "cacheRead": t.cache_read,
        "cacheWrite": t.cache_write, "output": t.output,
        "cost": (t.cost * 100.0).round() / 100.0, "requests": t.requests,
    })
}

/// 18 400 → "18k", 42 000 000 → "42M", 1 300 000 000 → "1.3B".
pub fn compact(n: u64) -> String {
    let f = n as f64;
    let (v, unit) = if f >= 1e9 {
        (f / 1e9, "B")
    } else if f >= 1e6 {
        (f / 1e6, "M")
    } else if f >= 1e3 {
        (f / 1e3, "k")
    } else {
        return n.to_string();
    };
    if v < 10.0 {
        format!("{:.1}{unit}", v).replace(".0", "")
    } else {
        format!("{:.0}{unit}", v)
    }
}

pub fn usd(x: f64) -> String {
    if x >= 1000.0 {
        format!(
            "${}",
            crate::web::group_thousands(x.round() as u32).replace('\u{2009}', ",")
        )
    } else if x >= 10.0 {
        format!("${:.0}", x)
    } else {
        format!("${:.2}", x)
    }
}

/// "312 commits · +18k/−6k lines · 42M tokens ≈ $310".
pub fn line(t: &Totals) -> String {
    format!(
        "{} commit{} · +{}/−{} lines · {} tokens ≈ {}",
        t.commits,
        if t.commits == 1 { "" } else { "s" },
        compact(t.added),
        compact(t.removed),
        compact(t.tokens),
        usd(t.cost)
    )
}

/// For `time_summary`: totals, by repository, by model, by account.
pub fn digest_json(w: &Work) -> Value {
    let full = w.json(&Totals::default());
    json!({
        "line": full["line"],
        "cost_note": "cost is the equivalent at API list prices; most of it is on subscriptions",
        "totals": full["totals"],
        "by_repository": full["repos"],
        "commits_by_author": full["commits"]["byAgent"],
        "by_model": full["modelTotals"],
        "by_account": full["accounts"],
        "by_project": full["projects"],
        "top_threads": full["threads"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::event::{Commit, UsageHour};

    fn ev(at: Millis, e: Event) -> Envelope {
        Envelope::new("pc", "test", at, at, e)
    }

    #[test]
    fn buckets_split_erik_from_agents_and_price_models() {
        let starts = [0, 3_600_000, 7_200_000];
        let events = vec![
            ev(
                10,
                Event::Commit(Commit {
                    repo: "ErikFrankling/time".into(),
                    lines_added: 10,
                    lines_removed: 2,
                    ..Default::default()
                }),
            ),
            ev(
                3_600_100,
                Event::Commit(Commit {
                    repo: "time".into(),
                    lines_added: 5,
                    agent: Some("claude-opus-5-5".into()),
                    ..Default::default()
                }),
            ),
            ev(
                3_600_000,
                Event::UsageHour(UsageHour {
                    provider: "claude".into(),
                    model: "claude-opus-5-5[1m]".into(),
                    input_tokens: 100,
                    cache_read_tokens: 1000,
                    output_tokens: 10,
                    cost_usd: 1.5,
                    requests: 3,
                    ..Default::default()
                }),
            ),
            ev(
                9_000_000,
                Event::Commit(Commit {
                    repo: "late".into(),
                    ..Default::default()
                }),
            ),
        ];
        let w = Work::between(&events, &starts, 7_200_000);
        let t = w.totals();
        assert_eq!(
            (t.commits, t.erik_commits, t.added, t.erik_added),
            (2, 1, 15, 10)
        );
        assert_eq!(t.tokens, 1110);
        let j = w.json(&Totals::default());
        assert_eq!(j["commits"]["erik"], json!([1, 0, 0]));
        assert_eq!(j["commits"]["agents"], json!([0, 1, 0]));
        assert_eq!(j["repos"][0]["name"], "time");
        assert_eq!(j["models"][0]["name"], "Opus 5.5");
        assert_eq!(j["models"][0]["cost"], json!([0.0, 1.5, 0.0]));
        assert_eq!(j["line"], "2 commits · +15/−2 lines · 1.1k tokens ≈ $1.50");
    }

    #[test]
    fn names_and_numbers_read_short() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("gpt-6-luna"), "gpt-6-luna");
        assert_eq!(compact(18_400), "18k");
        assert_eq!(compact(1_300_000_000), "1.3B");
        assert_eq!(compact(42_000_000), "42M");
        assert_eq!(usd(4100.39), "$4,100");
    }
}
