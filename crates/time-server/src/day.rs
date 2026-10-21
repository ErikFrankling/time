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
}

/// What was shipped that day, from git and GitHub.
#[derive(Debug, Default, Serialize)]
pub struct CodeTotals {
    pub commits: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub prs_opened: u32,
    pub prs_merged: u32,
    pub reviews: u32,
    pub repos: Vec<String>,
}

impl CodeTotals {
    fn add(&mut self, e: &Event) {
        let repo = match e {
            Event::Commit(c) => {
                self.commits += 1;
                self.lines_added += c.lines_added;
                self.lines_removed += c.lines_removed;
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
    for ((device, what), mins) in per_device {
        sig.computers
            .extend(spans(mins, &format!("{device} {what}"), 5, 2));
    }
    sig.computers.sort_by_key(|s| s.from_ms);
    sig
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
}
