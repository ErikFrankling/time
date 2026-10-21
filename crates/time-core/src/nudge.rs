//! Focus nudges: the rule, as pure functions.
//!
//! Every check looks at the last `window` minutes of the merged timeline.
//! Erik is nudged when at least `threshold` of them were unproductive (video,
//! social, games by default) and fewer than `productive_floor` minutes had
//! anything productive in them: a work or study label, a prompt he sent, a
//! dictation, a commit. YouTube playing next to real work therefore never
//! counts against him. Then the guards: quiet hours, a cooldown, a daily cap,
//! all relaxed or tightened by a focus day the personal agent can set.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::clock::{Millis, MINUTE_MS};
use crate::label::{Activity, MinuteLabel};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct NudgeConfig {
    pub enabled: bool,
    /// Unproductive minutes in the window that trigger a nudge.
    pub threshold_minutes: u32,
    pub window_minutes: u32,
    /// This much productive activity in the window and there is no nudge.
    pub productive_floor_minutes: u32,
    pub cooldown_minutes: u32,
    pub max_per_day: u32,
    /// Local hours in which nudges may be sent, `[start, end)`.
    pub start_hour: u32,
    pub end_hour: u32,
    pub weekdays_only: bool,
    pub unproductive: Vec<Activity>,
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_minutes: 30,
            window_minutes: 60,
            productive_floor_minutes: 5,
            cooldown_minutes: 45,
            max_per_day: 4,
            start_hour: 9,
            end_hour: 18,
            weekdays_only: true,
            unproductive: vec![Activity::Video, Activity::Social, Activity::Games],
        }
    }
}

/// A day (or part of one) the personal agent marked as needing focus:
/// nudges apply at any hour and the threshold can be lower.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Focus {
    pub until_ms: Millis,
    pub reason: String,
    #[serde(default)]
    pub threshold_minutes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    pub window_minutes: u32,
    pub unproductive_minutes: u32,
    pub productive_minutes: u32,
    pub threshold_minutes: u32,
    pub over: bool,
}

/// How the last window looks. `labels` covers the window; `signals` are the
/// minute starts with a productive event (prompt, dictation, commit).
pub fn assess(
    cfg: &NudgeConfig,
    focus: Option<&Focus>,
    labels: &[MinuteLabel],
    signals: &BTreeSet<Millis>,
) -> Assessment {
    let mut productive: BTreeSet<Millis> = signals.clone();
    let mut unproductive = 0;
    for l in labels {
        match l.activity {
            Some(a) if a.is_productive() => {
                productive.insert(l.ts);
            }
            Some(a) if cfg.unproductive.contains(&a) => unproductive += 1,
            _ => {}
        }
    }
    let threshold = focus
        .and_then(|f| f.threshold_minutes)
        .unwrap_or(cfg.threshold_minutes);
    let productive_minutes = productive.len() as u32;
    Assessment {
        window_minutes: cfg.window_minutes,
        unproductive_minutes: unproductive,
        productive_minutes,
        threshold_minutes: threshold,
        over: unproductive >= threshold && productive_minutes < cfg.productive_floor_minutes,
    }
}

/// Why a nudge was held back, if it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    Disabled,
    QuietHours,
    Weekend,
    Cooldown,
    DailyCap,
}

/// The guards. `local_hour` and `weekday` (0 = Monday) are in Erik's zone.
pub fn may_send(
    cfg: &NudgeConfig,
    focus: Option<&Focus>,
    now: Millis,
    local_hour: u32,
    weekday: u32,
    last_sent: Option<Millis>,
    sent_today: u32,
) -> Result<(), Hold> {
    if !cfg.enabled {
        return Err(Hold::Disabled);
    }
    let focused = focus.is_some_and(|f| f.until_ms > now);
    if !focused {
        if cfg.weekdays_only && weekday >= 5 {
            return Err(Hold::Weekend);
        }
        if local_hour < cfg.start_hour || local_hour >= cfg.end_hour {
            return Err(Hold::QuietHours);
        }
    }
    if last_sent.is_some_and(|t| now - t < i64::from(cfg.cooldown_minutes) * MINUTE_MS) {
        return Err(Hold::Cooldown);
    }
    if sent_today >= cfg.max_per_day {
        return Err(Hold::DailyCap);
    }
    Ok(())
}

/// "1 h 20 min" style, for the message.
pub fn minutes_text(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// The notification: what he did, then a nudge; the focus reason when set.
pub fn message(a: &Assessment, sources: &[String], focus: Option<&Focus>) -> (String, String) {
    let what = if sources.is_empty() {
        "unproductive things".to_owned()
    } else {
        sources.join(" and ")
    };
    let title = format!("{} on {what}", minutes_text(a.unproductive_minutes));
    let mut body = format!("In the last {}. ", minutes_text(a.window_minutes));
    match focus {
        Some(f) if !f.reason.is_empty() => body.push_str(&format!("Today's focus: {}.", f.reason)),
        _ => body.push_str("Time to get back to it?"),
    }
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{Decider, Place};

    fn label(i: i64, a: Activity) -> MinuteLabel {
        MinuteLabel {
            ts: i * MINUTE_MS,
            place: Place::Computer,
            device: Some("pc".into()),
            activity: Some(a),
            project: None,
            background: None,
            rule: "r".into(),
            decider: Decider::Rule,
            complete: true,
            location: None,
        }
    }

    #[test]
    fn forty_minutes_of_youtube_and_nothing_else_is_over() {
        let cfg = NudgeConfig::default();
        let labels: Vec<_> = (0..40).map(|i| label(i, Activity::Video)).collect();
        let a = assess(&cfg, None, &labels, &BTreeSet::new());
        assert!(a.over);
        assert_eq!(a.unproductive_minutes, 40);
    }

    #[test]
    fn youtube_next_to_real_work_is_not() {
        let cfg = NudgeConfig::default();
        let mut labels: Vec<_> = (0..40).map(|i| label(i, Activity::Video)).collect();
        labels.extend((40..50).map(|i| label(i, Activity::Work)));
        assert!(!assess(&cfg, None, &labels, &BTreeSet::new()).over);
        // Prompts sent while YouTube played count as productive too.
        let labels: Vec<_> = (0..40).map(|i| label(i, Activity::Video)).collect();
        let prompts: BTreeSet<Millis> = (0..6).map(|i| i * 5 * MINUTE_MS).collect();
        assert!(!assess(&cfg, None, &labels, &prompts).over);
    }

    #[test]
    fn a_focus_day_lowers_the_threshold_and_ignores_quiet_hours() {
        let cfg = NudgeConfig::default();
        let labels: Vec<_> = (0..20).map(|i| label(i, Activity::Social)).collect();
        assert!(!assess(&cfg, None, &labels, &BTreeSet::new()).over);
        let focus = Focus {
            until_ms: 10_000 * MINUTE_MS,
            reason: "DA2210 essay due".into(),
            threshold_minutes: Some(20),
        };
        assert!(assess(&cfg, Some(&focus), &labels, &BTreeSet::new()).over);
        assert_eq!(
            may_send(&cfg, None, 0, 21, 2, None, 0),
            Err(Hold::QuietHours)
        );
        assert_eq!(may_send(&cfg, Some(&focus), 0, 21, 6, None, 0), Ok(()));
    }

    #[test]
    fn guards() {
        let cfg = NudgeConfig::default();
        let now = 1000 * MINUTE_MS;
        assert_eq!(
            may_send(&cfg, None, now, 10, 5, None, 0),
            Err(Hold::Weekend)
        );
        assert_eq!(
            may_send(&cfg, None, now, 10, 1, Some(now - 30 * MINUTE_MS), 1),
            Err(Hold::Cooldown)
        );
        assert_eq!(
            may_send(&cfg, None, now, 10, 1, Some(now - 50 * MINUTE_MS), 4),
            Err(Hold::DailyCap)
        );
        assert_eq!(
            may_send(&cfg, None, now, 10, 1, Some(now - 50 * MINUTE_MS), 1),
            Ok(())
        );
        let off = NudgeConfig {
            enabled: false,
            ..cfg
        };
        assert_eq!(
            may_send(&off, None, now, 10, 1, None, 0),
            Err(Hold::Disabled)
        );
    }

    #[test]
    fn the_message_says_what_and_why() {
        let a = Assessment {
            window_minutes: 90,
            unproductive_minutes: 80,
            productive_minutes: 0,
            threshold_minutes: 30,
            over: true,
        };
        let (t, b) = message(&a, &["YouTube".into(), "X".into()], None);
        assert_eq!(t, "1 h 20 min on YouTube and X");
        assert_eq!(b, "In the last 1 h 30 min. Time to get back to it?");
        let f = Focus {
            until_ms: 0,
            reason: "the KTH lab report".into(),
            threshold_minutes: None,
        };
        assert!(message(&a, &[], Some(&f))
            .1
            .contains("Today's focus: the KTH lab report."));
    }
}
