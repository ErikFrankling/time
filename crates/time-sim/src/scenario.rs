//! A scripted day, as TOML.
//!
//! ```toml
//! name = "morning"
//! start = "2026-10-21T09:00:00+02:00"
//! end = "10:45"
//! desktops = ["pc"]
//!
//! [[desktop]]
//! device = "pc"
//! from = "09:00"
//! to = "09:40"
//! class = "firefox"
//! title = "Cats - YouTube — Mozilla Firefox"
//! tabs = [{ host = "youtube.com", title = "Cats - YouTube" }]
//! mouse = 3
//! audio = true
//!
//! [[phone]]
//! from = "09:40"
//! to = "09:50"
//! package = "com.twitter.android"
//!
//! [[offline]]
//! device = "phone"
//! from = "10:00"
//! to = "10:20"
//! ```
//!
//! Times are `HH:MM` on the start's calendar day, in the start's UTC offset.
//! Intervals are half-open: `from` is included, `to` is not.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, NaiveTime};
use serde::Deserialize;
use time_core::clock::{Millis, MINUTE_MS};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    /// RFC 3339, with offset. The simulation starts here.
    pub start: String,
    /// `HH:MM`; the simulation stops here.
    pub end: String,
    /// Desktop devices running an agent.
    #[serde(default)]
    pub desktops: Vec<String>,
    /// Whether a phone is simulated.
    #[serde(default)]
    pub phone_enabled: bool,
    #[serde(default, rename = "desktop")]
    pub desktop: Vec<DesktopSegment>,
    #[serde(default, rename = "phone")]
    pub phone: Vec<PhoneSegment>,
    #[serde(default, rename = "steps")]
    pub steps: Vec<StepsSegment>,
    /// A device cannot reach the server.
    #[serde(default, rename = "offline")]
    pub offline: Vec<Offline>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopSegment {
    pub device: String,
    pub from: String,
    pub to: String,
    /// Omitted: nothing focused.
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub fullscreen: bool,
    /// Every tab the browser extension reports while this segment runs, from
    /// all browsers. The agent must pick the one in the focused window.
    #[serde(default)]
    pub tabs: Vec<TabSpec>,
    /// Per minute.
    #[serde(default)]
    pub keys: u32,
    #[serde(default)]
    pub mouse: u32,
    #[serde(default)]
    pub audio: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSpec {
    pub host: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneSegment {
    pub from: String,
    pub to: String,
    pub package: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub media: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepsSegment {
    pub from: String,
    pub to: String,
    pub steps: u32,
    #[serde(default)]
    pub meters: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offline {
    pub device: String,
    pub from: String,
    pub to: String,
}

/// A scenario with every time resolved to epoch milliseconds.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub raw: Scenario,
    pub start_ms: Millis,
    pub end_ms: Millis,
    offset: FixedOffset,
    day: chrono::NaiveDate,
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Resolved> {
        let raw: Scenario = toml::from_str(text)?;
        raw.resolve()
    }

    pub fn load(path: &std::path::Path) -> Result<Resolved> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| path.display().to_string())
    }

    fn resolve(self) -> Result<Resolved> {
        let start = DateTime::parse_from_rfc3339(&self.start).context("start")?;
        let mut r = Resolved {
            start_ms: start.timestamp_millis(),
            end_ms: 0,
            offset: *start.offset(),
            day: start.date_naive(),
            raw: self.clone(),
        };
        r.end_ms = r.at(&self.end)?;
        if r.end_ms <= r.start_ms {
            bail!("end {} is not after start {}", self.end, self.start);
        }
        if r.start_ms % MINUTE_MS != 0 {
            bail!("start must be on a minute boundary");
        }
        // Every time in the file must parse, now rather than mid-run.
        for s in &self.desktop {
            r.span(&s.from, &s.to)?;
            if !self.desktops.contains(&s.device) {
                bail!(
                    "desktop segment for {} which is not in `desktops`",
                    s.device
                );
            }
        }
        for s in &self.phone {
            r.span(&s.from, &s.to)?;
        }
        for s in &self.steps {
            r.span(&s.from, &s.to)?;
        }
        for s in &self.offline {
            r.span(&s.from, &s.to)?;
        }
        Ok(r)
    }
}

impl Resolved {
    /// `HH:MM` on the scenario's day, in its offset.
    pub fn at(&self, hhmm: &str) -> Result<Millis> {
        let t = NaiveTime::parse_from_str(hhmm, "%H:%M")
            .with_context(|| format!("bad time {hhmm:?}"))?;
        let local = self.day.and_time(t);
        let dt = local
            .and_local_timezone(self.offset)
            .single()
            .with_context(|| format!("ambiguous time {hhmm}"))?;
        Ok(dt.timestamp_millis())
    }

    pub fn span(&self, from: &str, to: &str) -> Result<(Millis, Millis)> {
        let (a, b) = (self.at(from)?, self.at(to)?);
        if b <= a {
            bail!("interval {from}..{to} is empty or backwards");
        }
        Ok((a, b))
    }

    fn within(&self, from: &str, to: &str, t: Millis) -> bool {
        self.span(from, to)
            .map(|(a, b)| a <= t && t < b)
            .unwrap_or(false)
    }

    pub fn desktop_at(&self, device: &str, t: Millis) -> Option<&DesktopSegment> {
        self.raw
            .desktop
            .iter()
            .find(|s| s.device == device && self.within(&s.from, &s.to, t))
    }

    pub fn phone_at(&self, t: Millis) -> Option<&PhoneSegment> {
        self.raw
            .phone
            .iter()
            .find(|s| self.within(&s.from, &s.to, t))
    }

    pub fn offline(&self, device: &str, t: Millis) -> bool {
        self.raw
            .offline
            .iter()
            .any(|o| o.device == device && self.within(&o.from, &o.to, t))
    }

    /// Steps taken during the minute starting at `t`, spread evenly over
    /// each segment.
    pub fn steps_in_minute(&self, t: Millis) -> (u32, f64) {
        let mut steps = 0;
        let mut meters = 0.0;
        for s in &self.raw.steps {
            if let Ok((a, b)) = self.span(&s.from, &s.to) {
                if a <= t && t < b {
                    let minutes = ((b - a) / MINUTE_MS) as u32;
                    steps += s.steps / minutes;
                    meters += s.meters / f64::from(minutes);
                }
            }
        }
        (steps, meters)
    }

    /// Minute starts from start to end.
    pub fn minutes(&self) -> impl Iterator<Item = Millis> + '_ {
        (self.start_ms..self.end_ms).step_by(MINUTE_MS as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = r#"
        name = "t"
        start = "2026-10-21T09:00:00+02:00"
        end = "10:00"
        desktops = ["pc"]
        [[desktop]]
        device = "pc"
        from = "09:00"
        to = "09:30"
        class = "kitty"
    "#;

    #[test]
    fn resolves_local_times_in_the_start_offset() {
        let r = Scenario::parse(S).unwrap();
        assert_eq!(r.at("09:00").unwrap(), r.start_ms);
        assert_eq!(r.end_ms - r.start_ms, 60 * MINUTE_MS);
        assert!(r.desktop_at("pc", r.start_ms).is_some());
        assert!(
            r.desktop_at("pc", r.at("09:30").unwrap()).is_none(),
            "to is exclusive"
        );
        assert_eq!(r.minutes().count(), 60);
    }

    #[test]
    fn mistakes_fail_at_load() {
        assert!(Scenario::parse(&S.replace("09:30", "08:30")).is_err());
        assert!(Scenario::parse(&S.replace("desktops = [\"pc\"]", "desktops = []")).is_err());
        assert!(Scenario::parse(&S.replace("class = \"kitty\"", "klass = \"kitty\"")).is_err());
    }
}
