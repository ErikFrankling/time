//! Four tracks: computer screen, computer sound, phone screen, phone sound.
//!
//! The minute labels say where the attention was, one place a minute; these
//! tracks say what each device was doing, so a YouTube video on the phone
//! while Spotify plays on the computer shows as two filled tracks at once.
//! Screen tracks come from the labels (the attention and the parallel
//! activity), sound tracks from the `time.audio.listening` events, which
//! only exist for sound that could be heard.

use std::collections::BTreeMap;

use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::Listening;
use time_core::label::{MinuteLabel, Place};

pub const NAMES: [&str; 4] = [
    "Computer · screen",
    "Computer · sound",
    "Phone · screen",
    "Phone · sound",
];
pub const COMPUTER_SCREEN: usize = 0;
pub const COMPUTER_SOUND: usize = 1;
pub const PHONE_SCREEN: usize = 2;
pub const PHONE_SOUND: usize = 3;

/// Whether a track's keys are activities (screen) or sources (sound).
pub fn is_sound(track: usize) -> bool {
    track == COMPUTER_SOUND || track == PHONE_SOUND
}

/// A sound minute needs at least this much heard to be drawn as filled.
const SOUND_MINUTE_SECS: u32 = 15;

/// A stretch of one track doing one thing, in minutes from the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub track: usize,
    pub from: u32,
    pub to: u32,
    pub what: String,
}

#[derive(Debug, Default, Clone)]
pub struct Tracks {
    /// Seconds per track, by activity (screen) or source (sound).
    pub seconds: [BTreeMap<String, u32>; 4],
    /// Only for a single day.
    pub spans: Vec<Span>,
}

impl Tracks {
    pub fn build<'a>(
        labels: &[MinuteLabel],
        heard: impl IntoIterator<Item = (&'a str, Millis, &'a Listening)>,
        phones: &[String],
        from: Millis,
        to: Millis,
    ) -> Self {
        let n = ((to - from) / MINUTE_MS).max(0) as usize;
        let mut grid: [Vec<Option<String>>; 4] = std::array::from_fn(|_| vec![None; n]);
        let mut t = Tracks::default();
        let idx = |ts: Millis| -> Option<usize> {
            (ts >= from && ts < to).then(|| ((ts - from) / MINUTE_MS) as usize)
        };
        for l in labels {
            let Some(i) = idx(l.ts) else { continue };
            let primary = match l.place {
                Place::Computer => Some(COMPUTER_SCREEN),
                Place::Phone => Some(PHONE_SCREEN),
                Place::Away => None,
            };
            if let (Some(track), Some(a)) = (primary, l.activity) {
                grid[track][i] = Some(a.as_str().to_owned());
            }
            if let Some(b) = l.background {
                let on_phone = match &l.background_device {
                    Some(d) => phones.contains(d),
                    None => l.place != Place::Phone,
                };
                let track = if on_phone {
                    PHONE_SCREEN
                } else {
                    COMPUTER_SCREEN
                };
                if grid[track][i].is_none() {
                    grid[track][i] = Some(b.as_str().to_owned());
                }
            }
        }
        for track in [COMPUTER_SCREEN, PHONE_SCREEN] {
            for w in grid[track].iter().flatten() {
                *t.seconds[track].entry(w.clone()).or_default() += 60;
            }
        }
        // Sound: seconds per source; a minute is drawn as its loudest source.
        let mut per_minute: [BTreeMap<usize, BTreeMap<String, u32>>; 2] = Default::default();
        for (device, start, l) in heard {
            let Some(i) = idx(start - start.rem_euclid(MINUTE_MS)) else {
                continue;
            };
            let phone = phones.iter().any(|p| p == device);
            let track = if phone { PHONE_SOUND } else { COMPUTER_SOUND };
            let secs = l.audible_seconds.min(60);
            *t.seconds[track].entry(l.source.clone()).or_default() += secs;
            *per_minute[usize::from(phone)]
                .entry(i)
                .or_default()
                .entry(l.source.clone())
                .or_default() += secs;
        }
        for (k, track) in [COMPUTER_SOUND, PHONE_SOUND].into_iter().enumerate() {
            for (i, by) in &per_minute[k] {
                let total: u32 = by.values().sum();
                if total >= SOUND_MINUTE_SECS {
                    grid[track][*i] = by.iter().max_by_key(|(_, s)| **s).map(|(s, _)| s.clone());
                }
            }
        }
        for (track, cells) in grid.iter().enumerate() {
            let mut run: Option<Span> = None;
            for (i, c) in cells.iter().enumerate() {
                match (c, run.as_mut()) {
                    (Some(w), Some(r)) if r.what == *w && r.to == i as u32 => r.to += 1,
                    (Some(w), _) => {
                        t.spans.extend(run.take());
                        run = Some(Span {
                            track,
                            from: i as u32,
                            to: i as u32 + 1,
                            what: w.clone(),
                        });
                    }
                    (None, _) => t.spans.extend(run.take()),
                }
            }
            t.spans.extend(run);
        }
        t
    }

    /// Add another day's totals (not its spans).
    pub fn add(&mut self, o: &Tracks) {
        for (mine, theirs) in self.seconds.iter_mut().zip(&o.seconds) {
            for (k, s) in theirs {
                *mine.entry(k.clone()).or_default() += s;
            }
        }
    }

    pub fn minutes(&self, track: usize) -> u32 {
        crate::listening::minutes(self.seconds[track].values().sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::label::{Activity, Decider};

    fn label(ts: Millis, place: Place, a: Activity, bg: Option<(Activity, &str)>) -> MinuteLabel {
        MinuteLabel {
            ts,
            place,
            device: Some(if place == Place::Phone { "phone" } else { "pc" }.into()),
            activity: (place != Place::Away).then_some(a),
            project: None,
            background: bg.map(|(b, _)| b),
            background_device: bg.map(|(_, d)| d.to_owned()),
            rule: String::new(),
            decider: Decider::Rule,
            complete: true,
            location: None,
        }
    }

    fn heard(source: &str, secs: u32) -> Listening {
        Listening {
            source: source.into(),
            audible_seconds: secs,
            ..Default::default()
        }
    }

    #[test]
    fn each_device_and_modality_gets_its_own_track() {
        let m = MINUTE_MS;
        let phones = vec!["phone".to_owned()];
        // 0-9 work on the pc; 10-19 YouTube on the phone while the pc plays
        // a video in the background.
        let mut labels: Vec<MinuteLabel> = (0..10)
            .map(|i| label(i * m, Place::Computer, Activity::Work, None))
            .collect();
        labels.extend((10..20).map(|i| {
            label(
                i * m,
                Place::Phone,
                Activity::Video,
                Some((Activity::Video, "pc")),
            )
        }));
        let spotify = heard("spotify", 60);
        let youtube = heard("youtube.com", 60);
        let quiet = heard("spotify", 5);
        let mut events: Vec<(&str, Millis, &Listening)> =
            (0..20).map(|i| ("pc", i * m, &spotify)).collect();
        events.extend((10..20).map(|i| ("phone", i * m + 1000, &youtube)));
        events.push(("pc", 30 * m, &quiet));
        let t = Tracks::build(&labels, events, &phones, 0, 60 * m);
        assert_eq!(
            t.minutes(COMPUTER_SCREEN),
            20,
            "work, then video in the background"
        );
        assert_eq!(t.minutes(PHONE_SCREEN), 10);
        assert_eq!(t.minutes(COMPUTER_SOUND), 20);
        assert_eq!(t.minutes(PHONE_SOUND), 10);
        assert_eq!(t.seconds[PHONE_SOUND]["youtube.com"], 600);
        let spans = |track| -> Vec<(u32, u32, String)> {
            t.spans
                .iter()
                .filter(|s| s.track == track)
                .map(|s| (s.from, s.to, s.what.clone()))
                .collect()
        };
        assert_eq!(
            spans(COMPUTER_SCREEN),
            [(0, 10, "work".into()), (10, 20, "video".into())]
        );
        assert_eq!(spans(PHONE_SOUND), [(10, 20, "youtube.com".into())]);
        // Five seconds is counted but not drawn as a filled minute.
        assert_eq!(spans(COMPUTER_SOUND), [(0, 20, "spotify".into())]);
    }
}
