//! One local calendar day, summarised: what the "Today" page shows and what
//! `/api/day/<date>` returns.

use std::collections::BTreeMap;

use chrono::{NaiveDate, TimeZone};
use serde::Serialize;
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::Event;
use time_core::label::{Activity, MinuteLabel, Place};

use crate::store::Store;

#[derive(Debug, Serialize)]
pub struct Day {
    pub date: NaiveDate,
    pub start_ms: Millis,
    pub end_ms: Millis,
    pub minutes: Vec<MinuteLabel>,
    /// Minutes per activity, split by place.
    pub activities: Vec<ActivityTotal>,
    pub projects: Vec<(String, u32)>,
    pub computer_minutes: u32,
    pub phone_minutes: u32,
    pub unknown_minutes: u32,
    /// Something playing on another device while Erik was elsewhere.
    pub background_minutes: u32,
    pub steps: u32,
    pub distance_m: f64,
    /// Steps per local hour, 0..24 (23 or 25 on daylight-saving days).
    pub steps_by_hour: Vec<u32>,
    pub code: CodeTotals,
    pub signals: Signals,
    pub modalities: Modalities,
    /// Keyboard and mouse per ten minutes, chunks with no input left out.
    pub interactivity: Vec<crate::tracks::Chunk>,
}

/// What was shipped that day, from git and GitHub.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CodeTotals {
    pub commits: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub prs_opened: u32,
    pub prs_merged: u32,
    pub reviews: u32,
    pub repos: Vec<String>,
    /// Commits per repository.
    pub commits_by_repo: BTreeMap<String, u32>,
}

impl CodeTotals {
    pub const EVENTS: [&str; 3] = ["github.commit", "github.pr", "github.review"];

    pub fn add(&mut self, e: &Event) {
        let repo = match e {
            Event::Commit(c) => {
                self.commits += 1;
                self.lines_added += c.lines_added;
                self.lines_removed += c.lines_removed;
                *self.commits_by_repo.entry(c.repo.clone()).or_default() += 1;
                &c.repo
            }
            Event::PullRequest(p) if p.action == "opened" => {
                self.prs_opened += 1;
                &p.repo
            }
            Event::PullRequest(p) if p.action == "merged" => {
                self.prs_merged += 1;
                &p.repo
            }
            Event::Review(r) => {
                self.reviews += 1;
                &r.repo
            }
            _ => return,
        };
        if !self.repos.contains(repo) {
            self.repos.push(repo.clone());
        }
    }

    pub fn merge(&mut self, o: &CodeTotals) {
        self.commits += o.commits;
        self.lines_added += o.lines_added;
        self.lines_removed += o.lines_removed;
        self.prs_opened += o.prs_opened;
        self.prs_merged += o.prs_merged;
        self.reviews += o.reviews;
        for r in &o.repos {
            if !self.repos.contains(r) {
                self.repos.push(r.clone());
            }
        }
        for (r, n) in &o.commits_by_repo {
            *self.commits_by_repo.entry(r.clone()).or_default() += n;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.commits + self.prs_opened + self.prs_merged + self.reviews == 0
    }
}

#[derive(Debug, Serialize)]
pub struct ActivityTotal {
    pub activity: Activity,
    pub computer: u32,
    pub phone: u32,
}

impl ActivityTotal {
    pub fn total(&self) -> u32 {
        self.computer + self.phone
    }
}

/// Bounds of a local day, in epoch ms. Days are 23-25 hours long around
/// daylight-saving changes, so never `start + 24h`.
pub fn bounds(tz: chrono_tz::Tz, date: NaiveDate) -> anyhow::Result<(Millis, Millis)> {
    let local = |d: NaiveDate| {
        tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists"))
            .earliest()
            .map(|t| t.timestamp_millis())
            .ok_or_else(|| anyhow::anyhow!("no local midnight on {d}"))
    };
    let next = date
        .succ_opt()
        .ok_or_else(|| anyhow::anyhow!("date out of range"))?;
    Ok((local(date)?, local(next)?))
}

pub fn today(tz: chrono_tz::Tz, now: Millis) -> NaiveDate {
    tz.timestamp_millis_opt(now)
        .single()
        .map(|t| t.date_naive())
        .unwrap_or_default()
}

pub fn summarise(store: &Store, tz: chrono_tz::Tz, date: NaiveDate) -> anyhow::Result<Day> {
    let (start_ms, end_ms) = bounds(tz, date)?;
    let minutes = store.labels_between(start_ms, end_ms)?;

    let mut by_activity: BTreeMap<Activity, (u32, u32)> = BTreeMap::new();
    let mut projects: BTreeMap<String, u32> = BTreeMap::new();
    let (mut computer, mut phone, mut unknown, mut background) = (0, 0, 0, 0);
    for m in &minutes {
        let Some(a) = m.activity else { continue };
        let slot = by_activity.entry(a).or_default();
        match m.place {
            Place::Computer => {
                slot.0 += 1;
                computer += 1;
            }
            Place::Phone => {
                slot.1 += 1;
                phone += 1;
            }
            Place::Away => {}
        }
        if a == Activity::Unknown {
            unknown += 1;
        }
        if m.background.is_some() {
            background += 1;
        }
        if let Some(p) = &m.project {
            *projects.entry(p.clone()).or_default() += 1;
        }
    }
    let mut activities: Vec<ActivityTotal> = by_activity
        .into_iter()
        .map(|(activity, (computer, phone))| ActivityTotal {
            activity,
            computer,
            phone,
        })
        .collect();
    activities.sort_by_key(|a| std::cmp::Reverse(a.total()));
    let mut projects: Vec<(String, u32)> = projects.into_iter().collect();
    projects.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let hours = ((end_ms - start_ms) / (60 * MINUTE_MS)) as usize;
    let mut steps_by_hour = vec![0; hours];
    let (mut steps, mut distance_m) = (0, 0.0);
    let mut code = CodeTotals::default();
    let day_events = store.events_between(start_ms, end_ms)?;
    let signals = signals(&minutes, &day_events, start_ms, end_ms);
    let modalities = modalities(&day_events, start_ms, end_ms);
    let input = store.input_between(start_ms, end_ms)?;
    let interactivity = crate::tracks::Tracks::build(
        &[],
        std::iter::empty(),
        &input,
        &[],
        &[],
        tz,
        start_ms,
        end_ms,
    )
    .chunks;
    for e in day_events {
        if e.start_ms >= start_ms && e.start_ms < end_ms {
            code.add(&e.event);
        }
        if let Event::HealthSteps(h) = &e.event {
            // Attribute a span to the hour it started in; Health Connect
            // records are read back per hour, so they rarely straddle one.
            if e.start_ms < start_ms || e.start_ms >= end_ms {
                continue;
            }
            steps += h.steps;
            distance_m += h.distance_m;
            let i = ((e.start_ms - start_ms) / (60 * MINUTE_MS)) as usize;
            if let Some(slot) = steps_by_hour.get_mut(i) {
                *slot += h.steps;
            }
        }
    }

    Ok(Day {
        date,
        start_ms,
        end_ms,
        minutes,
        activities,
        projects,
        computer_minutes: computer,
        phone_minutes: phone,
        unknown_minutes: unknown,
        background_minutes: background,
        steps,
        distance_m,
        steps_by_hour,
        code,
        signals,
        modalities,
        interactivity,
    })
}

/// A stretch of time with a name: a place, or a state like "charging".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span {
    pub what: String,
    pub from_ms: Millis,
    pub to_ms: Millis,
}

/// Places and device states over the day, from the no-GPS signals.
#[derive(Debug, Default, Serialize)]
pub struct Signals {
    /// Where Erik was, from the networks his devices were on.
    pub places: Vec<Span>,
    /// The phone: charging, and how many times it was unlocked.
    pub phone: Vec<Span>,
    pub phone_unlocks: u32,
    /// Computers: locked, lid closed, asleep, charging.
    pub computers: Vec<Span>,
}

/// Contiguous minutes (gaps up to `bridge` minutes allowed) into spans.
fn spans(mut minutes: Vec<Millis>, what: &str, min_len: i64, bridge: i64) -> Vec<Span> {
    minutes.sort_unstable();
    minutes.dedup();
    let mut out: Vec<Span> = Vec::new();
    for t in minutes {
        match out.last_mut() {
            Some(s) if t - s.to_ms <= bridge * MINUTE_MS => s.to_ms = t + MINUTE_MS,
            _ => out.push(Span {
                what: what.to_owned(),
                from_ms: t,
                to_ms: t + MINUTE_MS,
            }),
        }
    }
    out.retain(|s| s.to_ms - s.from_ms >= min_len * MINUTE_MS);
    out
}

pub fn signals(
    labels: &[time_core::label::MinuteLabel],
    events: &[time_core::event::Envelope],
    start: Millis,
    end: Millis,
) -> Signals {
    use std::collections::BTreeMap;
    let mut sig = Signals::default();

    // Places: runs of the minute labels' location, bridging short gaps.
    let mut by_place: BTreeMap<&str, Vec<Millis>> = BTreeMap::new();
    for l in labels {
        if let Some(p) = &l.location {
            by_place.entry(p).or_default().push(l.ts);
        }
    }
    for (p, mins) in by_place {
        sig.places.extend(spans(mins, p, 5, 15));
    }
    sig.places.sort_by_key(|s| s.from_ms);

    let in_day = |t: Millis| t >= start && t < end;
    let mut phone_charging = Vec::new();
    let mut phone_states: Vec<(Millis, bool)> = Vec::new();
    let mut per_device: BTreeMap<(String, &str), Vec<Millis>> = BTreeMap::new();
    for e in events.iter().filter(|e| in_day(e.start_ms)) {
        let ts = e.start_ms - e.start_ms.rem_euclid(MINUTE_MS);
        match &e.event {
            Event::PhoneState(s) => {
                if s.charging {
                    phone_charging.push(ts);
                }
                phone_states.push((ts, s.locked));
            }
            Event::ScreenMinute(m) => {
                if m.locked == Some(true) {
                    per_device
                        .entry((e.device.clone(), "locked"))
                        .or_default()
                        .push(ts);
                }
                if m.lid_closed == Some(true) {
                    per_device
                        .entry((e.device.clone(), "lid closed"))
                        .or_default()
                        .push(ts);
                }
                if m.charging == Some(true) {
                    per_device
                        .entry((e.device.clone(), "charging"))
                        .or_default()
                        .push(ts);
                }
            }
            Event::Asleep(_) => {
                let mut t = e.start_ms;
                while t < e.end_ms {
                    per_device
                        .entry((e.device.clone(), "asleep"))
                        .or_default()
                        .push(t);
                    t += MINUTE_MS;
                }
            }
            _ => {}
        }
    }
    sig.phone = spans(phone_charging, "charging", 5, 2);
    phone_states.sort_unstable();
    sig.phone_unlocks = phone_states
        .windows(2)
        .filter(|w| w[0].1 && !w[1].1)
        .count() as u32;
    // A closed laptop wakes for a minute now and then; bridge those gaps so
    // a night on the charger is one span, not forty.
    for ((device, what), mins) in per_device {
        sig.computers
            .extend(spans(mins, &format!("{device} {what}"), 5, 15));
    }
    sig.computers.sort_by_key(|s| s.from_ms);
    sig
}

/// The four ways Erik works with a machine, and what the AI did with it:
/// typing, voice, listening, screen; prompts and tokens.
#[derive(Debug, Default, Serialize)]
pub struct Modalities {
    /// Minutes with at least one key press on a computer.
    pub typing_minutes: u32,
    pub key_presses: u32,
    /// Words in Erik's own prompts to T3 Code, typed and dictated.
    pub words_typed: u32,
    pub words_dictated: u32,
    pub prompts_typed: u32,
    pub prompts_dictated: u32,
    pub dictations: u32,
    /// Prompts sent by agents and scheduled tasks, not by Erik.
    pub prompts_by_agents: u32,
    /// Minutes with sound playing: computers, phone media, read-aloud.
    pub listening_minutes: u32,
    pub read_aloud_minutes: u32,
    /// Minutes with voice mode on (the hands-free loop), on any device.
    pub voice_mode_minutes: u32,
    /// Minutes with a T3 Code thread in front of Erik, per project.
    pub focus: Vec<FocusTotal>,
    pub tokens: Vec<TokenTotal>,
}

#[derive(Debug, Default, Serialize)]
pub struct FocusTotal {
    pub project: String,
    pub minutes: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct TokenTotal {
    /// The provider account (T3's provider instance), e.g. `claudeAgent_claude_2`.
    pub account: String,
    pub turns: u32,
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    /// Turns started by one of Erik's own prompts.
    pub turns_by_erik: u32,
}

impl Modalities {
    pub fn is_empty(&self) -> bool {
        self.typing_minutes
            + self.prompts_typed
            + self.prompts_dictated
            + self.dictations
            + self.prompts_by_agents
            + self.listening_minutes
            + self.voice_mode_minutes
            == 0
            && self.focus.is_empty()
            && self.tokens.is_empty()
    }

    pub fn tokens_total(&self) -> u64 {
        self.tokens.iter().map(|t| t.input + t.output).sum()
    }
}

/// The minutes of the day an event covers, as minute starts.
fn minutes_in(
    e: &time_core::event::Envelope,
    start: Millis,
    end: Millis,
    floor: impl Fn(Millis) -> Millis,
    into: &mut std::collections::BTreeSet<Millis>,
) {
    let mut t = floor(e.start_ms.max(start));
    while t < e.end_ms.min(end) {
        into.insert(t);
        t += MINUTE_MS;
    }
}

pub fn modalities(events: &[time_core::event::Envelope], start: Millis, end: Millis) -> Modalities {
    use std::collections::BTreeSet;
    use time_core::event::{InputMode, Origin};
    let mut m = Modalities::default();
    let mut typing: BTreeSet<Millis> = BTreeSet::new();
    let mut listening: BTreeSet<Millis> = BTreeSet::new();
    let mut voice_mode: BTreeSet<Millis> = BTreeSet::new();
    let mut focus: BTreeSet<(String, Millis)> = BTreeSet::new();
    let mut read_aloud_seconds = 0.0;
    let mut tokens: BTreeMap<String, TokenTotal> = BTreeMap::new();
    let floor = |t: Millis| t - t.rem_euclid(MINUTE_MS);
    for e in events {
        // Spans (phone apps, read-aloud) may start before the day.
        if e.end_ms < start || e.start_ms >= end {
            continue;
        }
        let in_day = e.start_ms >= start;
        match &e.event {
            Event::ScreenMinute(s) if in_day && !s.blocked => {
                if s.keys > 0 {
                    typing.insert(floor(e.start_ms));
                    m.key_presses += s.keys;
                }
                if s.audio_playing {
                    listening.insert(floor(e.start_ms));
                }
            }
            Event::PhoneApp(a) if a.media_playing => {
                let mut t = floor(e.start_ms.max(start));
                while t < e.end_ms.min(end) {
                    listening.insert(t);
                    t += MINUTE_MS;
                }
            }
            Event::ReadAloudPlayed(r) if in_day => {
                let mins = (r.seconds_played / 60.0).ceil() as i64;
                read_aloud_seconds += r.seconds_played;
                for i in 0..mins.max(1) {
                    listening.insert(floor(e.start_ms) + i * MINUTE_MS);
                }
            }
            Event::VoiceModeSpan(_) => minutes_in(e, start, end, floor, &mut voice_mode),
            Event::FocusSpan(f) if f.focused => {
                let mut mins = BTreeSet::new();
                minutes_in(e, start, end, floor, &mut mins);
                let project = f.project.clone().unwrap_or_else(|| "(no project)".into());
                focus.extend(mins.into_iter().map(|t| (project.clone(), t)));
            }
            Event::PromptSent(p) if in_day => match p.origin {
                Origin::Human => {
                    m.words_typed += p.words_typed;
                    m.words_dictated += p.words_dictated;
                    match p.input_mode {
                        InputMode::Typed => m.prompts_typed += 1,
                        InputMode::Dictated | InputMode::Mixed => m.prompts_dictated += 1,
                    }
                }
                Origin::Agent | Origin::Scheduled => m.prompts_by_agents += 1,
            },
            Event::DictationFinished(_) if in_day => m.dictations += 1,
            Event::TokensUsed(t) if in_day => {
                let key = if t.account.is_empty() {
                    t.provider.clone()
                } else {
                    t.account.clone()
                };
                let s = tokens.entry(key.clone()).or_default();
                s.account = key;
                s.turns += 1;
                s.input += t.input_tokens;
                s.cached_input += t.cached_input_tokens;
                s.output += t.output_tokens;
                if t.origin == Origin::Human {
                    s.turns_by_erik += 1;
                }
            }
            _ => {}
        }
    }
    m.typing_minutes = typing.len() as u32;
    m.listening_minutes = listening.len() as u32;
    m.read_aloud_minutes = (read_aloud_seconds / 60.0_f64).ceil() as u32;
    m.voice_mode_minutes = voice_mode.len() as u32;
    let mut per_project: BTreeMap<String, u32> = BTreeMap::new();
    for (project, _) in focus {
        *per_project.entry(project).or_default() += 1;
    }
    m.focus = per_project
        .into_iter()
        .map(|(project, minutes)| FocusTotal { project, minutes })
        .collect();
    m.focus.sort_by_key(|f| std::cmp::Reverse(f.minutes));
    m.tokens = tokens.into_values().collect();
    m.tokens
        .sort_by_key(|t| std::cmp::Reverse(t.input + t.output));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daylight_saving_days_are_not_24_hours() {
        let tz: chrono_tz::Tz = "Europe/Stockholm".parse().unwrap();
        let hours = |d: &str| {
            let (a, b) = bounds(tz, d.parse().unwrap()).unwrap();
            (b - a) / (60 * MINUTE_MS)
        };
        assert_eq!(hours("2026-10-21"), 24);
        assert_eq!(hours("2026-10-25"), 25, "clocks go back");
        assert_eq!(hours("2026-03-29"), 23, "clocks go forward");
    }

    #[test]
    fn modalities_count_typing_voice_listening_and_tokens() {
        use time_core::event::*;
        let m = MINUTE_MS;
        let screen = |t: Millis, keys: u32, audio: bool| {
            Envelope::new(
                "pc",
                "agent",
                t,
                t + m,
                Event::ScreenMinute(ScreenMinute {
                    keys,
                    audio_playing: audio,
                    ..Default::default()
                }),
            )
        };
        let prompt = |t: Millis, mode: InputMode, origin: Origin, typed: u32, said: u32| {
            Envelope::new(
                "pc",
                "t3-code",
                t,
                t,
                Event::PromptSent(PromptSent {
                    thread_id: "t".into(),
                    project: None,
                    provider: String::new(),
                    model: String::new(),
                    client: "web".into(),
                    words_typed: typed,
                    words_dictated: said,
                    input_mode: mode,
                    origin,
                }),
            )
        };
        let tokens = |t: Millis, account: &str, origin: Origin| {
            Envelope::new(
                "pc",
                "t3-code",
                t,
                t + m,
                Event::TokensUsed(TokensUsed {
                    provider: "claudeAgent".into(),
                    model: "claude-opus-5-5".into(),
                    account: account.into(),
                    input_tokens: 1000,
                    output_tokens: 10,
                    cached_input_tokens: 900,
                    reasoning_tokens: 0,
                    thread_id: "t".into(),
                    project: None,
                    origin,
                }),
            )
        };
        let events = vec![
            screen(0, 40, false),
            screen(m, 0, true),
            screen(2 * m, 12, true),
            Envelope::new(
                "phone",
                "app",
                5 * m,
                8 * m,
                Event::PhoneApp(PhoneApp {
                    package: "p".into(),
                    label: "Spotify".into(),
                    media_playing: true,
                }),
            ),
            prompt(3 * m, InputMode::Typed, Origin::Human, 12, 0),
            prompt(4 * m, InputMode::Dictated, Origin::Human, 0, 30),
            prompt(4 * m, InputMode::Typed, Origin::Agent, 0, 0),
            Envelope::new(
                "pc",
                "t3-code",
                4 * m,
                4 * m,
                Event::DictationFinished(DictationFinished {
                    recording_id: "r".into(),
                    client: "web".into(),
                    audio_seconds: 0.0,
                    words: 30,
                    outcome: "sent".into(),
                }),
            ),
            tokens(3 * m, "claudeAgent_claude_2", Origin::Human),
            tokens(4 * m, "claudeAgent_claude_2", Origin::Agent),
            tokens(5 * m, "codex", Origin::Agent),
        ];
        let md = modalities(&events, 0, 60 * m);
        assert_eq!((md.typing_minutes, md.key_presses), (2, 52));
        assert_eq!(md.listening_minutes, 5, "two computer minutes, three phone");
        assert_eq!((md.prompts_typed, md.words_typed), (1, 12));
        assert_eq!((md.prompts_dictated, md.words_dictated), (1, 30));
        assert_eq!((md.dictations, md.prompts_by_agents), (1, 1));
        assert_eq!(md.tokens.len(), 2);
        let claude = &md.tokens[0];
        assert_eq!(claude.account, "claudeAgent_claude_2");
        assert_eq!(
            (claude.turns, claude.turns_by_erik, claude.input),
            (2, 1, 2000)
        );
        assert_eq!(md.tokens_total(), 3030);
    }

    #[test]
    fn read_aloud_voice_mode_and_focus_add_up() {
        use time_core::event::{Envelope, FocusSpan, ReadAloudPlayed, VoiceModeSpan};
        let m = MINUTE_MS;
        let at = |s: Millis, e: Millis, event: Event| Envelope::new("pc", "t3-code", s, e, event);
        let focus = |s: Millis, e: Millis, project: &str| {
            at(
                s,
                e,
                Event::FocusSpan(FocusSpan {
                    client: "web".into(),
                    thread_id: "t".into(),
                    project: Some(project.into()),
                    focused: true,
                }),
            )
        };
        let events = vec![
            at(
                0,
                90_000,
                Event::ReadAloudPlayed(ReadAloudPlayed {
                    thread_id: "t".into(),
                    mode: String::new(),
                    voice: String::new(),
                    seconds_played: 70.0,
                    words_heard: 0,
                    outcome: "played".into(),
                }),
            ),
            at(
                10 * m,
                10 * m + 40_000,
                Event::ReadAloudPlayed(ReadAloudPlayed {
                    thread_id: "t".into(),
                    mode: String::new(),
                    voice: String::new(),
                    seconds_played: 40.0,
                    words_heard: 0,
                    outcome: "played".into(),
                }),
            ),
            at(
                20 * m,
                30 * m,
                Event::VoiceModeSpan(VoiceModeSpan {
                    thread_id: "t".into(),
                    turns: 0,
                }),
            ),
            // Two devices on the same project at once count the minutes once.
            focus(40 * m, 45 * m, "time"),
            focus(43 * m, 46 * m, "time"),
            focus(50 * m, 52 * m, "t3code"),
        ];
        let md = modalities(&events, 0, 60 * m);
        assert_eq!(md.read_aloud_minutes, 2, "110 s of playing, rounded up");
        assert_eq!(md.voice_mode_minutes, 10);
        let focus: Vec<_> = md
            .focus
            .iter()
            .map(|f| (f.project.as_str(), f.minutes))
            .collect();
        assert_eq!(focus, [("time", 6), ("t3code", 2)]);
        assert!(!md.is_empty());
    }
}
