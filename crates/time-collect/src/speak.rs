//! Voice on the pc outside T3 Code: what the speak daemon read aloud to
//! Erik, and when he dictated with the desktop hotkey.
//!
//! Two files under `~/.local/state/speak/`, both written by the dotfiles'
//! speak module:
//! - `utterances.jsonl`: one JSON object per step of an utterance; a
//!   `speaking` line and the `spoken` or `interrupted` line with the same id
//!   bound the time it was audible.
//! - `dictation-hook.log`: `<local time> recording focus=<class>` when the
//!   hotkey starts a dictation and `<local time> stopped focus=<class>` when
//!   it ends. Other lines (media paused and resumed) are ignored.
//!
//! Only times, lengths and word counts leave the machine; never the text.

use std::collections::HashMap;
use std::path::Path;

use chrono::TimeZone;
use time_core::clock::Millis;
use time_core::event::{DictationFinished, Envelope, Event, ReadAloudPlayed};

use crate::stable_id;

pub const SERVICE: &str = "speak";

/// Longer than this is a lost `stopped` or `spoken` line, not a real one.
const MAX_SPAN_MS: Millis = 15 * 60_000;

pub fn collect(dir: &Path, device: &str, since: Millis) -> Vec<Envelope> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(dir.join("utterances.jsonl")) {
        out.extend(utterances(&text, device, since));
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("dictation-hook.log")) {
        out.extend(dictations(&text, device, since, &chrono::Local));
    }
    out
}

#[derive(serde::Deserialize)]
struct Line {
    time: String,
    event: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    source: String,
}

/// Utterances that were heard: `speaking` up to `spoken` or `interrupted`.
pub fn utterances(text: &str, device: &str, since: Millis) -> Vec<Envelope> {
    let mut source: HashMap<String, String> = HashMap::new();
    let mut started: HashMap<String, (Millis, u32)> = HashMap::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        let Some(at) = parse_ms(&l.time) else {
            continue;
        };
        match l.event.as_str() {
            "queued" if !l.source.is_empty() => {
                source.insert(l.id, l.source);
            }
            "speaking" => {
                started.insert(l.id, (at, crate::t3::word_count(&l.text)));
            }
            "spoken" | "interrupted" => {
                let Some((from, words)) = started.remove(&l.id) else {
                    continue;
                };
                if from < since || at < from || at - from > MAX_SPAN_MS {
                    continue;
                }
                let mut env = Envelope::new(
                    device,
                    SERVICE,
                    from,
                    at,
                    Event::ReadAloudPlayed(ReadAloudPlayed {
                        thread_id: String::new(),
                        mode: "speak".into(),
                        voice: source.get(&l.id).cloned().unwrap_or_default(),
                        seconds_played: (at - from) as f64 / 1000.0,
                        words_heard: words,
                        outcome: l.event.clone(),
                    }),
                );
                env.id = stable_id(&format!("speak:utterance:{}", l.id));
                out.push(env);
            }
            _ => {}
        }
    }
    out
}

/// Hotkey dictations: each `recording` up to the next `stopped`.
pub fn dictations<Z: TimeZone>(text: &str, device: &str, since: Millis, tz: &Z) -> Vec<Envelope> {
    let mut out = Vec::new();
    let mut open: Option<(Millis, String)> = None;
    for line in text.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(time), Some(what)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(at) = chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M:%S")
            .ok()
            .and_then(|t| tz.from_local_datetime(&t).earliest())
            .map(|t| t.timestamp_millis())
        else {
            continue;
        };
        match what {
            "recording" => open = Some((at, time.to_owned())),
            "stopped" => {
                let Some((from, key)) = open.take() else {
                    continue;
                };
                if from < since || at < from || at - from > MAX_SPAN_MS {
                    continue;
                }
                let mut env = Envelope::new(
                    device,
                    SERVICE,
                    from,
                    at,
                    Event::DictationFinished(DictationFinished {
                        recording_id: format!("hotkey:{key}"),
                        client: "hotkey".into(),
                        audio_seconds: (at - from) as f64 / 1000.0,
                        words: 0,
                        outcome: "recorded".into(),
                    }),
                );
                env.id = stable_id(&format!("speak:dictation:{key}"));
                out.push(env);
            }
            _ => {}
        }
    }
    out
}

fn parse_ms(s: &str) -> Option<Millis> {
    chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s))
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utterances_are_timed_from_speaking_to_spoken() {
        let log = r#"{"time": "2026-10-25T23:05:40+0100", "event": "queued", "id": "a", "text": "Hi there Erik", "source": "personal-agent"}
{"time": "2026-10-25T23:05:58+0100", "event": "speaking", "id": "a", "text": "Hi there Erik"}
{"time": "2026-10-25T23:06:15+0100", "event": "spoken", "id": "a"}
{"time": "2026-10-25T23:07:00+0100", "event": "speaking", "id": "b", "text": "never finished"}
{"time": "2026-10-25T23:08:00+0100", "event": "speaking", "id": "c", "text": "cut short"}
{"time": "2026-10-25T23:08:03+0100", "event": "interrupted", "id": "c"}"#;
        let e = utterances(log, "pc", 0);
        assert_eq!(e.len(), 2);
        let Event::ReadAloudPlayed(r) = &e[0].event else {
            panic!()
        };
        assert_eq!((r.seconds_played, r.words_heard), (17.0, 3));
        assert_eq!(
            (r.mode.as_str(), r.voice.as_str()),
            ("speak", "personal-agent")
        );
        assert_eq!(e[0].end_ms - e[0].start_ms, 17_000);
        assert_eq!(
            utterances(log, "pc", 0)[0].id,
            e[0].id,
            "stable ids across runs"
        );
    }

    #[test]
    fn hotkey_dictations_pair_recording_with_stopped() {
        let log = "2026-10-23T08:38:49 recording focus=com.t3tools.T3Code
2026-10-23T08:39:20 stopped focus=com.t3tools.T3Code
2026-10-23T12:59:14 recording focus=com.t3tools.T3Code
2026-10-23T19:43:33 stopped focus=com.t3tools.T3Code
2026-10-25T16:44:59 recording focus=none
2026-10-25T16:44:59 paused firefox.instance_1_5827
2026-10-25T16:46:05 stopped focus=com.t3tools.T3Code";
        let e = dictations(log, "pc", 0, &chrono::Utc);
        assert_eq!(e.len(), 2, "a seven-hour 'dictation' is a lost line");
        let secs: Vec<f64> = e
            .iter()
            .map(|e| match &e.event {
                Event::DictationFinished(d) => d.audio_seconds,
                _ => 0.0,
            })
            .collect();
        assert_eq!(secs, [31.0, 66.0]);
    }
}
