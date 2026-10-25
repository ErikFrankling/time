//! Six tracks: computer screen, computer sound, phone screen, phone sound,
//! keyboard and mouse, and voice.
//!
//! The minute labels say where the attention was, one place a minute; these
//! tracks say what each device and channel was doing, so a YouTube video on
//! the phone while Spotify plays on the computer shows as two filled tracks
//! at once. Screen tracks come from the labels (the attention and the
//! parallel activity), sound tracks from the `time.audio.listening` events,
//! which only exist for sound that could be heard, the keyboard and mouse
//! track from the input counts in each computer minute, and the voice track
//! from dictations, voice mode, T3 read-aloud and the speak daemon.
//!
//! Every track keeps what filled it (`whats`: feeds -> x.com, reddit.com),
//! when in the local day it happened (`slots`, ten minutes each, for the
//! 24-hour clock), and for a single day its stretches (`spans`) and the
//! keyboard and mouse intensity per ten minutes (`chunks`).

use std::collections::BTreeMap;

use chrono::{TimeZone, Timelike};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, Listening};
use time_core::label::{MinuteLabel, Place};

use crate::store::InputMinute;

pub const NAMES: [&str; 6] = [
    "Computer · screen",
    "Computer · sound",
    "Phone · screen",
    "Phone · sound",
    "Keyboard & mouse",
    "Voice",
];
pub const COMPUTER_SCREEN: usize = 0;
pub const COMPUTER_SOUND: usize = 1;
pub const PHONE_SCREEN: usize = 2;
pub const PHONE_SOUND: usize = 3;
pub const INPUT: usize = 4;
pub const VOICE: usize = 5;
pub const TRACKS: usize = NAMES.len();

/// Ten-minute slots in a day: the clock's and the input chunks' resolution.
pub const SLOT_MIN: u32 = 10;
pub const SLOTS: usize = (24 * 60 / SLOT_MIN) as usize;

/// The event names the voice track is built from.
pub const VOICE_EVENTS: [&str; 3] = [
    "stt.dictation.finished",
    "t3.readaloud.played",
    "t3.voicemode.span",
];

/// Whether a track's keys are activities (screen) or sources (sound).
pub fn is_sound(track: usize) -> bool {
    track == COMPUTER_SOUND || track == PHONE_SOUND
}

/// A sound minute needs at least this much heard to be drawn as filled.
const SOUND_MINUTE_SECS: u32 = 15;
/// A T3 dictation records when it was sent, not how long it took.
const DICTATION_GUESS_MS: Millis = 20_000;

/// A stretch of one track doing one thing, in local minutes of the day
/// (00:00 = 0), so the axis is 24 hours even on a daylight-saving day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub track: usize,
    pub from: u32,
    pub to: u32,
    pub what: String,
    /// What filled it most: the site, program or app (screen), title
    /// (sound), or client (voice).
    pub detail: String,
}

/// Ten minutes of keyboard and mouse on the computers.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Chunk {
    /// Local time the chunk starts, "14:20".
    pub from: String,
    pub start_ms: Millis,
    /// Key presses (mouse buttons excluded where the agent tells them apart).
    pub keys: u32,
    pub clicks: u32,
    /// Pointer motion events.
    pub mouse: u32,
    /// Seconds with any input, of 600. Minutes from agents that did not
    /// count seconds count as 60 when they had any input.
    pub active_seconds: u32,
    /// Minutes with any input, of 10.
    pub input_minutes: u32,
    /// Key presses and clicks per minute over the chunk.
    pub per_minute: f64,
}

#[derive(Debug, Clone)]
pub struct Tracks {
    /// Seconds per track, by key: activity (screen), source (sound),
    /// typing or pointer (keyboard and mouse), kind (voice).
    pub seconds: [BTreeMap<String, u32>; TRACKS],
    /// Seconds per track and key, by what filled it.
    pub whats: [BTreeMap<String, BTreeMap<String, u32>>; TRACKS],
    /// Seconds per track and ten-minute slot of the local day, by key.
    pub slots: [Vec<BTreeMap<String, u32>>; TRACKS],
    /// Minutes with any track filled: the rings' full circle.
    pub active_minutes: u32,
    /// Days added up, for averages and the clock's shading.
    pub days: u32,
    /// Only for a single day.
    pub spans: Vec<Span>,
    pub chunks: Vec<Chunk>,
}

impl Default for Tracks {
    fn default() -> Self {
        Self {
            seconds: Default::default(),
            whats: Default::default(),
            slots: std::array::from_fn(|_| vec![BTreeMap::new(); SLOTS]),
            active_minutes: 0,
            days: 0,
            spans: Vec::new(),
            chunks: Vec::new(),
        }
    }
}

/// What a minute of one track was: its key and what filled it.
type Cell = Option<(String, String)>;
/// Sound in one minute, per source: seconds, and seconds per title.
type Heard = BTreeMap<String, (u32, BTreeMap<String, u32>)>;

/// The voice kind and detail of an event, if it is a voice event.
fn voice_kind(e: &Envelope) -> Option<(&'static str, String, Millis, Millis)> {
    let (kind, detail) = match &e.event {
        Event::DictationFinished(d) => (
            "dictation",
            match d.client.as_str() {
                "hotkey" => "desktop hotkey".to_owned(),
                "" => "T3 Code".to_owned(),
                c => format!("T3 Code ({c})"),
            },
        ),
        Event::VoiceModeSpan(_) => ("voice mode", "T3 Code".to_owned()),
        Event::ReadAloudPlayed(r) if r.mode == "speak" => (
            "speak",
            if r.voice.is_empty() {
                "speak".to_owned()
            } else {
                r.voice.clone()
            },
        ),
        Event::ReadAloudPlayed(_) => ("read aloud", "T3 Code".to_owned()),
        _ => return None,
    };
    let end = if e.end_ms > e.start_ms {
        e.end_ms
    } else {
        match &e.event {
            Event::DictationFinished(d) if d.audio_seconds > 0.0 => {
                e.start_ms + (d.audio_seconds * 1000.0) as Millis
            }
            Event::ReadAloudPlayed(r) if r.seconds_played > 0.0 => {
                e.start_ms + (r.seconds_played * 1000.0) as Millis
            }
            _ => e.start_ms + DICTATION_GUESS_MS,
        }
    };
    Some((kind, detail, e.start_ms, end))
}

impl Tracks {
    #[allow(clippy::too_many_arguments)]
    pub fn build<'a>(
        labels: &[MinuteLabel],
        heard: impl IntoIterator<Item = (&'a str, Millis, &'a Listening)>,
        input: &[InputMinute],
        voice: &[Envelope],
        phones: &[String],
        tz: chrono_tz::Tz,
        from: Millis,
        to: Millis,
    ) -> Self {
        let n = ((to - from) / MINUTE_MS).max(0) as usize;
        let mut grid: [Vec<Cell>; TRACKS] = std::array::from_fn(|_| vec![None; n]);
        let mut t = Tracks {
            days: 1,
            ..Default::default()
        };
        let idx = |ts: Millis| -> Option<usize> {
            (ts >= from && ts < to).then(|| ((ts - from) / MINUTE_MS) as usize)
        };
        // Local minute of the day for each minute index.
        let local: Vec<u32> = (0..n)
            .map(|i| {
                tz.timestamp_millis_opt(from + i as Millis * MINUTE_MS)
                    .single()
                    .map(|d| d.hour() * 60 + d.minute())
                    .unwrap_or(i as u32 % 1440)
            })
            .collect();
        let slot = |i: usize| (local[i] / SLOT_MIN) as usize;
        let add = |t: &mut Tracks, track: usize, i: usize, key: &str, what: &str, secs: u32| {
            *t.seconds[track].entry(key.to_owned()).or_default() += secs;
            if !what.is_empty() {
                *t.whats[track]
                    .entry(key.to_owned())
                    .or_default()
                    .entry(what.to_owned())
                    .or_default() += secs;
            }
            *t.slots[track][slot(i)].entry(key.to_owned()).or_default() += secs;
        };

        // Screens.
        for l in labels {
            let Some(i) = idx(l.ts) else { continue };
            let primary = match l.place {
                Place::Computer => Some(COMPUTER_SCREEN),
                Place::Phone => Some(PHONE_SCREEN),
                Place::Away => None,
            };
            if let (Some(track), Some(a)) = (primary, l.activity) {
                grid[track][i] = Some((a.as_str().to_owned(), l.what.clone().unwrap_or_default()));
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
                    grid[track][i] = Some((
                        b.as_str().to_owned(),
                        l.background_what.clone().unwrap_or_default(),
                    ));
                }
            }
        }
        for track in [COMPUTER_SCREEN, PHONE_SCREEN] {
            for (i, cell) in grid[track].iter().enumerate() {
                if let Some((k, w)) = cell {
                    add(&mut t, track, i, k, w, 60);
                }
            }
        }

        // Sound: seconds per source; a minute is drawn as its loudest source.
        let mut per_minute: [BTreeMap<usize, Heard>; 2] = Default::default();
        for (device, start, l) in heard {
            let Some(i) = idx(start - start.rem_euclid(MINUTE_MS)) else {
                continue;
            };
            let phone = phones.iter().any(|p| p == device);
            let track = if phone { PHONE_SOUND } else { COMPUTER_SOUND };
            let secs = l.audible_seconds.min(60);
            let what = if l.title.is_empty() { &l.app } else { &l.title };
            add(&mut t, track, i, &l.source, what, secs);
            let m = per_minute[usize::from(phone)]
                .entry(i)
                .or_default()
                .entry(l.source.clone())
                .or_default();
            m.0 += secs;
            *m.1.entry(what.clone()).or_default() += secs;
        }
        for (k, track) in [COMPUTER_SOUND, PHONE_SOUND].into_iter().enumerate() {
            for (i, by) in &per_minute[k] {
                let total: u32 = by.values().map(|v| v.0).sum();
                if total >= SOUND_MINUTE_SECS {
                    grid[track][*i] = by.iter().max_by_key(|(_, v)| v.0).map(|(s, v)| {
                        let top = v.1.iter().max_by_key(|(_, n)| **n).map(|(w, _)| w.clone());
                        (s.clone(), top.unwrap_or_default())
                    });
                }
            }
        }

        // Keyboard and mouse: every computer minute added up.
        let mut inputs: BTreeMap<usize, InputMinute> = BTreeMap::new();
        for m in input {
            let Some(i) = idx(m.ts - m.ts.rem_euclid(MINUTE_MS)) else {
                continue;
            };
            let s = inputs.entry(i).or_default();
            s.keys += m.keys;
            s.clicks += m.clicks;
            s.mouse += m.mouse;
            let secs = m
                .active_seconds
                .unwrap_or(if m.keys + m.mouse > 0 { 60 } else { 0 });
            s.active_seconds = Some(s.active_seconds.unwrap_or(0).max(secs));
        }
        let mut chunks: BTreeMap<usize, Chunk> = BTreeMap::new();
        for (i, m) in &inputs {
            let secs = m.active_seconds.unwrap_or(0);
            if m.keys + m.mouse > 0 {
                let typed = m.keys.saturating_sub(m.clicks) > 0;
                let key = if typed { "typing" } else { "pointer" };
                grid[INPUT][*i] = Some((key.to_owned(), String::new()));
                add(&mut t, INPUT, *i, key, "", secs.max(1));
            }
            let c = chunks.entry(slot(*i)).or_insert_with(|| Chunk {
                start_ms: from + *i as Millis * MINUTE_MS,
                from: format!("{:02}:{:02}", local[*i] / 60, local[*i] / SLOT_MIN % 6 * 10),
                ..Default::default()
            });
            c.keys += m.keys.saturating_sub(m.clicks);
            c.clicks += m.clicks;
            c.mouse += m.mouse;
            c.active_seconds += secs;
            if m.keys + m.mouse > 0 {
                c.input_minutes += 1;
            }
        }
        t.chunks = chunks
            .into_values()
            .map(|mut c| {
                c.start_ms -= c.start_ms.rem_euclid(SLOT_MIN as Millis * MINUTE_MS);
                c.per_minute = f64::from(c.keys + c.clicks) / f64::from(SLOT_MIN);
                c.per_minute = (c.per_minute * 10.0).round() / 10.0;
                c
            })
            .filter(|c| c.input_minutes > 0)
            .collect();

        // Voice: seconds per kind in each minute; a minute is its biggest.
        let mut vmin: BTreeMap<usize, BTreeMap<&str, (u32, String)>> = BTreeMap::new();
        for e in voice {
            let Some((kind, detail, start, end)) = voice_kind(e) else {
                continue;
            };
            let mut ts = start.max(from) - start.max(from).rem_euclid(MINUTE_MS);
            while ts < end.min(to) {
                let secs = ((end.min(ts + MINUTE_MS) - start.max(ts)) / 1000).clamp(0, 60) as u32;
                if let (Some(i), true) = (idx(ts), secs > 0) {
                    add(&mut t, VOICE, i, kind, &detail, secs);
                    let v = vmin.entry(i).or_default().entry(kind).or_default();
                    v.0 += secs;
                    v.1 = detail.clone();
                }
                ts += MINUTE_MS;
            }
        }
        for (i, by) in vmin {
            grid[VOICE][i] = by
                .iter()
                .max_by_key(|(_, v)| v.0)
                .map(|(k, v)| ((*k).to_owned(), v.1.clone()));
        }

        t.active_minutes = (0..n)
            .filter(|i| grid.iter().any(|g| g[*i].is_some()))
            .count() as u32;

        // Stretches, in local minutes; a daylight-saving jump ends one.
        for (track, cells) in grid.iter().enumerate() {
            let mut run: Option<(Span, BTreeMap<String, u32>)> = None;
            for (i, c) in cells.iter().enumerate() {
                let at = local[i];
                match (c, run.as_mut()) {
                    (Some((k, w)), Some((r, ws))) if r.what == *k && r.to == at => {
                        r.to += 1;
                        *ws.entry(w.clone()).or_default() += 1;
                    }
                    (Some((k, w)), _) => {
                        t.spans.extend(run.take().map(finish));
                        run = Some((
                            Span {
                                track,
                                from: at,
                                to: at + 1,
                                what: k.clone(),
                                detail: String::new(),
                            },
                            BTreeMap::from([(w.clone(), 1)]),
                        ));
                    }
                    (None, _) => t.spans.extend(run.take().map(finish)),
                }
            }
            t.spans.extend(run.map(finish));
        }
        t
    }

    /// Add another day's totals (not its spans or chunks).
    pub fn add(&mut self, o: &Tracks) {
        for (mine, theirs) in self.seconds.iter_mut().zip(&o.seconds) {
            for (k, s) in theirs {
                *mine.entry(k.clone()).or_default() += s;
            }
        }
        for (mine, theirs) in self.whats.iter_mut().zip(&o.whats) {
            for (k, by) in theirs {
                let m = mine.entry(k.clone()).or_default();
                for (w, s) in by {
                    *m.entry(w.clone()).or_default() += s;
                }
            }
        }
        for (mine, theirs) in self.slots.iter_mut().zip(&o.slots) {
            for (m, t) in mine.iter_mut().zip(theirs) {
                for (k, s) in t {
                    *m.entry(k.clone()).or_default() += s;
                }
            }
        }
        self.active_minutes += o.active_minutes;
        self.days += o.days;
    }

    pub fn minutes(&self, track: usize) -> u32 {
        crate::listening::minutes(self.seconds[track].values().sum())
    }
}

fn finish((mut s, ws): (Span, BTreeMap<String, u32>)) -> Span {
    s.detail = ws
        .into_iter()
        .filter(|(w, _)| !w.is_empty())
        .max_by_key(|(_, n)| *n)
        .map(|(w, _)| w)
        .unwrap_or_default();
    s
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
            what: None,
            background_what: None,
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
        let t = Tracks::build(
            &labels,
            events,
            &[],
            &[],
            &phones,
            chrono_tz::UTC,
            0,
            60 * m,
        );
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

    #[test]
    fn keyboard_and_mouse_in_ten_minute_chunks_and_voice_by_kind() {
        use time_core::event::{DictationFinished, ReadAloudPlayed};
        let m = MINUTE_MS;
        // 00:00-00:04 typing (30 active seconds a minute), 00:12 pointer only,
        // an older agent without seconds at 00:13.
        let mut input: Vec<InputMinute> = (0..5)
            .map(|i| InputMinute {
                ts: i * m,
                keys: 40,
                clicks: 2,
                mouse: 300,
                active_seconds: Some(30),
            })
            .collect();
        input.push(InputMinute {
            ts: 12 * m,
            mouse: 50,
            active_seconds: Some(6),
            ..Default::default()
        });
        input.push(InputMinute {
            ts: 13 * m,
            keys: 1,
            ..Default::default()
        });
        let dictation = Envelope::new(
            "pc",
            "speak",
            20 * m,
            20 * m + 31_000,
            Event::DictationFinished(DictationFinished {
                recording_id: "hotkey:x".into(),
                client: "hotkey".into(),
                audio_seconds: 31.0,
                words: 0,
                outcome: "recorded".into(),
            }),
        );
        let spoken = Envelope::new(
            "pc",
            "speak",
            30 * m,
            32 * m,
            Event::ReadAloudPlayed(ReadAloudPlayed {
                mode: "speak".into(),
                voice: "personal-agent".into(),
                seconds_played: 120.0,
                ..Default::default()
            }),
        );
        let t = Tracks::build(
            &[],
            std::iter::empty(),
            &input,
            &[dictation, spoken],
            &[],
            chrono_tz::UTC,
            0,
            60 * m,
        );
        assert_eq!(t.chunks.len(), 2);
        let c = &t.chunks[0];
        assert_eq!((c.from.as_str(), c.keys, c.clicks), ("00:00", 190, 10));
        assert_eq!(
            (c.active_seconds, c.input_minutes, c.per_minute),
            (150, 5, 20.0)
        );
        let c = &t.chunks[1];
        assert_eq!(
            (c.from.as_str(), c.active_seconds, c.input_minutes),
            ("00:10", 66, 2)
        );
        assert_eq!(t.seconds[INPUT]["typing"], 150 + 60);
        assert_eq!(t.seconds[INPUT]["pointer"], 6);
        assert_eq!(t.seconds[VOICE]["dictation"], 31);
        assert_eq!(t.seconds[VOICE]["speak"], 120);
        assert_eq!(t.whats[VOICE]["speak"]["personal-agent"], 120);
        assert_eq!(t.slots[VOICE][3]["speak"], 120, "00:30 is slot 3");
        let voice: Vec<(u32, u32, &str, &str)> = t
            .spans
            .iter()
            .filter(|s| s.track == VOICE)
            .map(|s| (s.from, s.to, s.what.as_str(), s.detail.as_str()))
            .collect();
        assert_eq!(
            voice,
            [
                (20, 21, "dictation", "desktop hotkey"),
                (30, 32, "speak", "personal-agent")
            ]
        );
        assert_eq!(t.active_minutes, 5 + 2 + 1 + 2);
    }
}
