//! What Erik has been doing, in the shape an agent wants it: a compact JSON
//! value plus one plain-English paragraph. Behind the MCP tools
//! (`time_now`, `time_summary`, `time_timeline`) and the one-line hourly
//! summary on `/api/summary/line`.
//!
//! Everything here is a pure function of minute labels and events, so it is
//! tested without a server. Which activities count as productive or
//! unproductive comes from `[mcp]` in `config.toml` ([`McpConfig`]); the rest
//! is neutral (gaming, music, browsing, comms, ...).

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use chrono::TimeZone;
use serde::Serialize;
use serde_json::{json, Value};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, ScreenMinute};
use time_core::label::{Activity, MinuteLabel, Place};

use crate::config::McpConfig;
use crate::store::Store;

/// A phone app must hold the foreground this long to own a minute (the same
/// threshold the timeline uses).
const PHONE_MIN_OVERLAP_MS: Millis = 30_000;
/// Input this recent at the end of a minute means someone was there.
const RECENT_INPUT_SECS: u32 = 90;
/// No event at all for this long and `time_now` says it does not know.
const STALE_MINUTES: i64 = 10;
const LONGEST_WINDOW_HOURS: i64 = 7 * 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Productive,
    Unproductive,
    Neutral,
}

impl Group {
    pub fn of(cfg: &McpConfig, a: Activity) -> Self {
        if cfg.unproductive.contains(&a) {
            Group::Unproductive
        } else if cfg.productive.contains(&a) {
            Group::Productive
        } else {
            Group::Neutral
        }
    }
}

/// JSON for machines, a paragraph for the model to read first.
pub struct Digest {
    pub json: Value,
    pub text: String,
}

impl Digest {
    /// The MCP tool result text: the paragraph, then the compact JSON.
    pub fn tool_text(&self) -> String {
        format!("{}\n\n{}", self.text, self.json)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub label: String,
    pub from_ms: Millis,
    pub to_ms: Millis,
}

fn floor(t: Millis) -> Millis {
    t - t.rem_euclid(MINUTE_MS)
}

/// `today`, `yesterday`, `3h`, `last 90 minutes`, `2d`; a bare number is
/// hours. Windows end now (except yesterday) and reach back at most a week.
pub fn parse_window(spec: &str, tz: chrono_tz::Tz, now: Millis) -> Result<Window> {
    let s: String = spec
        .to_lowercase()
        .replace("last", "")
        .replace("past", "")
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect();
    let day_start = |date| crate::day::bounds(tz, date).map(|(a, _)| a);
    let today = crate::day::today(tz, now);
    match s.as_str() {
        "" | "today" => {
            return Ok(Window {
                label: "today so far".into(),
                from_ms: day_start(today)?,
                to_ms: now,
            })
        }
        "yesterday" => {
            let y = today.pred_opt().unwrap_or(today);
            let (from_ms, to_ms) = crate::day::bounds(tz, y)?;
            return Ok(Window {
                label: "yesterday".into(),
                from_ms,
                to_ms,
            });
        }
        _ => {}
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = &s[digits.len()..];
    let Ok(n) = digits.parse::<i64>() else {
        bail!("window {spec:?}: use today, yesterday, or a length like 30m, 3h, 2d");
    };
    let minutes = match unit {
        "" | "h" | "hr" | "hrs" | "hour" | "hours" => n * 60,
        "m" | "min" | "mins" | "minute" | "minutes" => n,
        "d" | "day" | "days" => n * 24 * 60,
        _ => bail!("window {spec:?}: unit {unit:?} is not m, h or d"),
    };
    if !(1..=LONGEST_WINDOW_HOURS * 60).contains(&minutes) {
        bail!("window {spec:?}: between 1 minute and 7 days");
    }
    let label = if minutes % (24 * 60) == 0 {
        format!("last {}d", minutes / (24 * 60))
    } else if minutes % 60 == 0 {
        format!("last {}h", minutes / 60)
    } else {
        format!("last {minutes}m")
    };
    Ok(Window {
        label,
        from_ms: floor(now - minutes * MINUTE_MS),
        to_ms: now,
    })
}

/// Labels and events for `[from, to)`. Screen events start a minute early so
/// the first minute is attributed.
pub fn load(store: &Store, from: Millis, to: Millis) -> Result<(Vec<MinuteLabel>, Vec<Envelope>)> {
    Ok((
        store.labels_between(from, to)?,
        store.events_between(from - MINUTE_MS, to)?,
    ))
}

// -- attribution ------------------------------------------------------------

/// What was in front of Erik during one labelled minute.
#[derive(Debug, Clone, Default)]
struct Attr {
    app: String,
    site: Option<String>,
    title: String,
}

struct Index<'e> {
    screens: HashMap<(&'e str, Millis), &'e ScreenMinute>,
    phone: Vec<&'e Envelope>,
}

impl<'e> Index<'e> {
    fn new(events: &'e [Envelope]) -> Self {
        let mut screens = HashMap::new();
        let mut phone = Vec::new();
        for e in events {
            match &e.event {
                Event::ScreenMinute(m) => {
                    screens.insert((e.device.as_str(), floor(e.start_ms)), m);
                }
                Event::PhoneApp(_) => phone.push(e),
                _ => {}
            }
        }
        Self { screens, phone }
    }

    fn attribute(&self, l: &MinuteLabel) -> Option<Attr> {
        let device = l.device.as_deref()?;
        if let Some(m) = self.screens.get(&(device, l.ts)) {
            if m.blocked {
                return Some(Attr {
                    app: "(private window)".into(),
                    ..Default::default()
                });
            }
            return Some(Attr {
                app: m.window_class.clone(),
                site: m.browser_host.clone(),
                title: m.window_title.clone(),
            });
        }
        let end = l.ts + MINUTE_MS;
        self.phone
            .iter()
            .filter(|e| e.device == device)
            .map(|e| (*e, e.end_ms.min(end) - e.start_ms.max(l.ts)))
            .filter(|(_, o)| *o >= PHONE_MIN_OVERLAP_MS)
            .max_by_key(|(_, o)| *o)
            .and_then(|(e, _)| match &e.event {
                Event::PhoneApp(a) => Some(Attr {
                    app: if a.label.is_empty() {
                        a.package.clone()
                    } else {
                        a.label.clone()
                    },
                    ..Default::default()
                }),
                _ => None,
            })
    }
}

/// Minutes per name, remembering what they were.
#[derive(Default)]
struct Tally(HashMap<String, (u32, BTreeMap<Activity, u32>)>);

impl Tally {
    fn add(&mut self, name: &str, a: Activity) {
        if name.is_empty() {
            return;
        }
        let slot = self.0.entry(name.to_owned()).or_default();
        slot.0 += 1;
        *slot.1.entry(a).or_default() += 1;
    }

    /// Most minutes first, ties by name so output is stable.
    fn top(&self, n: usize) -> Vec<(String, u32, Activity)> {
        let mut v: Vec<_> = self
            .0
            .iter()
            .map(|(name, (m, acts))| {
                let main = acts
                    .iter()
                    .max_by_key(|(a, c)| (**c, std::cmp::Reverse(**a)))
                    .map(|(a, _)| *a)
                    .unwrap_or(Activity::Unknown);
                (name.clone(), *m, main)
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

// -- formatting -------------------------------------------------------------

fn dur(minutes: u32) -> String {
    match minutes {
        0..=59 => format!("{minutes}m"),
        _ if minutes.is_multiple_of(60) => format!("{}h", minutes / 60),
        _ => format!("{}h {:02}m", minutes / 60, minutes % 60),
    }
}

fn local(tz: chrono_tz::Tz, ms: Millis) -> chrono::DateTime<chrono_tz::Tz> {
    tz.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| tz.timestamp_millis_opt(0).unwrap())
}

fn hhmm(tz: chrono_tz::Tz, ms: Millis) -> String {
    local(tz, ms).format("%H:%M").to_string()
}

fn iso(tz: chrono_tz::Tz, ms: Millis) -> String {
    local(tz, ms).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn items(v: &[(String, u32)]) -> String {
    v.iter()
        .map(|(n, m)| format!("{n} {}", dur(*m)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(n - 1).collect();
    out.push('…');
    out
}

// -- stats ------------------------------------------------------------------

struct Stats {
    /// Minutes per activity, computer and phone.
    by_activity: BTreeMap<Activity, (u32, u32)>,
    away: u32,
    productive: u32,
    unproductive: u32,
    neutral: u32,
    apps: Tally,
    sites: Tally,
    projects: BTreeMap<String, u32>,
    devices: BTreeMap<String, u32>,
}

impl Stats {
    fn new(cfg: &McpConfig, labels: &[MinuteLabel], events: &[Envelope]) -> Self {
        let idx = Index::new(events);
        let mut s = Stats {
            by_activity: BTreeMap::new(),
            away: 0,
            productive: 0,
            unproductive: 0,
            neutral: 0,
            apps: Tally::default(),
            sites: Tally::default(),
            projects: BTreeMap::new(),
            devices: BTreeMap::new(),
        };
        for l in labels {
            let Some(a) = l.activity else {
                s.away += 1;
                continue;
            };
            let slot = s.by_activity.entry(a).or_default();
            if l.place == Place::Phone {
                slot.1 += 1;
            } else {
                slot.0 += 1;
            }
            match Group::of(cfg, a) {
                Group::Productive => s.productive += 1,
                Group::Unproductive => s.unproductive += 1,
                Group::Neutral => s.neutral += 1,
            }
            if let Some(p) = &l.project {
                *s.projects.entry(p.clone()).or_default() += 1;
            }
            if let Some(d) = &l.device {
                *s.devices.entry(d.clone()).or_default() += 1;
            }
            if let Some(at) = idx.attribute(l) {
                s.apps.add(&at.app, a);
                if let Some(site) = &at.site {
                    s.sites.add(site, a);
                }
            }
        }
        s
    }

    fn tracked(&self) -> u32 {
        self.productive + self.unproductive + self.neutral
    }

    /// Activities, biggest first, as (name, minutes).
    fn categories(&self) -> Vec<(Activity, u32, u32, u32)> {
        let mut v: Vec<_> = self
            .by_activity
            .iter()
            .map(|(a, (c, p))| (*a, c + p, *c, *p))
            .collect();
        v.sort_by_key(|(a, total, _, _)| (std::cmp::Reverse(*total), *a));
        v
    }

    fn unproductive_detail(&self, cfg: &McpConfig) -> Vec<(String, u32)> {
        self.categories()
            .into_iter()
            .filter(|(a, ..)| cfg.unproductive.contains(a))
            .map(|(a, total, ..)| (a.to_string(), total))
            .collect()
    }

    /// "work 38m, video 12m, away 10m"
    fn breakdown(&self) -> String {
        let mut parts: Vec<(String, u32)> = self
            .categories()
            .into_iter()
            .map(|(a, t, ..)| (a.to_string(), t))
            .collect();
        if self.away > 0 {
            parts.push(("away".into(), self.away));
        }
        items(&parts)
    }
}

fn top_json(t: &Tally, cfg: &McpConfig, n: usize) -> Value {
    Value::Array(
        t.top(n)
            .into_iter()
            .map(|(name, minutes, a)| {
                json!({"name": name, "minutes": minutes, "activity": a, "group": Group::of(cfg, a)})
            })
            .collect(),
    )
}

fn names(t: &Tally, n: usize) -> String {
    items(
        &t.top(n)
            .into_iter()
            .map(|(name, m, _)| (name, m))
            .collect::<Vec<_>>(),
    )
}

// -- time_summary -----------------------------------------------------------

pub fn summary(
    cfg: &McpConfig,
    tz: chrono_tz::Tz,
    w: &Window,
    labels: &[MinuteLabel],
    events: &[Envelope],
) -> Digest {
    let s = Stats::new(cfg, labels, events);
    let span_min = ((w.to_ms - w.from_ms) / MINUTE_MS).max(0) as u32;
    let labelled = labels.len() as u32;
    let missing = span_min.saturating_sub(labelled);

    let m = crate::day::modalities(events, w.from_ms, w.to_ms);
    let commits = events
        .iter()
        .filter(|e| {
            matches!(e.event, Event::Commit(_)) && e.start_ms >= w.from_ms && e.start_ms < w.to_ms
        })
        .count() as u32;

    let categories: Vec<Value> = s
        .categories()
        .into_iter()
        .map(|(a, total, computer, phone)| {
            json!({"activity": a, "minutes": total, "computer": computer, "phone": phone,
                   "group": Group::of(cfg, a)})
        })
        .collect();
    let projects = {
        let mut v: Vec<_> = s.projects.iter().collect();
        v.sort_by_key(|(n, m)| (std::cmp::Reverse(**m), (*n).clone()));
        v.into_iter()
            .take(5)
            .map(|(n, m)| json!({"name": n, "minutes": m}))
            .collect::<Vec<_>>()
    };
    let json = json!({
        "window": {
            "label": w.label,
            "from": iso(tz, w.from_ms),
            "to": iso(tz, w.to_ms),
            "minutes": span_min,
        },
        "tracked_minutes": s.tracked(),
        "away_minutes": s.away,
        "unlabelled_minutes": missing,
        "totals": {
            "productive": s.productive,
            "unproductive": s.unproductive,
            "neutral": s.neutral,
        },
        "categories": categories,
        "top_apps": top_json(&s.apps, cfg, 8),
        "top_sites": top_json(&s.sites, cfg, 8),
        "projects": projects,
        "devices": s.devices,
        "signals": {
            "typing_minutes": m.typing_minutes,
            "prompts_by_erik": m.prompts_typed + m.prompts_dictated,
            "commits": commits,
        },
        "groups": {
            "productive": cfg.productive,
            "unproductive": cfg.unproductive,
        },
    });

    let mut text = format!(
        "{} ({}–{}): ",
        capitalise(&w.label),
        hhmm(tz, w.from_ms),
        hhmm(tz, w.to_ms)
    );
    if s.tracked() == 0 && s.away == 0 {
        text.push_str("no data in this window.");
        return Digest { json, text };
    }
    text.push_str(&format!("{}.", s.breakdown()));
    text.push_str(&format!(
        " {} productive, {} neutral, {} unproductive",
        dur(s.productive),
        dur(s.neutral),
        dur(s.unproductive)
    ));
    let un = s.unproductive_detail(cfg);
    if un.is_empty() {
        text.push('.');
    } else {
        text.push_str(&format!(" ({}).", items(&un)));
    }
    if !s.apps.0.is_empty() {
        text.push_str(&format!(" Mostly in {}.", names(&s.apps, 3)));
    }
    if !s.sites.0.is_empty() {
        text.push_str(&format!(" Sites: {}.", names(&s.sites, 3)));
    }
    if m.typing_minutes > 0 || commits > 0 {
        text.push_str(&format!(
            " Typed in {} of those minutes, {} prompt(s) to agents, {commits} commit(s).",
            m.typing_minutes,
            m.prompts_typed + m.prompts_dictated
        ));
    }
    if missing > span_min / 10 && missing > 5 {
        text.push_str(&format!(
            " {} of the window has no data (devices offline or not yet synced), so these totals are a floor.",
            dur(missing)
        ));
    }
    Digest { json, text }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

// -- time_timeline ----------------------------------------------------------

struct Block {
    from: Millis,
    to: Millis,
    activity: Option<Activity>,
    device: Option<String>,
    apps: Tally,
    sites: Tally,
    projects: BTreeMap<String, u32>,
}

impl Block {
    fn minutes(&self) -> u32 {
        ((self.to - self.from) / MINUTE_MS) as u32
    }

    fn describe(&self, cfg: &McpConfig, tz: chrono_tz::Tz) -> String {
        let what = self.activity.map_or("away".to_owned(), |a| a.to_string());
        let on = self
            .device
            .as_deref()
            .map_or(String::new(), |d| format!(" on {d}"));
        let _ = cfg;
        format!(
            "{what} {}{on} ({}–{})",
            dur(self.minutes()),
            hhmm(tz, self.from),
            hhmm(tz, self.to)
        )
    }
}

pub fn timeline(
    cfg: &McpConfig,
    tz: chrono_tz::Tz,
    w: &Window,
    labels: &[MinuteLabel],
    events: &[Envelope],
    min_minutes: u32,
    limit: usize,
) -> Digest {
    let idx = Index::new(events);
    let mut blocks: Vec<Block> = Vec::new();
    for l in labels {
        let joins = blocks
            .last()
            .is_some_and(|b| b.to == l.ts && b.activity == l.activity && b.device == l.device);
        if !joins {
            blocks.push(Block {
                from: l.ts,
                to: l.ts,
                activity: l.activity,
                device: l.device.clone(),
                apps: Tally::default(),
                sites: Tally::default(),
                projects: BTreeMap::new(),
            });
        }
        let b = blocks.last_mut().expect("just pushed");
        b.to = l.ts + MINUTE_MS;
        if let (Some(a), Some(at)) = (l.activity, idx.attribute(l)) {
            b.apps.add(&at.app, a);
            if let Some(site) = &at.site {
                b.sites.add(site, a);
            }
        }
        if let Some(p) = &l.project {
            *b.projects.entry(p.clone()).or_default() += 1;
        }
    }
    let total_blocks = blocks.len();
    let hidden_minutes: u32 = blocks
        .iter()
        .filter(|b| b.minutes() < min_minutes)
        .map(Block::minutes)
        .sum();
    blocks.retain(|b| b.minutes() >= min_minutes);
    let earlier = blocks.len().saturating_sub(limit);
    blocks.drain(..earlier);

    let rows: Vec<Value> = blocks
        .iter()
        .map(|b| {
            let project = b
                .projects
                .iter()
                .max_by_key(|(n, m)| (**m, std::cmp::Reverse((*n).clone())))
                .map(|(n, _)| n.clone());
            json!({
                "from": hhmm(tz, b.from),
                "to": hhmm(tz, b.to),
                "minutes": b.minutes(),
                "activity": b.activity.map_or("away".to_owned(), |a| a.to_string()),
                "group": b.activity.map(|a| Group::of(cfg, a)),
                "device": b.device,
                "apps": b.apps.top(3).into_iter().map(|(n, ..)| n).collect::<Vec<_>>(),
                "sites": b.sites.top(3).into_iter().map(|(n, ..)| n).collect::<Vec<_>>(),
                "project": project,
            })
        })
        .collect();
    let json = json!({
        "window": {
            "label": w.label,
            "from": iso(tz, w.from_ms),
            "to": iso(tz, w.to_ms),
        },
        "blocks": rows,
        "blocks_total": total_blocks,
        "hidden_short_minutes": hidden_minutes,
        "earlier_blocks_cut": earlier,
    });

    let mut text = format!(
        "{} ({}–{}): ",
        capitalise(&w.label),
        hhmm(tz, w.from_ms),
        hhmm(tz, w.to_ms)
    );
    match (blocks.iter().max_by_key(|b| b.minutes()), blocks.last()) {
        (Some(longest), Some(last)) => {
            text.push_str(&format!(
                "{} block(s) of {min_minutes}+ minutes. Longest: {}. Latest: {}.",
                blocks.len(),
                longest.describe(cfg, tz),
                last.describe(cfg, tz)
            ));
            if earlier > 0 {
                text.push_str(&format!(
                    " {earlier} earlier block(s) cut; ask for a shorter window."
                ));
            }
        }
        _ => text.push_str("no data in this window."),
    }
    Digest { json, text }
}

// -- time_now ---------------------------------------------------------------

/// The last minutes of context `time_now` and the hourly line look at.
pub const NOW_LOOKBACK_MIN: i64 = 60;

fn device_state(
    events: &[Envelope],
    device: &str,
    now: Millis,
) -> Option<(Millis, String, Option<Attr>)> {
    let e = events.iter().rev().find(|e| {
        e.device == device
            && matches!(
                e.event,
                Event::ScreenMinute(_) | Event::PhoneApp(_) | Event::PhoneState(_)
            )
            && e.start_ms <= now
    })?;
    let (state, attr) = match &e.event {
        Event::ScreenMinute(m) if m.locked == Some(true) => ("locked".to_owned(), None),
        Event::ScreenMinute(m) if m.lid_closed == Some(true) => ("lid closed".to_owned(), None),
        Event::ScreenMinute(m) => {
            let idle = m.idle_seconds.unwrap_or(0);
            let state = if m.keys == 0 && m.mouse == 0 && idle >= RECENT_INPUT_SECS {
                format!("idle for {}", dur((idle / 60).max(1)))
            } else {
                "in use".to_owned()
            };
            (
                state,
                (!m.blocked).then(|| Attr {
                    app: m.window_class.clone(),
                    site: m.browser_host.clone(),
                    title: m.window_title.clone(),
                }),
            )
        }
        Event::PhoneApp(a) => (
            "in use".to_owned(),
            Some(Attr {
                app: if a.label.is_empty() {
                    a.package.clone()
                } else {
                    a.label.clone()
                },
                ..Default::default()
            }),
        ),
        Event::PhoneState(s) => (
            if s.locked {
                "locked".to_owned()
            } else {
                "unlocked".to_owned()
            },
            None,
        ),
        _ => return None,
    };
    Some((e.end_ms.max(e.start_ms), state, attr))
}

pub fn now(
    cfg: &McpConfig,
    tz: chrono_tz::Tz,
    now_ms: Millis,
    labels: &[MinuteLabel],
    events: &[Envelope],
) -> Digest {
    let idx = Index::new(events);
    let hour = Stats::new(cfg, labels, events);

    let last5_from = floor(now_ms) - 5 * MINUTE_MS;
    let last5: Vec<&MinuteLabel> = labels.iter().filter(|l| l.ts >= last5_from).collect();
    let mut by5: BTreeMap<String, u32> = BTreeMap::new();
    for l in &last5 {
        *by5.entry(l.activity.map_or("away".into(), |a| a.to_string()))
            .or_default() += 1;
    }

    let newest_event = events.iter().map(|e| e.end_ms.max(e.start_ms)).max();
    let stale = newest_event.is_none_or(|t| now_ms - t > STALE_MINUTES * MINUTE_MS);

    // The latest labelled minute, and the run of the same activity before it.
    let latest = labels.last();
    let current = latest.filter(|l| l.activity.is_some());
    let run = current.map_or(0, |c| {
        labels
            .iter()
            .rev()
            .take_while(|l| l.activity == c.activity && l.device == c.device)
            .count()
    });
    let attr = current.and_then(|l| idx.attribute(l));
    let streak = labels
        .iter()
        .rev()
        .take_while(|l| l.activity.is_some_and(|a| cfg.unproductive.contains(&a)))
        .count();

    let mut device_rows = Vec::new();
    let mut seen: Vec<&str> = events.iter().map(|e| e.device.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();
    for d in seen {
        if let Some((at, state, a)) = device_state(events, d, now_ms) {
            let ago = ((now_ms - at).max(0) / MINUTE_MS) as u32;
            if ago > 30 {
                continue;
            }
            device_rows.push(json!({
                "device": d,
                "last_seen_minutes_ago": ago,
                "state": state,
                "app": a.as_ref().map(|a| a.app.clone()).filter(|s| !s.is_empty()),
                "site": a.as_ref().and_then(|a| a.site.clone()),
            }));
        }
    }

    let activity = current.and_then(|l| l.activity);
    let active = activity.is_some() && !stale;
    let current_json = current.map(|l| {
        json!({
            "activity": activity,
            "group": activity.map(|a| Group::of(cfg, a)),
            "device": l.device,
            "app": attr.as_ref().map(|a| a.app.clone()).filter(|s| !s.is_empty()),
            "site": attr.as_ref().and_then(|a| a.site.clone()),
            "title": attr.as_ref().map(|a| clip(&a.title, 100)).filter(|s| !s.is_empty()),
            "project": l.project,
            "for_minutes": run,
        })
    });
    let json = json!({
        "at": iso(tz, now_ms),
        "active": active,
        "idle": !active,
        "stale": stale,
        "newest_event_minutes_ago": newest_event.map(|t| ((now_ms - t).max(0) / MINUTE_MS) as u32),
        "current": current_json,
        "last_5_min": by5,
        "last_hour": {
            "productive": hour.productive,
            "unproductive": hour.unproductive,
            "neutral": hour.neutral,
            "away": hour.away,
        },
        "unproductive_streak_minutes": streak,
        "devices": device_rows,
    });

    let text = if stale {
        match newest_event {
            Some(t) => format!(
                "No device has reported for {}, so I do not know what Erik is doing right now.",
                dur(((now_ms - t) / MINUTE_MS) as u32)
            ),
            None => {
                "No data at all for the last hour, so I do not know what Erik is doing right now."
                    .to_owned()
            }
        }
    } else if let (Some(a), Some(l)) = (activity, current) {
        let mut t = format!(
            "At {} Erik is on {} doing {a} ({}), for {} so far",
            hhmm(tz, now_ms),
            l.device.as_deref().unwrap_or("?"),
            match Group::of(cfg, a) {
                Group::Productive => "productive",
                Group::Unproductive => "unproductive",
                Group::Neutral => "neutral",
            },
            dur(run as u32)
        );
        if let Some(at) = &attr {
            let place = match (&at.site, at.app.is_empty()) {
                (Some(site), false) => format!(" in {} on {site}", at.app),
                (Some(site), true) => format!(" on {site}"),
                (None, false) => format!(" in {}", at.app),
                (None, true) => String::new(),
            };
            t.push_str(&place);
            if !at.title.is_empty() {
                t.push_str(&format!(" (\"{}\")", clip(&at.title, 60)));
            }
        }
        t.push('.');
        if let Some(p) = &l.project {
            t.push_str(&format!(" Project: {p}."));
        }
        if streak >= 10 {
            t.push_str(&format!(
                " That is {} of unproductive time in a row.",
                dur(streak as u32)
            ));
        }
        t.push_str(&format!(
            " Last hour: {} productive, {} unproductive, {} neutral, {} away.",
            dur(hour.productive),
            dur(hour.unproductive),
            dur(hour.neutral),
            dur(hour.away)
        ));
        t
    } else {
        format!(
            "At {} Erik is away from every device (idle or not at the computer). Last hour: {} productive, {} unproductive, {} neutral, {} away.",
            hhmm(tz, now_ms),
            dur(hour.productive),
            dur(hour.unproductive),
            dur(hour.neutral),
            dur(hour.away)
        )
    };
    Digest { json, text }
}

// -- the hourly line --------------------------------------------------------

/// One line for the hourly review: the last hour in a sentence.
pub fn line(
    cfg: &McpConfig,
    tz: chrono_tz::Tz,
    now_ms: Millis,
    labels: &[MinuteLabel],
    events: &[Envelope],
) -> String {
    let s = Stats::new(cfg, labels, events);
    let from = floor(now_ms) - NOW_LOOKBACK_MIN * MINUTE_MS;
    let head = format!("Last hour ({}–{}): ", hhmm(tz, from), hhmm(tz, now_ms));
    if s.tracked() == 0 && s.away == 0 {
        return format!("{head}no data.");
    }
    let un = s.unproductive_detail(cfg);
    let mut out = format!(
        "{head}{} — {} productive, {} unproductive",
        s.breakdown(),
        dur(s.productive),
        dur(s.unproductive)
    );
    if !un.is_empty() {
        out.push_str(&format!(" ({})", items(&un)));
    }
    out.push('.');
    let top = s.apps.top(1);
    if let Some((app, ..)) = top.first() {
        out.push_str(&format!(" Mostly {app}."));
    }
    let n = now(cfg, tz, now_ms, labels, events).json;
    let current = &n["current"];
    out.push_str(
        &match (n["stale"].as_bool(), current["activity"].as_str()) {
            (Some(true), _) => " Now: no device has reported lately.".to_owned(),
            (_, Some(a)) => {
                let place = current["site"]
                    .as_str()
                    .or(current["app"].as_str())
                    .map_or(String::new(), |p| format!(" on {p}"));
                let device = current["device"]
                    .as_str()
                    .map_or(String::new(), |d| format!(" ({d})"));
                format!(
                    " Now: {a}{place}{device}, {}.",
                    dur(current["for_minutes"].as_u64().unwrap_or(0) as u32)
                )
            }
            _ => " Now: away.".to_owned(),
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::event::PhoneApp;
    use time_core::label::Decider;

    const TZ: chrono_tz::Tz = chrono_tz::Europe::Stockholm;
    /// 2026-10-23 12:00 Stockholm (UTC+2).
    const T0: Millis = 1_792_749_600_000;
    const HOUR_MS: Millis = 60 * MINUTE_MS;

    fn cfg() -> McpConfig {
        McpConfig::default()
    }

    fn label(i: i64, activity: Option<Activity>, device: &str) -> MinuteLabel {
        MinuteLabel {
            ts: T0 + i * MINUTE_MS,
            place: if activity.is_none() {
                Place::Away
            } else if device == "phone" {
                Place::Phone
            } else {
                Place::Computer
            },
            device: activity.map(|_| device.to_owned()),
            activity,
            project: activity
                .filter(|a| *a == Activity::Work)
                .map(|_| "time".to_owned()),
            background: None,
            background_device: None,
            rule: "test".into(),
            decider: Decider::Rule,
            complete: true,
            location: None,
        }
    }

    fn screen(i: i64, class: &str, host: Option<&str>, title: &str) -> Envelope {
        Envelope::new(
            "pc",
            "agent",
            T0 + i * MINUTE_MS,
            T0 + (i + 1) * MINUTE_MS,
            Event::ScreenMinute(ScreenMinute {
                window_class: class.into(),
                window_title: title.into(),
                browser_host: host.map(Into::into),
                keys: 5,
                ..Default::default()
            }),
        )
    }

    /// 30 minutes of work, 12 of YouTube, 8 of gaming, 10 away.
    fn morning() -> (Vec<MinuteLabel>, Vec<Envelope>) {
        let mut labels = Vec::new();
        let mut events = Vec::new();
        for i in 0..30 {
            labels.push(label(i, Some(Activity::Work), "pc"));
            events.push(screen(i, "com.t3tools.T3Code", None, "agent thread"));
        }
        for i in 30..42 {
            labels.push(label(i, Some(Activity::Video), "pc"));
            events.push(screen(
                i,
                "firefox",
                Some("youtube.com"),
                "Some video - YouTube",
            ));
        }
        for i in 42..50 {
            labels.push(label(i, Some(Activity::Games), "pc"));
            events.push(screen(i, "steam_app_1", None, "A game"));
        }
        for i in 50..60 {
            labels.push(label(i, None, "pc"));
        }
        (labels, events)
    }

    #[test]
    fn windows_parse() {
        let now = T0 + 5 * HOUR_MS;
        let w = |s: &str| parse_window(s, TZ, now).unwrap();
        assert_eq!(w("3h").from_ms, now - 3 * HOUR_MS);
        assert_eq!(w("last 90 minutes").from_ms, now - 90 * MINUTE_MS);
        assert_eq!(w("last_3_hours").label, "last 3h");
        assert_eq!(w("2").label, "last 2h");
        let today = w("today");
        assert_eq!(today.to_ms, now);
        assert_eq!(today.from_ms, T0 - 12 * HOUR_MS, "local midnight");
        let y = w("yesterday");
        assert_eq!(y.to_ms, today.from_ms);
        assert!(parse_window("tomorrow", TZ, now).is_err());
        assert!(parse_window("0m", TZ, now).is_err());
        assert!(parse_window("9d", TZ, now).is_err());
        assert!(parse_window("5x", TZ, now).is_err());
    }

    #[test]
    fn summary_groups_games_as_neutral_and_video_as_unproductive() {
        let (labels, events) = morning();
        let w = Window {
            label: "last 1h".into(),
            from_ms: T0,
            to_ms: T0 + 60 * MINUTE_MS,
        };
        let d = summary(&cfg(), TZ, &w, &labels, &events);
        let t = &d.json["totals"];
        assert_eq!(t["productive"], 30);
        assert_eq!(t["unproductive"], 12);
        assert_eq!(t["neutral"], 8, "gaming is fine");
        assert_eq!(d.json["away_minutes"], 10);
        assert_eq!(d.json["unlabelled_minutes"], 0);
        assert_eq!(d.json["top_sites"][0]["name"], "youtube.com");
        assert_eq!(d.json["top_sites"][0]["group"], "unproductive");
        assert_eq!(d.json["top_apps"][0]["name"], "com.t3tools.T3Code");
        assert_eq!(d.json["projects"][0]["name"], "time");
        assert!(
            d.text.contains("work 30m, video 12m, games 8m, away 10m"),
            "{}",
            d.text
        );
        assert!(
            d.text.contains("12m unproductive (video 12m)"),
            "{}",
            d.text
        );
        // Tuned: gaming counts against him after all.
        let mut strict = cfg();
        strict.unproductive.push(Activity::Games);
        let d = summary(&strict, TZ, &w, &labels, &events);
        assert_eq!(d.json["totals"]["unproductive"], 20);
        assert_eq!(d.json["totals"]["neutral"], 0);
    }

    #[test]
    fn summary_admits_missing_data() {
        let (labels, events) = morning();
        let w = Window {
            label: "last 3h".into(),
            from_ms: T0 - 2 * HOUR_MS,
            to_ms: T0 + 60 * MINUTE_MS,
        };
        let d = summary(&cfg(), TZ, &w, &labels, &events);
        assert_eq!(d.json["unlabelled_minutes"], 120);
        assert!(d.text.contains("a floor"), "{}", d.text);
        let empty = summary(&cfg(), TZ, &w, &[], &[]);
        assert!(empty.text.contains("no data"), "{}", empty.text);
    }

    #[test]
    fn now_says_what_he_is_doing_and_how_long() {
        let (labels, events) = morning();
        let labels = &labels[..42]; // up to the end of the YouTube stretch
        let now_ms = T0 + 42 * MINUTE_MS + 20_000;
        let d = now(&cfg(), TZ, now_ms, labels, &events);
        assert_eq!(d.json["active"], true);
        assert_eq!(d.json["current"]["activity"], "video");
        assert_eq!(d.json["current"]["group"], "unproductive");
        assert_eq!(d.json["current"]["site"], "youtube.com");
        assert_eq!(d.json["current"]["app"], "firefox");
        assert_eq!(d.json["current"]["for_minutes"], 12);
        assert_eq!(d.json["unproductive_streak_minutes"], 12);
        assert_eq!(d.json["last_5_min"]["video"], 5);
        assert!(d.text.contains("doing video (unproductive)"), "{}", d.text);
        assert!(d.text.contains("youtube.com"), "{}", d.text);
        assert!(
            d.text.contains("12m of unproductive time in a row"),
            "{}",
            d.text
        );
    }

    #[test]
    fn now_away_and_stale() {
        let (labels, events) = morning();
        let now_ms = T0 + 60 * MINUTE_MS + 20_000;
        // Events end at minute 50; the desk has been empty since, but the
        // devices are still talking (heartbeat-like screen minutes).
        let mut ev = events.clone();
        for i in 50..60 {
            let mut e = screen(i, "", None, "");
            if let Event::ScreenMinute(m) = &mut e.event {
                m.keys = 0;
                m.idle_seconds = Some(600);
            }
            ev.push(e);
        }
        let d = now(&cfg(), TZ, now_ms, &labels, &ev);
        assert_eq!(d.json["idle"], true);
        assert_eq!(d.json["current"], Value::Null);
        assert!(d.text.contains("away from every device"), "{}", d.text);
        assert_eq!(d.json["devices"][0]["state"], "idle for 10m");
        // Nobody has reported for an hour: say so, do not guess.
        let d = now(&cfg(), TZ, now_ms + 2 * HOUR_MS, &labels, &events);
        assert_eq!(d.json["stale"], true);
        assert!(d.text.contains("do not know"), "{}", d.text);
    }

    #[test]
    fn phone_minutes_name_the_app() {
        let labels = vec![
            label(0, Some(Activity::Social), "phone"),
            label(1, Some(Activity::Social), "phone"),
        ];
        let events = vec![Envelope::new(
            "phone",
            "app",
            T0,
            T0 + 2 * MINUTE_MS,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                label: "X".into(),
                media_playing: false,
            }),
        )];
        let w = Window {
            label: "last 5m".into(),
            from_ms: T0,
            to_ms: T0 + 2 * MINUTE_MS,
        };
        let d = summary(&cfg(), TZ, &w, &labels, &events);
        assert_eq!(d.json["top_apps"][0]["name"], "X");
        assert_eq!(d.json["categories"][0]["phone"], 2);
    }

    #[test]
    fn timeline_merges_runs_and_cuts_blips() {
        let (mut labels, events) = morning();
        // A one-minute blip of comms inside the work stretch.
        labels[10] = label(10, Some(Activity::Comms), "pc");
        let w = Window {
            label: "last 1h".into(),
            from_ms: T0,
            to_ms: T0 + 60 * MINUTE_MS,
        };
        let d = timeline(&cfg(), TZ, &w, &labels, &events, 2, 60);
        let blocks = d.json["blocks"].as_array().unwrap();
        let acts: Vec<_> = blocks
            .iter()
            .map(|b| b["activity"].as_str().unwrap())
            .collect();
        assert_eq!(acts, ["work", "work", "video", "games", "away"]);
        assert_eq!(d.json["hidden_short_minutes"], 1);
        assert_eq!(blocks[2]["sites"][0], "youtube.com");
        assert_eq!(blocks[2]["from"], "12:30");
        assert_eq!(blocks[2]["group"], "unproductive");
        let d = timeline(&cfg(), TZ, &w, &labels, &events, 1, 2);
        assert_eq!(d.json["blocks"].as_array().unwrap().len(), 2);
        assert_eq!(d.json["earlier_blocks_cut"], 4);
    }

    #[test]
    fn the_hourly_line_is_one_line() {
        let (labels, events) = morning();
        let l = line(&cfg(), TZ, T0 + 60 * MINUTE_MS, &labels, &events);
        assert!(!l.contains('\n'));
        assert!(
            l.starts_with("Last hour (12:00–13:00): work 30m, video 12m, games 8m, away 10m"),
            "{l}"
        );
        assert!(
            l.contains("30m productive, 12m unproductive (video 12m)"),
            "{l}"
        );
        assert!(line(&cfg(), TZ, T0, &[], &[]).ends_with("no data."));
    }
}
