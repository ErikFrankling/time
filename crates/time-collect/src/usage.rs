//! Tokens every coding agent used, per hour, session and model, priced at
//! API list prices; and when each agent was working in which repository, so
//! commits can be told apart (Erik's own or an agent's).
//!
//! Sources, read only:
//! - T3 Code's usage scan cache (`usage-scan-cache-v*.json`): T3 already
//!   parses the Claude (both accounts share `~/.claude/projects`), Codex and
//!   Grok transcripts for its usage page, and keeps rows for transcripts that
//!   have since been deleted. Its price table (`usage-model-rates.json`,
//!   LiteLLM) prices them.
//! - OpenCode's database (`opencode*.db`): one row per model call, with the
//!   provider's reported cost.
//! - T3's state database: which T3 thread (and Claude account) ran a session.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use time_core::clock::Millis;
use time_core::event::{Envelope, Event, UsageHour};

use crate::pricing::{RateTable, Tokens};
use crate::stable_id;

pub const SERVICE: &str = "agent-usage";
const HOUR_MS: Millis = 3_600_000;

/// One model call.
#[derive(Debug, Clone)]
pub struct Call {
    pub provider: String,
    pub model: String,
    /// The model the price is looked up under, when not `model`.
    pub rate_model: Option<String>,
    pub session: String,
    pub at: Millis,
    pub tokens: Tokens,
    pub reasoning: u64,
    pub reported_usd: Option<f64>,
    pub speed: u8,
    /// Where it ran: a real path, or Claude's encoded project folder.
    pub cwd: Cwd,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Cwd {
    Path(String),
    Encoded(String),
    Unknown,
}

/// Claude Code's project folder name for a path: everything but letters and
/// digits becomes `-`.
pub fn encode(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Folders an agent can work in, each with the name its work is reported
/// under: the repository's (as commits are), else the folder's.
#[derive(Debug, Default)]
pub struct Places {
    /// (encoded path, name), longest first.
    keys: Vec<(String, String)>,
    home: String,
}

impl Places {
    pub fn new(home: &Path) -> Self {
        Self {
            keys: Vec::new(),
            home: home.display().to_string(),
        }
    }

    pub fn add(&mut self, path: &Path, name: &str) {
        let enc = encode(&path.display().to_string());
        if !self.keys.iter().any(|(k, _)| *k == enc) {
            self.keys.push((enc, name.to_owned()));
            self.keys.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        }
    }

    /// Every repository found, its worktrees, T3's worktree folders and T3's
    /// project roots.
    pub fn build(home: &Path, repos: &[PathBuf], t3_projects: &[(String, PathBuf)]) -> Self {
        let mut p = Self::new(home);
        let mut names_by_dir: HashMap<String, String> = HashMap::new();
        for r in repos {
            let Ok(repo) = git2::Repository::open(r) else {
                continue;
            };
            let name = crate::git::repo_name(&repo, r);
            p.add(r, &name);
            if let Some(d) = r.file_name() {
                names_by_dir.insert(d.to_string_lossy().to_string(), name.clone());
            }
            if let Ok(wts) = repo.worktrees() {
                for w in wts.iter().flatten().flatten() {
                    if let Ok(wt) = repo.find_worktree(w) {
                        p.add(wt.path(), &name);
                    }
                }
            }
        }
        for (title, root) in t3_projects {
            let name = repo_at(root).unwrap_or_else(|| title.clone());
            p.add(root, &name);
        }
        // T3 worktrees: ~/.t3/worktrees/<project>/<branch>. Deleted ones
        // keep their transcripts, so the project folder itself is a place.
        let wt_root = home.join(".t3/worktrees");
        if let Ok(entries) = std::fs::read_dir(&wt_root) {
            for e in entries.flatten() {
                let dir = e.path();
                let found = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .find_map(|c| repo_at(&c.path()));
                let base = e.file_name().to_string_lossy().to_string();
                let name = found
                    .or_else(|| names_by_dir.get(&base).cloned())
                    .unwrap_or(base);
                p.add(&dir, &name);
            }
        }
        p
    }

    fn lookup(&self, enc: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| {
                enc == k
                    || enc
                        .strip_prefix(k.as_str())
                        .is_some_and(|r| r.starts_with('-'))
            })
            .map(|(_, n)| n.as_str())
    }

    /// The project a call ran in; None for the home folder or unknown.
    pub fn name(&self, cwd: &Cwd) -> Option<String> {
        let (enc, last) = match cwd {
            Cwd::Path(p) => (
                encode(p),
                Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string()),
            ),
            Cwd::Encoded(e) => (e.clone(), None),
            Cwd::Unknown => return None,
        };
        if let Some(n) = self.lookup(&enc) {
            return Some(n.to_owned());
        }
        let home = encode(&self.home);
        if enc == home {
            return None;
        }
        last.or_else(|| {
            enc.strip_prefix(&home)
                .map(|r| r.trim_matches('-').to_owned())
                .filter(|r| !r.is_empty())
        })
    }
}

/// The repository name of the clone at `path`, if it is one.
fn repo_at(path: &Path) -> Option<String> {
    let repo = git2::Repository::discover(path).ok()?;
    let dir = repo.workdir().unwrap_or(path).to_path_buf();
    Some(crate::git::repo_name(&repo, &dir))
}

#[derive(Deserialize)]
struct Cache {
    models: Vec<String>,
    sessions: Vec<String>,
    files: HashMap<String, CacheFile>,
}

#[derive(Deserialize)]
struct CacheFile {
    p: String,
    #[serde(default)]
    r: Vec<Row>,
    #[serde(default)]
    t: Vec<Row>,
}

/// One cached call: `[time, model, session, uncached, cache read, cache
/// write, output, reasoning, dedupe key, reported cost, speed, rate model,
/// ...]`, read straight into numbers (the cache is a few hundred thousand
/// rows; a generic JSON tree of it costs hundreds of megabytes).
#[derive(Default)]
struct Row {
    at: f64,
    model: Option<usize>,
    session: Option<usize>,
    n: [f64; 5],
    dedupe: Option<String>,
    reported: Option<f64>,
    speed: u8,
    rate_model: Option<usize>,
    valid: bool,
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Row;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a usage row")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut s: A,
            ) -> std::result::Result<Row, A::Error> {
                type N = Option<f64>;
                let mut r = Row::default();
                let idx = |v: N| v.filter(|x| *x >= 0.0).map(|x| x as usize);
                let Some(Some(at)) = s.next_element::<N>()? else {
                    while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                    return Ok(r);
                };
                r.at = at;
                r.model = idx(s.next_element::<N>()?.flatten());
                r.session = idx(s.next_element::<N>()?.flatten());
                let mut count = 3;
                for i in 0..5 {
                    if let Some(v) = s.next_element::<N>()? {
                        r.n[i] = v.unwrap_or(0.0).max(0.0);
                        count += 1;
                    }
                }
                if let Some(k) = s.next_element::<Option<String>>()? {
                    r.dedupe = k;
                    count += 1;
                }
                if let Some(c) = s.next_element::<N>()? {
                    r.reported = c;
                    count += 1;
                }
                if let Some(sp) = s.next_element::<N>()? {
                    r.speed = sp.unwrap_or(0.0).clamp(0.0, 2.0) as u8;
                    count += 1;
                }
                if let Some(m) = s.next_element::<N>()? {
                    r.rate_model = idx(m);
                }
                while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                r.valid = count >= 11;
                Ok(r)
            }
        }
        d.deserialize_seq(V)
    }
}

/// The newest `usage-scan-cache*.json` in T3's state folder.
fn newest_cache(userdata: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for e in std::fs::read_dir(userdata).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("usage-scan-cache") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(".json") else {
            continue;
        };
        let v = rest
            .strip_prefix("-v")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// The `"cwd":"..."` a transcript records in its first lines.
fn cwd_in(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    // The first lines say where the session ran; never more than 64 KB.
    for line in std::io::BufReader::new(file.take(64 * 1024)).lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        for c in [&v["cwd"], &v["payload"]["cwd"]] {
            if let Some(s) = c.as_str().filter(|s| !s.is_empty()) {
                return Some(s.to_owned());
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a transcript's session ran.
fn cwd_of(provider: &str, file: &Path) -> Cwd {
    if let Some(c) = cwd_in(file) {
        return Cwd::Path(c);
    }
    let parts: Vec<String> = file
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let after = |name: &str| {
        parts
            .iter()
            .position(|p| p == name)
            .and_then(|i| parts.get(i + 1))
            .cloned()
    };
    match provider {
        "claude" => after("projects").map_or(Cwd::Unknown, Cwd::Encoded),
        "grok" => after("sessions").map_or(Cwd::Unknown, |d| Cwd::Path(percent_decode(&d))),
        _ => Cwd::Unknown,
    }
}

/// Every call in T3's usage cache.
pub fn t3_calls(userdata: &Path) -> Result<Vec<Call>> {
    let Some(path) = newest_cache(userdata) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let cache: Cache = serde_json::from_str(&text).with_context(|| path.display().to_string())?;
    // Rows seen in two transcripts (a resumed or forked session) are one
    // call, as on T3's usage page. The oldest transcript keeps it, so a
    // call stays in the same session's hour from run to run.
    let first = |f: &CacheFile| {
        f.r.iter()
            .chain(&f.t)
            .filter(|r| r.valid)
            .map(|r| r.at)
            .fold(f64::INFINITY, f64::min)
    };
    let mut files: Vec<(&String, &CacheFile)> = cache.files.iter().collect();
    files.sort_by(|a, b| first(a.1).total_cmp(&first(b.1)).then(a.0.cmp(b.0)));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (file, f) in files {
        let cwd = cwd_of(&f.p, Path::new(file));
        for row in f.r.iter().chain(&f.t) {
            if !row.valid {
                continue;
            }
            if let Some(k) = &row.dedupe {
                if !seen.insert(k.clone()) {
                    continue;
                }
            }
            let Some(model) = row.model.and_then(|i| cache.models.get(i)) else {
                continue;
            };
            if model == "<synthetic>" {
                continue;
            }
            out.push(Call {
                provider: f.p.clone(),
                model: model.clone(),
                rate_model: row.rate_model.and_then(|i| cache.models.get(i)).cloned(),
                session: row
                    .session
                    .and_then(|i| cache.sessions.get(i))
                    .cloned()
                    .unwrap_or_default(),
                at: row.at as Millis,
                tokens: Tokens {
                    uncached: row.n[0] as u64,
                    cache_read: row.n[1] as u64,
                    cache_write: row.n[2] as u64,
                    output: row.n[3] as u64,
                },
                reasoning: row.n[4] as u64,
                reported_usd: row.reported,
                speed: row.speed,
                cwd: cwd.clone(),
            });
        }
    }
    Ok(out)
}

#[derive(Deserialize, Default)]
struct OcMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    tokens: OcTokens,
    #[serde(default, rename = "modelID")]
    model_id: String,
    #[serde(default)]
    path: OcPath,
}

#[derive(Deserialize, Default)]
struct OcTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default)]
    cache: OcCache,
}

#[derive(Deserialize, Default)]
struct OcCache {
    #[serde(default)]
    read: u64,
    #[serde(default)]
    write: u64,
}

#[derive(Deserialize, Default)]
struct OcPath {
    #[serde(default)]
    cwd: String,
}

/// Every assistant message in OpenCode's databases (`opencode.db` and the
/// stable channel's `opencode-stable.db`).
pub fn opencode_calls(dir: &Path) -> Result<Vec<Call>> {
    let mut out = Vec::new();
    for name in ["opencode.db", "opencode-stable.db"] {
        let db = dir.join(name);
        if !db.exists() {
            continue;
        }
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| db.display().to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let mut stmt = conn.prepare("select session_id, time_created, data from message")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (session, at, data) = row?;
            let Ok(m) = serde_json::from_str::<OcMessage>(&data) else {
                continue;
            };
            let t = &m.tokens;
            if m.role != "assistant" || t.input + t.output + t.cache.read + t.cache.write == 0 {
                continue;
            }
            out.push(Call {
                provider: "opencode".into(),
                model: m.model_id,
                rate_model: None,
                session,
                at,
                tokens: Tokens {
                    uncached: t.input,
                    cache_read: t.cache.read,
                    cache_write: t.cache.write,
                    // OpenCode counts reasoning apart; elsewhere it is output.
                    output: t.output + t.reasoning,
                },
                reasoning: t.reasoning,
                // Zero on a subscription (OpenAI via ChatGPT): list-price it.
                reported_usd: Some(m.cost).filter(|c| *c > 0.0),
                speed: 0,
                cwd: if m.path.cwd.is_empty() {
                    Cwd::Unknown
                } else {
                    Cwd::Path(m.path.cwd)
                },
            });
        }
    }
    Ok(out)
}

/// A T3 thread that ran a provider session.
#[derive(Debug, Clone, Default)]
pub struct T3Session {
    pub thread_id: String,
    pub title: String,
    /// "claude", "claude_2", "codex", ...: the provider instance.
    pub account: String,
    pub project: Option<String>,
}

/// T3's provider instance id as an account name: `claudeAgent` is the main
/// Claude account, `claudeAgent_claude_2` the second.
pub fn account_name(instance: &str) -> String {
    match instance {
        "claudeAgent" => "claude".into(),
        s => s
            .strip_prefix("claudeAgent_")
            .map(str::to_owned)
            .unwrap_or_else(|| s.to_owned()),
    }
}

/// T3's projects: title and workspace root.
pub type T3Projects = Vec<(String, PathBuf)>;

/// Native session id (Claude session, Codex rollout, OpenCode session) → the
/// T3 thread that ran it; and T3's projects (title, workspace root).
pub fn t3_sessions(userdata: &Path) -> Result<(HashMap<String, T3Session>, T3Projects)> {
    let db = userdata.join("statev2.sqlite");
    if !db.exists() {
        return Ok(Default::default());
    }
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| db.display().to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let mut projects = Vec::new();
    let mut titles: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "select project_id, title, workspace_root from projection_projects where deleted_at is null",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, title, root) = row?;
            titles.insert(id, title.clone());
            projects.push((title, PathBuf::from(root)));
        }
    }
    let mut stmt = conn.prepare(
        "select json_extract(p.payload_json, '$.nativeThreadRef.nativeId'), p.thread_id,
                coalesce(p.provider_instance_id, p.provider), coalesce(t.title, ''), t.project_id
         from orchestration_v2_projection_provider_threads p
         left join orchestration_v2_projection_threads t on t.thread_id = p.thread_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (native, thread_id, instance, title, project) = row?;
        let Some(native) = native.filter(|n| !n.is_empty()) else {
            continue;
        };
        out.insert(
            native,
            T3Session {
                thread_id,
                title,
                account: account_name(&instance),
                project: project.and_then(|p| titles.get(&p).cloned()),
            },
        );
    }
    Ok((out, projects))
}

/// The session id T3 knows a call's session by: Codex rollouts end in it.
fn native_id(session: &str) -> &str {
    session.rsplit_once("rollout-").map_or(session, |(_, r)| {
        r.get(r.len().saturating_sub(36)..).unwrap_or(r)
    })
}

/// One event per (agent, session, model, hour), from `since` on.
pub fn hours(
    calls: &[Call],
    rates: &RateTable,
    places: &Places,
    t3: &HashMap<String, T3Session>,
    device: &str,
    since: Millis,
) -> Vec<Envelope> {
    let mut buckets: BTreeMap<(Millis, String, String, String), UsageHour> = BTreeMap::new();
    let mut names: HashMap<&Cwd, Option<String>> = HashMap::new();
    for c in calls.iter().filter(|c| c.at >= since) {
        let hour = c.at - c.at.rem_euclid(HOUR_MS);
        let key = (hour, c.provider.clone(), c.session.clone(), c.model.clone());
        let b = buckets.entry(key).or_insert_with(|| {
            let thread = t3.get(native_id(&c.session));
            let project = names
                .entry(&c.cwd)
                .or_insert_with(|| places.name(&c.cwd))
                .clone()
                .or_else(|| thread.and_then(|t| t.project.clone()));
            UsageHour {
                provider: c.provider.clone(),
                model: c.model.clone(),
                account: thread.map(|t| t.account.clone()).unwrap_or_default(),
                session_id: c.session.clone(),
                project,
                thread_id: thread.map(|t| t.thread_id.clone()),
                thread_title: thread.map(|t| t.title.clone()).filter(|t| !t.is_empty()),
                ..Default::default()
            }
        });
        b.requests += 1;
        b.input_tokens += c.tokens.uncached;
        b.cache_read_tokens += c.tokens.cache_read;
        b.cache_write_tokens += c.tokens.cache_write;
        b.output_tokens += c.tokens.output;
        b.reasoning_tokens += c.reasoning;
        let list = rates.price(
            c.rate_model.as_deref().unwrap_or(&c.model),
            c.speed,
            &c.tokens,
        );
        match (c.reported_usd, list) {
            (Some(r), _) => b.cost_usd += r,
            (None, Some(l)) => b.cost_usd += l,
            (None, None) => b.unpriced += 1,
        }
    }
    buckets
        .into_iter()
        .map(|((hour, provider, session, model), mut u)| {
            // Sub-cent noise would make every resend look like a change.
            u.cost_usd = (u.cost_usd * 1e6).round() / 1e6;
            let mut env = Envelope::new(
                device,
                SERVICE,
                hour,
                hour + HOUR_MS - 1,
                Event::UsageHour(u),
            );
            env.id = stable_id(&format!("usage:{provider}:{session}:{model}:{hour}"));
            env
        })
        .collect()
}

/// When agents were working, per repository: a commit made while one was is
/// the agent's.
#[derive(Debug, Default)]
pub struct Activity {
    by_repo: HashMap<String, Vec<(Millis, String)>>,
    /// By the folder the agent ran in (encoded as Claude does): an agent in
    /// `~/projects/work` commits in the repositories under it.
    by_dir: Vec<(String, Vec<(Millis, String)>)>,
}

/// A commit this long after an agent's last call is still its own (hooks
/// and tests run between the call that asked for it and the commit).
const BEFORE_MS: Millis = 5 * 60_000;
/// Or this long before its next call (the one that reads the result).
const AFTER_MS: Millis = 2 * 60_000;

/// The call in `v` (sorted by time) nearest to `at` within the window.
fn nearest(v: &[(Millis, String)], at: Millis) -> Option<(Millis, &str)> {
    let i = v.partition_point(|(t, _)| *t < at - BEFORE_MS);
    v[i..]
        .iter()
        .take_while(|(t, _)| *t <= at + AFTER_MS)
        .min_by_key(|(t, _)| (t - at).abs())
        .map(|(t, m)| ((t - at).abs(), m.as_str()))
}

impl Activity {
    pub fn build(calls: &[Call], places: &Places) -> Self {
        let mut names: HashMap<&Cwd, Option<String>> = HashMap::new();
        let mut by_repo: HashMap<String, Vec<(Millis, String)>> = HashMap::new();
        let mut by_dir: HashMap<String, Vec<(Millis, String)>> = HashMap::new();
        let home = encode(&places.home);
        for c in calls {
            let dir = match &c.cwd {
                Cwd::Path(p) => Some(encode(p)),
                Cwd::Encoded(e) => Some(e.clone()),
                Cwd::Unknown => None,
            };
            // Home is where everything is under: no evidence for any one
            // repository.
            if let Some(d) = dir.filter(|d| *d != home) {
                by_dir.entry(d).or_default().push((c.at, c.model.clone()));
            }
            if let Some(name) = names.entry(&c.cwd).or_insert_with(|| places.name(&c.cwd)) {
                by_repo
                    .entry(name.clone())
                    .or_default()
                    .push((c.at, c.model.clone()));
            }
        }
        for v in by_repo.values_mut().chain(by_dir.values_mut()) {
            v.sort_unstable();
        }
        Self {
            by_repo,
            by_dir: by_dir.into_iter().collect(),
        }
    }

    /// The model working in `repo` around `at` (or in a folder holding the
    /// clone at `path`), the nearest call first.
    pub fn agent_at(&self, repo: &str, path: Option<&Path>, at: Millis) -> Option<&str> {
        if let Some((_, m)) = self.by_repo.get(repo).and_then(|v| nearest(v, at)) {
            return Some(m);
        }
        let enc = encode(&path?.display().to_string());
        self.by_dir
            .iter()
            .filter(|(d, _)| {
                enc.strip_prefix(d.as_str())
                    .is_some_and(|r| r.starts_with('-'))
            })
            .filter_map(|(_, v)| nearest(v, at))
            .min_by_key(|(d, _)| *d)
            .map(|(_, m)| m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_resolve_paths_and_claude_folders() {
        let mut p = Places::new(Path::new("/home/e"));
        p.add(Path::new("/home/e/projects/personal/time"), "time");
        p.add(Path::new("/home/e/.t3/worktrees/time"), "time");
        p.add(Path::new("/home/e/.dotfiles"), "dotfiles");
        assert_eq!(
            p.name(&Cwd::Path("/home/e/projects/personal/time/crates".into()))
                .as_deref(),
            Some("time")
        );
        assert_eq!(
            p.name(&Cwd::Encoded(
                "-home-e--t3-worktrees-time-old-branch".into()
            ))
            .as_deref(),
            Some("time")
        );
        assert_eq!(
            p.name(&Cwd::Encoded("-home-e--dotfiles".into())).as_deref(),
            Some("dotfiles")
        );
        assert_eq!(p.name(&Cwd::Path("/home/e".into())), None);
        assert_eq!(
            p.name(&Cwd::Path("/tmp/grokwork".into())).as_deref(),
            Some("grokwork")
        );
        assert_eq!(
            p.name(&Cwd::Encoded("-home-e-projects-kth".into()))
                .as_deref(),
            Some("projects-kth")
        );
    }

    fn call(at: Millis, model: &str, cwd: &str, out: u64) -> Call {
        Call {
            provider: "claude".into(),
            model: model.into(),
            rate_model: None,
            session: "s1".into(),
            at,
            tokens: Tokens {
                uncached: 10,
                cache_read: 100,
                cache_write: 0,
                output: out,
            },
            reasoning: 0,
            reported_usd: None,
            speed: 0,
            cwd: Cwd::Path(cwd.into()),
        }
    }

    #[test]
    fn hours_bucket_and_activity_finds_the_agent() {
        let mut p = Places::new(Path::new("/home/e"));
        p.add(Path::new("/home/e/time"), "time");
        let h = 1_800_000_000_000 - 1_800_000_000_000 % HOUR_MS;
        let calls = vec![
            call(h + 1_000, "m", "/home/e/time", 5),
            call(h + 2_000, "m", "/home/e/time", 7),
            call(h + HOUR_MS + 5, "m", "/home/e/time", 1),
        ];
        let ev = hours(&calls, &RateTable::default(), &p, &HashMap::new(), "pc", 0);
        assert_eq!(ev.len(), 2);
        let Event::UsageHour(u) = &ev[0].event else {
            unreachable!()
        };
        assert_eq!((u.requests, u.output_tokens, u.unpriced), (2, 12, 2));
        assert_eq!(u.project.as_deref(), Some("time"));
        assert_eq!(ev[0].start_ms, h);
        let again = hours(&calls, &RateTable::default(), &p, &HashMap::new(), "pc", 0);
        assert_eq!(ev[0].id, again[0].id, "ids are stable");

        let a = Activity::build(&calls, &p);
        assert_eq!(a.agent_at("time", None, h + 60_000), Some("m"));
        assert_eq!(a.agent_at("time", None, h + 30 * 60_000), None);
        assert_eq!(a.agent_at("other", None, h), None);
        // An agent in a folder above the clone.
        let up = vec![call(h + 1_000, "m2", "/home/e/projects/work", 1)];
        let b = Activity::build(&up, &p);
        assert_eq!(
            b.agent_at(
                "naiaclaw",
                Some(Path::new("/home/e/projects/work/naiaclaw")),
                h
            ),
            Some("m2")
        );
        assert_eq!(
            b.agent_at("naiaclaw", Some(Path::new("/home/e/projects/workshop")), h),
            None
        );
    }

    #[test]
    fn cache_rows_decode_without_a_json_tree() {
        let f: CacheFile = serde_json::from_str(
            r#"{"p":"claude","r":[[1792000000000.5,1,0,2,11506,8484,548,0,"msg:req",null,1,null],[1,0]],"t":[[5,0,0,1,1,1,1,0,null,0.25,0,2,"turn"]]}"#,
        )
        .unwrap();
        let r = &f.r[0];
        assert!(r.valid);
        assert_eq!((r.model, r.session, r.speed), (Some(1), Some(0), 1));
        assert_eq!(r.n, [2.0, 11506.0, 8484.0, 548.0, 0.0]);
        assert_eq!(r.dedupe.as_deref(), Some("msg:req"));
        assert!(!f.r[1].valid, "short rows are skipped");
        assert_eq!((f.t[0].reported, f.t[0].rate_model), (Some(0.25), Some(2)));
    }

    #[test]
    fn codex_rollouts_map_to_their_session() {
        assert_eq!(
            native_id("rollout-2026-10-26T06-30-14-01a176b0-170b-7720-b4aa-c0a1beb5d3b0"),
            "01a176b0-170b-7720-b4aa-c0a1beb5d3b0"
        );
        assert_eq!(
            native_id("bd48e3df-34c4-4656-978d-418cf8ae3e9e"),
            "bd48e3df-34c4-4656-978d-418cf8ae3e9e"
        );
        assert_eq!(percent_decode("%2Ftmp%2Fgrokwork"), "/tmp/grokwork");
    }
}
