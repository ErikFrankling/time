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
    for e in store.events_between(start_ms, end_ms)? {
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
    })
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
