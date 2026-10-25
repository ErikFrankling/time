//! When Erik slept: the candidate the rules find before a model looks.
//!
//! Erik has nothing that tracks sleep. What he does have: he falls asleep
//! as soon as he puts the phone or computer away, and picks one up the
//! moment he wakes. So a night's sleep is the longest stretch of the night
//! with no human use of any device, where a short blip (a look at the
//! phone at 05:00, under [`BLIP_MAX_MIN`] of use) does not end it. What is
//! not Erik never counted as use in the first place: agents on the AGENT-1
//! seat, the 06:00 update waking a screen, a phone lit by a notification
//! while locked (see [`crate::timeline`]).
//!
//! A night is the 24 hours from 18:00 one day to 18:00 the next: he
//! sometimes falls asleep at 04:00 and sleeps into the afternoon.

use serde::{Deserialize, Serialize};

use crate::clock::{Millis, MINUTE_MS};
use crate::label::{MinuteLabel, Place};

/// Use shorter than this, between long quiet stretches, does not wake him.
pub const BLIP_MAX_MIN: usize = 10;
/// A blip's use spread over at most this long (a few looks close together).
const BLIP_SPAN_MIN: usize = 20;
/// Use with gaps under this long belongs to one blip.
const BLIP_JOIN_MIN: usize = 5;
/// Quiet on the far side of a blip for it to be part of the night.
const QUIET_MIN: usize = 30;
/// The shortest stretch that can be a night's sleep.
pub const MIN_SLEEP_MIN: usize = 90;

/// A minute of human use: a device in use, or walking.
pub fn human(l: &MinuteLabel) -> bool {
    l.place != Place::Away || l.rule == "presence.moving"
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub start_ms: Millis,
    pub end_ms: Millis,
    /// Short uses inside it: `[from, to)`.
    pub interruptions: Vec<(Millis, Millis)>,
}

impl Candidate {
    pub fn minutes(&self) -> i64 {
        (self.end_ms - self.start_ms) / MINUTE_MS
    }
}

/// The longest quiet stretch in `[from, to)`, bridged across blips.
/// Minutes with no label count as quiet.
pub fn candidate(labels: &[MinuteLabel], from: Millis, to: Millis) -> Option<Candidate> {
    let n = ((to - from) / MINUTE_MS).max(0) as usize;
    let mut used = vec![false; n];
    for l in labels {
        if l.ts >= from && l.ts < to && human(l) {
            used[((l.ts - from) / MINUTE_MS) as usize] = true;
        }
    }
    // Quiet runs, [a, b) in minute indexes.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if used[i] {
            i += 1;
            continue;
        }
        let a = i;
        while i < n && !used[i] {
            i += 1;
        }
        runs.push((a, i));
    }
    let k = (0..runs.len()).max_by_key(|k| (runs[*k].1 - runs[*k].0, usize::MAX - k))?;
    let (mut lo, mut hi) = (k, k);
    let mut interruptions = Vec::new();
    // Grow over a blip to the next quiet run while the blip is short and
    // the quiet beyond it long.
    loop {
        let mut grew = false;
        if lo > 0 {
            if let Some(j) = blip(&runs, lo, true, &used) {
                interruptions.push((runs[j].1, runs[lo].0));
                lo = j;
                grew = true;
            }
        }
        if hi + 1 < runs.len() {
            if let Some(j) = blip(&runs, hi, false, &used) {
                interruptions.push((runs[hi].1, runs[j].0));
                hi = j;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let (a, b) = (runs[lo].0, runs[hi].1);
    if b - a < MIN_SLEEP_MIN {
        return None;
    }
    interruptions.sort();
    let at = |i: usize| from + i as Millis * MINUTE_MS;
    Some(Candidate {
        start_ms: at(a),
        end_ms: at(b),
        interruptions: interruptions
            .into_iter()
            .map(|(x, y)| (at(x), at(y)))
            .collect(),
    })
}

/// From quiet run `k`, the run on the other side of the blip next to it
/// (`back`: earlier), if the blip is short and that run long; several
/// quiet runs under [`BLIP_JOIN_MIN`] apart are one blip.
fn blip(runs: &[(usize, usize)], k: usize, back: bool, used: &[bool]) -> Option<usize> {
    let mut j = k;
    loop {
        let next = if back { j.checked_sub(1)? } else { j + 1 };
        let r = *runs.get(next)?;
        // The blip spans from the edge of run k to the edge of run `next`.
        let (ba, bb) = if back {
            (r.1, runs[k].0)
        } else {
            (runs[k].1, r.0)
        };
        let use_min = used[ba..bb].iter().filter(|u| **u).count();
        if use_min >= BLIP_MAX_MIN || bb - ba > BLIP_SPAN_MIN {
            return None;
        }
        if r.1 - r.0 >= QUIET_MIN {
            return Some(next);
        }
        // A short quiet run inside the blip: keep looking past it.
        if r.1 - r.0 >= BLIP_JOIN_MIN {
            return None;
        }
        j = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{Activity, Decider};

    const T: Millis = 1_800_000_000_000 - 1_800_000_000_000 % (60 * MINUTE_MS);

    fn minute(i: usize, on: bool) -> MinuteLabel {
        MinuteLabel {
            ts: T + i as Millis * MINUTE_MS,
            place: if on { Place::Phone } else { Place::Away },
            device: on.then(|| "phone".into()),
            activity: on.then_some(Activity::Feeds),
            project: None,
            background: None,
            background_device: None,
            rule: if on { "r" } else { "presence.away" }.into(),
            decider: Decider::Rule,
            complete: true,
            location: None,
            what: None,
            background_what: None,
        }
    }

    fn day(on: &[(usize, usize)]) -> Vec<MinuteLabel> {
        (0..24 * 60)
            .map(|i| minute(i, on.iter().any(|(a, b)| *a <= i && i < *b)))
            .collect()
    }

    #[test]
    fn the_longest_quiet_stretch_bridged_across_a_look_at_the_phone() {
        // In use until 9:26 in, a 1-minute look at 10:59, back at 16:36.
        let labels = day(&[
            (0, 9 * 60 + 26),
            (10 * 60 + 59, 11 * 60),
            (16 * 60 + 36, 24 * 60),
        ]);
        let c = candidate(&labels, T, T + 24 * 60 * MINUTE_MS).unwrap();
        assert_eq!(c.start_ms, T + (9 * 60 + 26) * MINUTE_MS);
        assert_eq!(c.end_ms, T + (16 * 60 + 36) * MINUTE_MS);
        assert_eq!(c.interruptions.len(), 1);
    }

    #[test]
    fn falling_asleep_right_after_a_short_scroll_is_after_it_not_before() {
        // Work until 3:18, 1 quiet minute, 7 minutes of X, then sleep: the
        // 7 minutes are not a blip because the quiet before them is short.
        let labels = day(&[
            (0, 3 * 60 + 18),
            (3 * 60 + 19, 3 * 60 + 26),
            (10 * 60, 24 * 60),
        ]);
        let c = candidate(&labels, T, T + 24 * 60 * MINUTE_MS).unwrap();
        assert_eq!(c.start_ms, T + (3 * 60 + 26) * MINUTE_MS);
    }

    #[test]
    fn a_long_use_in_the_night_splits_it_and_no_quiet_is_no_sleep() {
        let labels = day(&[(0, 60), (180, 200), (400, 24 * 60)]);
        let c = candidate(&labels, T, T + 24 * 60 * MINUTE_MS).unwrap();
        assert_eq!(c.start_ms, T + 200 * MINUTE_MS);
        assert!(c.interruptions.is_empty());
        assert!(candidate(&day(&[(0, 24 * 60)]), T, T + 24 * 60 * MINUTE_MS).is_none());
    }
}
