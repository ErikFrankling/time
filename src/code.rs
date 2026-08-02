//! What got *produced*, as opposed to what got looked at.
//!
//! The minute table answers "where did the day go". It cannot answer "was any
//! of that worth anything", because a screenshot of an editor looks identical
//! whether you shipped a feature or stared at it. Commits, diffs and pull
//! requests are the other half: retrospective, cheap, and impossible to fake by
//! wiggling the mouse.
//!
//! Two sources, deliberately kept apart in the table:
//!
//! * **git** — the local clones. This is the only place line counts exist, and
//!   the only place private and client repositories show up at all: GitHub
//!   reports those as a bare `restrictedContributionsCount` with no repository,
//!   no day and no diff.
//! * **github** — one GraphQL call for pull requests, issues and reviews, which
//!   leave no trace in a local clone.
//!
//! Their commit counts overlap, so nothing sums the two together.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::ServerConfig;

/// One day's output for one repository, from one source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeDay {
    /// Local date, `YYYY-MM-DD`. Days, not minutes: a commit timestamp says
    /// when work was sealed, not when it was done.
    pub day: String,
    pub source: String,
    pub repo: String,
    pub commits: i64,
    pub added: i64,
    pub removed: i64,
    pub files: i64,
    pub prs_opened: i64,
    pub prs_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    pub reviews: i64,
}

pub const SOURCE_GIT: &str = "git";
pub const SOURCE_GITHUB: &str = "github";

/// Accumulator keyed the same way the table is, so collectors can just add.
#[derive(Default)]
struct Tally(HashMap<(String, String, String), CodeDay>);

impl Tally {
    fn at(&mut self, source: &str, day: &str, repo: &str) -> &mut CodeDay {
        self.0
            .entry((source.to_string(), day.to_string(), repo.to_string()))
            .or_insert_with(|| CodeDay {
                day: day.to_string(),
                source: source.to_string(),
                repo: repo.to_string(),
                ..Default::default()
            })
    }

    fn into_rows(self) -> Vec<CodeDay> {
        let mut v: Vec<CodeDay> = self.0.into_values().collect();
        v.sort_by(|a, b| a.day.cmp(&b.day).then(a.repo.cmp(&b.repo)));
        v
    }
}

/// Unix seconds -> local calendar date. The whole table hangs off this, so it
/// lives in one place: a commit at 01:30 belongs to the day you were awake for,
/// not to UTC's idea of it.
fn local_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

fn day_from_rfc3339(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
        .map(local_day)
}

// ---------------------------------------------------------------- local git

/// Find git repositories under `root`, without descending into them.
///
/// A checkout can contain hundreds of thousands of files and a `.git` of its
/// own inside `node_modules`; stopping at the first one found on a path keeps
/// the walk proportional to the number of projects rather than to their size.
fn find_repos(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    if root.join(".git").exists() {
        out.push(root.to_path_buf());
        return;
    }
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() || p.is_symlink() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        // Dependency and build trees are other people's commits.
        if name.starts_with('.') || matches!(name.as_str(), "node_modules" | "target" | "vendor") {
            continue;
        }
        find_repos(&p, depth - 1, out);
    }
}

/// The name a repository is reported under.
///
/// Taken from the origin remote when there is one, so the same project cloned
/// three times -- or snapshotted as `foo-daily-20260727` -- collapses to a
/// single row instead of tripling the day's apparent output.
fn repo_name(repo: &git2::Repository, path: &Path) -> String {
    repo.find_remote("origin")
        .ok()
        .and_then(|r| r.url().ok().map(str::to_string))
        .map(|url| {
            url.trim_end_matches('/')
                .trim_end_matches(".git")
                .rsplit(['/', ':'])
                .next()
                .unwrap_or(&url)
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string())
        })
}

/// Lines in one file, in one commit, past which it stops being writing.
///
/// A checked-in data dump is a real change to the repository and a completely
/// fake day's work: one `site-data/listings.json` refresh here is 650k lines,
/// which is more than every hand-written line of the month put together and
/// flattens every other day in the chart to a single pixel. Nobody authors ten
/// thousand lines of one file in one commit, so above that it is machine
/// output and gets counted as the one file it is rather than as a career.
const GENERATED_FILE_LINES: usize = 10_000;

/// Paths that are someone else's output even when they are small.
const GENERATED: [&str; 9] = [
    "node_modules/",
    "vendor/",
    "/dist/",
    ".lock",
    "-lock.json",
    ".min.js",
    ".min.css",
    ".snap",
    ".pb.go",
];

/// Per-file line counts for a commit, with generated files dropped.
///
/// git2 hands out `Diff::stats()` as one aggregate, which cannot tell a
/// refactor from a vendored dependency drop. Walking the diff's own line
/// callback is the supported way to get the same numbers per file -- still
/// libgit2 doing the diffing, we only decide what to count.
fn commit_lines(diff: &git2::Diff) -> (i64, i64, i64) {
    let mut per_file: HashMap<String, (usize, usize)> = HashMap::new();
    let path_of = |d: &git2::DiffDelta| {
        d.new_file()
            .path()
            .or_else(|| d.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    let _ = diff.foreach(
        &mut |_, _| true,
        None,
        None,
        Some(&mut |d, _, line| {
            let e = per_file.entry(path_of(&d)).or_default();
            match line.origin() {
                '+' => e.0 += 1,
                '-' => e.1 += 1,
                _ => {}
            }
            true
        }),
    );

    let mut added = 0i64;
    let mut removed = 0i64;
    let mut files = 0i64;
    for (path, (a, r)) in per_file {
        let p = path.to_lowercase();
        if GENERATED.iter().any(|g| p.contains(g)) || a + r > GENERATED_FILE_LINES {
            continue;
        }
        added += a as i64;
        removed += r as i64;
        files += 1;
    }
    (added, removed, files)
}

/// Is this commit the user's? Matched on substrings of author name and email.
fn is_mine(sig: &git2::Signature, authors: &[String]) -> bool {
    let name = sig.name().unwrap_or("").to_lowercase();
    let email = sig.email().unwrap_or("").to_lowercase();
    authors
        .iter()
        .any(|a| email.contains(a.as_str()) || name.contains(a.as_str()))
}

/// Walk one repository's history and add every commit of the user's inside the
/// window to the tally.
fn scan_repo(
    path: &Path,
    cfg_authors: &[String],
    since: i64,
    tally: &mut Tally,
    seen: &mut HashSet<git2::Oid>,
) -> Result<()> {
    let repo = git2::Repository::open(path)?;
    let name = repo_name(&repo, path);

    // Falling back to the repository's own identity means work committed under
    // a client address still counts without having to list every address in
    // the config -- which is exactly the work most likely to be forgotten.
    let mut authors: Vec<String> = cfg_authors.to_vec();
    if authors.is_empty() {
        if let Ok(email) = repo.config().and_then(|c| c.get_string("user.email")) {
            authors.push(email.to_lowercase());
        }
    }
    if authors.is_empty() {
        return Ok(());
    }

    let mut walk = repo.revwalk()?;
    // Every local branch, not just HEAD: a day spent on a feature branch is
    // still a day's work, whether or not it has landed on the main line yet.
    walk.push_glob("refs/heads/*")?;
    walk.set_sorting(git2::Sort::TIME)?;

    // TIME ordering is only approximately chronological -- a rebase or a wrong
    // system clock can put an old commit late in the walk -- so the window is
    // enforced per commit and the walk merely gives up after a long stretch of
    // nothing but old commits, rather than stopping at the first one.
    let mut stale = 0;
    for oid in walk.flatten() {
        if stale > 500 {
            break;
        }
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if commit.time().seconds() < since {
            stale += 1;
            continue;
        }
        stale = 0;

        // The same commit reachable from two clones is one piece of work.
        if !seen.insert(oid) {
            continue;
        }
        // A merge introduces no lines of its own, and diffing it against one
        // parent credits the whole other branch to whoever pressed the button.
        if commit.parent_count() > 1 {
            continue;
        }
        if !is_mine(&commit.author(), &authors) {
            continue;
        }

        let day = local_day(commit.time().seconds());
        let diff = commit.tree().and_then(|tree| {
            let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
            repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), None)
        });

        let row = tally.at(SOURCE_GIT, &day, &name);
        row.commits += 1;
        if let Ok(d) = diff {
            let (added, removed, files) = commit_lines(&d);
            row.added += added;
            row.removed += removed;
            row.files += files;
        }
    }
    Ok(())
}

pub fn scan_git(roots: &[String], authors: &[String], since: i64) -> Result<Vec<CodeDay>> {
    let mut repos = Vec::new();
    for root in roots {
        find_repos(&expand(root), 4, &mut repos);
    }
    repos.sort();
    repos.dedup();

    let mut tally = Tally::default();
    let mut seen = HashSet::new();
    for path in &repos {
        if let Err(e) = scan_repo(path, authors, since, &mut tally, &mut seen) {
            eprintln!("git: skipping {}: {e}", path.display());
        }
    }
    eprintln!("git: scanned {} repositories", repos.len());
    Ok(tally.into_rows())
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

// ------------------------------------------------------------------- github

/// Everything GitHub knows, in one request.
///
/// `contributionsCollection` is the same aggregate that draws the contribution
/// graph, so GitHub has already done the per-day rollup server-side. Asking for
/// it costs one point of the 5000/hour GraphQL budget; walking the REST events
/// API for the same answer costs a page per repository and still misses
/// anything older than 90 days.
const QUERY: &str = r#"
query($from: DateTime!, $to: DateTime!, $closed: String!) {
  viewer {
    contributionsCollection(from: $from, to: $to) {
      restrictedContributionsCount
      commitContributionsByRepository(maxRepositories: 100) {
        repository { nameWithOwner }
        contributions(first: 100) { nodes { occurredAt commitCount } }
      }
      pullRequestContributions(first: 100) {
        nodes { occurredAt pullRequest { merged mergedAt repository { nameWithOwner } } }
      }
      issueContributions(first: 100) {
        nodes { occurredAt issue { repository { nameWithOwner } } }
      }
      pullRequestReviewContributions(first: 100) {
        nodes { occurredAt repository { nameWithOwner } }
      }
    }
  }
  search(type: ISSUE, first: 100, query: $closed) {
    nodes { ... on Issue { closedAt repository { nameWithOwner } } }
  }
}
"#;

/// Never from the config file. Prefer an explicit environment variable so the
/// server can take one from a Kubernetes secret, and fall back to asking the
/// `gh` CLI, which already holds an OAuth token in the system keyring on a
/// workstation -- there is no reason to make the user mint a second one.
fn token() -> Option<String> {
    for var in ["TIME_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !t.is_empty()).then_some(t)
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
        .to_string()
}

pub fn fetch_github(user: &str, from: i64, to: i64) -> Result<Vec<CodeDay>> {
    let Some(token) = token() else {
        anyhow::bail!("no GitHub token (set TIME_GITHUB_TOKEN, or run `gh auth login`)");
    };
    let iso = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .unwrap_or_default()
            .to_rfc3339()
    };
    // Closing an issue is not a contribution type, so the one thing the
    // collection cannot answer is asked as a search in the same request.
    let closed = format!(
        "author:{user} is:issue is:closed closed:{}..{}",
        local_day(from),
        local_day(to)
    );

    let body = serde_json::json!({
        "query": QUERY,
        "variables": { "from": iso(from), "to": iso(to), "closed": closed },
    });

    let resp = reqwest::blocking::Client::new()
        .post("https://api.github.com/graphql")
        .bearer_auth(token)
        .header("User-Agent", "time-tracker")
        .json(&body)
        .send()
        .context("calling the GitHub GraphQL API")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().context("parsing the GitHub response")?;
    if !status.is_success() || v.get("errors").is_some() {
        anyhow::bail!("github {status}: {}", v);
    }

    let c = &v["data"]["viewer"]["contributionsCollection"];
    let mut tally = Tally::default();

    for r in nodes(c, &["commitContributionsByRepository"]) {
        let repo = repo_of(&r["repository"]);
        for n in nodes(r, &["contributions", "nodes"]) {
            let Some(day) = n["occurredAt"].as_str().and_then(day_from_rfc3339) else {
                continue;
            };
            tally.at(SOURCE_GITHUB, &day, &repo).commits += n["commitCount"].as_i64().unwrap_or(0);
        }
    }

    for n in nodes(c, &["pullRequestContributions", "nodes"]) {
        let pr = &n["pullRequest"];
        let repo = repo_of(&pr["repository"]);
        if let Some(day) = n["occurredAt"].as_str().and_then(day_from_rfc3339) {
            tally.at(SOURCE_GITHUB, &day, &repo).prs_opened += 1;
        }
        // Merged is credited to the day it merged, not the day it opened: a PR
        // that sat in review for a week is not last Tuesday's output.
        if pr["merged"].as_bool() == Some(true) {
            if let Some(day) = pr["mergedAt"].as_str().and_then(day_from_rfc3339) {
                tally.at(SOURCE_GITHUB, &day, &repo).prs_merged += 1;
            }
        }
    }

    for n in nodes(c, &["issueContributions", "nodes"]) {
        let repo = repo_of(&n["issue"]["repository"]);
        if let Some(day) = n["occurredAt"].as_str().and_then(day_from_rfc3339) {
            tally.at(SOURCE_GITHUB, &day, &repo).issues_opened += 1;
        }
    }

    for n in nodes(c, &["pullRequestReviewContributions", "nodes"]) {
        let repo = repo_of(&n["repository"]);
        if let Some(day) = n["occurredAt"].as_str().and_then(day_from_rfc3339) {
            tally.at(SOURCE_GITHUB, &day, &repo).reviews += 1;
        }
    }

    for n in nodes(&v, &["data", "search", "nodes"]) {
        let repo = repo_of(&n["repository"]);
        if let Some(day) = n["closedAt"].as_str().and_then(day_from_rfc3339) {
            tally.at(SOURCE_GITHUB, &day, &repo).issues_closed += 1;
        }
    }

    if let Some(n) = c["restrictedContributionsCount"]
        .as_i64()
        .filter(|n| *n > 0)
    {
        // Worth saying out loud: this is the gap the local scan exists to fill.
        eprintln!("github: {n} private contributions hidden by GitHub — local git covers those");
    }
    Ok(tally.into_rows())
}

// ------------------------------------------------------------------ collect

/// Gather both sources for the last `days` days.
pub fn collect(cfg: &ServerConfig, days: i64) -> Result<Vec<CodeDay>> {
    let now = chrono::Local::now().timestamp();
    let since = now - days * 86_400;

    let mut rows = scan_git(&cfg.code_roots, &cfg.code_authors, since)?;

    match cfg.github_user.as_deref() {
        Some(user) if !user.is_empty() => match fetch_github(user, since, now) {
            Ok(gh) => rows.extend(gh),
            // A missing token or a rate limit should not throw away a
            // perfectly good local scan.
            Err(e) => eprintln!("github: {e:#}"),
        },
        _ => eprintln!("github: no github_user configured, skipping"),
    }
    Ok(rows)
}

/// Hand a run to the server, the same direction a frame goes: the workstation
/// has the repositories, the server has the database.
pub fn post(agent: &crate::config::AgentConfig, days: &[CodeDay]) -> Result<String> {
    let url = format!("{}/v1/code", agent.server.trim_end_matches('/'));
    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(&url)
        .json(&crate::proto::CodeReport {
            days: days.to_vec(),
        })
        .send()
        .with_context(|| format!("posting to {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    anyhow::ensure!(status.is_success(), "server returned {status}: {body}");
    Ok(body)
}
