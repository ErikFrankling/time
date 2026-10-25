//! What is derived on top of the minute labels: gaps filled, and sleep.
//!
//! Every few minutes, once the classifier has nothing left to label:
//! - **gaps**: [`time_core::fill::plan`] fills the obvious ones; the
//!   ambiguous ones are put to the decision model two hours at a time, as
//!   five-minute summaries, asking when the activity before each gap
//!   stopped and the one after it started. The result is the `fill` table.
//! - **sleep**: per night (18:00 to 18:00), [`time_core::sleep::candidate`]
//!   finds the longest stretch with no human use of any device; once Erik
//!   has been up a while the decision model confirms or adjusts it from ten
//!   minutes summaries of every signal. The result is the `sleep` table.
//!
//! Both are read back by [`crate::store::Store::labels_between`], so the
//! day, the week, the clock and the MCP tools all see them, and the raw
//! minutes stay one click away (`?raw=1`).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::Arc;

use anyhow::Result;
use chrono::{NaiveDate, TimeZone, Timelike};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, Origin};
use time_core::fill::{Fill, FillSource, Gap, GapKind, Screen};
use time_core::label::{MinuteLabel, Place};
use time_core::sleep::Candidate;

use crate::store::Store;
use crate::AppState;

/// Bump when a change here can change what is derived: everything since
/// `[decide] since` is derived again on start.
pub const DERIVE_VERSION: u32 = 1;
/// Recent history re-derived each pass, for late data.
const RECENT_MS: Millis = 36 * 60 * MINUTE_MS;
/// Gaps are put to the model this many hours at a time.
const WINDOW_MIN: u32 = 120;
/// A window is asked about only this long after it ends, so a gap at its
/// end is known to be a gap.
const WINDOW_SETTLE_MS: Millis = 20 * MINUTE_MS;
/// The model looks at a night this long after the rules think Erik woke,
/// and this far either side of the candidate.
const SLEEP_SETTLE_MS: Millis = 3 * 60 * MINUTE_MS;
const SLEEP_CONTEXT_MS: Millis = 3 * 60 * MINUTE_MS;
/// Seconds between passes.
const EVERY_SECS: u64 = 300;

/// A short use inside a night's sleep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interruption {
    pub from_ms: Millis,
    pub to_ms: Millis,
    pub what: String,
}

/// A night: 18:00 on `night` to 18:00 the next day.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Night {
    pub night: NaiveDate,
    pub fell_asleep_ms: Option<Millis>,
    pub woke_up_ms: Option<Millis>,
    pub interruptions: Vec<Interruption>,
    pub confidence: Option<f64>,
    /// One line: why. The model's own words when it decided.
    pub reason: String,
    /// `rule` (the candidate, not yet confirmed) or `model`.
    pub source: String,
    pub candidate_start_ms: Option<Millis>,
    pub candidate_end_ms: Option<Millis>,
    pub decision_id: Option<i64>,
    /// The night's 24 hours are over.
    pub done: bool,
}

impl Night {
    /// Minutes asleep: in bed, less the interruptions.
    pub fn minutes(&self) -> Option<i64> {
        let (a, b) = (self.fell_asleep_ms?, self.woke_up_ms?);
        let awake: i64 = self
            .interruptions
            .iter()
            .map(|i| (i.to_ms.min(b) - i.from_ms.max(a)).max(0))
            .sum();
        Some(((b - a - awake) / MINUTE_MS).max(0))
    }
}

/// 18:00 on `night` to 18:00 the next day, local.
pub fn night_bounds(tz: chrono_tz::Tz, night: NaiveDate) -> Result<(Millis, Millis)> {
    let at = |d: NaiveDate| {
        tz.from_local_datetime(&d.and_hms_opt(18, 0, 0).expect("18:00 exists"))
            .earliest()
            .map(|t| t.timestamp_millis())
            .ok_or_else(|| anyhow::anyhow!("no 18:00 on {d}"))
    };
    Ok((at(night)?, at(night.succ_opt().unwrap_or(night))?))
}

/// The night that ends on the morning of `day`.
pub fn night_before(day: NaiveDate) -> NaiveDate {
    day.pred_opt().unwrap_or(day)
}

/// Nights `[from, to]`, by the date they start on.
pub fn nights(store: &Store, from: NaiveDate, to: NaiveDate) -> Result<Vec<Night>> {
    store.with(|c| {
        let mut stmt = c.prepare(
            "SELECT night, fell_asleep_ms, woke_up_ms, interruptions, confidence, reason, source,
                    candidate_start_ms, candidate_end_ms, decision_id, final
             FROM sleep WHERE night >= ?1 AND night <= ?2 ORDER BY night",
        )?;
        let rows = stmt.query_map(params![from.to_string(), to.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<Millis>>(1)?,
                r.get::<_, Option<Millis>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<Millis>>(7)?,
                r.get::<_, Option<Millis>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, bool>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (night, fell, woke, ints, confidence, reason, source, cs, ce, did, done) = row?;
            out.push(Night {
                night: night.parse()?,
                fell_asleep_ms: fell,
                woke_up_ms: woke,
                interruptions: serde_json::from_str(&ints).unwrap_or_default(),
                confidence,
                reason,
                source,
                candidate_start_ms: cs,
                candidate_end_ms: ce,
                decision_id: did,
                done,
            });
        }
        Ok(out)
    })
}

fn save_night(store: &Store, n: &Night, now: Millis) -> Result<()> {
    store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO sleep (night, fell_asleep_ms, woke_up_ms, interruptions, confidence, reason,
                source, candidate_start_ms, candidate_end_ms, decision_id, final, computed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                n.night.to_string(),
                n.fell_asleep_ms,
                n.woke_up_ms,
                serde_json::to_string(&n.interruptions)?,
                n.confidence,
                n.reason,
                n.source,
                n.candidate_start_ms,
                n.candidate_end_ms,
                n.decision_id,
                n.done,
                now
            ],
        )?;
        Ok(())
    })
}

// ------------------------------------------------------------- summaries

fn hm(tz: chrono_tz::Tz, t: Millis) -> String {
    tz.timestamp_millis_opt(t)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn ymd_hm(tz: chrono_tz::Tz, t: Millis) -> String {
    tz.timestamp_millis_opt(t)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Every signal in `[from, to)`, one line per `slot_min` minutes, with
/// identical lines in a row folded into one: what the model reads.
pub fn signal_rows(
    labels: &[MinuteLabel],
    events: &[Envelope],
    places: &[time_core::places::PlaceSpec],
    tz: chrono_tz::Tz,
    from: Millis,
    to: Millis,
    slot_min: i64,
) -> Vec<String> {
    let slot_ms = slot_min * MINUTE_MS;
    let n = ((to - from + slot_ms - 1) / slot_ms).max(0) as usize;
    #[derive(Default)]
    struct S {
        uses: BTreeMap<String, u32>,
        away: BTreeMap<&'static str, u32>,
        keys: u32,
        mouse: u32,
        woken: u32,
        locked: u32,
        unlocked: u32,
        lit_locked: u32,
        charging: Option<bool>,
        net: Option<String>,
        steps: f64,
        audio: BTreeMap<String, u32>,
        human_prompts: u32,
        agent_prompts: u32,
        voice: u32,
        alarms: Vec<String>,
    }
    let mut s: Vec<S> = (0..n).map(|_| S::default()).collect();
    let idx = |t: Millis| (t >= from && t < to).then(|| ((t - from) / slot_ms) as usize);
    for l in labels {
        let Some(i) = idx(l.ts) else { continue };
        match l.place {
            Place::Away => {
                let k = l.away_kind().map_or("away", |k| k.as_str());
                *s[i].away.entry(k).or_default() += 1;
            }
            _ => {
                let what = format!(
                    "{} {}{}",
                    l.device.as_deref().unwrap_or("?"),
                    l.activity.map_or("?", |a| a.as_str()),
                    l.what
                        .as_deref()
                        .map(|w| format!(" ({w})"))
                        .unwrap_or_default()
                );
                *s[i].uses.entry(what).or_default() += 1;
            }
        }
    }
    let mut last_alarm: Option<i64> = None;
    let mut states: Vec<&Envelope> = Vec::new();
    for e in events {
        match &e.event {
            Event::ScreenMinute(m) => {
                let Some(i) = idx(e.start_ms) else { continue };
                s[i].keys += m.keys;
                s[i].mouse += m.mouse;
                if m.keys == 0 && m.mouse == 0 && m.idle_seconds.is_some_and(|x| x < 90) {
                    s[i].woken += 1;
                }
                if m.locked == Some(true) || m.lid_closed == Some(true) {
                    s[i].locked += 1;
                }
            }
            Event::PhoneState(_) => states.push(e),
            Event::HealthSteps(h) if e.end_ms > e.start_ms => {
                let per_ms = f64::from(h.steps) / (e.end_ms - e.start_ms) as f64;
                for (i, slot) in s.iter_mut().enumerate() {
                    let a = from + i as Millis * slot_ms;
                    let overlap = (e.end_ms.min(a + slot_ms) - e.start_ms.max(a)).max(0);
                    slot.steps += per_ms * overlap as f64;
                }
            }
            Event::Listening(l) => {
                let Some(i) = idx(e.start_ms) else { continue };
                *s[i]
                    .audio
                    .entry(format!("{} {}", e.device.as_str(), l.source))
                    .or_default() += l.audible_seconds;
            }
            Event::PromptSent(p) => {
                let Some(i) = idx(e.start_ms) else { continue };
                if p.origin == Origin::Human {
                    s[i].human_prompts += 1;
                } else {
                    s[i].agent_prompts += 1;
                }
            }
            Event::DictationFinished(_) | Event::ReadAloudPlayed(_) | Event::VoiceModeSpan(_) => {
                if let Some(i) = idx(e.start_ms) {
                    s[i].voice += 1;
                }
            }
            _ => {}
        }
    }
    states.sort_by_key(|e| e.start_ms);
    for e in states {
        let Event::PhoneState(p) = &e.event else {
            continue;
        };
        // The alarm moving on after its time came: dismissed.
        if let Some(prev) = last_alarm {
            if p.alarm_next_ms != Some(prev) {
                let note = if prev <= e.start_ms + MINUTE_MS {
                    format!("alarm for {} turned off", hm(tz, prev))
                } else {
                    match p.alarm_next_ms {
                        Some(n) => format!("alarm changed to {}", hm(tz, n)),
                        None => "alarm removed".into(),
                    }
                };
                if let Some(i) = idx(e.start_ms) {
                    s[i].alarms.push(note);
                }
            }
        } else if let (Some(n), Some(i)) = (p.alarm_next_ms, idx(e.start_ms)) {
            s[i].alarms.push(format!("next alarm {}", ymd_hm(tz, n)));
        }
        last_alarm = p.alarm_next_ms;
        let Some(i) = idx(e.start_ms) else { continue };
        if !p.locked && p.interactive {
            s[i].unlocked += 1;
        } else if p.interactive {
            s[i].lit_locked += 1;
        }
        s[i].charging = Some(p.charging);
        s[i].net = Some(
            time_core::places::resolve(places, &p.network)
                .unwrap_or_else(|| "elsewhere/mobile data".into()),
        );
    }

    let mut out: Vec<(Millis, Millis, String)> = Vec::new();
    for (i, x) in s.iter().enumerate() {
        let mut parts: Vec<String> = Vec::new();
        if !x.uses.is_empty() {
            let mut u: Vec<_> = x.uses.iter().collect();
            u.sort_by(|a, b| b.1.cmp(a.1));
            parts.push(format!(
                "used: {}",
                u.iter()
                    .take(3)
                    .map(|(w, n)| format!("{w} {n}m"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !x.away.is_empty() {
            parts.push(format!(
                "no use: {}",
                x.away
                    .iter()
                    .map(|(k, n)| format!("{k} {n}m"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if x.keys + x.mouse > 0 {
            parts.push(format!(
                "computer input keys {} pointer {}",
                x.keys, x.mouse
            ));
        }
        if x.woken > 0 {
            parts.push(format!(
                "computer idle timer reset with no key/pointer {}m",
                x.woken
            ));
        }
        if x.locked > 0 {
            parts.push(format!("computer locked/lid closed {}m", x.locked));
        }
        if x.unlocked > 0 {
            parts.push(format!("phone unlocked {}m", x.unlocked));
        }
        if x.lit_locked > 0 {
            parts.push(format!("phone screen lit while locked {}m", x.lit_locked));
        }
        if let Some(c) = x.charging {
            parts.push(
                if c {
                    "phone charging"
                } else {
                    "phone on battery"
                }
                .into(),
            );
        }
        if let Some(net) = &x.net {
            parts.push(format!("phone network: {net}"));
        }
        if x.steps >= 1.0 {
            parts.push(format!("steps {}", x.steps.round()));
        }
        for (src, secs) in &x.audio {
            parts.push(format!("audible {src} {secs}s"));
        }
        if x.human_prompts > 0 {
            parts.push(format!("Erik sent {} prompt(s)", x.human_prompts));
        }
        if x.agent_prompts > 0 {
            parts.push(format!(
                "agents (not Erik) sent {} prompt(s)",
                x.agent_prompts
            ));
        }
        if x.voice > 0 {
            parts.push(format!("voice events {}", x.voice));
        }
        parts.extend(x.alarms.iter().cloned());
        let body = if parts.is_empty() {
            "nothing reported".to_owned()
        } else {
            parts.join(" | ")
        };
        let a = from + i as Millis * slot_ms;
        let b = (a + slot_ms).min(to);
        match out.last_mut() {
            Some(last) if last.2 == body => last.1 = b,
            _ => out.push((a, b, body)),
        }
    }
    out.into_iter()
        .map(|(a, b, body)| {
            if b - a > slot_ms {
                format!("{}–{} {body}", hm(tz, a), hm(tz, b))
            } else {
                format!("{} {body}", hm(tz, a))
            }
        })
        .collect()
}

/// `HH:MM` (or `YYYY-MM-DD HH:MM`) as the moment nearest to `near`.
pub fn parse_time(tz: chrono_tz::Tz, s: &str, near: Millis) -> Option<Millis> {
    let s = s.trim();
    let to_ms = |dt: chrono::NaiveDateTime| {
        tz.from_local_datetime(&dt)
            .earliest()
            .map(|t| t.timestamp_millis())
    };
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return to_ms(dt);
    }
    let t = chrono::NaiveTime::parse_from_str(s, "%H:%M").ok()?;
    let day = tz.timestamp_millis_opt(near).single()?.date_naive();
    [day.pred_opt()?, day, day.succ_opt()?]
        .into_iter()
        .filter_map(|d| to_ms(d.and_time(t)))
        .min_by_key(|m| (m - near).abs())
}

// ------------------------------------------------------------------ gaps

const GAP_SYSTEM: &str = "You fill gaps in a timeline of what Erik did on his devices. \
A gap is minutes where he touched no key or pointer and his phone was not in use, between or after minutes of use. \
People read, watch and think without touching anything, so a gap is often still the activity around it; \
but he may also have stood up, gone to the kitchen, or been on another device. \
For each gap, decide until when he was still doing the activity before it (before_until), and, for gaps that have a next activity, \
from when he was already doing the activity after it (after_from). Minutes between before_until and after_from were away. \
Use the five-minute summary around it: the window that stayed on screen, input before and after, phone use, steps, sound. \
A computer idle timer reset without any key or pointer, and prompts sent by agents, are not Erik. \
Times are local HH:MM within the gap: before_until from the gap's start (nothing continued) to its end; after_from from before_until to the gap's end (nothing early). \
Answer JSON only: {\"gaps\":[{\"id\":\"G1\",\"before_until\":\"HH:MM\",\"after_from\":\"HH:MM\",\"confidence\":0.0,\"reason\":\"one short sentence\"}]}";

fn describe(l: &MinuteLabel) -> String {
    format!(
        "{} {}{}",
        l.device.as_deref().unwrap_or("?"),
        l.activity.map_or("?", |a| a.as_str()),
        l.what
            .as_deref()
            .map(|w| format!(" · {w}"))
            .unwrap_or_default()
    )
}

/// What each computer showed during a gap, folded into runs.
fn gap_screens(tz: chrono_tz::Tz, g: &Gap, screens: &HashMap<(String, Millis), Screen>) -> String {
    let Some(d) = g.before.device.as_deref() else {
        return String::new();
    };
    let mut runs: Vec<(Millis, Millis, String)> = Vec::new();
    let mut t = g.start_ms;
    while t < g.end_ms {
        let s = screens
            .get(&(d.to_owned(), t))
            .map_or("nothing reported".into(), |s| {
                let title: String = s.title.chars().take(50).collect();
                format!(
                    "{} \"{title}\"{}",
                    s.class,
                    if s.locked { " (locked)" } else { "" }
                )
            });
        match runs.last_mut() {
            Some(r) if r.2 == s => r.1 = t + MINUTE_MS,
            _ => runs.push((t, t + MINUTE_MS, s)),
        }
        t += MINUTE_MS;
    }
    runs.iter()
        .map(|(a, b, s)| format!("{}–{} {s}", hm(tz, *a), hm(tz, *b)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Local start of the two-hour window holding `t`.
fn window_of(tz: chrono_tz::Tz, t: Millis) -> Millis {
    let Some(local) = tz.timestamp_millis_opt(t).single() else {
        return t - t.rem_euclid(WINDOW_MIN as Millis * MINUTE_MS);
    };
    let mins = local.hour() * 60 + local.minute();
    let back = (mins % WINDOW_MIN) as Millis * MINUTE_MS + local.second() as Millis * 1000;
    t - back
}

fn fills_pass(
    state: &AppState,
    from: Millis,
    to: Millis,
    now: Millis,
    budget: &mut usize,
) -> Result<(usize, usize)> {
    let store = &state.store;
    let tz = state.tz;
    let pad = 2 * 60 * MINUTE_MS;
    let labels = store.raw_labels_between(from - pad, to)?;
    let events = store.events_between(from - pad, to)?;
    let mut screens: HashMap<(String, Millis), Screen> = HashMap::new();
    for e in &events {
        if let Event::ScreenMinute(m) = &e.event {
            screens.insert(
                (
                    e.device.as_str().to_owned(),
                    e.start_ms - e.start_ms.rem_euclid(MINUTE_MS),
                ),
                Screen {
                    class: m.window_class.clone(),
                    title: if m.blocked {
                        "(private window)".into()
                    } else {
                        m.window_title.clone()
                    },
                    locked: m.locked == Some(true) || m.lid_closed == Some(true),
                },
            );
        }
    }
    let (rule_fills, gaps) = time_core::fill::plan(&labels, &screens);
    let mut fills: Vec<(Fill, Option<i64>)> = rule_fills
        .into_iter()
        .filter(|f| f.start_ms >= from)
        .map(|f| (f, None))
        .collect();

    let mut windows: BTreeMap<Millis, Vec<Gap>> = BTreeMap::new();
    for g in gaps.into_iter().filter(|g| g.start_ms >= from) {
        windows
            .entry(window_of(tz, g.start_ms))
            .or_default()
            .push(g);
    }
    let mut asked = 0;
    let mut to_ask: Vec<(String, String, Vec<Gap>)> = Vec::new();
    for (ws, gs) in windows {
        let we = ws + WINDOW_MIN as Millis * MINUTE_MS;
        if we + WINDOW_SETTLE_MS > now {
            continue;
        }
        let ctx_from = ws - 30 * MINUTE_MS;
        let ctx_to = (we + 30 * MINUTE_MS).min(now);
        let ctx_labels: Vec<MinuteLabel> = labels
            .iter()
            .filter(|l| l.ts >= ctx_from && l.ts < ctx_to)
            .cloned()
            .collect();
        let rows = signal_rows(
            &ctx_labels,
            &events,
            &state.config.places,
            tz,
            ctx_from,
            ctx_to,
            5,
        );
        let mut user = format!(
            "Window {} to {} ({}), with half an hour either side. Five-minute summary:\n",
            hm(tz, ws),
            hm(tz, we),
            tz.timestamp_millis_opt(ws)
                .single()
                .map(|d| d.format("%A %Y-%m-%d").to_string())
                .unwrap_or_default()
        );
        for r in &rows {
            let _ = writeln!(user, "{r}");
        }
        user.push_str("\nGaps:\n");
        for (k, g) in gs.iter().enumerate() {
            let _ = writeln!(
                user,
                "G{} {}–{} ({} min, {}): before: {} at {}; after: {}; on screen during it: {}",
                k + 1,
                hm(tz, g.start_ms),
                hm(tz, g.end_ms),
                (g.end_ms - g.start_ms) / MINUTE_MS,
                match g.kind {
                    GapKind::Between => "between two uses",
                    GapKind::Trailing =>
                        "after the last use, then no use for a quarter hour or more",
                },
                describe(&g.before),
                hm(tz, g.before.ts),
                g.after
                    .as_ref()
                    .map_or("nothing (see the summary)".into(), |a| format!(
                        "{} at {}",
                        describe(a),
                        hm(tz, a.ts)
                    )),
                gap_screens(tz, g, &screens),
            );
        }
        let key = ymd_hm(tz, ws);
        match state
            .decider
            .cached(store, "gaps", &key, GAP_SYSTEM, &user)?
        {
            Some(a) => fills.extend(
                model_fills(tz, &gs, &a.value)
                    .into_iter()
                    .map(|f| (f, Some(a.id))),
            ),
            None if *budget > 0 && state.decider.available() => {
                *budget -= 1;
                to_ask.push((key, user, gs));
            }
            None => {}
        }
    }
    // New questions, a few at a time: a backfill is minutes, not an hour.
    for chunk in to_ask.chunks(state.config.decide.parallel.max(1)) {
        let answers: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(key, user, _)| {
                    s.spawn(move || state.decider.ask(store, "gaps", key, GAP_SYSTEM, user, now))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")))
                })
                .collect()
        });
        for ((key, _, gs), answer) in chunk.iter().zip(answers) {
            asked += 1;
            match answer {
                Ok(a) => fills.extend(
                    model_fills(tz, gs, &a.value)
                        .into_iter()
                        .map(|f| (f, Some(a.id))),
                ),
                Err(e) => eprintln!("derive: gaps {key}: {e:#}"),
            }
        }
    }

    let n = fills.len();
    store.with(|c| {
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM fill WHERE start_ms >= ?1 AND start_ms < ?2",
            params![from, to],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO fill (start_ms, end_ms, place, device, activity, what, project,
                    confidence, source, reason, decision_id, computed_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for (f, id) in &fills {
                ins.execute(params![
                    f.start_ms,
                    f.end_ms,
                    serde_json::to_value(f.place)?.as_str().unwrap_or("computer"),
                    f.device,
                    f.activity.as_str(),
                    f.what,
                    f.project,
                    f64::from(f.confidence),
                    f.source.as_str(),
                    f.reason,
                    id,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok((n, asked))
}

/// The model's answer as fills: `[start, before_until)` was the activity
/// before, `[after_from, end)` the one after.
fn model_fills(tz: chrono_tz::Tz, gaps: &[Gap], answer: &Value) -> Vec<Fill> {
    let mut out = Vec::new();
    let Some(list) = answer["gaps"].as_array() else {
        return out;
    };
    for (k, g) in gaps.iter().enumerate() {
        let id = format!("G{}", k + 1);
        let Some(a) = list.iter().find(|a| a["id"].as_str() == Some(id.as_str())) else {
            continue;
        };
        let clamp = |t: Option<Millis>, default: Millis| {
            let t = t.unwrap_or(default).clamp(g.start_ms, g.end_ms);
            t - t.rem_euclid(MINUTE_MS)
        };
        let until = clamp(
            a["before_until"]
                .as_str()
                .and_then(|s| parse_time(tz, s, g.start_ms)),
            g.start_ms,
        );
        let from = clamp(
            a["after_from"]
                .as_str()
                .and_then(|s| parse_time(tz, s, g.end_ms)),
            g.end_ms,
        )
        .max(until);
        let confidence = a["confidence"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0) as f32;
        let reason: String = a["reason"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        if until > g.start_ms {
            if let Some(mut f) = Fill::from_label(&g.before, g.start_ms, until) {
                f.source = FillSource::Model;
                f.confidence = confidence;
                f.reason = reason.clone();
                out.push(f);
            }
        }
        if let Some(after) = &g.after {
            if from < g.end_ms {
                if let Some(mut f) = Fill::from_label(after, from, g.end_ms) {
                    f.source = FillSource::Model;
                    f.confidence = confidence;
                    f.reason = reason.clone();
                    out.push(f);
                }
            }
        }
    }
    out
}

// ----------------------------------------------------------------- sleep

const SLEEP_SYSTEM: &str = "You decide when Erik slept, from what his devices reported. He has no sleep tracker. \
What is known about him: he falls asleep as soon as he stops using his phone or computer at night, and picks one up as soon as he wakes. \
Sometimes he falls asleep as late as 04:00 and sleeps into the afternoon. He may wake, use a device briefly, and sleep again: \
use under about 10 minutes in the middle of the night is an interruption, not waking up. When the phone alarm is turned off and he starts using devices, he is awake. \
Not Erik: prompts sent by agents, a computer idle timer reset with no key or pointer (agents, the 06:00 automatic update), \
a phone screen lit while locked (a notification or the alarm ringing). Steps or the phone on another network mean he is up and out. \
You get the rules' candidate (the longest stretch with no use, bridged over short blips) and a summary of every signal in ten-minute rows. \
Confirm or adjust it. Answer JSON only: {\"fell_asleep\":\"YYYY-MM-DD HH:MM\",\"woke_up\":\"YYYY-MM-DD HH:MM\",\
\"interruptions\":[{\"from\":\"YYYY-MM-DD HH:MM\",\"to\":\"YYYY-MM-DD HH:MM\",\"what\":\"what he did, e.g. phone Clock (alarm)\"}],\"confidence\":0.0,\"reason\":\"one line, under 25 words\"}. \
If the data shows no sleep in this window, answer {\"fell_asleep\":null,\"woke_up\":null,\"interruptions\":[],\"confidence\":0.0,\"reason\":\"...\"}.";

fn what_in(labels: &[MinuteLabel], a: Millis, b: Millis) -> String {
    let mut by: BTreeMap<String, u32> = BTreeMap::new();
    for l in labels
        .iter()
        .filter(|l| l.ts >= a && l.ts < b && l.place != Place::Away)
    {
        *by.entry(format!(
            "{} {}",
            l.device.as_deref().unwrap_or("?"),
            l.what
                .as_deref()
                .or(l.activity.map(|a| a.as_str()))
                .unwrap_or("?")
        ))
        .or_default() += 1;
    }
    by.into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(w, _)| w)
        .unwrap_or_else(|| "a device".into())
}

fn sleep_pass(state: &AppState, night: NaiveDate, now: Millis, budget: &mut usize) -> Result<bool> {
    let store = &state.store;
    let tz = state.tz;
    let (ws, we) = night_bounds(tz, night)?;
    if ws >= now {
        return Ok(false);
    }
    let end = we.min(now);
    let labels = store.raw_labels_between(ws, end)?;
    let done = we <= now;
    let cand: Option<Candidate> = time_core::sleep::candidate(&labels, ws, end);
    let mut n = Night {
        night,
        fell_asleep_ms: cand.as_ref().map(|c| c.start_ms),
        woke_up_ms: cand.as_ref().map(|c| c.end_ms),
        interruptions: cand
            .as_ref()
            .map(|c| {
                c.interruptions
                    .iter()
                    .map(|(a, b)| Interruption {
                        from_ms: *a,
                        to_ms: *b,
                        what: what_in(&labels, *a, *b),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        confidence: None,
        reason: match &cand {
            Some(c) => format!(
                "Rules: no human use of any device from {} to {}{}; not yet confirmed by the model.",
                hm(tz, c.start_ms),
                hm(tz, c.end_ms),
                if c.interruptions.is_empty() {
                    String::new()
                } else {
                    format!(" apart from {} short look(s)", c.interruptions.len())
                }
            ),
            None => "No stretch of 90 minutes or more without a device.".into(),
        },
        source: "rule".into(),
        candidate_start_ms: cand.as_ref().map(|c| c.start_ms),
        candidate_end_ms: cand.as_ref().map(|c| c.end_ms),
        decision_id: None,
        done,
    };
    let mut asked = false;
    if let Some(c) = &cand {
        if done || c.end_ms + SLEEP_SETTLE_MS <= now {
            let in_from = ws.max(c.start_ms - SLEEP_CONTEXT_MS);
            let in_to = we.min(c.end_ms + SLEEP_CONTEXT_MS).min(now);
            let events = store.events_between(in_from - 15 * MINUTE_MS, in_to)?;
            let ctx: Vec<MinuteLabel> = labels
                .iter()
                .filter(|l| l.ts >= in_from && l.ts < in_to)
                .cloned()
                .collect();
            let rows = signal_rows(&ctx, &events, &state.config.places, tz, in_from, in_to, 10);
            let mut user = format!(
                "Night of {} (window {} to {}).\nRules' candidate: asleep {} to {} ({:.1} h){}.\n\nTen-minute summary:\n",
                night.format("%A %Y-%m-%d"),
                ymd_hm(tz, ws),
                ymd_hm(tz, we),
                ymd_hm(tz, c.start_ms),
                ymd_hm(tz, c.end_ms),
                c.minutes() as f64 / 60.0,
                if n.interruptions.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", interrupted by {}",
                        n.interruptions
                            .iter()
                            .map(|i| format!("{}–{} {}", hm(tz, i.from_ms), hm(tz, i.to_ms), i.what))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
            for r in &rows {
                let _ = writeln!(user, "{r}");
            }
            let key = night.to_string();
            let answer = match state
                .decider
                .cached(store, "sleep", &key, SLEEP_SYSTEM, &user)?
            {
                Some(a) => Some(a),
                None if *budget > 0 && state.decider.available() => {
                    *budget -= 1;
                    asked = true;
                    match state
                        .decider
                        .ask(store, "sleep", &key, SLEEP_SYSTEM, &user, now)
                    {
                        Ok(a) => Some(a),
                        Err(e) => {
                            eprintln!("derive: sleep {key}: {e:#}");
                            None
                        }
                    }
                }
                None => None,
            };
            if let Some(a) = answer {
                apply_sleep_answer(tz, &mut n, &a.value, ws, we);
                n.decision_id = Some(a.id);
            }
        }
    }
    save_night(store, &n, now)?;
    Ok(asked)
}

fn apply_sleep_answer(tz: chrono_tz::Tz, n: &mut Night, v: &Value, ws: Millis, we: Millis) {
    let t = |k: &str| {
        v[k].as_str()
            .and_then(|s| parse_time(tz, s, ws))
            .map(|m| m - m.rem_euclid(MINUTE_MS))
    };
    let reason: String = v["reason"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    match (t("fell_asleep"), t("woke_up")) {
        (Some(a), Some(b)) if a < b && a >= ws - 60 * MINUTE_MS && b <= we + 60 * MINUTE_MS => {
            n.fell_asleep_ms = Some(a);
            n.woke_up_ms = Some(b);
            n.interruptions = v["interruptions"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|i| {
                            let p = |k: &str| {
                                i[k].as_str()
                                    .and_then(|s| parse_time(tz, s, a))
                                    .map(|m| m - m.rem_euclid(MINUTE_MS))
                            };
                            let (x, y) = (p("from")?, p("to")?);
                            (x >= a && y <= b && x < y).then(|| Interruption {
                                from_ms: x,
                                to_ms: y,
                                what: i["what"].as_str().unwrap_or("").chars().take(80).collect(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            n.confidence = v["confidence"].as_f64().map(|c| c.clamp(0.0, 1.0));
            n.reason = reason;
            n.source = "model".into();
        }
        (None, None) if v["fell_asleep"].is_null() => {
            n.fell_asleep_ms = None;
            n.woke_up_ms = None;
            n.interruptions.clear();
            n.confidence = v["confidence"].as_f64();
            n.reason = reason;
            n.source = "model".into();
        }
        _ => {
            n.reason = format!(
                "{} (the model's answer was out of range, the rules' stand)",
                n.reason
            );
        }
    }
}

// ------------------------------------------------------------------- run

/// One pass. Returns (fills, questions asked).
pub fn run_once(state: &AppState, now: Millis) -> Result<(usize, usize)> {
    // Labels first: deriving from minutes about to change is wasted.
    if state.store.dirty_stats()?.0 > 0 {
        return Ok((0, 0));
    }
    let tz = state.tz;
    let version = DERIVE_VERSION.to_string();
    let backfill = state.store.meta("derive_version")?.as_deref() != Some(version.as_str());
    let since = state
        .config
        .decide
        .since
        .filter(|_| backfill)
        .and_then(|d| crate::day::bounds(tz, d).ok().map(|b| b.0));
    let from = since.unwrap_or(now - RECENT_MS).min(now - RECENT_MS);
    let mut budget = state.config.decide.max_calls_per_pass;
    let mut asked = 0;
    // Day by day, so a backfill writes as it goes.
    let mut fills = 0;
    let mut a = from;
    while a < now {
        let b = (a + 24 * 60 * MINUTE_MS).min(now);
        let (f, q) = fills_pass(state, a, b, now, &mut budget)?;
        fills += f;
        asked += q;
        a = b;
    }
    let first = crate::day::today(tz, from)
        .pred_opt()
        .unwrap_or(crate::day::today(tz, from));
    let mut night = first;
    while night <= crate::day::today(tz, now) {
        if sleep_pass(state, night, now, &mut budget)? {
            asked += 1;
        }
        night = night.succ_opt().unwrap_or(night);
        if night == first {
            break;
        }
    }
    // A backfill that ran out of questions finishes on the next pass.
    if backfill && budget > 0 {
        state.store.set_meta("derive_version", &version)?;
    }
    Ok((fills, asked))
}

pub async fn run_forever(state: Arc<AppState>) {
    loop {
        let st = state.clone();
        let r = tokio::task::spawn_blocking(move || run_once(&st, st.clock.now_ms())).await;
        match r {
            Ok(Ok((_, asked))) if asked > 0 => {
                eprintln!("derive: asked the decision model {asked} question(s)")
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("derive: pass failed: {e:#}"),
            Err(e) => eprintln!("derive: pass panicked: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(EVERY_SECS)).await;
    }
}

// ----------------------------------------------------------------- spans

/// A stretch of one activity on one device, or of one kind of absence: the
/// timeline as people tell it, with the minutes the rules or the model
/// filled counted in it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span {
    pub start_ms: Millis,
    pub end_ms: Millis,
    /// The activity, or the kind of absence (`sleep`, `idle`, `out`, ...).
    pub activity: String,
    /// What filled it most: site, program, app.
    pub what: Option<String>,
    pub project: Option<String>,
    pub device: Option<String>,
    /// Minutes in it that were gaps, filled.
    pub filled_minutes: u32,
    /// The lowest confidence of any fill in it; 1 when nothing was filled.
    pub confidence: f32,
}

/// Effective labels (with fills) folded into spans.
pub fn spans(labels: &[MinuteLabel], fills: &[Fill]) -> Vec<Span> {
    let key = |l: &MinuteLabel| -> (String, Option<String>) {
        match l.activity {
            Some(a) => (a.as_str().to_owned(), l.device.clone()),
            None => (
                l.away_kind().map_or("away", |k| k.as_str()).to_owned(),
                None,
            ),
        }
    };
    // Each span with minutes by what filled it and by project.
    type Acc = (Span, BTreeMap<String, u32>, BTreeMap<String, u32>);
    let mut out: Vec<Acc> = Vec::new();
    for l in labels {
        let (activity, device) = key(l);
        let filled = l.rule.starts_with("fill.");
        let conf = if filled {
            fills
                .iter()
                .find(|f| f.start_ms <= l.ts && l.ts < f.end_ms)
                .map_or(0.5, |f| f.confidence)
        } else {
            1.0
        };
        match out.last_mut() {
            Some((s, _, _)) if s.activity == activity && s.device == device && s.end_ms == l.ts => {
                s.end_ms += MINUTE_MS;
            }
            _ => out.push((
                Span {
                    start_ms: l.ts,
                    end_ms: l.ts + MINUTE_MS,
                    activity,
                    what: None,
                    project: None,
                    device,
                    filled_minutes: 0,
                    confidence: 1.0,
                },
                BTreeMap::new(),
                BTreeMap::new(),
            )),
        }
        let (s, whats, projects) = out.last_mut().expect("just pushed");
        if filled {
            s.filled_minutes += 1;
            s.confidence = s.confidence.min(conf);
        }
        if let Some(w) = &l.what {
            *whats.entry(w.clone()).or_default() += 1;
        }
        if let Some(p) = &l.project {
            *projects.entry(p.clone()).or_default() += 1;
        }
    }
    out.into_iter()
        .map(|(mut s, w, p)| {
            s.what = w.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
            s.project = p.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
            s
        })
        .collect()
}
