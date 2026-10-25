//! Filling the gaps between input.
//!
//! Erik reads what is on screen. A minute with no key or pointer is labelled
//! away by [`crate::timeline`], so two minutes of reading T3 Code between
//! two bursts of typing used to show as two minutes of "nothing". The
//! minutes stay as they are (they are the raw record); this module works
//! out which of those gaps were really the activity around them.
//!
//! Only gaps shorter than [`crate::timeline::PRESENT_IDLE_SECS`] are ever
//! filled: a quarter of an hour without input is away, by the presence
//! rule, whatever happens after. Inside that:
//! - **obvious** gaps are filled here, deterministically: the same window
//!   stayed focused on the same computer, nothing else was used, and Erik
//!   came back to a device afterwards;
//! - **ambiguous** gaps (the window changed, the phone was in between, or
//!   nothing came after) are returned as [`Gap`]s for the decision model,
//!   which answers when the activity really stopped and the next started.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::clock::{Millis, MINUTE_MS};
use crate::label::{Activity, MinuteLabel, Place};
use crate::timeline::PRESENT_IDLE_SECS;

/// The longest gap that can be filled, in minutes: one less than the
/// presence rule's quarter of an hour.
pub const MAX_GAP_MIN: usize = (PRESENT_IDLE_SECS / 60) as usize - 1;

/// What a computer showed in one minute: enough to tell "same window".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Screen {
    pub class: String,
    pub title: String,
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillSource {
    /// The same window stayed up between two minutes of use.
    Rule,
    /// The decision model said so; its reason is kept.
    Model,
}

impl FillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FillSource::Rule => "rule",
            FillSource::Model => "model",
        }
    }
}

/// Minutes `[start_ms, end_ms)` that were `activity`, though nobody typed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub start_ms: Millis,
    pub end_ms: Millis,
    pub place: Place,
    pub device: String,
    pub activity: Activity,
    pub what: Option<String>,
    pub project: Option<String>,
    /// 0..1: how sure the rule or the model was.
    pub confidence: f32,
    pub source: FillSource,
    pub reason: String,
}

impl Fill {
    /// The fill copied from the label it continues.
    pub fn from_label(l: &MinuteLabel, start_ms: Millis, end_ms: Millis) -> Option<Self> {
        Some(Self {
            start_ms,
            end_ms,
            place: l.place,
            device: l.device.clone()?,
            activity: l.activity?,
            what: l.what.clone(),
            project: l.project.clone(),
            confidence: 0.0,
            source: FillSource::Rule,
            reason: String::new(),
        })
    }

    /// Paint this fill over a raw away minute.
    pub fn apply(&self, l: &mut MinuteLabel) {
        l.place = self.place;
        l.device = Some(self.device.clone());
        l.activity = Some(self.activity);
        l.what = self.what.clone();
        l.project = self.project.clone();
        l.rule = format!("fill.{}", self.source.as_str());
        l.decider = match self.source {
            FillSource::Rule => crate::label::Decider::Rule,
            FillSource::Model => crate::label::Decider::Model,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    /// Between two minutes of use, but not obviously one activity.
    Between,
    /// After the last minute of use, before a long absence: when did he
    /// stop?
    Trailing,
}

/// A gap the rules cannot settle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gap {
    pub start_ms: Millis,
    pub end_ms: Millis,
    pub kind: GapKind,
    /// The last minute of use before it.
    pub before: MinuteLabel,
    /// The first minute of use after it (`Between` only).
    pub after: Option<MinuteLabel>,
    /// Whether the window from `before` stayed focused all through.
    pub same_window: bool,
}

/// An away minute the presence rule left open: nobody touched anything, but
/// nothing says he left (locked, out, walking, asleep, untracked).
fn open(l: &MinuteLabel) -> bool {
    l.place == Place::Away && l.rule == "presence.away"
}

/// Decide every gap in `labels` (sorted by minute). `screens` holds what
/// each computer showed, by `(device, minute)`.
pub fn plan(
    labels: &[MinuteLabel],
    screens: &HashMap<(String, Millis), Screen>,
) -> (Vec<Fill>, Vec<Gap>) {
    let mut fills = Vec::new();
    let mut gaps = Vec::new();
    let mut i = 1;
    while i < labels.len() {
        let before = &labels[i - 1];
        if !open(&labels[i]) || before.place == Place::Away || labels[i].ts != before.ts + MINUTE_MS
        {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        while j < labels.len()
            && open(&labels[j])
            && labels[j].ts == labels[start].ts + (j - start) as Millis * MINUTE_MS
        {
            j += 1;
        }
        i = j;
        let len = j - start;
        let after = labels
            .get(j)
            .filter(|a| a.place != Place::Away && a.ts == labels[j - 1].ts + MINUTE_MS);
        let gap_start = labels[start].ts;

        // How many of the gap's minutes kept the window `before` had.
        let kept = match (before.place, before.device.as_deref()) {
            (Place::Computer, Some(d)) => {
                let first = screens.get(&(d.to_owned(), before.ts));
                (0..len.min(MAX_GAP_MIN))
                    .take_while(|k| {
                        let s = screens.get(&(d.to_owned(), gap_start + *k as Millis * MINUTE_MS));
                        match (first, s) {
                            (Some(f), Some(s)) => !s.locked && s.class == f.class,
                            _ => false,
                        }
                    })
                    .count()
            }
            // A phone app is held across a gap when the same app has the
            // minute after it.
            (Place::Phone, _) => match after {
                Some(a) if a.place == Place::Phone && a.what == before.what => len,
                _ => 0,
            },
            _ => 0,
        };

        match after {
            Some(a) if len <= MAX_GAP_MIN => {
                if kept == len {
                    let same = a.activity == before.activity && a.what == before.what;
                    if let Some(mut f) =
                        Fill::from_label(before, gap_start, gap_start + len as Millis * MINUTE_MS)
                    {
                        f.confidence = if same { 0.9 } else { 0.75 };
                        f.reason = format!(
                            "{len} min without input, {} still in front of him, and back {} right after",
                            before.what.as_deref().unwrap_or("the same window"),
                            if same { "to it" } else { "at a device" },
                        );
                        fills.push(f);
                    }
                } else if before.place == Place::Computer {
                    // A phone between two apps with nothing in front of
                    // it was put down: away, no question.
                    gaps.push(Gap {
                        start_ms: gap_start,
                        end_ms: gap_start + len as Millis * MINUTE_MS,
                        kind: GapKind::Between,
                        before: before.clone(),
                        after: Some(a.clone()),
                        same_window: false,
                    });
                }
            }
            // Followed by a long absence, or by nothing yet. Only a window
            // still up says he may have been reading; a locked screen or a
            // phone put down says he left.
            _ if kept > 0 => gaps.push(Gap {
                start_ms: gap_start,
                end_ms: gap_start + kept as Millis * MINUTE_MS,
                kind: GapKind::Trailing,
                before: before.clone(),
                after: None,
                same_window: true,
            }),
            _ => {}
        }
    }
    (fills, gaps)
}

/// Paint fills over labels: only minutes the presence rule left open.
pub fn overlay(labels: &mut [MinuteLabel], fills: &[Fill]) {
    if fills.is_empty() {
        return;
    }
    for l in labels.iter_mut().filter(|l| open(l)) {
        if let Some(f) = fills.iter().find(|f| f.start_ms <= l.ts && l.ts < f.end_ms) {
            f.apply(l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Decider;

    const T: Millis = 1_800_000_000_000 - 1_800_000_000_000 % MINUTE_MS;

    fn used(i: i64, what: &str, a: Activity) -> MinuteLabel {
        MinuteLabel {
            ts: T + i * MINUTE_MS,
            place: Place::Computer,
            device: Some("pc".into()),
            activity: Some(a),
            project: None,
            background: None,
            background_device: None,
            rule: "r".into(),
            decider: Decider::Rule,
            complete: true,
            location: None,
            what: Some(what.into()),
            background_what: None,
        }
    }

    fn away(i: i64, rule: &str) -> MinuteLabel {
        MinuteLabel {
            ts: T + i * MINUTE_MS,
            place: Place::Away,
            device: None,
            activity: None,
            project: None,
            background: None,
            background_device: None,
            rule: rule.into(),
            decider: Decider::Presence,
            complete: true,
            location: None,
            what: None,
            background_what: None,
        }
    }

    fn screens(class: &[(i64, &str)]) -> HashMap<(String, Millis), Screen> {
        class
            .iter()
            .map(|(i, c)| {
                (
                    ("pc".to_string(), T + i * MINUTE_MS),
                    Screen {
                        class: (*c).into(),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn reading_the_same_window_between_two_bursts_is_that_activity() {
        let labels = vec![
            used(0, "T3", Activity::Work),
            away(1, "presence.away"),
            away(2, "presence.away"),
            away(3, "presence.away"),
            used(4, "T3", Activity::Work),
        ];
        let s = screens(&[(0, "t3"), (1, "t3"), (2, "t3"), (3, "t3"), (4, "t3")]);
        let (fills, gaps) = plan(&labels, &s);
        assert!(gaps.is_empty());
        assert_eq!(fills.len(), 1);
        assert_eq!(
            (fills[0].start_ms, fills[0].end_ms),
            (T + MINUTE_MS, T + 4 * MINUTE_MS)
        );
        assert_eq!(fills[0].activity, Activity::Work);
        assert!(fills[0].confidence >= 0.9);

        let mut l = labels.clone();
        overlay(&mut l, &fills);
        assert!(l.iter().all(|m| m.activity == Some(Activity::Work)));
        assert_eq!(l[2].rule, "fill.rule");
    }

    #[test]
    fn a_quarter_hour_without_input_stays_away() {
        let mut labels = vec![used(0, "T3", Activity::Work)];
        let mut s = vec![(0, "t3")];
        for i in 1..=15 {
            labels.push(away(i, "presence.away"));
            s.push((i, "t3"));
        }
        labels.push(used(16, "T3", Activity::Work));
        let (fills, gaps) = plan(&labels, &screens(&s));
        assert!(fills.is_empty());
        // ...but when he stopped reading is a question for the model.
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].kind, GapKind::Trailing);
        assert_eq!(
            gaps[0].end_ms - gaps[0].start_ms,
            MAX_GAP_MIN as Millis * MINUTE_MS
        );
    }

    #[test]
    fn a_changed_window_is_a_question_and_a_locked_screen_is_gone() {
        let labels = vec![
            used(0, "T3", Activity::Work),
            away(1, "presence.away"),
            away(2, "presence.away"),
            used(3, "x.com", Activity::Feeds),
        ];
        let s = screens(&[(0, "t3"), (1, "t3"), (2, "firefox"), (3, "firefox")]);
        let (fills, gaps) = plan(&labels, &s);
        assert!(fills.is_empty());
        assert_eq!(gaps[0].kind, GapKind::Between);

        // The last minute before a long absence with the screen locked: no
        // question, he left.
        let labels = vec![
            used(0, "T3", Activity::Work),
            away(1, "presence.away"),
            away(2, "presence.away"),
        ];
        let mut s = screens(&[(0, "t3"), (1, "t3"), (2, "t3")]);
        s.get_mut(&("pc".to_string(), T + MINUTE_MS))
            .unwrap()
            .locked = true;
        let (fills, gaps) = plan(&labels, &s);
        assert!(fills.is_empty() && gaps.is_empty());
    }

    #[test]
    fn out_walking_and_untracked_minutes_are_never_filled() {
        let labels = vec![
            used(0, "T3", Activity::Work),
            away(1, "presence.moving"),
            used(2, "T3", Activity::Work),
        ];
        let (fills, gaps) = plan(&labels, &screens(&[(0, "t3"), (1, "t3"), (2, "t3")]));
        assert!(fills.is_empty() && gaps.is_empty());
    }
}
