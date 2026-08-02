//! How much of the day was the agents, as opposed to you.
//!
//! The minute table cannot answer this. A screenshot of an editor looks the
//! same whether you wrote the line or watched one appear, and on the PC that
//! ambiguity is not hypothetical -- the device note says outright that
//! computer-use sessions drive the screen with nobody present. Screen activity
//! is therefore evidence that *something* happened, never evidence it was you.
//! These rows are the other half of that: what the agents did, counted from
//! their own records rather than inferred from pixels.
//!
//! Three tools, three sources, deliberately kept apart in the table because
//! their token counts are not comparable and must never be summed blindly:
//!
//! * **opencode** -- `opencode.db`, read over SQL. The `session` table already
//!   carries `tokens_input`/`tokens_output`/`cost` as columns that opencode
//!   maintains itself, so there is nothing to parse and nothing to get wrong.
//!   This is the source to trust.
//! * **claude** -- the JSONL transcripts under `~/.claude/projects`. No API, no
//!   database, no official export; the file format is the only way in.
//! * **codex** -- the rollout JSONL under `~/.codex/sessions`, for the same
//!   reason.
//!
//! # This module WILL break on upstream updates
//!
//! Two of the three sources are private on-disk formats with no stability
//! promise, and that is not a theoretical worry. `~/.claude/history.jsonl` --
//! the obvious place to count prompts, and what earlier usage tools read --
//! silently stopped being written on 2026-07-28 partway through this data set.
//! Nothing errored; the numbers would simply have flatlined.
//!
//! So: every parser here is best-effort and per-tool. A tool whose format moved
//! yields zero rows and logs, it never fails the run or corrupts its
//! neighbours. When a number looks wrong, suspect this file first.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::config::ServerConfig;
use crate::proto::AgentReport;

pub const TOOL_CLAUDE: &str = "claude";
pub const TOOL_OPENCODE: &str = "opencode";
pub const TOOL_CODEX: &str = "codex";

/// One tool's day in one project, on one machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDay {
    /// Local date, `YYYY-MM-DD`.
    pub day: String,
    pub device: String,
    pub tool: String,
    /// Repository directory the session ran in, shortened for display.
    pub project: String,

    pub sessions: i64,
    pub prompts: i64,

    /// Prompt length in characters. The percentiles matter more than the mean:
    /// one 57k-character paste drags an average far away from anything typed.
    pub prompt_chars: i64,
    pub prompt_p50: i64,
    pub prompt_p90: i64,

    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read: i64,
    pub cache_write: i64,

    /// Minutes in which at least one session of this tool did something.
    pub active_minutes: i64,
    /// Most sessions of this tool active in any single minute that day.
    pub peak_parallel: i64,
}

/// One minute in which agents were doing something, so the UI can lay this
/// over the tracked-time timeline instead of only totalling it.
///
/// Stored per tool rather than merged: "codex was running" and "claude was
/// running" are different claims about the same minute, and the interesting
/// question is usually how often both were true at once.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMinute {
    pub ts: i64,
    pub device: String,
    pub tool: String,
    /// Distinct sessions of this tool active in this minute.
    pub sessions: i64,
}

/// Everything a single session contributed, before it is folded into a day.
#[derive(Default)]
struct Raw {
    /// (day, project) -> accumulating totals.
    days: HashMap<(String, String), Acc>,
    /// (minute_ts) -> distinct session ids active then.
    minutes: BTreeMap<i64, BTreeSet<String>>,
}

#[derive(Default)]
struct Acc {
    sessions: BTreeSet<String>,
    prompt_lens: Vec<i64>,
    tokens_in: i64,
    tokens_out: i64,
    cache_read: i64,
    cache_write: i64,
    /// minute -> sessions active, scoped to this (day, project).
    minutes: BTreeMap<i64, BTreeSet<String>>,
}

impl Raw {
    fn acc(&mut self, day: &str, project: &str) -> &mut Acc {
        self.days
            .entry((day.to_string(), project.to_string()))
            .or_default()
    }

    /// Note that `session` did something at `ts`, both for the day's
    /// concurrency and for the cross-project minute overlay.
    fn active(&mut self, day: &str, project: &str, session: &str, ts: i64) {
        let minute = ts - ts.rem_euclid(60);
        self.acc(day, project)
            .minutes
            .entry(minute)
            .or_default()
            .insert(session.to_string());
        self.minutes
            .entry(minute)
            .or_default()
            .insert(session.to_string());
    }

    fn finish(self, device: &str, tool: &str) -> (Vec<AgentDay>, Vec<AgentMinute>) {
        let days = self
            .days
            .into_iter()
            .map(|((day, project), a)| {
                let mut lens = a.prompt_lens;
                lens.sort_unstable();
                AgentDay {
                    day,
                    device: device.to_string(),
                    tool: tool.to_string(),
                    project,
                    sessions: a.sessions.len() as i64,
                    prompts: lens.len() as i64,
                    prompt_chars: lens.iter().sum(),
                    prompt_p50: percentile(&lens, 0.50),
                    prompt_p90: percentile(&lens, 0.90),
                    tokens_in: a.tokens_in,
                    tokens_out: a.tokens_out,
                    cache_read: a.cache_read,
                    cache_write: a.cache_write,
                    active_minutes: a.minutes.len() as i64,
                    peak_parallel: a.minutes.values().map(|s| s.len() as i64).max().unwrap_or(0),
                }
            })
            .collect();

        let minutes = self
            .minutes
            .into_iter()
            .map(|(ts, s)| AgentMinute {
                ts,
                device: device.to_string(),
                tool: tool.to_string(),
                sessions: s.len() as i64,
            })
            .collect();

        (days, minutes)
    }
}

fn percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME not set")?,
    ))
}

/// Which project a session was working on, as a name worth charting.
///
/// The working directory is not it. Sessions get started three levels down a
/// tree, and taking the last path component yields `src`, `png` and `tokens`
/// as though they were projects. The repository root is the unit that actually
/// means something, so walk up to the nearest `.git`.
///
/// Worktrees are cut off first: `.../husk/.claude/worktrees/x-a4704e` carries
/// its own `.git`, and left alone every branch becomes a separate project and
/// buries the real one under a dozen near-duplicates.
fn project_for(dir: &str, cache: &mut HashMap<String, String>) -> String {
    if let Some(hit) = cache.get(dir) {
        return hit.clone();
    }
    let base = dir.split("/.claude/worktrees/").next().unwrap_or(dir);
    let mut cur = Path::new(base);
    let name = loop {
        if cur.join(".git").exists() {
            break cur.file_name().and_then(|n| n.to_str()).map(str::to_string);
        }
        match cur.parent() {
            // Stop at $HOME: above it every path shares one ancestor and
            // everything would collapse into a single bogus project.
            Some(p) if p.parent().is_some() => cur = p,
            _ => break None,
        }
    };
    // A directory that is not in a repository at all still gets a label, so
    // scratch work shows up rather than silently vanishing.
    let name = name.unwrap_or_else(|| {
        base.trim_end_matches('/')
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string()
    });
    cache.insert(dir.to_string(), name.clone());
    name
}

/// Unix seconds and the local date from an RFC3339 stamp.
fn when(ts: &str) -> Option<(i64, String)> {
    let t = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let local = t.with_timezone(&chrono::Local);
    Some((t.timestamp(), local.format("%Y-%m-%d").to_string()))
}

fn cutoff(days: i64) -> i64 {
    chrono::Local::now().timestamp() - days.max(1) * 86_400
}

/// Files whose contents could possibly fall inside the window. Transcripts are
/// append-only, so an untouched file cannot contain a recent line -- and at
/// 1.2 GB of history, reading everything to discover that would make a nightly
/// job take minutes instead of seconds.
fn recent_files(root: &Path, since: i64, matches: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if !matches(&p) {
                continue;
            }
            let fresh = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|d| d.as_secs() as i64 >= since);
            if fresh {
                out.push(p);
            }
        }
    }
    out
}

/// Read a JSONL file, handing back every line that parsed.
///
/// Malformed lines are skipped rather than fatal. Transcripts are written live
/// and the tail of an open file is regularly a half-flushed line; treating that
/// as an error would drop the whole session, which is exactly the file you most
/// want to count.
fn jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    let Ok(f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<T>(&l).ok())
        .collect()
}

// ---------------------------------------------------------------- claude code

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcLine {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    message: Option<CcMessage>,
}

#[derive(Deserialize)]
struct CcMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<CcUsage>,
}

#[derive(Deserialize, Clone)]
struct CcUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
}

/// Claude Code writes one `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` per
/// session, plus `<uuid>/subagents/agent-*.jsonl` for every subagent it spawns.
/// Subagent files repeat the parent `sessionId`, which is what we want: a
/// session that fans out to twenty subagents is still one thing you started.
fn claude(device: &str, days: i64) -> Result<(Vec<AgentDay>, Vec<AgentMinute>)> {
    let root = home()?.join(".claude/projects");
    if !root.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }
    let since = cutoff(days);
    let mut raw = Raw::default();
    let mut projects: HashMap<String, String> = HashMap::new();

    // The same assistant message is appended once per streaming update, so its
    // usage block reappears on many lines. Summing every line inflates output
    // tokens ~4x (46.5M against a true 11.5M over nine days here), so each
    // message id may only be counted once.
    //
    // Which occurrence, though, is not a detail: the repeats are *snapshots of
    // a response still being written*, and their counts grow. Keeping the first
    // undercounts by 3x just as badly as keeping all of them overcounts. Only
    // the last line for an id has the finished figure -- hence a map that
    // overwrites, folded in after every file, rather than a seen-set that
    // skips. The map spans files because a resumed session re-emits earlier
    // messages and subagent output lands in its own transcript entirely.
    let mut final_usage: HashMap<String, (String, String, CcUsage)> = HashMap::new();

    for path in recent_files(&root, since, &|p| {
        p.extension().is_some_and(|e| e == "jsonl")
    }) {
        for line in jsonl::<CcLine>(&path) {
            let (Some(ts), Some(session)) = (line.timestamp.as_deref(), line.session_id.as_deref())
            else {
                continue;
            };
            let Some((secs, day)) = when(ts) else { continue };
            if secs < since {
                continue;
            }
            let project = project_for(line.cwd.as_deref().unwrap_or(""), &mut projects);

            match line.r#type.as_str() {
                "assistant" => {
                    let Some(msg) = &line.message else { continue };
                    let Some(u) = &msg.usage else { continue };
                    // A session is "active" in a minute if it produced
                    // something then. Session open/close spans are useless for
                    // this: a resumed session stays open for days.
                    raw.active(&day, &project, session, secs);
                    raw.acc(&day, &project).sessions.insert(session.to_string());

                    let id = msg.id.clone().unwrap_or_else(|| format!("{path:?}:{ts}"));
                    final_usage.insert(id, (day, project, u.clone()));
                }
                // Only top-level turns are prompts *you* wrote. Sidechain turns
                // are the harness talking to a subagent, and meta turns are
                // injected context; counting either would say you type far more
                // often, and far more verbosely, than you do.
                "user" if !line.is_sidechain && !line.is_meta => {
                    let Some(msg) = &line.message else { continue };
                    let Some(text) = prompt_text(msg.content.as_ref()) else {
                        continue;
                    };
                    raw.acc(&day, &project).prompt_lens.push(text as i64);
                }
                _ => {}
            }
        }
    }

    for (_, (day, project, u)) in final_usage {
        let a = raw.acc(&day, &project);
        a.tokens_in += u.input_tokens;
        a.tokens_out += u.output_tokens;
        a.cache_read += u.cache_read_input_tokens;
        a.cache_write += u.cache_creation_input_tokens;
    }
    Ok(raw.finish(device, TOOL_CLAUDE))
}

/// Length of the human-written part of a user turn, or None when the turn is
/// not one.
///
/// A user record is also how tool results come back, and those arrive as a
/// content array of `tool_result` blocks. Those are the machine answering
/// itself, sometimes with a megabyte of file content, and letting them through
/// would drown the real prompts.
fn prompt_text(content: Option<&serde_json::Value>) -> Option<usize> {
    let text = match content? {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let joined: String = parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            joined
        }
        _ => return None,
    };
    let t = text.trim();
    // Slash commands and injected reminders are expanded inline into the
    // transcript as tagged blocks. They are not something you typed.
    if t.is_empty() || t.starts_with("<command-name>") || t.starts_with("<local-command")
        || t.starts_with("<system-reminder>") || t.starts_with("<bash-stdout>")
    {
        return None;
    }
    Some(t.chars().count())
}

// ------------------------------------------------------------------- opencode

/// opencode keeps a real database and maintains the token columns itself, so
/// this is a query rather than a parser -- the one source here that upstream is
/// unlikely to break silently.
///
/// Opened read-only, and a failure is shrugged off: opencode may well be
/// running, and its database being momentarily locked is not a reason to lose
/// the other two tools' numbers.
fn opencode(device: &str, days: i64) -> Result<(Vec<AgentDay>, Vec<AgentMinute>)> {
    let path = home()?.join(".local/share/opencode/opencode.db");
    if !path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    let since_ms = cutoff(days) * 1000;
    let mut raw = Raw::default();
    let mut projects: HashMap<String, String> = HashMap::new();

    // Timestamps are milliseconds here, unlike every other source.
    let mut stmt = conn.prepare(
        "SELECT s.id, s.directory, s.time_created, s.time_updated,
                s.tokens_input, s.tokens_output, s.tokens_cache_read, s.tokens_cache_write
           FROM session s
          WHERE s.time_updated >= ?1",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;

    let mut sessions: HashMap<String, (String, String)> = HashMap::new();
    for row in rows.flatten() {
        let (id, dir, created, _updated, tin, tout, cr, cw) = row;
        let day = chrono::DateTime::from_timestamp(created / 1000, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default();
        let project = project_for(&dir, &mut projects);
        sessions.insert(id.clone(), (day.clone(), project.clone()));

        let a = raw.acc(&day, &project);
        a.sessions.insert(id);
        a.tokens_in += tin;
        a.tokens_out += tout;
        a.cache_read += cr;
        a.cache_write += cw;
    }

    // Message timestamps give both the activity minutes and the prompt sizes.
    // The prompt text lives in `part`, one text part per message.
    let mut stmt = conn.prepare(
        "SELECT m.session_id, m.time_created,
                json_extract(m.data, '$.role') AS role,
                (SELECT sum(length(json_extract(p.data, '$.text')))
                   FROM part p
                  WHERE p.message_id = m.id
                    AND json_extract(p.data, '$.type') = 'text') AS chars
           FROM message m
          WHERE m.time_created >= ?1",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;

    for (session, created, role, chars) in rows.flatten() {
        let Some((day, project)) = sessions.get(&session).cloned() else {
            continue;
        };
        let secs = created / 1000;
        if role == "assistant" {
            raw.active(&day, &project, &session, secs);
        } else if role == "user" {
            if let Some(c) = chars.filter(|c| *c > 0) {
                raw.acc(&day, &project).prompt_lens.push(c);
            }
        }
    }

    Ok(raw.finish(device, TOOL_OPENCODE))
}

// ---------------------------------------------------------------------- codex

#[derive(Deserialize)]
struct CxLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Codex writes `~/.codex/sessions/YYYY/MM/DD/rollout-<stamp>-<uuid>.jsonl`.
///
/// Tokens come from `token_count` events, and only `last_token_usage` is a
/// per-turn delta -- `total_token_usage` is a running total for the session and
/// summing it would multiply the real figure by the number of turns.
fn codex(device: &str, days: i64) -> Result<(Vec<AgentDay>, Vec<AgentMinute>)> {
    let root = home()?.join(".codex/sessions");
    if !root.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }
    let since = cutoff(days);
    let mut raw = Raw::default();
    let mut projects: HashMap<String, String> = HashMap::new();

    for path in recent_files(&root, since, &|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
    }) {
        // The session id and cwd are in the first record; everything after it
        // is anonymous, so a rollout whose header did not parse is unusable.
        let lines = jsonl::<CxLine>(&path);
        let Some(meta) = lines
            .iter()
            .find(|l| l.r#type == "session_meta")
            .and_then(|l| l.payload.as_ref())
        else {
            continue;
        };
        let session = meta
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if session.is_empty() {
            continue;
        }
        let project = project_for(meta.get("cwd").and_then(|v| v.as_str()).unwrap_or(""), &mut projects);

        // Codex spawns child threads whose instructions are recorded as
        // `user_message` exactly like something you typed. Their work is real
        // and counts, but their prompts are the orchestrator talking to itself
        // -- and they are long, so leaving them in would roughly double the
        // apparent length of a prompt. 115 of 1040 recent rollouts are these.
        let typed_by_human = meta.get("thread_source").and_then(|v| v.as_str()) != Some("subagent");

        for line in &lines {
            let Some((secs, day)) = line.timestamp.as_deref().and_then(when) else {
                continue;
            };
            if secs < since {
                continue;
            }
            let Some(p) = &line.payload else { continue };
            let kind = p.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match kind {
                "token_count" => {
                    raw.active(&day, &project, &session, secs);
                    raw.acc(&day, &project).sessions.insert(session.clone());
                    let Some(u) = p.pointer("/info/last_token_usage") else {
                        continue;
                    };
                    let n = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                    let a = raw.acc(&day, &project);
                    // Codex reports cached input inside input_tokens, so the
                    // cached part is subtracted out to keep `tokens_in`
                    // meaning the same thing it does for the other two tools.
                    let cached = n("cached_input_tokens");
                    a.tokens_in += (n("input_tokens") - cached).max(0);
                    a.cache_read += cached;
                    a.tokens_out += n("output_tokens");
                }
                "user_message" if typed_by_human => {
                    if let Some(t) = p.get("message").and_then(|v| v.as_str()) {
                        let t = t.trim();
                        if !t.is_empty() {
                            raw.acc(&day, &project)
                                .prompt_lens
                                .push(t.chars().count() as i64);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(raw.finish(device, TOOL_CODEX))
}

// ------------------------------------------------------------------ collector

/// Read every agent tool present on this machine.
///
/// One tool failing is reported and skipped. These are three independent
/// best-effort readers of formats nobody promised us, and losing codex is no
/// reason to lose opencode. `cfg.agent_tools` is the escape hatch for the case
/// this module's header warns about: a parser that has gone quietly wrong can
/// be taken out of the run without waiting for a rebuild.
pub fn collect(cfg: &ServerConfig, device: &str, days: i64) -> Result<AgentReport> {
    let mut out = AgentReport::default();
    type Reader = fn(&str, i64) -> Result<(Vec<AgentDay>, Vec<AgentMinute>)>;
    for (name, result) in [
        (TOOL_CLAUDE, claude as Reader),
        (TOOL_OPENCODE, opencode),
        (TOOL_CODEX, codex),
    ]
    .into_iter()
    .filter(|(name, _)| cfg.agent_tools.iter().any(|t| t == name))
    .map(|(name, read)| (name, read(device, days)))
    {
        match result {
            Ok((days, minutes)) => {
                out.days.extend(days);
                out.minutes.extend(minutes);
            }
            Err(e) => eprintln!("{name}: {e:#}"),
        }
    }
    out.days.sort_by(|a, b| {
        a.day
            .cmp(&b.day)
            .then(a.tool.cmp(&b.tool))
            .then(a.project.cmp(&b.project))
    });
    out.minutes.sort_by_key(|m| (m.ts, m.tool.clone()));
    Ok(out)
}

/// Ship a collection to the server.
///
/// Deliberately not part of `Frame`. A frame is one minute observed live and
/// must stay small enough to send every sixty seconds; this is a retrospective
/// re-read of the last few days that only changes when a session ends. Folding
/// it into the per-minute path would mean re-sending days of history 1,440
/// times a day to carry a few rows that moved.
///
/// Rows are keyed by (day, device, tool, project) so re-sending is an upsert,
/// not a duplicate -- which is what makes a nightly job safe to run twice.
pub fn post(agent: &crate::config::AgentConfig, report: &AgentReport) -> Result<String> {
    let url = format!("{}/v1/agents", agent.server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let resp = client
        .post(&url)
        .json(report)
        .send()
        .with_context(|| format!("posting to {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "server returned {status}: {}",
        body.chars().take(300).collect::<String>()
    );
    Ok(body)
}
