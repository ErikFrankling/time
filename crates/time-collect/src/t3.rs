//! T3 Code, read from its own state on this machine: prompts (typed or
//! dictated, Erik's or an agent's), tokens per provider turn, dictations, read-aloud
//! plays, voice-mode spans and thread focus.
//!
//! Read only, never written: `statev2.sqlite` is opened read-only (T3 keeps
//! writing it; WAL lets both happen) and the dictation delivery files are
//! only listed and parsed. No message text leaves this function: prompts and
//! dictations become word counts.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use time_core::clock::Millis;
use time_core::event::{
    DictationFinished, Envelope, Event, FocusSpan, InputMode, Origin, PromptSent, ReadAloudPlayed,
    TokensUsed, VoiceModeSpan,
};

use crate::stable_id;

pub const SERVICE: &str = "t3-code";

/// Everything from `since` on, as events. `device` is this machine; prompts
/// sent from the T3 phone app are attributed to `phone`.
pub fn collect(userdata: &Path, device: &str, since: Millis) -> Result<Vec<Envelope>> {
    let dictations = deliveries(&userdata.join("dictation-deliveries"), since)?;
    let db = userdata.join("statev2.sqlite");
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| db.display().to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let since_iso = iso(since);

    let projects = projects(&conn)?;
    let threads = threads(&conn, &projects)?;
    let runs = runs(&conn, &since_iso)?;
    let mut out = Vec::new();
    let mut origins: HashMap<String, Origin> = HashMap::new();

    // Prompts.
    let mut stmt = conn.prepare(
        "select message_id, thread_id, created_at, payload_json
         from orchestration_v2_projection_messages
         where role = 'user' and created_at >= ?1",
    )?;
    let rows = stmt.query_map([&since_iso], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let by_message: HashMap<&str, &Run> = runs
        .values()
        .filter_map(|r| r.user_message.as_deref().map(|m| (m, r)))
        .collect();
    for row in rows {
        let (id, thread, created, payload) = row?;
        let Some(start) = parse_ms(&created) else {
            continue;
        };
        let p: MessagePayload = serde_json::from_str(&payload).unwrap_or_default();
        let origin = p.origin();
        origins.insert(id.clone(), origin);
        let t = threads.get(&thread);
        let run = by_message.get(id.as_str());
        let words = if origin == Origin::Human {
            word_count(&p.text)
        } else {
            0
        };
        let dictated = dictations.get(&id);
        let input_mode = match dictated {
            Some(d) if d.used_draft => InputMode::Mixed,
            Some(_) => InputMode::Dictated,
            None => InputMode::Typed,
        };
        let (words_typed, words_dictated) = match input_mode {
            InputMode::Typed => (words, 0),
            InputMode::Dictated => (0, words),
            // The draft's share is not recorded: count the dictation's own
            // words as spoken and the rest as typed.
            InputMode::Mixed => {
                let spoken = dictated.map_or(0, |d| d.words).min(words);
                (words - spoken, spoken)
            }
        };
        let client = p.creation_source.clone().unwrap_or_default();
        let dev = if client == "mobile" { "phone" } else { device };
        let mut env = Envelope::new(
            dev,
            SERVICE,
            start,
            start,
            Event::PromptSent(PromptSent {
                thread_id: thread.clone(),
                project: t.and_then(|t| t.project.clone()),
                provider: run
                    .map(|r| r.account.clone())
                    .or_else(|| t.map(|t| t.account.clone()))
                    .unwrap_or_default(),
                model: run
                    .map(|r| r.model.clone())
                    .or_else(|| t.map(|t| t.model.clone()))
                    .unwrap_or_default(),
                client,
                words_typed,
                words_dictated,
                input_mode,
                origin,
            }),
        );
        env.id = stable_id(&format!("t3:prompt:{id}"));
        out.push(env);
    }

    // Tokens, one event per finished provider turn.
    let mut stmt = conn.prepare(
        "select provider_turn_id, thread_id, started_at, completed_at, payload_json
         from orchestration_v2_projection_provider_turns
         where started_at >= ?1 and status in ('completed', 'failed', 'interrupted', 'cancelled')",
    )?;
    let rows = stmt.query_map([&since_iso], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, thread, started, completed, payload) = row?;
        let p: TurnPayload = serde_json::from_str(&payload).unwrap_or_default();
        let Some(u) = p.turn_token_usage else {
            continue;
        };
        if u.input_tokens + u.output_tokens == 0 {
            continue;
        }
        let Some(start) = parse_ms(&started) else {
            continue;
        };
        let end = completed.as_deref().and_then(parse_ms).unwrap_or(start);
        let run = p
            .run_attempt_id
            .as_deref()
            .and_then(run_of_attempt)
            .and_then(|r| runs.get(&r));
        let t = threads.get(&thread);
        let origin = run
            .and_then(|r| r.user_message.as_ref())
            .and_then(|m| origins.get(m).copied().or_else(|| message_origin(&conn, m)))
            .unwrap_or(Origin::Agent);
        let mut env = Envelope::new(
            device,
            SERVICE,
            start,
            end.max(start),
            Event::TokensUsed(TokensUsed {
                provider: driver_of_turn(&id),
                model: run
                    .map(|r| r.model.clone())
                    .or_else(|| t.map(|t| t.model.clone()))
                    .unwrap_or_default(),
                account: run
                    .map(|r| r.account.clone())
                    .or_else(|| t.map(|t| t.account.clone()))
                    .unwrap_or_default(),
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cached_input_tokens: u.cached_input_tokens,
                reasoning_tokens: u.reasoning_tokens,
                thread_id: thread.clone(),
                project: t.and_then(|t| t.project.clone()),
                origin,
            }),
        );
        env.id = stable_id(&format!("t3:turn:{id}"));
        out.push(env);
    }

    // Dictations.
    for (message, d) in &dictations {
        let mut env = Envelope::new(
            if d.client == "mobile" {
                "phone"
            } else {
                device
            },
            SERVICE,
            d.at,
            d.at,
            Event::DictationFinished(DictationFinished {
                recording_id: message.clone(),
                client: d.client.clone(),
                // Not recorded by T3; the words are.
                audio_seconds: 0.0,
                words: d.words,
                outcome: d.status.clone(),
            }),
        );
        env.id = stable_id(&format!("t3:dictation:{message}"));
        out.push(env);
    }

    out.extend(activity(&conn, &since_iso, since, device, &threads)?);
    Ok(out)
}

/// How long a thread in front of Erik stays counted after its last report:
/// the clients report once a minute, so half a beat is the expected overshoot.
const FOCUS_TAIL_MS: Millis = 30_000;
/// A focus report comes this long after the thread came to the front.
const FOCUS_SETTLE_MS: Millis = 5_000;
/// Two reports further apart than this are two stretches of focus.
const FOCUS_GAP_MS: Millis = 90_000;

/// Read-aloud plays, voice-mode spans and thread focus, from T3's
/// `activity_log` (a table newer T3 versions have; older ones simply give
/// nothing). Focus arrives as a beat a minute and is joined into stretches;
/// a stretch is reported once it has ended, so its id and length stay put.
fn activity(
    conn: &Connection,
    since_iso: &str,
    since: Millis,
    device: &str,
    threads: &HashMap<String, Thread>,
) -> Result<Vec<Envelope>> {
    let has_table = conn
        .query_row(
            "select 1 from sqlite_master where type = 'table' and name = 'activity_log'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_table {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "select kind, thread_id, message_id, client, started_at, ended_at, seconds
         from activity_log where started_at >= ?1 order by started_at",
    )?;
    let rows = stmt
        .query_map([since_iso], |r| {
            Ok(Activity {
                kind: r.get(0)?,
                thread: r.get(1)?,
                message: r.get(2)?,
                client: r.get(3)?,
                started: r.get(4)?,
                ended: r.get(5)?,
                seconds: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let machine = |client: &str| if client == "mobile" { "phone" } else { device };
    let now = chrono::Utc::now().timestamp_millis();
    let mut out = Vec::new();
    // Focus beats by (client, thread), oldest first.
    let mut beats: HashMap<(String, String), Vec<Millis>> = HashMap::new();
    for a in rows {
        let (Some(start), Some(end)) = (parse_ms(&a.started), parse_ms(&a.ended)) else {
            continue;
        };
        let key = format!("{}|{}|{}|{}|{}", a.kind, a.client, a.started, a.thread, a.message);
        match a.kind.as_str() {
            "t3.readaloud.played" => {
                let mut env = Envelope::new(
                    machine(&a.client),
                    SERVICE,
                    start,
                    end,
                    Event::ReadAloudPlayed(ReadAloudPlayed {
                        thread_id: a.thread,
                        mode: String::new(),
                        voice: String::new(),
                        seconds_played: a.seconds,
                        words_heard: 0,
                        outcome: "played".into(),
                    }),
                );
                env.id = stable_id(&format!("t3:activity:{key}"));
                out.push(env);
            }
            "t3.voicemode.span" => {
                let mut env = Envelope::new(
                    machine(&a.client),
                    SERVICE,
                    start,
                    end,
                    Event::VoiceModeSpan(VoiceModeSpan {
                        thread_id: a.thread,
                        turns: 0,
                    }),
                );
                env.id = stable_id(&format!("t3:activity:{key}"));
                out.push(env);
            }
            "thread.focus" => beats.entry((a.client, a.thread)).or_default().push(start),
            _ => {}
        }
    }
    for ((client, thread), times) in beats {
        let mut i = 0;
        while i < times.len() {
            let mut j = i;
            while j + 1 < times.len() && times[j + 1] - times[j] <= FOCUS_GAP_MS {
                j += 1;
            }
            let from = times[i] - FOCUS_SETTLE_MS;
            let to = times[j] + FOCUS_TAIL_MS;
            // Still going, or cut by the start of the window: reported later.
            if now - times[j] > FOCUS_GAP_MS && from >= since {
                let mut env = Envelope::new(
                    machine(&client),
                    SERVICE,
                    from,
                    to,
                    Event::FocusSpan(FocusSpan {
                        client: client.clone(),
                        thread_id: thread.clone(),
                        project: threads.get(&thread).and_then(|t| t.project.clone()),
                        focused: true,
                    }),
                );
                env.id = stable_id(&format!("t3:focus:{client}|{thread}|{}", times[i]));
                out.push(env);
            }
            i = j + 1;
        }
    }
    Ok(out)
}

struct Activity {
    kind: String,
    thread: String,
    message: String,
    client: String,
    started: String,
    ended: String,
    seconds: f64,
}

struct Thread {
    project: Option<String>,
    account: String,
    model: String,
}

struct Run {
    account: String,
    model: String,
    user_message: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessagePayload {
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default)]
    creation_source: Option<String>,
    #[serde(default)]
    scheduled_task_id: Option<String>,
    #[serde(default)]
    text: String,
}

impl MessagePayload {
    fn origin(&self) -> Origin {
        if self.scheduled_task_id.is_some() {
            Origin::Scheduled
        } else if self.created_by.as_deref() == Some("user") {
            Origin::Human
        } else {
            Origin::Agent
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnPayload {
    #[serde(default)]
    run_attempt_id: Option<String>,
    #[serde(default)]
    turn_token_usage: Option<TurnUsage>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelection {
    #[serde(default)]
    instance_id: String,
    #[serde(default)]
    model: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WithModel {
    #[serde(default)]
    model_selection: Option<ModelSelection>,
    #[serde(default)]
    user_message_id: Option<String>,
}

fn projects(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("select project_id, title from projection_projects")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn threads(
    conn: &Connection,
    projects: &HashMap<String, String>,
) -> Result<HashMap<String, Thread>> {
    let mut stmt = conn.prepare(
        "select thread_id, project_id, coalesce(provider_instance_id, default_provider), payload_json
         from orchestration_v2_projection_threads",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, project, account, payload) = row?;
        let m = serde_json::from_str::<WithModel>(&payload)
            .ok()
            .and_then(|p| p.model_selection)
            .unwrap_or_default();
        out.insert(
            id,
            Thread {
                project: projects.get(&project).cloned(),
                account: if m.instance_id.is_empty() {
                    account
                } else {
                    m.instance_id
                },
                model: m.model,
            },
        );
    }
    Ok(out)
}

fn runs(conn: &Connection, since_iso: &str) -> Result<HashMap<String, Run>> {
    let mut stmt = conn.prepare(
        "select run_id, provider_instance_id, payload_json
         from orchestration_v2_projection_runs where requested_at >= ?1",
    )?;
    let rows = stmt.query_map([since_iso], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, account, payload) = row?;
        let p: WithModel = serde_json::from_str(&payload).unwrap_or_default();
        let m = p.model_selection.unwrap_or_default();
        out.insert(
            id,
            Run {
                account: account.unwrap_or(m.instance_id),
                model: m.model,
                user_message: p.user_message_id,
            },
        );
    }
    Ok(out)
}

/// A run started by a message older than the window.
fn message_origin(conn: &Connection, id: &str) -> Option<Origin> {
    conn.query_row(
        "select payload_json from orchestration_v2_projection_messages where message_id = ?1",
        [id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|p| serde_json::from_str::<MessagePayload>(&p).ok())
    .map(|p| p.origin())
}

struct Dictation {
    at: Millis,
    client: String,
    words: u32,
    status: String,
    used_draft: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivery {
    #[serde(default)]
    queued_at: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    used_draft: bool,
    turn: Option<DeliveryTurn>,
}

#[derive(Deserialize)]
struct DeliveryTurn {
    command: Option<DeliveryCommand>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryCommand {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    creation_source: String,
    #[serde(default)]
    text: String,
}

/// Dictations by the message they became. Test deliveries are skipped.
fn deliveries(dir: &Path, since: Millis) -> Result<HashMap<String, Dictation>> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(d) = serde_json::from_str::<Delivery>(&text) else {
            continue;
        };
        let Some(c) = d.turn.and_then(|t| t.command) else {
            continue;
        };
        let Some(at) = parse_ms(&d.queued_at) else {
            continue;
        };
        if at < since || c.message_id.is_empty() || is_test(&c.text) {
            continue;
        }
        out.insert(
            c.message_id,
            Dictation {
                at,
                client: c.creation_source,
                words: word_count(&c.text),
                status: d.status,
                used_draft: d.used_draft,
            },
        );
    }
    Ok(out)
}

fn is_test(text: &str) -> bool {
    text.trim_start()
        .to_ascii_lowercase()
        .starts_with("this is a transcription delivery test")
}

pub fn word_count(text: &str) -> u32 {
    // Bracketed markers like `[dictation:<id>]` are not words Erik said.
    text.split_whitespace()
        .filter(|w| !(w.starts_with('[') && w.ends_with(']') && w.contains(':')))
        .count() as u32
}

/// `provider-turn:provider:<driver>:native-turn:...` → `<driver>`.
fn driver_of_turn(id: &str) -> String {
    id.strip_prefix("provider-turn:provider:")
        .and_then(|r| r.split(':').next())
        .unwrap_or_default()
        .to_owned()
}

/// `run-attempt:<percent-encoded run id>:attempt:<n>` → the run id.
fn run_of_attempt(attempt: &str) -> Option<String> {
    let rest = attempt.strip_prefix("run-attempt:")?;
    let enc = &rest[..rest.rfind(":attempt:")?];
    // Older ids nest one more `run:` around the encoded id.
    let enc = enc
        .strip_prefix("run:")
        .filter(|e| e.starts_with("run%3A"))
        .unwrap_or(enc);
    Some(percent_decode(enc))
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

fn parse_ms(iso: &str) -> Option<Millis> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// T3's own timestamp format, so string comparison in SQL is time order.
fn iso(ms: Millis) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn run_ids_from_attempts() {
        assert_eq!(
            run_of_attempt(
                "run-attempt:run:run%3Athread%3Apersonal-agent%3Aordinal%3A223:attempt:1"
            )
            .as_deref(),
            Some("run:thread:personal-agent:ordinal:223")
        );
        assert_eq!(
            run_of_attempt("run-attempt:run:run%3Athread%3Amcp%253A745a%3Aordinal%3A40:attempt:2")
                .as_deref(),
            Some("run:thread:mcp%3A745a:ordinal:40")
        );
    }

    #[test]
    fn drivers_and_words() {
        assert_eq!(
            driver_of_turn("provider-turn:provider:claudeAgent:native-turn:turn%3Ax"),
            "claudeAgent"
        );
        assert_eq!(word_count("[dictation:abc] fix the  status bar\nplease"), 5);
        assert!(is_test("This is a transcription delivery test, ignore"));
    }

    /// Listening and attention from `activity_log`: plays and voice spans as
    /// they are, focus beats joined into stretches per project.
    #[test]
    fn reads_listening_and_focus_from_the_activity_log() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("statev2.sqlite")).unwrap();
        db.execute_batch(
            r#"
            create table projection_projects(project_id text, title text);
            insert into projection_projects values ('p1', 'time');
            create table orchestration_v2_projection_threads(thread_id text, project_id text,
              default_provider text, provider_instance_id text, payload_json text);
            insert into orchestration_v2_projection_threads values ('t1', 'p1', 'codex', null, '{}');
            create table orchestration_v2_projection_runs(run_id text, provider_instance_id text,
              requested_at text, payload_json text);
            create table orchestration_v2_projection_messages(message_id text, thread_id text,
              role text, created_at text, payload_json text);
            create table orchestration_v2_projection_provider_turns(provider_turn_id text,
              thread_id text, status text, started_at text, completed_at text, payload_json text);
            create table activity_log(id integer primary key, kind text, thread_id text,
              message_id text, client text, started_at text, ended_at text, seconds real);
            insert into activity_log(kind, thread_id, message_id, client, started_at, ended_at, seconds) values
              ('t3.readaloud.played', 't1', 'm1', 'web', '2026-10-22T10:00:00.000Z', '2026-10-22T10:01:30.000Z', 72.5),
              ('t3.voicemode.span', 't1', '', 'mobile', '2026-10-22T11:00:00.000Z', '2026-10-22T11:10:00.000Z', 600),
              ('thread.focus', 't1', '', 'web', '2026-10-22T12:00:05.000Z', '2026-10-22T12:00:05.000Z', 0),
              ('thread.focus', 't1', '', 'web', '2026-10-22T12:01:05.000Z', '2026-10-22T12:01:05.000Z', 0),
              ('thread.focus', 't1', '', 'web', '2026-10-22T12:02:05.000Z', '2026-10-22T12:02:05.000Z', 0),
              ('thread.focus', 't1', '', 'web', '2026-10-22T14:00:05.000Z', '2026-10-22T14:00:05.000Z', 0);
            "#,
        )
        .unwrap();
        drop(db);

        let since = parse_ms("2026-10-22T00:00:00Z").unwrap();
        let events = collect(dir.path(), "pc", since).unwrap();

        let plays: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::ReadAloudPlayed(p) => Some((e, p)),
                _ => None,
            })
            .collect();
        assert_eq!(plays.len(), 1);
        assert_eq!(plays[0].0.device, "pc");
        assert_eq!(plays[0].0.end_ms - plays[0].0.start_ms, 90_000);
        assert_eq!(
            (plays[0].1.seconds_played, plays[0].1.thread_id.as_str()),
            (72.5, "t1")
        );

        let voice: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.event, Event::VoiceModeSpan(_)))
            .collect();
        assert_eq!(voice.len(), 1);
        assert_eq!(voice[0].device, "phone", "the phone app reports as the phone");
        assert_eq!(voice[0].end_ms - voice[0].start_ms, 600_000);

        let mut focus: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::FocusSpan(f) => Some((e.start_ms, e.end_ms, f)),
                _ => None,
            })
            .collect();
        focus.sort_by_key(|f| f.0);
        assert_eq!(focus.len(), 2, "three beats a minute apart are one stretch");
        // Five seconds before the first beat to half a beat after the last.
        assert_eq!(focus[0].1 - focus[0].0, 2 * 60_000 + 35_000);
        assert_eq!(focus[1].1 - focus[1].0, 35_000);
        assert_eq!(focus[0].2.project.as_deref(), Some("time"));
        assert!(focus[0].2.focused);

        let again = collect(dir.path(), "pc", since).unwrap();
        let ids: HashSet<_> = events.iter().map(|e| e.id.clone()).collect();
        assert!(again.iter().all(|e| ids.contains(&e.id)), "ids stay put");
    }

    /// A miniature of T3's schema: a dictated prompt from Erik, an agent's
    /// prompt, and the turn each started.
    #[test]
    fn reads_a_t3_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("statev2.sqlite")).unwrap();
        db.execute_batch(
            r#"
            create table projection_projects(project_id text, title text);
            insert into projection_projects values ('p1', 'time');
            create table orchestration_v2_projection_threads(thread_id text, project_id text,
              default_provider text, provider_instance_id text, payload_json text);
            insert into orchestration_v2_projection_threads values ('t1', 'p1', 'claudeAgent',
              'claudeAgent_claude_2', '{"modelSelection":{"instanceId":"claudeAgent_claude_2","model":"claude-opus-5-5"}}');
            create table orchestration_v2_projection_runs(run_id text, provider_instance_id text,
              requested_at text, payload_json text);
            insert into orchestration_v2_projection_runs values ('run:thread:t1:ordinal:1',
              'claudeAgent_claude_2', '2026-10-22T10:00:00.100Z',
              '{"modelSelection":{"model":"claude-opus-5-5"},"userMessageId":"m1"}');
            insert into orchestration_v2_projection_runs values ('run:thread:t1:ordinal:2',
              'codex', '2026-10-22T10:05:00.100Z',
              '{"modelSelection":{"model":"gpt-6-luna"},"userMessageId":"m2"}');
            create table orchestration_v2_projection_messages(message_id text, thread_id text,
              role text, created_at text, payload_json text);
            insert into orchestration_v2_projection_messages values ('m1', 't1', 'user',
              '2026-10-22T10:00:00.000Z', '{"createdBy":"user","creationSource":"web","text":"fix the status bar now"}');
            insert into orchestration_v2_projection_messages values ('m2', 't1', 'user',
              '2026-10-22T10:05:00.000Z', '{"createdBy":"agent","creationSource":"mcp","text":"check CI"}');
            insert into orchestration_v2_projection_messages values ('m0', 't1', 'user',
              '2026-10-01T10:00:00.000Z', '{"createdBy":"user","text":"too old"}');
            create table orchestration_v2_projection_provider_turns(provider_turn_id text,
              thread_id text, status text, started_at text, completed_at text, payload_json text);
            insert into orchestration_v2_projection_provider_turns values
              ('provider-turn:provider:claudeAgent:native-turn:a', 't1', 'completed',
               '2026-10-22T10:00:01.000Z', '2026-10-22T10:01:00.000Z',
               '{"runAttemptId":"run-attempt:run:run%3Athread%3At1%3Aordinal%3A1:attempt:1","turnTokenUsage":{"inputTokens":1000,"cachedInputTokens":900,"outputTokens":50,"reasoningTokens":5}}');
            insert into orchestration_v2_projection_provider_turns values
              ('provider-turn:provider:codex:native-turn:b', 't1', 'completed',
               '2026-10-22T10:05:01.000Z', '2026-10-22T10:06:00.000Z',
               '{"runAttemptId":"run-attempt:run:run%3Athread%3At1%3Aordinal%3A2:attempt:1","turnTokenUsage":{"inputTokens":200,"outputTokens":20}}');
            insert into orchestration_v2_projection_provider_turns values
              ('provider-turn:provider:claudeAgent:native-turn:c', 't1', 'running',
               '2026-10-22T10:07:00.000Z', null, '{}');
            "#,
        )
        .unwrap();
        drop(db);
        let dd = dir.path().join("dictation-deliveries");
        std::fs::create_dir(&dd).unwrap();
        std::fs::write(
            dd.join("a.json"),
            r#"{"queuedAt":"2026-10-22T09:59:58.000Z","status":"sent","usedDraft":false,
               "turn":{"command":{"messageId":"m1","creationSource":"web","text":"fix the status bar now"}}}"#,
        )
        .unwrap();

        let since = parse_ms("2026-10-22T00:00:00Z").unwrap();
        let events = collect(dir.path(), "pc", since).unwrap();
        let json = serde_json::to_string(&events).unwrap();
        assert!(
            !json.contains("status bar"),
            "no message text leaves: {json}"
        );

        let prompts: Vec<&PromptSent> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::PromptSent(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(prompts.len(), 2);
        let erik = prompts.iter().find(|p| p.origin == Origin::Human).unwrap();
        assert_eq!(erik.input_mode, InputMode::Dictated);
        assert_eq!((erik.words_typed, erik.words_dictated), (0, 5));
        assert_eq!(erik.project.as_deref(), Some("time"));
        assert_eq!(erik.model, "claude-opus-5-5");
        let agent = prompts.iter().find(|p| p.origin == Origin::Agent).unwrap();
        assert_eq!(agent.words_typed + agent.words_dictated, 0);

        let tokens: Vec<&TokensUsed> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::TokensUsed(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tokens.len(), 2, "the running turn waits until it ends");
        let claude = tokens.iter().find(|t| t.provider == "claudeAgent").unwrap();
        assert_eq!(claude.account, "claudeAgent_claude_2");
        assert_eq!(claude.origin, Origin::Human);
        assert_eq!(
            (
                claude.input_tokens,
                claude.cached_input_tokens,
                claude.output_tokens
            ),
            (1000, 900, 50)
        );
        let codex = tokens.iter().find(|t| t.provider == "codex").unwrap();
        assert_eq!(
            (codex.model.as_str(), codex.origin),
            ("gpt-6-luna", Origin::Agent)
        );

        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, Event::DictationFinished(_)))
                .count(),
            1
        );
        // Rereading gives the same ids, so the server stores each once.
        let again = collect(dir.path(), "pc", since).unwrap();
        let ids: HashSet<_> = events.iter().map(|e| e.id.clone()).collect();
        assert!(again.iter().all(|e| ids.contains(&e.id)));
    }
}
