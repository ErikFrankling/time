//! Day, week and month: one period at a time, next to the one before it.
//!
//! Every view answers the same three questions in the same places, so a day
//! and a month read alike (docs/ui-design.md):
//! - where the time went: a donut of what had Erik's attention, with an
//!   outer ring for what another device was doing at the same time;
//! - how that compares: deltas against the previous period, on daily
//!   averages so a half-finished week is not "down 50%";
//! - when: an hour-by-hour two-lane timeline (computers above, phone below)
//!   for a day, a bar per day for a week or month, a calendar for a month.
//!
//! Steps are steps and kilometres, with a daily goal ring like a health app.

use std::collections::BTreeMap;
use std::fmt::Write;

use chrono::{Datelike, Days, Months, NaiveDate, TimeZone};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::label::{Activity, AwayKind, MinuteLabel, Place};

use crate::store::Store;
use crate::web::esc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Day,
    Week,
    Month,
}

/// A local calendar period: `[start, end)` in days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub kind: Kind,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl Period {
    /// The day, the Monday-to-Sunday week, or the month that holds `d`.
    pub fn containing(kind: Kind, d: NaiveDate) -> Self {
        let (start, end) = match kind {
            Kind::Day => (d, d + Days::new(1)),
            Kind::Week => {
                let monday = d - Days::new(u64::from(d.weekday().num_days_from_monday()));
                (monday, monday + Days::new(7))
            }
            Kind::Month => {
                let first = d.with_day(1).unwrap_or(d);
                (first, first + Months::new(1))
            }
        };
        Self { kind, start, end }
    }

    pub fn prev(self) -> Self {
        Self::containing(self.kind, self.start - Days::new(1))
    }

    pub fn next(self) -> Self {
        Self::containing(self.kind, self.end)
    }

    pub fn days(self) -> impl Iterator<Item = NaiveDate> {
        let end = self.end;
        self.start.iter_days().take_while(move |d| *d < end)
    }

    pub fn contains(self, d: NaiveDate) -> bool {
        d >= self.start && d < self.end
    }

    pub fn url(self) -> String {
        match self.kind {
            Kind::Day => format!("/day/{}", self.start),
            Kind::Week => format!("/week/{}", self.start),
            Kind::Month => format!("/month/{}", self.start.format("%Y-%m")),
        }
    }

    pub fn title(self, today: NaiveDate) -> String {
        match self.kind {
            Kind::Day if self.start == today => {
                format!("Today, {}", self.start.format("%a %-d %b"))
            }
            Kind::Day => self.start.format("%A %-d %B").to_string(),
            Kind::Week => {
                let last = self.end - Days::new(1);
                let span = if self.start.month() == last.month() {
                    format!("{}–{}", self.start.format("%-d"), last.format("%-d %b"))
                } else {
                    format!(
                        "{} – {}",
                        self.start.format("%-d %b"),
                        last.format("%-d %b")
                    )
                };
                format!("Week {}, {span}", self.start.iso_week().week())
            }
            Kind::Month => self.start.format("%B %Y").to_string(),
        }
    }

    /// "yesterday", "last week": what a delta is against.
    pub fn prev_name(self) -> &'static str {
        match self.kind {
            Kind::Day => "the day before",
            Kind::Week => "the week before",
            Kind::Month => "the month before",
        }
    }
}

/// One activity's minutes, split by where the attention was, and what ran
/// in parallel on another device while it had it.
#[derive(Debug, Default, Clone)]
pub struct Row {
    pub computer: u32,
    pub phone: u32,
    pub with: BTreeMap<Activity, u32>,
}

impl Row {
    pub fn total(&self) -> u32 {
        self.computer + self.phone
    }
}

/// Two things at once: `primary` had the attention on `primary_on`, while
/// `other` ran on `other_on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pair {
    pub primary: Activity,
    pub primary_on: Place,
    pub other: Activity,
    pub other_on: Place,
}

/// A day's totals, as small as a month of them needs.
#[derive(Debug, Default, Clone)]
pub struct DayTotals {
    pub date: Option<NaiveDate>,
    pub rows: BTreeMap<Activity, Row>,
    pub pairs: BTreeMap<Pair, u32>,
    pub projects: BTreeMap<String, u32>,
    pub steps: u32,
    pub distance_m: f64,
    /// Sound that could really be heard, per source.
    pub listening: crate::listening::Tally,
    /// What each device was doing on screen and in sound.
    pub tracks: crate::tracks::Tracks,
    /// After today: nothing yet, not nothing.
    pub future: bool,
    /// Attention minutes per activity, by what filled them (x.com, discord).
    pub whats: BTreeMap<Activity, BTreeMap<String, u32>>,
    /// Commits, pull requests and reviews.
    pub code: crate::day::CodeTotals,
    /// The night that ended this morning.
    pub sleep: Option<crate::derive::Night>,
}

impl DayTotals {
    pub fn tracked(&self) -> u32 {
        self.rows.values().map(Row::total).sum()
    }
    pub fn work(&self) -> u32 {
        self.rows
            .iter()
            .filter(|(a, _)| a.is_productive())
            .map(|(_, r)| r.total())
            .sum()
    }
    pub fn phone(&self) -> u32 {
        self.rows.values().map(|r| r.phone).sum()
    }
    pub fn computer(&self) -> u32 {
        self.rows.values().map(|r| r.computer).sum()
    }
    pub fn parallel(&self) -> u32 {
        self.pairs.values().sum()
    }

    fn add(&mut self, o: &DayTotals) {
        for (a, r) in &o.rows {
            let me = self.rows.entry(*a).or_default();
            me.computer += r.computer;
            me.phone += r.phone;
            for (b, n) in &r.with {
                *me.with.entry(*b).or_default() += n;
            }
        }
        for (p, n) in &o.pairs {
            *self.pairs.entry(*p).or_default() += n;
        }
        for (p, n) in &o.projects {
            *self.projects.entry(p.clone()).or_default() += n;
        }
        self.steps += o.steps;
        self.distance_m += o.distance_m;
        for (s, h) in &o.listening.sources {
            self.listening.sources.entry(s.clone()).or_default().add(*h);
        }
        for (k, n) in &o.listening.titles {
            *self.listening.titles.entry(k.clone()).or_default() += n;
        }
        self.tracks.add(&o.tracks);
        for (a, by) in &o.whats {
            let me = self.whats.entry(*a).or_default();
            for (w, n) in by {
                *me.entry(w.clone()).or_default() += n;
            }
        }
        self.code.merge(&o.code);
    }
}

/// Totals from one day's labels. `phones` are the device names that are
/// phones, so a parallel activity knows where it ran.
pub fn day_totals(labels: &[MinuteLabel], phones: &[String]) -> DayTotals {
    let mut t = DayTotals::default();
    for l in labels {
        let Some(a) = l.activity else { continue };
        let row = t.rows.entry(a).or_default();
        match l.place {
            Place::Computer => row.computer += 1,
            Place::Phone => row.phone += 1,
            Place::Away => continue,
        }
        if let Some(b) = l.background {
            *row.with.entry(b).or_default() += 1;
            let other_on = match &l.background_device {
                Some(d) if phones.contains(d) => Place::Phone,
                Some(_) => Place::Computer,
                // Older labels: something playing on "the other" device.
                None if l.place == Place::Phone => Place::Computer,
                None => Place::Phone,
            };
            *t.pairs
                .entry(Pair {
                    primary: a,
                    primary_on: l.place,
                    other: b,
                    other_on,
                })
                .or_default() += 1;
        }
        *t.whats
            .entry(a)
            .or_default()
            .entry(l.what.clone().unwrap_or_else(|| "(not recorded)".into()))
            .or_default() += 1;
        if let Some(p) = &l.project {
            *t.projects.entry(p.clone()).or_default() += 1;
        }
    }
    t
}

/// Sleep over a period, per night.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SleepAvg {
    pub minutes: u32,
    /// Local minute of the day.
    pub bedtime: u32,
    pub wake: u32,
    pub nights: usize,
}

/// A period's totals, one entry per day.
#[derive(Debug, Clone)]
pub struct Summary {
    pub period: Period,
    pub days: Vec<DayTotals>,
    pub total: DayTotals,
    /// Days that had anything recorded, the divisor for averages.
    pub days_with_data: u32,
    pub step_days: u32,
    /// Local minute of the day now, when the period holds today: the
    /// clock's "now" hand.
    pub now_min: Option<u32>,
    pub tz: chrono_tz::Tz,
    /// Built from the raw minutes (`?raw=1`), without gaps filled or sleep.
    pub raw: bool,
    /// Commits and AI usage: per hour for a day, per day otherwise.
    pub work: crate::work::Work,
}

impl Summary {
    pub fn avg(&self, f: impl Fn(&DayTotals) -> u32) -> u32 {
        if self.days_with_data == 0 {
            return 0;
        }
        (f64::from(f(&self.total)) / f64::from(self.days_with_data)).round() as u32
    }

    /// Nights in this period with a sleep found: average minutes asleep,
    /// and average bedtime and wake-up as local minutes of the day.
    pub fn sleep_avg(&self) -> Option<SleepAvg> {
        let tz = self.tz;
        let local = |t: Millis| -> i64 {
            use chrono::Timelike;
            tz.timestamp_millis_opt(t)
                .single()
                .map_or(0, |d| i64::from(d.hour() * 60 + d.minute()))
        };
        let ns: Vec<&crate::derive::Night> = self
            .days
            .iter()
            .filter_map(|d| d.sleep.as_ref())
            .filter(|n| n.minutes().is_some())
            .collect();
        if ns.is_empty() {
            return None;
        }
        let k = ns.len() as i64;
        // Clock times measured from 18:00, so 23:30 and 02:30 average to 01:00.
        let from_six = |m: i64| (m - 18 * 60).rem_euclid(1440);
        let avg = |f: &dyn Fn(&crate::derive::Night) -> Option<Millis>| {
            let s: i64 = ns
                .iter()
                .filter_map(|n| f(n))
                .map(|t| from_six(local(t)))
                .sum();
            ((s / k + 18 * 60) % 1440) as u32
        };
        Some(SleepAvg {
            minutes: (ns.iter().filter_map(|n| n.minutes()).sum::<i64>() / k) as u32,
            bedtime: avg(&|n| n.fell_asleep_ms),
            wake: avg(&|n| n.woke_up_ms),
            nights: ns.len(),
        })
    }

    pub fn avg_steps(&self) -> u32 {
        if self.step_days == 0 {
            return 0;
        }
        self.total.steps / self.step_days
    }
}

/// A period's commits and AI usage, in one read: an hour a bucket for a
/// day, else a day.
pub fn work_of(
    store: &Store,
    tz: chrono_tz::Tz,
    period: Period,
) -> anyhow::Result<crate::work::Work> {
    let (from, _) = crate::day::bounds(tz, period.start)?;
    let (_, to) = crate::day::bounds(tz, period.end - Days::new(1))?;
    let starts: Vec<Millis> = match period.kind {
        Kind::Day => (from..to).step_by(3_600_000).collect(),
        _ => period
            .days()
            .map(|d| crate::day::bounds(tz, d).map(|b| b.0))
            .collect::<anyhow::Result<_>>()?,
    };
    Ok(crate::work::Work::between(
        &store.named_between(&crate::work::EVENTS, from, to)?,
        &starts,
        to,
    ))
}

/// Bucket labels and links for a period, as the charts show them.
pub fn bucket_labels(period: Period, buckets: usize) -> (Vec<String>, Vec<String>) {
    match period.kind {
        Kind::Day => ((0..buckets).map(|h| format!("{h:02}")).collect(), vec![]),
        _ => (
            period.days().map(|d| d.to_string()).collect(),
            period.days().map(|d| format!("/day/{d}")).collect(),
        ),
    }
}

pub fn summarise(
    store: &Store,
    tz: chrono_tz::Tz,
    period: Period,
    phones: &[String],
    today: NaiveDate,
    raw: bool,
) -> anyhow::Result<Summary> {
    let mut days = Vec::new();
    let mut total = DayTotals::default();
    let (mut days_with_data, mut step_days) = (0, 0);
    for date in period.days() {
        if date > today {
            days.push(DayTotals {
                date: Some(date),
                future: true,
                ..Default::default()
            });
            continue;
        }
        let (from, to) = crate::day::bounds(tz, date)?;
        let labels = if raw {
            store.raw_labels_between(from, to)?
        } else {
            store.labels_between(from, to)?
        };
        let mut t = day_totals(&labels, phones);
        if !raw {
            let night = crate::derive::night_before(date);
            t.sleep = crate::derive::nights(store, night, night)?.pop();
        }
        (t.steps, t.distance_m) = store.steps_between(from, to)?;
        let heard = store.listening_between(from, to)?;
        t.listening = crate::listening::Tally::new(
            heard.iter().map(|(d, s, l)| (d.as_str(), *s, l)),
            &labels,
            from,
            to,
            3_600_000,
        );
        let input = store.input_between(from, to)?;
        let voice = store.named_between(&crate::tracks::VOICE_EVENTS, from, to)?;
        t.tracks = crate::tracks::Tracks::build(
            &labels,
            heard.iter().map(|(d, s, l)| (d.as_str(), *s, l)),
            &input,
            &voice,
            phones,
            tz,
            from,
            to,
        );
        for e in store.named_between(&crate::day::CodeTotals::EVENTS, from, to)? {
            if e.start_ms >= from {
                t.code.add(&e.event);
            }
        }
        t.date = Some(date);
        if t.tracked() > 0 {
            days_with_data += 1;
        }
        if t.steps > 0 {
            step_days += 1;
        }
        total.add(&t);
        days.push(t);
    }
    let work = work_of(store, tz, period)?;
    Ok(Summary {
        period,
        days,
        total,
        days_with_data,
        step_days,
        now_min: None,
        tz,
        raw,
        work,
    })
}

// ---------------------------------------------------------------- render

/// Day | Week | Month, then ‹ previous · title · next ›, then "Today" when
/// looking at another period.
pub fn nav(p: Period, today: NaiveDate) -> String {
    let anchor = if p.contains(today) { today } else { p.start };
    let tab = |kind: Kind, label: &str| {
        let url = Period::containing(kind, anchor).url();
        let on = if kind == p.kind {
            r#" class="on" aria-current="page""#
        } else {
            ""
        };
        format!(r#"<a href="{url}"{on}>{label}</a>"#)
    };
    let current = Period::containing(p.kind, today);
    let next = if p.contains(today) || p.start > today {
        r#"<span class="arrow dim" aria-hidden="true">›</span>"#.to_owned()
    } else {
        format!(
            r#"<a class="arrow" href="{}" aria-label="Next">›</a>"#,
            p.next().url()
        )
    };
    let back = if p == current {
        String::new()
    } else {
        format!(
            r#"<a class="pill" href="{}">{}</a>"#,
            current.url(),
            match p.kind {
                Kind::Day => "Today",
                Kind::Week => "This week",
                Kind::Month => "This month",
            }
        )
    };
    format!(
        r#"<nav class="period"><div class="seg">{}{}{}</div><div class="pnav"><a class="arrow" href="{}" aria-label="Previous">‹</a><strong>{}</strong>{next}</div>{back}</nav>"#,
        tab(Kind::Day, "Day"),
        tab(Kind::Week, "Week"),
        tab(Kind::Month, "Month"),
        p.prev().url(),
        esc(&p.title(today)),
    )
}

/// "▲ 13%", "▼ 10%", or nothing when there is nothing to compare with.
fn chip(cur: u32, prev: u32) -> String {
    if prev == 0 {
        return String::new();
    }
    let d = (f64::from(cur) - f64::from(prev)) * 100.0 / f64::from(prev);
    let (arrow, class) = if d >= 0.5 {
        ("▲", "up")
    } else if d <= -0.5 {
        ("▼", "down")
    } else {
        ("=", "flat")
    };
    format!(
        r#"<span class="chip {class}" title="vs {}">{arrow} {:.0}%</span>"#,
        "the period before",
        d.abs()
    )
}

fn place_name(p: Place) -> &'static str {
    match p {
        Place::Phone => "phone",
        Place::Computer => "computer",
        Place::Away => "away",
    }
}

/// Short duration for big headline figures: "5h 26m", "26m".
fn short(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m:02}m"),
    }
}

/// Three big figures, nothing else: tracked, work and study, phone. Weeks and
/// months show the daily average; the chip compares it with the period
/// before. Totals live in the charts' tap details.
pub fn headline(cur: &Summary, prev: &Summary) -> String {
    let per_day = cur.period.kind != Kind::Day;
    let v = |s: &Summary, f: &dyn Fn(&DayTotals) -> u32| {
        if per_day {
            s.avg(f)
        } else {
            f(&s.total)
        }
    };
    // One big number; the two others small under it.
    let small = |label: &str, f: &dyn Fn(&DayTotals) -> u32| {
        let c = v(cur, f);
        format!(
            r#"<span class="sf"><b>{}</b> {label} {}</span>"#,
            short(c),
            chip(c, v(prev, f))
        )
    };
    let t = v(cur, &|t| t.tracked());
    let sleep = match (cur.sleep_avg(), prev.sleep_avg()) {
        (Some(c), p) => format!(
            r#"<span class="sf"><b>{}</b> sleep{} {}</span>"#,
            short(c.minutes),
            if per_day { " a night" } else { "" },
            chip(c.minutes, p.map_or(0, |p| p.minutes))
        ),
        _ => String::new(),
    };
    format!(
        r#"<section class="figs"><div class="fv">{}</div><div class="fk">tracked{} {}</div><div class="fs">{}{}{sleep}</div></section>"#,
        short(t),
        if per_day { " a day" } else { "" },
        chip(t, v(prev, &|t| t.tracked())),
        small("work &amp; study", &|t| t.work()),
        small("phone", &|t| t.phone()),
    )
}

fn clock_of(m: u32) -> String {
    format!("{:02}:{:02}", m / 60 % 24, m % 60)
}

/// Sleep: for a day, the night that ended that morning, with the reason it
/// was decided so; for a week or month, the average night, bedtime and
/// wake-up, and each night as a bar.
pub fn sleep_card(cur: &Summary) -> String {
    if cur.raw {
        return String::new();
    }
    let tz = cur.tz;
    let mut out = String::from(r#"<section class="card sleep"><h2>Sleep</h2>"#);
    let interruptions = |n: &crate::derive::Night| -> String {
        n.interruptions
            .iter()
            .map(|i| format!("{}–{} {}", hm(tz, i.from_ms), hm(tz, i.to_ms), esc(&i.what)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match cur.period.kind {
        Kind::Day => {
            let Some(n) = cur.days.first().and_then(|d| d.sleep.as_ref()) else {
                out.push_str(r#"<div class="sub">No night's sleep found before this day yet.</div></section>"#);
                return out;
            };
            match (n.fell_asleep_ms, n.woke_up_ms, n.minutes()) {
                (Some(a), Some(b), Some(m)) => {
                    let _ = write!(
                        out,
                        r#"<div class="stats"><div class="stat"><div class="k">fell asleep</div><div class="v">{}</div></div><div class="stat"><div class="k">woke up</div><div class="v">{}</div></div><div class="stat"><div class="k">asleep</div><div class="v">{}</div></div><div class="stat"><div class="k">interruptions</div><div class="v">{}</div></div></div>"#,
                        hm(tz, a),
                        hm(tz, b),
                        short(m as u32),
                        n.interruptions.len()
                    );
                    if !n.interruptions.is_empty() {
                        let _ =
                            write!(out, r#"<div class="sub">Awake: {}</div>"#, interruptions(n));
                    }
                }
                _ => out.push_str(r#"<div class="sub">No sleep found that night.</div>"#),
            }
            let who = match (n.source.as_str(), n.confidence) {
                ("model", Some(c)) => format!("decided by the model, {:.0}% sure", c * 100.0),
                ("model", None) => "decided by the model".to_owned(),
                _ => "the rules' estimate, not yet confirmed by the model".to_owned(),
            };
            let _ = write!(
                out,
                r#"<div class="sub">{} <span class="d">({who}{})</span></div>"#,
                esc(&n.reason),
                n.decision_id.map_or(String::new(), |_| format!(
                    r#" · <a href="/api/decisions?kind=sleep&amp;key={}">what it saw</a>"#,
                    n.night
                )),
            );
        }
        _ => {
            let Some(avg) = cur.sleep_avg() else {
                out.push_str(
                    r#"<div class="sub">No nights found in this period yet.</div></section>"#,
                );
                return out;
            };
            let _ = write!(
                out,
                r#"<div class="stats"><div class="stat"><div class="k">asleep a night</div><div class="v">{}</div></div><div class="stat"><div class="k">bedtime</div><div class="v">{}</div></div><div class="stat"><div class="k">up</div><div class="v">{}</div></div><div class="stat"><div class="k">nights</div><div class="v">{}</div></div></div><div class="acts" style="margin-top:.7rem">"#,
                short(avg.minutes),
                clock_of(avg.bedtime),
                clock_of(avg.wake),
                avg.nights
            );
            for d in &cur.days {
                let (Some(date), Some(n)) = (d.date, d.sleep.as_ref()) else {
                    continue;
                };
                let (Some(a), Some(b), Some(m)) = (n.fell_asleep_ms, n.woke_up_ms, n.minutes())
                else {
                    if n.done {
                        let _ = write!(
                            out,
                            r#"<a class="act" href="/day/{date}" style="text-decoration:none;color:inherit" title="{}"><span class="sw"></span><span class="nm">{}</span><span class="n d">no sleep found</span><span class="pc">0h</span></a>"#,
                            esc(&n.reason),
                            date.format("%a %-d"),
                        );
                    }
                    continue;
                };
                let _ = write!(
                    out,
                    r#"<a class="act" href="/day/{date}" style="text-decoration:none;color:inherit" title="{}"><span class="sw aw-sleep"></span><span class="nm">{}</span><span class="n">{}–{}</span><span class="pc">{}</span><span class="bar"><span style="width:{:.0}%;background:#4651a6"></span></span></a>"#,
                    esc(&n.reason),
                    date.format("%a %-d"),
                    hm(tz, a),
                    hm(tz, b),
                    short(m as u32),
                    (m as f64 / 6.0 / 1.2).min(100.0),
                );
            }
            out.push_str("</div>");
        }
    }
    out.push_str("</section>");
    out
}

/// A chart slot with a drill-down panel under it, filled when a mark is tapped.
fn drill_slot(id: &str, title: &str, height: &str, note: &str) -> String {
    format!(
        r#"<section class="chartcard"><h2>{title}</h2>{note}<div class="chart" id="{id}" style="height:{height}"></div><div class="drill" id="{id}-drill" hidden><div class="dh"><b></b><button type="button" aria-label="Close">×</button></div><div class="chart"></div></div></section>"#
    )
}

/// A chart slot; `time.js` draws into it from the page's data.
fn slot(id: &str, title: &str, height: &str, note: &str) -> String {
    format!(
        r#"<section class="chartcard"><h2>{title}</h2>{note}<div class="chart" id="{id}" style="height:{height}"></div></section>"#
    )
}

/// Everything the charts need, as one JSON object in the page.
pub fn chart_data(
    cur: &Summary,
    prev: &Summary,
    goal: u32,
    day: Option<&crate::day::Day>,
) -> serde_json::Value {
    use serde_json::json;
    let per_day = cur.period.kind != Kind::Day;
    let acts: Vec<&str> = Activity::ALL.iter().map(|a| a.as_str()).collect();

    // Donut: inner = attention; outer = for each slice, the parallel part
    // by what ran alongside, then a transparent rest.
    let mut inner = Vec::new();
    let mut outer = Vec::new();
    for a in Activity::ALL {
        let Some(r) = cur.total.rows.get(&a) else {
            continue;
        };
        if r.total() == 0 {
            continue;
        }
        inner.push(json!({"name": a.as_str(), "value": r.total(),
            "computer": r.computer, "phone": r.phone}));
        let mut used = 0;
        for b in Activity::ALL {
            if let Some(m) = r.with.get(&b).copied().filter(|m| *m > 0) {
                outer.push(json!({"name": b.as_str(), "with": a.as_str(), "value": m}));
                used += m;
            }
        }
        if r.total() > used {
            outer.push(
                json!({"name": "", "with": a.as_str(), "value": r.total() - used, "gap": true}),
            );
        }
    }

    // Over time: minutes per activity per hour (day) or per day.
    let (labels, stacks, links, future): (Vec<String>, Vec<Vec<u32>>, Vec<String>, Vec<bool>) =
        match day {
            Some(d) => {
                let hours = ((d.end_ms - d.start_ms) / 3_600_000) as usize;
                let mut s = vec![vec![0u32; hours]; Activity::ALL.len()];
                for m in &d.minutes {
                    let (Some(a), true) = (m.activity, m.place != Place::Away) else {
                        continue;
                    };
                    let h = ((m.ts - d.start_ms) / 3_600_000) as usize;
                    let i = Activity::ALL.iter().position(|x| *x == a).unwrap_or(0);
                    if let Some(slot) = s[i].get_mut(h) {
                        *slot += 1;
                    }
                }
                (
                    (0..hours).map(|h| format!("{h:02}")).collect(),
                    s,
                    vec![],
                    vec![false; hours],
                )
            }
            None => {
                let mut s = vec![Vec::new(); Activity::ALL.len()];
                for t in &cur.days {
                    for (i, a) in Activity::ALL.iter().enumerate() {
                        s[i].push(t.rows.get(a).map(Row::total).unwrap_or(0));
                    }
                }
                (
                    cur.days
                        .iter()
                        .map(|t| {
                            t.date
                                .map(|d| match cur.period.kind {
                                    Kind::Week => d.format("%a").to_string(),
                                    _ => d.day().to_string(),
                                })
                                .unwrap_or_default()
                        })
                        .collect(),
                    s,
                    cur.days
                        .iter()
                        .map(|t| t.date.map(|d| format!("/day/{d}")).unwrap_or_default())
                        .collect(),
                    cur.days.iter().map(|t| t.future).collect(),
                )
            }
        };
    let series: Vec<serde_json::Value> = acts
        .iter()
        .zip(&stacks)
        .filter(|(_, v)| v.iter().any(|n| *n > 0))
        .map(|(a, v)| json!({"name": a, "data": v}))
        .collect();

    // Steps.
    let (step_labels, step_values): (Vec<String>, Vec<u32>) = match day {
        Some(d) => (
            (0..d.steps_by_hour.len())
                .map(|h| format!("{h:02}"))
                .collect(),
            d.steps_by_hour.clone(),
        ),
        None => (labels.clone(), cur.days.iter().map(|t| t.steps).collect()),
    };
    let steps_now = if per_day {
        cur.avg_steps()
    } else {
        cur.total.steps
    };
    let steps_prev = if per_day {
        prev.avg_steps()
    } else {
        prev.total.steps
    };
    let km = if per_day {
        cur.total.distance_m / 1000.0 / f64::from(cur.step_days.max(1))
    } else {
        cur.total.distance_m / 1000.0
    };

    // This period against the one before, per activity (per day for weeks
    // and months).
    let avg = |n: u32, s: &Summary| {
        if per_day {
            (f64::from(n) / f64::from(s.days_with_data.max(1))).round() as u32
        } else {
            n
        }
    };
    let mut compare: Vec<serde_json::Value> = Activity::ALL
        .iter()
        .filter_map(|a| {
            let c = cur.total.rows.get(a).map(Row::total).unwrap_or(0);
            let p = prev.total.rows.get(a).map(Row::total).unwrap_or(0);
            (c + p > 0).then(|| {
                json!({"name": a.as_str(), "cur": avg(c, cur), "prev": avg(p, prev),
                "total": c})
            })
        })
        .collect();
    compare.sort_by_key(|v| std::cmp::Reverse(v["cur"].as_u64().unwrap_or(0)));

    let mut pairs: Vec<(&Pair, &u32)> = cur.total.pairs.iter().collect();
    pairs.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let pairs: Vec<serde_json::Value> = pairs
        .into_iter()
        .take(6)
        .map(|(p, n)| {
            json!({"a": p.primary.as_str(), "aOn": place_name(p.primary_on),
                "b": p.other.as_str(), "bOn": place_name(p.other_on), "m": n})
        })
        .collect();

    let mut projects: Vec<(&String, &u32)> = cur.total.projects.iter().collect();
    projects.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let projects: Vec<serde_json::Value> = projects
        .into_iter()
        .take(8)
        .map(|(p, n)| json!({"name": p, "m": n}))
        .collect();

    let calendar: Vec<serde_json::Value> = if cur.period.kind == Kind::Month {
        cur.days
            .iter()
            .filter(|t| !t.future)
            .filter_map(|t| {
                t.date
                    .map(|d| json!([d.to_string(), t.work(), t.tracked(), t.steps]))
            })
            .collect()
    } else {
        vec![]
    };

    json!({
        "kind": match cur.period.kind { Kind::Day => "day", Kind::Week => "week", Kind::Month => "month" },
        "perDay": per_day,
        "prevName": cur.period.prev_name(),
        "month": cur.period.start.format("%Y-%m").to_string(),
        "donut": {"inner": inner, "outer": outer, "total": cur.total.tracked(),
            "parallel": cur.total.parallel()},
        "time": {"labels": labels, "series": series, "links": links, "future": future,
            "avg": cur.avg(DayTotals::tracked), "prevAvg": prev.avg(DayTotals::tracked)},
        "figures": {
            "tracked": [cur.total.tracked(), prev.total.tracked()],
            "work": [cur.total.work(), prev.total.work()],
            "phone": [cur.total.phone(), prev.total.phone()],
            "parallel": [cur.total.parallel(), prev.total.parallel()],
            "days": [cur.days_with_data, prev.days_with_data],
        },
        "steps": {"labels": step_labels, "values": step_values, "goal": goal,
            "now": steps_now, "prev": steps_prev, "km": (km * 10.0).round() / 10.0,
            "total": cur.total.steps, "any": cur.total.steps + prev.total.steps > 0},
        "compare": compare,
        "pairs": pairs,
        "projects": projects,
        "calendar": calendar,
        "listening": listening_chart(cur, prev, &labels),
        "tracks": tracks_chart(cur, &labels),
        "rings": rings_chart(cur),
        "clock": clock_chart(cur),
        "drill": cur.total.whats.iter().map(|(a, by)| (a.as_str().to_owned(), serde_json::Value::from(drill(by, 60)))).collect::<serde_json::Map<_, _>>(),
        "code": code_chart(&cur.total.code),
        "work": work_chart(cur, prev, &labels, &links, &future),
    })
}

/// The chart part of every period page: headline figures, then charts.
pub fn charts(cur: &Summary, prev: &Summary, goal: u32, day: Option<&crate::day::Day>) -> String {
    let data = chart_data(cur, prev, goal, day);
    let mut out = headline(cur, prev);
    out.push_str(&work_line(cur));
    out.push_str(&sleep_card(cur));
    let per_day = cur.period.kind != Kind::Day;
    let full = cur.total.tracks.active_minutes;
    if data["tracks"]["any"].as_bool() == Some(true) {
        out.push_str(&drill_slot(
            "c-rings",
            "Every track at once",
            "22rem",
            &format!(
                r#"<div class="cap">One ring per track, outside in: computer screen and sound, phone screen and sound, keyboard &amp; mouse, voice, and sleep innermost. A full circle is every minute anything was going on or Erik was asleep ({}{} with anything on). Colours are what filled each ring: hover for the breakdown, tap to open it.</div>"#,
                short(if per_day { full / cur.total.tracks.days.max(1) } else { full }),
                if per_day { " a day" } else { "" }
            ),
        ));
        out.push_str(&drill_slot(
            "c-clock",
            "Around the clock",
            "25rem",
            if per_day {
                r#"<div class="cap">The same rings on a 24-hour dial (outside in: computer screen, computer sound, phone screen, phone sound, keyboard &amp; mouse, voice, sleep), midnight at the top and noon at the bottom. Each ten minutes is shaded by how often it was filled on these days, coloured by what filled it most.</div>"#
            } else {
                r#"<div class="cap">The same rings on a 24-hour dial (outside in: computer screen, computer sound, phone screen, phone sound, keyboard &amp; mouse, voice, sleep), midnight at the top and noon at the bottom: each stretch sits at the hour it happened. Keyboard &amp; mouse is shaded by how much of each ten minutes had input.</div>"#
            },
        ));
    }
    out.push_str(&drill_slot(
        "c-donut",
        "Where the attention went",
        "19rem",
        r#"<div class="cap"><span class="k-in"></span>inside: what had your attention <span class="k-out"></span>outer ring: a second device busy at the same time, grey when nothing else was. Hover a category for its apps and sites; tap to open it.</div>"#,
    ));
    if data["tracks"]["any"].as_bool() == Some(true) {
        let when = match cur.period.kind {
            Kind::Day => ("When, track by track", "Seven tracks: what was on each screen (by activity), what could be heard (by source), how much of each ten minutes had keyboard or mouse input (darker: more), when you dictated or were read to, and when you slept. Filled at the same time means at the same time."),
            _ => ("Tracks by day", "Minutes per track each day. Tap a day to open it."),
        };
        out.push_str(&slot(
            "c-tracks",
            when.0,
            if per_day { "13rem" } else { "17rem" },
            &format!(r#"<div class="cap">{}</div>"#, when.1),
        ));
    }
    if !cur.total.projects.is_empty() || !cur.total.code.is_empty() {
        let c = &cur.total.code;
        let code = if c.is_empty() {
            "no commits or pull requests".to_owned()
        } else {
            format!(
                "{} commits · +{} −{} lines · {} pull requests opened, {} merged · {} reviews",
                c.commits,
                crate::web::group_thousands(c.lines_added),
                crate::web::group_thousands(c.lines_removed),
                c.prs_opened,
                c.prs_merged,
                c.reviews
            )
        };
        let n = cur
            .total
            .projects
            .len()
            .clamp(1, 8)
            .max(c.commits_by_repo.len().min(8));
        let _ = write!(
            out,
            r#"<section class="chartcard"><h2>Projects and code</h2><div class="cap">Left: time on each project. Right: commits per repository, yours in gold and your agents' in green. {code}.</div><div class="two"><div class="chart" id="c-projects" style="height:{h:.1}rem"></div><div class="chart" id="c-code" style="height:{h:.1}rem"></div></div></section>"#,
            h = 2.0 + 1.6 * n as f64
        );
    }
    if !cur.work.is_empty() {
        out.push_str(&work_cards(cur.period.kind));
    }
    if cur.total.parallel() > 0 {
        out.push_str(&slot(
            "c-pairs",
            "Two devices at once",
            "13rem",
            r#"<div class="cap">Each bar: left colour what you were focused on, right colour what the other device (📱 phone, 💻 computer) was doing.</div>"#,
        ));
    }
    let over = match cur.period.kind {
        Kind::Day => "Over the day",
        Kind::Week => "Over the week",
        Kind::Month => "Over the month",
    };
    let avg = cur.avg(DayTotals::tracked);
    let pavg = prev.avg(DayTotals::tracked);
    let lines = if cur.period.kind == Kind::Day || avg == 0 {
        String::new()
    } else {
        format!(
            r#"<div class="cap"><span class="k-avg"></span>average {} a day{}{}</div>"#,
            short(avg),
            if pavg > 0 {
                format!(
                    r#" <span class="k-prev"></span>{} {} a day"#,
                    cur.period.prev_name(),
                    short(pavg)
                )
            } else {
                String::new()
            },
            if cur.days.iter().any(|t| t.future) {
                " · shaded: still to come"
            } else {
                ""
            },
        )
    };
    out.push_str(&slot("c-time", over, "15rem", &lines));
    if cur.period.kind == Kind::Month {
        out.push_str(&slot(
            "c-cal",
            "Work and study by day",
            "17rem",
            r#"<div class="cap">Darker blue: more hours of work and study. Tap a day to open it.</div>"#,
        ));
    }
    if data["steps"]["any"].as_bool() == Some(true) {
        out.push_str(
            r#"<section class="chartcard"><h2>Steps</h2><div class="steps2"><div class="chart" id="c-ring" style="height:9rem"></div><div class="chart" id="c-steps" style="height:9rem"></div></div></section>"#,
        );
    } else {
        out.push_str(r#"<section class="chartcard"><h2>Steps</h2><div class="sub">No steps recorded in this period. <a href="/status">Status</a> shows whether the phone is sending them.</div></section>"#);
    }
    out.push_str(&listening_card(&data["listening"], cur.period.kind));
    out.push_str(&slot(
        "c-compare",
        &format!("Compared with {}", cur.period.prev_name()),
        "16rem",
        "",
    ));
    // `</` cannot appear inside JSON strings in a script element.
    let json = data.to_string().replace("</", "<\\/");
    let _ = write!(
        out,
        r#"<script type="application/json" id="time-data">{json}</script><script src="/assets/echarts-5.6.0.min.js"></script><script src="/assets/time.js?v={}"></script>"#,
        crate::web::TIME_JS.len()
    );
    out
}

/// Hour by hour in two lanes, computers above and the phone below, so time
/// spent on both at once shows as both lanes filled.
pub fn lanes(
    minutes: &[MinuteLabel],
    start_ms: Millis,
    end_ms: Millis,
    tz: chrono_tz::Tz,
    phones: &[String],
) -> String {
    use std::collections::HashMap;
    let by_ts: HashMap<Millis, &MinuteLabel> = minutes.iter().map(|m| (m.ts, m)).collect();
    // Out or walking shows in the phone's lane: the phone is how we know,
    // and it is where Erik was. Other away minutes stay empty.
    let lane = |m: Option<&&MinuteLabel>, phone: bool| -> Option<Cell> {
        let m = m?;
        let primary_here = match m.place {
            Place::Phone => phone,
            Place::Computer => !phone,
            Place::Away => {
                return match m.away_kind() {
                    Some(k @ (AwayKind::Walking | AwayKind::Out | AwayKind::Sleep)) if phone => {
                        Some(Cell::Away(k))
                    }
                    _ => None,
                }
            }
        };
        if primary_here {
            return m.activity.map(Cell::A);
        }
        let bg_phone = match &m.background_device {
            Some(d) => phones.contains(d),
            None => m.place == Place::Computer,
        };
        if bg_phone == phone {
            m.background.map(Cell::A)
        } else {
            None
        }
    };
    let hours = ((end_ms - start_ms) / 3_600_000) as usize;
    let local_hour = |t: Millis| {
        tz.timestamp_millis_opt(t)
            .single()
            .map(|t| t.format("%H").to_string())
            .unwrap_or_default()
    };
    let mut out = String::from(
        r#"<section><h2>Minute by minute</h2><div class="lanehead">Each hour: computers on top, the phone below, and a thin line between them where both were in use.</div><div class="hours">"#,
    );
    // Runs of empty hours fold into one line.
    let mut empty_from: Option<Millis> = None;
    let flush = |out: &mut String, from: Option<Millis>, to: Millis| {
        if let Some(f) = from {
            let _ = write!(
                out,
                r#"<div class="hour none"><span class="h">{}</span><span class="sub">{}–{} nothing recorded</span></div>"#,
                local_hour(f),
                hm(tz, f),
                hm(tz, to)
            );
        }
    };
    for h in 0..hours {
        let hour_start = start_ms + h as i64 * 3_600_000;
        let lanes_of: Vec<[Option<Cell>; 2]> = (0..60)
            .map(|i| {
                let m = by_ts.get(&(hour_start + i * MINUTE_MS));
                [lane(m, false), lane(m, true)]
            })
            .collect();
        if lanes_of.iter().all(|l| l[0].is_none() && l[1].is_none()) {
            empty_from.get_or_insert(hour_start);
            continue;
        }
        flush(&mut out, empty_from.take(), hour_start);
        let mut strips = String::new();
        for (k, place) in [(0usize, "computer"), (2, "both"), (1, "phone")] {
            let mut runs: Vec<(Option<Cell>, usize, Millis)> = Vec::new();
            for (i, l) in lanes_of.iter().enumerate() {
                let ts = hour_start + i as i64 * MINUTE_MS;
                // The middle line marks minutes with both lanes filled.
                let a = if k == 2 {
                    matches!(l, [Some(Cell::A(_)), Some(Cell::A(_))])
                        .then_some(Cell::A(Activity::Other))
                } else {
                    l[k]
                };
                match runs.last_mut() {
                    Some(r) if r.0 == a => r.1 += 1,
                    _ => runs.push((a, 1, ts)),
                }
            }
            let spans: String = runs
                .iter()
                .map(|(a, n, ts)| match a {
                    None => format!(r#"<span style="flex:{n}"></span>"#),
                    Some(_) if k == 2 => format!(
                        r#"<span class="on" style="flex:{n}" title="{}–{} · two things at once"></span>"#,
                        hm(tz, *ts),
                        hm(tz, ts + *n as i64 * MINUTE_MS)
                    ),
                    Some(Cell::A(a)) => {
                        let from = hm(tz, *ts);
                        let to = hm(tz, ts + *n as i64 * MINUTE_MS);
                        format!(
                            r#"<span class="a-{a}" style="flex:{n}" title="{from}–{to} · {a} on the {place}"></span>"#
                        )
                    }
                    Some(Cell::Away(w)) => {
                        let from = hm(tz, *ts);
                        let to = hm(tz, ts + *n as i64 * MINUTE_MS);
                        format!(
                            r#"<span class="aw-{w}" style="flex:{n}" title="{from}–{to} · {}"></span>"#,
                            w.describe()
                        )
                    }
                })
                .collect();
            let _ = write!(strips, r#"<div class="lane {place}">{spans}</div>"#);
        }
        let _ = write!(
            out,
            r#"<div class="hour"><span class="h">{}</span><div class="lanes">{strips}</div></div>"#,
            local_hour(hour_start)
        );
    }
    flush(&mut out, empty_from.take(), end_ms);
    out.push_str("</div>");
    out.push_str(&legend());
    out.push_str("</section>");
    out
}

/// One minute in a lane: an activity, or why Erik was away.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cell {
    A(Activity),
    Away(AwayKind),
}

pub fn legend() -> String {
    let mut out = String::from(r#"<div class="legend">"#);
    for a in Activity::ALL {
        let _ = write!(out, r#"<span><span class="sw a-{a}"></span> {a}</span>"#);
    }
    for k in [AwayKind::Walking, AwayKind::Out, AwayKind::Sleep] {
        let _ = write!(
            out,
            r#"<span><span class="sw aw-{k}"></span> {}</span>"#,
            k.describe()
        );
    }
    out.push_str("</div>");
    out
}

fn hm(tz: chrono_tz::Tz, t: Millis) -> String {
    tz.timestamp_millis_opt(t)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// A whole week or month page: headline figures, then charts.
pub fn period_page(cur: &Summary, prev: &Summary, goal: u32, today: NaiveDate) -> String {
    let mut body = nav(cur.period, today);
    body.push_str(&charts(cur, prev, goal, None));
    let here = match cur.period.kind {
        Kind::Week => "/week",
        _ => "/month",
    };
    crate::web::page_with(
        &format!("Time · {}", cur.period.title(today)),
        here,
        None,
        &format!("{}{STYLE}", crate::web::ACTIVITY_STYLE),
        &body,
    )
}

/// Styles for the period views; colours come from the page's theme.
pub const STYLE: &str = r#"
.cap { color:var(--muted); font-size:.78rem; line-height:1.35; margin:-.2rem .3rem .2rem; }
.cap span { display:inline-block; vertical-align:middle; margin:0 .3rem 0 .5rem; }
.cap span:first-child { margin-left:0; }
.k-in { width:.8rem; height:.8rem; border-radius:50%; border:3px solid var(--muted); }
.k-out { width:.8rem; height:.8rem; border-radius:50%; border:1.5px dashed var(--muted); }
.k-avg { width:1.1rem; border-top:2px dashed var(--fg); }
.k-prev { width:1.1rem; border-top:2px dotted var(--muted); }
.k-afk { width:.8rem; height:.8rem; border-radius:2px; background:var(--muted); opacity:.4; }
.figs { background:none; border:0; padding:0 .3rem; text-align:center; }
.figs .fs { display:flex; justify-content:center; flex-wrap:wrap; gap:.2rem 1rem; margin-top:.35rem; font-size:.85rem; color:var(--muted); }
.figs .fs b { color:var(--fg); font-variant-numeric:tabular-nums; }
.figs .fv { font-size:2.6rem; line-height:1.1; font-weight:750; font-variant-numeric:tabular-nums; letter-spacing:-.02em; white-space:nowrap; }
.figs .fk { color:var(--muted); font-size:.75rem; line-height:1.3; }
.workline { margin-top:-.4rem; }
.workline .wl { font-size:1rem; font-weight:650; font-variant-numeric:tabular-nums; }
.seg { display:inline-flex; border:1px solid var(--line); border-radius:.5rem; overflow:hidden; margin:.1rem 0 .3rem; }
.seg button { background:none; border:0; color:var(--muted); font:inherit; font-size:.8rem; padding:.2rem .7rem; cursor:pointer; }
.seg button.on { background:var(--line); color:var(--fg); }
.chip { display:inline-block; font-size:.7rem; font-weight:700; padding:0 .3rem; border-radius:99px; background:var(--line); color:var(--fg); white-space:nowrap; }
.chartcard { padding:.8rem .7rem .6rem; }
.chartcard h2 { margin-left:.3rem; }
.chart { width:100%; }
.steps2 { display:grid; grid-template-columns:9rem minmax(0,1fr); gap:.3rem; align-items:center; }
.period { display:grid; gap:.6rem; }
.seg { display:grid; grid-template-columns:repeat(3,1fr); background:var(--card); border:1px solid var(--line); border-radius:10px; padding:3px; }
.seg a { text-align:center; padding:.45rem 0; border-radius:8px; color:var(--muted); text-decoration:none; font-weight:600; }
.seg a.on { background:var(--bg); color:var(--fg); box-shadow:0 0 0 1px var(--line); }
.pnav { display:grid; grid-template-columns:2.75rem 1fr 2.75rem; align-items:center; text-align:center; }
.pnav strong { font-size:1.05rem; }
.arrow { display:block; font-size:1.6rem; line-height:2.75rem; height:2.75rem; color:var(--fg); text-decoration:none; border-radius:8px; }
.arrow.dim { color:var(--line); }
.pill { justify-self:center; font-size:.85rem; padding:.25rem .8rem; border:1px solid var(--line); border-radius:99px; color:var(--fg); text-decoration:none; }
.hero { display:grid; grid-template-columns:minmax(0,1fr); gap:.6rem; }
@media (min-width:34rem) { .hero { grid-template-columns: 15rem minmax(0,1fr); align-items:center; } }
.hero .stats { grid-template-columns:repeat(2,minmax(0,1fr)); }
.stat .k { color:var(--muted); font-size:.82rem; }
.stat .v { font-size:1.2rem; font-weight:650; font-variant-numeric:tabular-nums; white-space:nowrap; }
.stat .v small, .big small { font-size:.8rem; font-weight:500; color:var(--muted); }
.stat .k2, .k2 { color:var(--muted); font-size:.8rem; }
.d { font-size:.8rem; color:var(--muted); font-variant-numeric:tabular-nums; }
.acts { display:grid; gap:.55rem; }
.act { display:grid; grid-template-columns:1rem minmax(0,1fr) auto 2.6rem; gap:.15rem .5rem; align-items:center; }
.act .nm { font-weight:600; } .act .n { font-variant-numeric:tabular-nums; } .act .pc { color:var(--muted); text-align:right; font-size:.85rem; }
.act .bar, .pair .bar { grid-column:2 / -1; height:6px; background:var(--line); border-radius:3px; overflow:hidden; display:flex; }
.act .bar span, .pair .bar span { background:var(--c); }
.act .meta { grid-column:2 / -1; color:var(--muted); font-size:.8rem; }
.pairs { display:grid; gap:.6rem; margin-top:.6rem; }
.pair { display:grid; grid-template-columns:minmax(0,1fr) auto; gap:.25rem .5rem; align-items:center; }
.pair .bar { grid-column:1 / -1; }
.pair .plus { color:var(--muted); }
.steprow { display:grid; grid-template-columns:6.5rem minmax(0,1fr); gap:1rem; align-items:center; }
.big { font-size:1.5rem; font-weight:700; font-variant-numeric:tabular-nums; }
.vbars { margin-top:.8rem; }
.vbars .plot { position:relative; display:flex; align-items:stretch; gap:3px; height:8rem; border-bottom:1px solid var(--line); }
.vbars .col { flex:1; min-width:0; display:flex; flex-direction:column; justify-content:flex-end; text-decoration:none; }
.vbars .stack { display:flex; flex-direction:column-reverse; height:100%; justify-content:flex-start; }
.vbars .stack span { background:var(--c); display:block; flex:none; }
.vbars .stack span:last-child { border-radius:3px 3px 0 0; }
.vbars .sb { display:block; background:#3987e5; border-radius:3px 3px 0 0; min-height:1px; }
.vbars .sb.hit { background:var(--ok); }
.vbars .ticks { display:flex; gap:3px; }
.vbars .ticks i { flex:1; min-width:0; font-style:normal; font-size:.7rem; color:var(--muted); text-align:center; white-space:nowrap; overflow:visible; }
.vbars .avg, .vbars .pavg, .vbars .goal { position:absolute; left:0; right:0; height:0; border-top:2px dashed var(--fg); opacity:.75; pointer-events:none; z-index:1; }
.vbars .pavg { border-top:2px dotted var(--muted); }
.vbars .goal { border-top:2px dashed var(--ok); }
.avgkey, .pavgkey, .lanekey { display:inline-block; width:1.2rem; vertical-align:middle; border-top:2px dashed var(--fg); }
.pavgkey { border-top:2px dotted var(--muted); }
.lanekey { border:0; height:9px; background:linear-gradient(var(--muted) 0 3px, transparent 3px 6px, var(--muted) 6px 9px); }
.cal { display:grid; grid-template-columns:repeat(7,minmax(0,1fr)); gap:3px; }
.cal b { font-size:.7rem; color:var(--muted); text-align:center; font-weight:500; }
.cal .cell { aspect-ratio:1; border-radius:6px; padding:3px 4px; display:grid; grid-template-rows:auto 1fr auto; text-decoration:none; color:var(--fg);
  background:color-mix(in srgb, #3987e5 var(--w), var(--bg)); border:1px solid var(--line); font-size:.7rem; line-height:1.1; }
.cal .cell.today { outline:2px solid var(--fg); }
.cal .cell u { text-decoration:none; color:var(--muted); } .cal .cell small { font-weight:700; font-size:.85rem; align-self:end; } .cal .cell em { font-style:normal; color:var(--muted); }
.hours { display:grid; gap:4px; }
.hour { display:grid; grid-template-columns:1.6rem minmax(0,1fr); gap:.4rem; align-items:center; }
.hour .h { color:var(--muted); font-size:.75rem; font-variant-numeric:tabular-nums; }
.lanes { display:grid; gap:2px; }
.lane { display:flex; height:8px; background:var(--bg); border-radius:3px; overflow:hidden; }
.lane.both { height:3px; background:transparent; } .lane.both > span.on { background:var(--fg); border-radius:2px; }
.hour.none { grid-template-columns:1.6rem minmax(0,1fr); } .hour.none .sub { font-size:.75rem; }
.lanehead { font-size:.8rem; color:var(--muted); margin:-.2rem 0 .6rem; }
.hl::before { content:""; display:inline-block; width:1.1rem; height:8px; border-radius:2px; background:var(--muted); margin-right:.35rem; vertical-align:middle; }
.hl.ov::before { height:3px; background:var(--fg); }
.ringkey { display:flex; flex-wrap:wrap; justify-content:center; gap:.2rem .9rem; font-size:.78rem; color:var(--muted); margin-top:.3rem; }
.ringkey span::before { content:""; display:inline-block; width:.9rem; height:.9rem; border-radius:50%; vertical-align:-.15rem; margin-right:.3rem; box-sizing:border-box; }
.ringkey .in::before { border:4px solid var(--muted); } .ringkey .out::before { border:2px solid var(--muted); outline:2px solid var(--muted); outline-offset:2px; width:.6rem; height:.6rem; margin:0 .45rem 0 .15rem; }
.vbars .col.future { background:repeating-linear-gradient(45deg, transparent 0 4px, var(--line) 4px 5px); border-radius:3px; opacity:.6; }
details.card { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:.9rem 1rem; }
details.card > summary { cursor:pointer; font-size:.8rem; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); font-weight:700; list-style:none; }
details.card > summary::after { content:" ›"; } details.card[open] > summary::after { content:" ⌄"; } details.card[open] > summary { margin-bottom:.6rem; }
.lane > span { min-width:0; background:var(--c, transparent); }
.two { display:grid; grid-template-columns:minmax(0,1fr) minmax(0,1fr); gap:.5rem; }
.drill { border-top:1px solid var(--line); margin:.4rem .3rem 0; padding-top:.5rem; }
.drill .dh { display:flex; justify-content:space-between; align-items:center; font-size:.85rem; }
.drill .dh button { background:none; border:0; color:var(--muted); font-size:1.3rem; line-height:1; cursor:pointer; padding:0 .3rem; }
.sleep .act { grid-template-columns:1rem minmax(0,1fr) auto 4rem; }
"#;

/// Most sources drawn on their own; the rest are "Other".
const LISTEN_SOURCES: usize = 6;

/// The listening card: minutes per source (at a device or away from the
/// keyboard), and per hour or per day by source.
fn listening_chart(cur: &Summary, prev: &Summary, labels: &[String]) -> serde_json::Value {
    use serde_json::json;
    let l = &cur.total.listening;
    let ranked = l.ranked();
    let top: Vec<&String> = ranked
        .iter()
        .take(LISTEN_SOURCES)
        .map(|(s, _)| *s)
        .collect();
    let mut sources: Vec<serde_json::Value> = ranked
        .iter()
        .take(LISTEN_SOURCES)
        .map(|(s, h)| {
            json!({"key": s, "name": crate::listening::name(s), "m": h.minutes(),
                "away": h.away_minutes()})
        })
        .collect();
    let rest: crate::listening::Heard =
        ranked
            .iter()
            .skip(LISTEN_SOURCES)
            .fold(Default::default(), |mut a, (_, h)| {
                a.add(**h);
                a
            });
    if rest.minutes() > 0 {
        sources.push(
            json!({"key": "", "name": "Other", "m": rest.minutes(), "away": rest.away_minutes()}),
        );
    }
    let per_slot = |key: Option<&String>| -> Vec<u32> {
        // Hours of a day: from its slots.
        let pick = |t: &crate::listening::Tally| -> Vec<u32> {
            let mut out: Vec<u32> = Vec::new();
            for (s, v) in &t.slots {
                if key.is_some_and(|k| k != s) || (key.is_none() && top.contains(&s)) {
                    continue;
                }
                if out.len() < v.len() {
                    out.resize(v.len(), 0);
                }
                for (o, x) in out.iter_mut().zip(v) {
                    *o += x;
                }
            }
            out.into_iter().map(crate::listening::minutes).collect()
        };
        match cur.period.kind {
            Kind::Day => cur
                .days
                .first()
                .map(|d| pick(&d.listening))
                .unwrap_or_default(),
            _ => cur
                .days
                .iter()
                .map(|d| {
                    let secs: u32 = d
                        .listening
                        .sources
                        .iter()
                        .filter(|(s, _)| match key {
                            Some(k) => *s == k,
                            None => !top.contains(s),
                        })
                        .map(|(_, h)| h.seconds)
                        .sum();
                    crate::listening::minutes(secs)
                })
                .collect(),
        }
    };
    let mut series: Vec<serde_json::Value> = top
        .iter()
        .map(|s| json!({"key": s, "name": crate::listening::name(s), "data": per_slot(Some(s))}))
        .collect();
    if rest.minutes() > 0 {
        series.push(json!({"key": "", "name": "Other", "data": per_slot(None)}));
    }
    let slot_labels: Vec<String> = match cur.period.kind {
        Kind::Day => {
            let n = series
                .first()
                .and_then(|s| s["data"].as_array().map(Vec::len))
                .unwrap_or(24);
            (0..n).map(|h| format!("{h:02}")).collect()
        }
        _ => labels.to_vec(),
    };
    let total = l.total();
    json!({
        "total": total.minutes(),
        "away": total.away_minutes(),
        "prev": prev.total.listening.total().minutes(),
        "sentence": l.sentence(4),
        "sources": sources,
        "labels": slot_labels,
        "series": series,
    })
}

/// "Listening": minutes per source, then when. Only sound that could be
/// heard counts, so a paused or muted tab left open is not in it.
fn listening_card(l: &serde_json::Value, kind: Kind) -> String {
    let total = l["total"].as_u64().unwrap_or(0) as u32;
    if total == 0 {
        return r#"<section class="chartcard"><h2>Listening</h2><div class="sub">Nothing audible recorded in this period. Only sound that could really be heard counts: playing, not muted, above silence.</div></section>"#.to_owned();
    }
    let away = l["away"].as_u64().unwrap_or(0) as u32;
    let away_note = if away > 0 {
        format!(
            r#" <span class="k-afk"></span>faded: away from the keyboard ({})"#,
            short(away)
        )
    } else {
        String::new()
    };
    let n = l["sources"].as_array().map_or(1, Vec::len).max(1);
    let mut out = slot(
        "c-listen",
        &format!("Listening · {}", short(total)),
        &format!("{:.1}rem", 2.5 + 2.0 * n as f64),
        &format!(
            r#"<div class="cap">{}. Only sound you could hear: playing, not muted, above silence.{away_note}</div>"#,
            esc(l["sentence"].as_str().unwrap_or_default())
        ),
    );
    let when = match kind {
        Kind::Day => "Listening by hour",
        _ => "Listening by day",
    };
    out.push_str(&slot("c-listen-time", when, "11rem", ""));
    out
}

/// Kinds of thing drawn per track before the rest is "other".
const TRACK_TOP: usize = 5;

/// The four tracks: totals per track by what filled it, then when (lanes
/// for a day, a bar per track per day for a week or month).
fn tracks_chart(cur: &Summary, labels: &[String]) -> serde_json::Value {
    use crate::tracks::{is_sound, NAMES, TRACKS};
    use serde_json::json;
    let t = &cur.total.tracks;
    let name = |track: usize, key: &str| {
        if is_sound(track) {
            crate::listening::name(key)
        } else {
            key.to_owned()
        }
    };
    let totals: Vec<serde_json::Value> = (0..TRACKS)
        .map(|track| {
            let mut v: Vec<(&String, &u32)> = t.seconds[track].iter().collect();
            v.sort_by_key(|(k, s)| (std::cmp::Reverse(**s), (*k).clone()));
            let mut parts: Vec<serde_json::Value> = v
                .iter()
                .take(TRACK_TOP)
                .map(|(k, s)| {
                    json!({"key": k, "name": name(track, k),
                        "m": crate::listening::minutes(**s)})
                })
                .collect();
            let rest: u32 = v.iter().skip(TRACK_TOP).map(|(_, s)| **s).sum();
            if rest >= 30 {
                parts.push(
                    json!({"key": "", "name": "other", "m": crate::listening::minutes(rest)}),
                );
            }
            json!({"name": NAMES[track], "sound": is_sound(track), "m": t.minutes(track),
                "parts": parts})
        })
        .collect();
    let per_day: Vec<Vec<u32>> = (0..TRACKS)
        .map(|track| cur.days.iter().map(|d| d.tracks.minutes(track)).collect())
        .collect();
    json!({
        "names": NAMES,
        "totals": totals,
        "minutes": 24 * 60,
        "labels": labels,
        "perDay": per_day,
        "any": (0..TRACKS).any(|i| t.minutes(i) > 0),
    })
}

/// Most named parts drawn in a drill-down before the rest is "other".
const DRILL_TOP: usize = 12;

/// A breakdown, biggest first: `[{name, m}]` in minutes (one decimal, so a
/// 20-second dictation is not zero). `unit` is seconds per count.
fn drill(by: &BTreeMap<String, u32>, unit: u32) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut v: Vec<(&String, &u32)> = by.iter().filter(|(_, n)| **n > 0).collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let m = |n: u32| (f64::from(n) * f64::from(unit) / 6.0).round() / 10.0;
    let mut out: Vec<serde_json::Value> = v
        .iter()
        .take(DRILL_TOP)
        .map(|(k, n)| json!({"name": k, "m": m(**n)}))
        .collect();
    let rest: u32 = v.iter().skip(DRILL_TOP).map(|(_, n)| **n).sum();
    if rest > 0 {
        out.push(
            json!({"name": format!("{} more", v.len() - DRILL_TOP), "m": m(rest), "rest": true}),
        );
    }
    out
}

/// A track key as a person reads it.
fn key_name(track: usize, key: &str) -> String {
    use crate::tracks::{is_sound, INPUT};
    if is_sound(track) {
        crate::listening::name(key)
    } else if track == INPUT && key == "pointer" {
        "pointer only".to_owned()
    } else {
        key.to_owned()
    }
}

/// Concentric rings, one per track; the full circle is every minute any
/// track was filled, so rings compare. Each ring's segments are what filled
/// it, each with its own breakdown for the tooltip and the drill-down.
fn rings_chart(cur: &Summary) -> serde_json::Value {
    use crate::tracks::{is_sound, NAMES, TRACKS};
    use serde_json::json;
    let t = &cur.total.tracks;
    let tracks: Vec<serde_json::Value> = (0..TRACKS)
        .map(|track| {
            let mut v: Vec<(&String, &u32)> = t.seconds[track].iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let parts: Vec<serde_json::Value> = v
                .iter()
                .filter(|(_, s)| **s > 0)
                .map(|(k, s)| {
                    let top = t.whats[track]
                        .get(*k)
                        .map(|by| drill(by, 1))
                        .unwrap_or_default();
                    json!({"key": k, "name": key_name(track, k),
                        "m": (f64::from(**s) / 6.0).round() / 10.0, "top": top})
                })
                .collect();
            json!({"name": NAMES[track], "sound": is_sound(track), "m": t.minutes(track),
                "parts": parts})
        })
        .collect();
    // The rest of the day so far was neither: untracked, honestly.
    let days = t.days.max(1);
    let span = (days - 1) * 1440 + cur.now_min.unwrap_or(1440);
    let untracked = span.saturating_sub(t.active_minutes + t.sleep_minutes);
    json!({"full": t.active_minutes + t.sleep_minutes, "active": t.active_minutes,
        "sleep": t.sleep_minutes, "untracked": untracked, "tracks": tracks, "days": days})
}

/// The 24-hour clock: `[track, from, to, key, name, detail, intensity]` in
/// local minutes of the day. A day draws its stretches minute by minute and
/// keyboard and mouse as ten-minute chunks shaded by how much of each had
/// input; a week or month draws ten-minute slots shaded by how often they
/// were filled.
fn clock_chart(cur: &Summary) -> serde_json::Value {
    use crate::tracks::{INPUT, SLOTS, SLOT_MIN, TRACKS};
    use serde_json::json;
    let mut items: Vec<serde_json::Value> = Vec::new();
    match (cur.period.kind, cur.days.first()) {
        (Kind::Day, Some(d)) => {
            let t = &d.tracks;
            for s in t.spans.iter().filter(|s| s.track != INPUT) {
                items.push(json!([
                    s.track,
                    s.from,
                    s.to,
                    s.what,
                    key_name(s.track, &s.what),
                    s.detail,
                    1.0
                ]));
            }
            for c in &t.chunks {
                let (h, m) = c.from.split_once(':').unwrap_or(("0", "0"));
                let from = h.parse::<u32>().unwrap_or(0) * 60 + m.parse::<u32>().unwrap_or(0);
                let key = if c.keys > 0 { "typing" } else { "pointer" };
                let detail = format!(
                    "{} keys · {} clicks · {} s of 600 with input · {}/min",
                    c.keys, c.clicks, c.active_seconds, c.per_minute
                );
                let intensity = (f64::from(c.active_seconds) / 600.0).clamp(0.08, 1.0);
                items.push(json!([
                    INPUT,
                    from,
                    from + SLOT_MIN,
                    key,
                    key_name(INPUT, key),
                    detail,
                    (intensity * 100.0).round() / 100.0
                ]));
            }
        }
        _ => {
            let t = &cur.total.tracks;
            let days = f64::from(t.days.max(1));
            for track in 0..TRACKS {
                for slot in 0..SLOTS {
                    let by = &t.slots[track][slot];
                    let total: u32 = by.values().sum();
                    let Some((key, _)) = by.iter().max_by_key(|(_, s)| **s) else {
                        continue;
                    };
                    if total == 0 {
                        continue;
                    }
                    let share = f64::from(total) / (days * f64::from(SLOT_MIN) * 60.0);
                    let from = slot as u32 * SLOT_MIN;
                    let detail = format!(
                        "{} on average · mostly {}",
                        crate::listening::minutes((f64::from(total) / days) as u32).max(1),
                        key_name(track, key)
                    );
                    items.push(json!([
                        track,
                        from,
                        from + SLOT_MIN,
                        key,
                        key_name(track, key),
                        detail,
                        (share.sqrt().clamp(0.15, 1.0) * 100.0).round() / 100.0
                    ]));
                }
            }
        }
    }
    json!({"items": items, "now": cur.now_min, "names": crate::tracks::NAMES})
}

/// The Code and AI cards: commits and lines over time, then tokens and
/// their API-price equivalent, then where they went.
fn work_cards(kind: Kind) -> String {
    let per = if kind == Kind::Day { "hour" } else { "day" };
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<section class="chartcard"><h2>Code</h2><div class="cap">Commits each {per}, by repository; the line is yours, the rest your agents' (a Co-Authored-By trailer, or an agent session working in that repository when it was committed). Under it, lines added and removed: yours solid, agents' light. Lockfiles, generated, vendored and data files are left out. Hover for the breakdown.</div><div class="chart" id="c-commits" style="height:14rem"></div><div class="chart" id="c-lines" style="height:9rem"></div></section>"#
    );
    let _ = write!(
        out,
        r#"<section class="chartcard"><h2>AI usage</h2><div class="cap">Every agent's tokens each {per} (Claude on both accounts, Codex, Grok, OpenCode), by model, from their own transcripts. Cost is the equivalent at API list prices: most of it is on subscriptions. Hover for input, cache and output.</div><div class="seg" id="c-ai-metric" role="group" aria-label="Metric"><button type="button" data-m="cost" class="on">Cost</button><button type="button" data-m="tokens">Tokens</button></div><div class="chart" id="c-ai" style="height:15rem"></div><div class="two"><div class="chart" id="c-ai-by" style="height:13rem"></div><div class="chart" id="c-ai-projects" style="height:13rem"></div></div><div class="chart" id="c-ai-threads" style="height:13rem"></div></section>"#
    );
    out
}

/// Code and AI over the period: the same buckets as the time chart.
fn work_chart(
    cur: &Summary,
    prev: &Summary,
    labels: &[String],
    links: &[String],
    future: &[bool],
) -> serde_json::Value {
    let mut v = cur.work.json(&prev.work.totals());
    v["labels"] = labels.into();
    v["links"] = links.into();
    v["future"] = future.into();
    v
}

/// "312 commits · +18k/−6k lines · 42M tokens ≈ $310 this week", with
/// Erik's own share under it.
pub fn work_line(cur: &Summary) -> String {
    let t = cur.work.totals();
    if cur.work.is_empty() {
        return String::new();
    }
    let when = match cur.period.kind {
        Kind::Day => "this day",
        Kind::Week => "this week",
        Kind::Month => "this month",
    };
    let mine = if t.commits == 0 {
        String::new()
    } else {
        format!(
            r#"<span class="sf"><b>{}</b> by you, +{}/−{}</span><span class="sf"><b>{}</b> by agents</span>"#,
            t.erik_commits,
            crate::work::compact(t.erik_added),
            crate::work::compact(t.erik_removed),
            t.commits - t.erik_commits
        )
    };
    format!(
        r#"<section class="figs workline"><div class="wl">{} {when}</div><div class="fs">{mine}<span class="sf">tokens ≈ {} equivalent at API prices</span></div></section>"#,
        esc(&crate::work::line(&t)),
        esc(&crate::work::usd(t.cost)),
    )
}

/// Code in the main view: totals and commits per repository.
fn code_chart(c: &crate::day::CodeTotals) -> serde_json::Value {
    use serde_json::json;
    let mut repos: Vec<(&String, &u32)> = c.commits_by_repo.iter().collect();
    repos.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    json!({
        "commits": c.commits, "added": c.lines_added, "removed": c.lines_removed,
        "prsOpened": c.prs_opened, "prsMerged": c.prs_merged, "reviews": c.reviews,
        "repos": repos.iter().take(8).map(|(r, n)| json!({"name": r.rsplit('/').next().unwrap_or(r), "n": n})).collect::<Vec<_>>(),
        "any": !c.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::label::Decider;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn periods_step_by_day_week_and_month() {
        let w = Period::containing(Kind::Week, d("2026-10-23"));
        assert_eq!((w.start, w.end), (d("2026-10-19"), d("2026-10-26")));
        assert_eq!(w.prev().start, d("2026-10-12"));
        let m = Period::containing(Kind::Month, d("2026-10-23"));
        assert_eq!((m.start, m.end), (d("2026-10-01"), d("2026-11-01")));
        assert_eq!(m.prev().start, d("2026-09-01"));
        assert_eq!(m.url(), "/month/2026-10");
        assert_eq!(m.days().count(), 31);
        assert_eq!(
            Period::containing(Kind::Month, d("2026-03-31"))
                .prev()
                .start,
            d("2026-02-01")
        );
        assert_eq!(w.title(d("2026-10-23")), "Week 43, 19–25 Oct");
    }

    fn label(place: Place, a: Activity, bg: Option<(Activity, &str)>) -> MinuteLabel {
        MinuteLabel {
            ts: 0,
            place,
            device: Some(if place == Place::Phone { "phone" } else { "pc" }.into()),
            activity: Some(a),
            project: None,
            background: bg.map(|b| b.0),
            background_device: bg.map(|b| b.1.to_owned()),
            rule: "r".into(),
            decider: Decider::Rule,
            complete: true,
            location: None,
            what: None,
            background_what: None,
        }
    }

    #[test]
    fn parallel_minutes_name_both_sides() {
        let phones = vec!["phone".to_owned()];
        let labels = vec![
            label(
                Place::Computer,
                Activity::Work,
                Some((Activity::Video, "phone")),
            ),
            label(
                Place::Computer,
                Activity::Work,
                Some((Activity::Video, "phone")),
            ),
            label(Place::Computer, Activity::Work, None),
            label(Place::Phone, Activity::Feeds, Some((Activity::Music, "pc"))),
        ];
        let t = day_totals(&labels, &phones);
        assert_eq!(
            (t.tracked(), t.work(), t.phone(), t.parallel()),
            (4, 3, 1, 3)
        );
        let pair = Pair {
            primary: Activity::Work,
            primary_on: Place::Computer,
            other: Activity::Video,
            other_on: Place::Phone,
        };
        assert_eq!(t.pairs.get(&pair), Some(&2));
        assert_eq!(t.rows[&Activity::Work].with[&Activity::Video], 2);
    }

    #[test]
    fn chips_and_figures_stay_short() {
        assert!(chip(90, 60).contains("▲ 50%"));
        assert!(chip(60, 90).contains("▼ 33%"));
        assert_eq!(chip(10, 0), "");
        assert_eq!(short(326), "5h 26m");
        assert_eq!(short(26), "26m");
    }
}
