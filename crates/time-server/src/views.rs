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
use std::f64::consts::{FRAC_PI_2, TAU};
use std::fmt::Write;

use chrono::{Datelike, Days, Months, NaiveDate, TimeZone};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::label::{Activity, MinuteLabel, Place};

use crate::store::Store;
use crate::web::{duration, esc, group_thousands};

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
            Kind::Day if self.start + Days::new(1) == today => {
                format!("Yesterday, {}", self.start.format("%a %-d %b"))
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
        if let Some(p) = &l.project {
            *t.projects.entry(p.clone()).or_default() += 1;
        }
    }
    t
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
}

impl Summary {
    pub fn avg(&self, f: impl Fn(&DayTotals) -> u32) -> u32 {
        if self.days_with_data == 0 {
            return 0;
        }
        (f64::from(f(&self.total)) / f64::from(self.days_with_data)).round() as u32
    }

    pub fn avg_steps(&self) -> u32 {
        if self.step_days == 0 {
            return 0;
        }
        self.total.steps / self.step_days
    }
}

pub fn summarise(
    store: &Store,
    tz: chrono_tz::Tz,
    period: Period,
    phones: &[String],
    today: NaiveDate,
) -> anyhow::Result<Summary> {
    let mut days = Vec::new();
    let mut total = DayTotals::default();
    let (mut days_with_data, mut step_days) = (0, 0);
    for date in period.days() {
        if date > today {
            days.push(DayTotals {
                date: Some(date),
                ..Default::default()
            });
            continue;
        }
        let (from, to) = crate::day::bounds(tz, date)?;
        let labels = store.labels_between(from, to)?;
        let mut t = day_totals(&labels, phones);
        (t.steps, t.distance_m) = store.steps_between(from, to)?;
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
    Ok(Summary {
        period,
        days,
        total,
        days_with_data,
        step_days,
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

/// "+1 h 12 min", "−8%", or nothing when there is nothing to compare with.
fn delta_minutes(cur: u32, prev: u32) -> String {
    if prev == 0 && cur == 0 {
        return String::new();
    }
    let d = i64::from(cur) - i64::from(prev);
    let sign = if d > 0 {
        "+"
    } else if d < 0 {
        "−"
    } else {
        "±"
    };
    let pct = if prev > 0 {
        format!(
            " ({sign}{:.0}%)",
            (d.abs() as f64) * 100.0 / f64::from(prev)
        )
    } else {
        String::new()
    };
    format!("{sign}{}{pct}", duration(d.unsigned_abs() as u32))
}

fn delta_count(cur: u32, prev: u32) -> String {
    if prev == 0 {
        return String::new();
    }
    let d = i64::from(cur) - i64::from(prev);
    let sign = if d > 0 {
        "+"
    } else if d < 0 {
        "−"
    } else {
        "±"
    };
    format!("{sign}{:.0}%", (d.abs() as f64) * 100.0 / f64::from(prev))
}

/// The headline: the donut, and the numbers next to it with their change.
pub fn hero(cur: &Summary, prev: &Summary) -> String {
    let per_day = cur.period.kind != Kind::Day;
    let pick = |s: &Summary, f: &dyn Fn(&DayTotals) -> u32| {
        if per_day {
            s.avg(f)
        } else {
            f(&s.total)
        }
    };
    let stat = |label: &str, f: &dyn Fn(&DayTotals) -> u32| {
        let c = pick(cur, f);
        let p = pick(prev, f);
        let main = if per_day {
            format!(r#"{}<small> /day</small>"#, duration(c))
        } else {
            duration(c)
        };
        let total = if per_day {
            format!(
                r#"<div class="k2">{} in total</div>"#,
                duration(f(&cur.total))
            )
        } else {
            String::new()
        };
        let d = delta_minutes(c, p);
        let d = if d.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="d">{d} vs {}</div>"#, cur.period.prev_name())
        };
        format!(
            r#"<div class="stat"><div class="k">{label}</div><div class="v">{main}</div>{total}{d}</div>"#
        )
    };
    format!(
        r#"<section class="hero"><div class="donut">{}</div><div class="stats">{}{}{}{}</div></section>"#,
        donut(&cur.total),
        stat("Tracked", &|t| t.tracked()),
        stat("Work and study", &|t| t.work()),
        stat("On the phone", &|t| t.phone()),
        stat("In parallel", &|t| t.parallel()),
    )
}

fn arc(cx: f64, cy: f64, r0: f64, r1: f64, a0: f64, a1: f64) -> String {
    let p = |r: f64, a: f64| (cx + r * a.cos(), cy + r * a.sin());
    let large = if a1 - a0 > std::f64::consts::PI { 1 } else { 0 };
    let (x0, y0) = p(r1, a0);
    let (x1, y1) = p(r1, a1);
    let (x2, y2) = p(r0, a1);
    let (x3, y3) = p(r0, a0);
    format!(
        "M{x0:.2},{y0:.2}A{r1},{r1} 0 {large} 1 {x1:.2},{y1:.2}L{x2:.2},{y2:.2}A{r0},{r0} 0 {large} 0 {x3:.2},{y3:.2}Z"
    )
}

/// What had the attention, by activity (inner ring), and, around each slice,
/// what another device was doing at the same time (outer ring). An empty
/// stretch of the outer ring is time with only one thing going on.
pub fn donut(t: &DayTotals) -> String {
    let total = t.tracked();
    let (cx, cy) = (100.0, 100.0);
    if total == 0 {
        return r#"<svg viewBox="0 0 200 200" role="img" aria-label="Nothing recorded"><circle cx="100" cy="100" r="72" class="track" fill="none" stroke-width="28"/><text x="100" y="105" class="c1">—</text></svg>"#.to_owned();
    }
    let mut inner = String::new();
    let mut outer = String::new();
    let mut labels = String::new();
    let mut angle = -FRAC_PI_2;
    let gap = 0.012;
    for a in Activity::ALL {
        let Some(row) = t.rows.get(&a) else { continue };
        let n = row.total();
        if n == 0 {
            continue;
        }
        let sweep = (f64::from(n) / f64::from(total) * TAU).min(TAU - 0.0005);
        let (a0, a1) = (angle, angle + sweep);
        angle = a1;
        let g = if sweep > 4.0 * gap { gap / 2.0 } else { 0.0 };
        let _ = write!(
            inner,
            r#"<path class="a-{a}" d="{}"><title>{a}: {} ({:.0}%)</title></path>"#,
            arc(cx, cy, 58.0, 86.0, a0 + g, a1 - g),
            duration(n),
            f64::from(n) * 100.0 / f64::from(total),
        );
        let frac = f64::from(n) / f64::from(total);
        if frac >= 0.08 {
            let mid = (a0 + a1) / 2.0;
            let _ = write!(
                labels,
                r#"<text x="{:.1}" y="{:.1}" class="pl">{:.0}%</text>"#,
                cx + 72.0 * mid.cos(),
                cy + 72.0 * mid.sin() + 3.5,
                frac * 100.0
            );
        }
        // The parallel part of this slice, by what ran alongside.
        let mut b0 = a0;
        for b in Activity::ALL {
            let Some(m) = row.with.get(&b).copied().filter(|m| *m > 0) else {
                continue;
            };
            let s = f64::from(m) / f64::from(total) * TAU;
            let _ = write!(
                outer,
                r#"<path class="a-{b} par" d="{}"><title>{b} at the same time as {a}: {}</title></path>"#,
                arc(cx, cy, 90.0, 99.0, b0 + g, (b0 + s).min(a1) - g),
                duration(m),
            );
            b0 += s;
        }
    }
    let par = t.parallel();
    let sub = if par > 0 {
        format!(
            r#"<text x="100" y="125" class="c2">{}</text><text x="100" y="135" class="c3">in parallel</text>"#,
            duration(par)
        )
    } else {
        String::new()
    };
    format!(
        r#"<svg viewBox="0 0 200 200" role="img" aria-label="Time by activity; the outer ring is what ran at the same time on another device"><circle cx="100" cy="100" r="94.5" class="track" fill="none" stroke-width="9"/>{inner}{outer}{labels}<text x="100" y="99" class="c1">{}</text><text x="100" y="113" class="c3">tracked</text>{sub}</svg>"#,
        duration(total)
    )
}

/// The activities, largest first, with their share, the change, and the
/// split between computer and phone.
pub fn activity_table(cur: &Summary, prev: &Summary) -> String {
    let per_day = cur.period.kind != Kind::Day;
    let total = cur.total.tracked().max(1);
    let mut rows: Vec<(Activity, &Row)> = cur.total.rows.iter().map(|(a, r)| (*a, r)).collect();
    rows.sort_by_key(|(_, r)| std::cmp::Reverse(r.total()));
    let mut out = String::from(r#"<section><h2>By activity</h2><div class="acts">"#);
    for (a, r) in rows {
        if r.total() == 0 {
            continue;
        }
        let p = prev.total.rows.get(&a).map(Row::total).unwrap_or(0);
        let (c, p) = if per_day {
            let avg = |n: u32, s: &Summary| {
                (f64::from(n) / f64::from(s.days_with_data.max(1))).round() as u32
            };
            (avg(r.total(), cur), avg(p, prev))
        } else {
            (r.total(), p)
        };
        let d = delta_minutes(c, p);
        let split = match (r.computer, r.phone) {
            (0, ph) => format!("phone {}", duration(ph)),
            (pc, 0) => format!("computer {}", duration(pc)),
            (pc, ph) => format!("computer {} · phone {}", duration(pc), duration(ph)),
        };
        let _ = write!(
            out,
            r#"<div class="act"><span class="sw a-{a}"></span><span class="nm">{a}</span><span class="n">{}</span><span class="pc">{:.0}%</span><div class="bar"><span class="a-{a}" style="width:{:.1}%"></span></div><div class="meta">{split}{}{}</div></div>"#,
            duration(r.total()),
            f64::from(r.total()) * 100.0 / f64::from(total),
            f64::from(r.total()) * 100.0 / f64::from(total),
            if per_day {
                format!(" · {}/day", duration(c))
            } else {
                String::new()
            },
            if d.is_empty() {
                String::new()
            } else {
                format!(" · {d} vs {}", cur.period.prev_name())
            },
        );
    }
    if cur.total.tracked() == 0 {
        out.push_str(r#"<div class="sub">Nothing recorded.</div>"#);
    }
    out.push_str("</div></section>");
    out
}

fn place_name(p: Place) -> &'static str {
    match p {
        Place::Phone => "phone",
        Place::Computer => "computer",
        Place::Away => "away",
    }
}

/// The biggest overlaps, in words: "work on the computer + video on the phone".
pub fn parallel_list(cur: &Summary) -> String {
    let par = cur.total.parallel();
    if par == 0 {
        return String::new();
    }
    let mut pairs: Vec<(&Pair, &u32)> = cur.total.pairs.iter().collect();
    pairs.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let max = pairs.first().map(|(_, n)| **n).unwrap_or(1).max(1);
    let mut out = format!(
        r#"<section><h2>In parallel</h2><div class="sub">{} of {} tracked had a second device going at the same time: the outer ring of the donut.</div><div class="pairs">"#,
        duration(par),
        duration(cur.total.tracked())
    );
    for (p, n) in pairs.into_iter().take(5) {
        let _ = write!(
            out,
            r#"<div class="pair"><span class="who"><span class="sw a-{}"></span> {} on the {} <span class="plus">+</span> <span class="sw a-{}"></span> {} on the {}</span><span class="n">{}</span><div class="bar"><span class="a-{}" style="width:{:.1}%"></span><span class="a-{} par" style="width:{:.1}%"></span></div></div>"#,
            p.primary,
            p.primary,
            place_name(p.primary_on),
            p.other,
            p.other,
            place_name(p.other_on),
            duration(*n),
            p.primary,
            f64::from(*n) * 50.0 / f64::from(max),
            p.other,
            f64::from(*n) * 50.0 / f64::from(max),
        );
    }
    out.push_str("</div></section>");
    out
}

/// A goal ring: steps against the daily goal, kilometres underneath.
fn ring(value: u32, goal: u32, label: &str) -> String {
    let frac = (f64::from(value) / f64::from(goal.max(1))).min(1.0);
    let c = TAU * 42.0;
    let done = if value >= goal { " done" } else { "" };
    format!(
        r#"<svg viewBox="0 0 100 100" class="ring{done}" role="img" aria-label="{label}"><circle cx="50" cy="50" r="42" class="track" fill="none" stroke-width="10"/><circle cx="50" cy="50" r="42" class="fill" fill="none" stroke-width="10" stroke-linecap="round" stroke-dasharray="{:.1} {c:.1}" transform="rotate(-90 50 50)"/><text x="50" y="49" class="c1">{:.0}%</text><text x="50" y="62" class="c3">of goal</text></svg>"#,
        frac * c,
        f64::from(value) * 100.0 / f64::from(goal.max(1)),
    )
}

/// Steps: the ring, the numbers, and bars (per hour for a day, per day for
/// a week or month) against the goal line.
pub fn steps_card(cur: &Summary, prev: &Summary, goal: u32, by_hour: &[u32]) -> String {
    let day = cur.period.kind == Kind::Day;
    let (steps, km) = if day {
        (cur.total.steps, cur.total.distance_m / 1000.0)
    } else {
        let n = f64::from(cur.step_days.max(1));
        (cur.avg_steps(), cur.total.distance_m / 1000.0 / n)
    };
    let prev_steps = if day {
        prev.total.steps
    } else {
        prev.avg_steps()
    };
    if cur.total.steps == 0 && prev.total.steps == 0 {
        return format!(
            r#"<section><h2>Steps</h2><div class="sub">No steps recorded{}. They come from Health Connect: in the phone app, Phone settings → Grant Health Connect access → Steps and Distance.</div></section>"#,
            if day { " this day" } else { " in this period" }
        );
    }
    let d = delta_count(steps, prev_steps);
    let unit = if day { "" } else { " a day" };
    let mut out = format!(
        r#"<section><h2>Steps</h2><div class="steprow">{}<div><div class="big">{}<small> steps{unit}</small></div><div class="k2">{km:.1} km{unit} · goal {}</div>{}{}</div></div>"#,
        ring(steps, goal, "Steps against the daily goal"),
        group_thousands(steps),
        group_thousands(goal),
        if d.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="d">{d} vs {}</div>"#, cur.period.prev_name())
        },
        if day {
            String::new()
        } else {
            format!(
                r#"<div class="k2">{} steps · {:.1} km in total · goal reached on {} of {} days</div>"#,
                group_thousands(cur.total.steps),
                cur.total.distance_m / 1000.0,
                cur.days.iter().filter(|t| t.steps >= goal).count(),
                cur.step_days
            )
        },
    );
    // Bars.
    let (values, labels): (Vec<u32>, Vec<String>) = if day {
        (
            by_hour.to_vec(),
            (0..by_hour.len())
                .map(|h| {
                    if h % 6 == 0 {
                        format!("{h:02}")
                    } else {
                        String::new()
                    }
                })
                .collect(),
        )
    } else {
        cur.days
            .iter()
            .map(|t| (t.steps, day_tick(cur.period.kind, t.date)))
            .unzip()
    };
    let line = if day { 0 } else { goal };
    let max = values.iter().copied().max().unwrap_or(0).max(line).max(1);
    let mut bars = String::new();
    for v in &values {
        let hit = if !day && *v >= goal { " hit" } else { "" };
        let _ = write!(
            bars,
            r#"<div class="col"><span class="sb{hit}" style="height:{:.1}%" title="{} steps"></span></div>"#,
            f64::from(*v) * 100.0 / f64::from(max),
            group_thousands(*v)
        );
    }
    let goal_line = if line > 0 {
        format!(
            r#"<span class="goal" style="bottom:{:.1}%" title="goal {}"></span>"#,
            f64::from(line) * 100.0 / f64::from(max),
            group_thousands(line)
        )
    } else {
        String::new()
    };
    let _ = write!(
        out,
        r#"<div class="vbars"><div class="plot">{goal_line}{bars}</div>{}</div></section>"#,
        ticks(&labels)
    );
    out
}

fn ticks(labels: &[String]) -> String {
    let mut out = String::from(r#"<div class="ticks">"#);
    for l in labels {
        let _ = write!(out, "<i>{l}</i>");
    }
    out.push_str("</div>");
    out
}

fn day_tick(kind: Kind, d: Option<NaiveDate>) -> String {
    let Some(d) = d else { return String::new() };
    match kind {
        Kind::Week => d.format("%a").to_string(),
        _ if d.day() == 1 || d.day() % 5 == 0 => d.day().to_string(),
        _ => String::new(),
    }
}

/// One stacked bar per day, with the period's daily average as a dashed
/// line and the previous period's as a dotted one.
pub fn day_bars(cur: &Summary, prev: &Summary) -> String {
    let max = cur
        .days
        .iter()
        .map(DayTotals::tracked)
        .max()
        .unwrap_or(0)
        .max(1);
    let avg = cur.avg(DayTotals::tracked);
    let pavg = prev.avg(DayTotals::tracked);
    let max = max.max(pavg);
    let mut bars = String::new();
    let mut labels = Vec::new();
    for t in &cur.days {
        let date = t.date.unwrap_or(cur.period.start);
        labels.push(day_tick(cur.period.kind, Some(date)));
        let mut segs = String::new();
        for a in Activity::ALL {
            let n = t.rows.get(&a).map(Row::total).unwrap_or(0);
            if n > 0 {
                let _ = write!(
                    segs,
                    r#"<span class="a-{a}" style="height:{:.2}%" title="{a} {}"></span>"#,
                    f64::from(n) * 100.0 / f64::from(max),
                    duration(n)
                );
            }
        }
        let _ = write!(
            bars,
            r#"<a class="col" href="/day/{date}" title="{} · {} tracked, {} work and study"><span class="stack">{segs}</span></a>"#,
            date.format("%a %-d %b"),
            duration(t.tracked()),
            duration(t.work()),
        );
    }
    let line = |v: u32, class: &str, title: String| {
        if v == 0 {
            return String::new();
        }
        format!(
            r#"<span class="{class}" style="bottom:{:.1}%" title="{title}"></span>"#,
            f64::from(v) * 100.0 / f64::from(max)
        )
    };
    format!(
        r#"<section><h2>Day by day</h2><div class="vbars"><div class="plot">{}{}{bars}</div>{}</div><div class="legend"><span><span class="avgkey"></span> average {} /day</span>{}</div>{}</section>"#,
        line(avg, "avg", format!("average {}", duration(avg))),
        line(
            pavg,
            "pavg",
            format!("{} average {}", cur.period.prev_name(), duration(pavg))
        ),
        ticks(&labels),
        duration(avg),
        if pavg > 0 {
            format!(
                r#"<span><span class="pavgkey"></span> {} {} /day</span>"#,
                cur.period.prev_name(),
                duration(pavg)
            )
        } else {
            String::new()
        },
        legend(),
    )
}

/// The month as a calendar: each day shaded by its work-and-study time,
/// with the hours and the steps written in.
pub fn calendar(cur: &Summary, today: NaiveDate) -> String {
    let max = cur
        .days
        .iter()
        .map(DayTotals::work)
        .max()
        .unwrap_or(0)
        .max(60);
    let mut cells = String::new();
    for d in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
        let _ = write!(cells, r#"<b>{d}</b>"#);
    }
    for _ in 0..cur.period.start.weekday().num_days_from_monday() {
        cells.push_str("<span></span>");
    }
    for t in &cur.days {
        let Some(date) = t.date else { continue };
        let w = t.work();
        let pct = (f64::from(w) * 85.0 / f64::from(max)).round();
        let class = if date == today { " today" } else { "" };
        let steps = if t.steps > 0 {
            format!(r#"<em>{}k</em>"#, (f64::from(t.steps) / 1000.0).round())
        } else {
            String::new()
        };
        let _ = write!(
            cells,
            r#"<a class="cell{class}" href="/day/{date}" style="--w:{pct}%" title="{} · {} work and study, {} tracked{}"><u>{}</u><small>{}</small>{steps}</a>"#,
            date.format("%a %-d %b"),
            duration(w),
            duration(t.tracked()),
            if t.steps > 0 {
                format!(", {} steps", group_thousands(t.steps))
            } else {
                String::new()
            },
            date.day(),
            if w > 0 {
                format!("{:.1}h", f64::from(w) / 60.0)
            } else {
                String::new()
            },
        );
    }
    format!(
        r#"<section><h2>Calendar</h2><div class="cal">{cells}</div><div class="legend"><span>shade and hours = work and study · k = thousand steps</span></div></section>"#
    )
}

/// Projects, largest first.
pub fn projects(cur: &Summary) -> String {
    if cur.total.projects.is_empty() {
        return String::new();
    }
    let mut v: Vec<(&String, &u32)> = cur.total.projects.iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let max = *v[0].1;
    let mut out = String::from(r#"<section><h2>Projects</h2><div class="acts">"#);
    for (p, n) in v.into_iter().take(12) {
        let _ = write!(
            out,
            r#"<div class="act"><span class="sw a-work"></span><span class="nm">{}</span><span class="n">{}</span><span class="pc"></span><div class="bar"><span class="a-work" style="width:{:.1}%"></span></div></div>"#,
            esc(p),
            duration(*n),
            f64::from(*n) * 100.0 / f64::from(max.max(1))
        );
    }
    out.push_str("</div></section>");
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
    let lane = |m: Option<&&MinuteLabel>, phone: bool| -> Option<Activity> {
        let m = m?;
        let primary_here = match m.place {
            Place::Phone => phone,
            Place::Computer => !phone,
            Place::Away => return None,
        };
        if primary_here {
            return m.activity;
        }
        let bg_phone = match &m.background_device {
            Some(d) => phones.contains(d),
            None => m.place == Place::Computer,
        };
        if bg_phone == phone {
            m.background
        } else {
            None
        }
    };
    let hours = ((end_ms - start_ms) / 3_600_000) as usize;
    let mut out = String::from(r#"<section><h2>Hour by hour</h2><div class="hours">"#);
    for h in 0..hours {
        let hour_start = start_ms + h as i64 * 3_600_000;
        let local_hour = tz
            .timestamp_millis_opt(hour_start)
            .single()
            .map(|t| t.format("%H").to_string())
            .unwrap_or_default();
        let mut strips = String::new();
        for phone in [false, true] {
            let mut runs: Vec<(Option<Activity>, usize, Millis)> = Vec::new();
            for i in 0..60 {
                let ts = hour_start + i * MINUTE_MS;
                let a = lane(by_ts.get(&ts), phone);
                match runs.last_mut() {
                    Some(r) if r.0 == a => r.1 += 1,
                    _ => runs.push((a, 1, ts)),
                }
            }
            let place = if phone { "phone" } else { "computer" };
            let spans: String = runs
                .iter()
                .map(|(a, n, ts)| match a {
                    None => format!(r#"<span style="flex:{n}"></span>"#),
                    Some(a) => {
                        let from = hm(tz, *ts);
                        let to = hm(tz, ts + *n as i64 * MINUTE_MS);
                        format!(
                            r#"<span class="a-{a}" style="flex:{n}" title="{from}–{to} · {a} on the {place}"></span>"#
                        )
                    }
                })
                .collect();
            let _ = write!(strips, r#"<div class="lane {place}">{spans}</div>"#);
        }
        let _ = write!(
            out,
            r#"<div class="hour"><span class="h">{local_hour}</span><div class="lanes">{strips}</div></div>"#
        );
    }
    out.push_str(r#"</div><div class="legend"><span><span class="lanekey"></span> upper line: computers · lower line: phone · both filled: in parallel</span></div>"#);
    out.push_str(&legend());
    out.push_str("</section>");
    out
}

pub fn legend() -> String {
    let mut out = String::from(r#"<div class="legend">"#);
    for a in Activity::ALL {
        let _ = write!(out, r#"<span><span class="sw a-{a}"></span> {a}</span>"#);
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

/// A whole week or month page.
pub fn period_page(cur: &Summary, prev: &Summary, goal: u32, today: NaiveDate) -> String {
    let mut body = nav(cur.period, today);
    body.push_str(&hero(cur, prev));
    body.push_str(&parallel_list(cur));
    body.push_str(&day_bars(cur, prev));
    if cur.period.kind == Kind::Month {
        body.push_str(&calendar(cur, today));
    }
    body.push_str(&steps_card(cur, prev, goal, &[]));
    body.push_str(&activity_table(cur, prev));
    body.push_str(&projects(cur));
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
.donut svg { width:100%; max-width:16rem; display:block; margin:0 auto; }
svg .track { stroke:var(--line); }
svg path { fill:var(--c); }
svg path.par { opacity:.9; }
svg text { text-anchor:middle; fill:var(--fg); font-family:inherit; }
svg .pl { font-size:9px; font-weight:700; fill:#141413; } svg .c1 { font-size:16px; font-weight:700; } svg .c2 { font-size:9.5px; fill:var(--muted); } svg .c3 { font-size:8.5px; fill:var(--muted); }
.hero .stats { grid-template-columns:repeat(2,minmax(0,1fr)); }
.stat .k { color:var(--muted); font-size:.82rem; }
.stat .v { font-size:1.3rem; font-weight:650; font-variant-numeric:tabular-nums; }
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
.ring .track { stroke:var(--line); } .ring .fill { stroke:#3987e5; } .ring.done .fill { stroke:var(--ok); }
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
.cal .cell u { text-decoration:none; color:var(--muted); } .cal .cell small { font-weight:700; font-size:.78rem; align-self:center; } .cal .cell em { font-style:normal; color:var(--muted); }
.hours { display:grid; gap:4px; }
.hour { display:grid; grid-template-columns:1.6rem minmax(0,1fr); gap:.4rem; align-items:center; }
.hour .h { color:var(--muted); font-size:.75rem; font-variant-numeric:tabular-nums; }
.lanes { display:grid; gap:2px; }
.lane { display:flex; height:8px; background:var(--bg); border-radius:3px; overflow:hidden; }
.lane.phone { height:6px; }
.lane > span { min-width:0; background:var(--c, transparent); }
"#;

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
            label(
                Place::Phone,
                Activity::Social,
                Some((Activity::Music, "pc")),
            ),
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
        let svg = donut(&t);
        assert!(
            svg.contains("video at the same time as work: 2 min"),
            "{svg}"
        );
    }

    #[test]
    fn deltas_read_as_words() {
        assert_eq!(delta_minutes(90, 60), "+30 min (+50%)");
        assert_eq!(delta_minutes(60, 90), "−30 min (−33%)");
        assert_eq!(delta_minutes(0, 0), "");
        assert_eq!(delta_count(12_000, 10_000), "+20%");
    }
}
