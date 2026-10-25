//! Listening: what was really audible, per source ("Spotify 2 h 10 min,
//! YouTube 40 min").
//!
//! Built from `time.audio.listening` events, which the desktop agent sends
//! only for sound that could be heard (a running, unmuted stream above
//! silence). Each audible minute is also split by whether Erik was at a
//! device: a minute labelled away is "listening (away from keyboard)", music
//! across the room, not screen time.
//!
//! Track and tab titles never leave Time: they are in `/api/listening` and
//! the MCP tools, which are lan-only, and nowhere else.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::{json, Value};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, Listening};
use time_core::label::{MinuteLabel, Place};

/// One source's seconds in a period.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Heard {
    pub seconds: u32,
    /// Of those, while no device was in use.
    pub away_seconds: u32,
}

impl Heard {
    pub fn minutes(&self) -> u32 {
        minutes(self.seconds)
    }
    pub fn away_minutes(&self) -> u32 {
        minutes(self.away_seconds)
    }
    pub fn add(&mut self, o: Heard) {
        self.seconds += o.seconds;
        self.away_seconds += o.away_seconds;
    }
}

pub fn minutes(seconds: u32) -> u32 {
    (seconds + 30) / 60
}

/// Per source: totals, seconds per slot (hour of a day), and the titles
/// heard most.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    pub sources: BTreeMap<String, Heard>,
    pub slots: BTreeMap<String, Vec<u32>>,
    pub titles: BTreeMap<(String, String), u32>,
}

impl Tally {
    /// Listening events in `events`, split by `labels` (away or not), with
    /// `slot_ms`-long slots from `from` (an hour for a day).
    pub fn new<'a>(
        events: impl IntoIterator<Item = (&'a str, Millis, &'a Listening)>,
        labels: &[MinuteLabel],
        from: Millis,
        to: Millis,
        slot_ms: Millis,
    ) -> Self {
        let away: HashMap<Millis, bool> = labels
            .iter()
            .map(|l| (l.ts, l.place == Place::Away))
            .collect();
        let slots = ((to - from + slot_ms - 1) / slot_ms).max(0) as usize;
        let mut t = Tally::default();
        for (_device, start, l) in events {
            if start < from || start >= to || l.audible_seconds == 0 {
                continue;
            }
            let secs = l.audible_seconds.min(60);
            let minute = start - start.rem_euclid(MINUTE_MS);
            // A minute with no label yet counts as at the computer.
            let is_away = away.get(&minute).copied().unwrap_or(false);
            t.sources.entry(l.source.clone()).or_default().add(Heard {
                seconds: secs,
                away_seconds: if is_away { secs } else { 0 },
            });
            let v = t
                .slots
                .entry(l.source.clone())
                .or_insert_with(|| vec![0; slots]);
            if let Some(s) = v.get_mut(((start - from) / slot_ms) as usize) {
                *s += secs;
            }
            let title = match (l.artist.is_empty(), l.title.is_empty()) {
                (_, true) => continue,
                (true, false) => l.title.clone(),
                (false, false) => format!("{} – {}", l.artist, l.title),
            };
            *t.titles.entry((l.source.clone(), title)).or_default() += secs;
        }
        t
    }

    pub fn from_events(
        events: &[Envelope],
        labels: &[MinuteLabel],
        from: Millis,
        to: Millis,
        slot_ms: Millis,
    ) -> Self {
        Self::new(of(events), labels, from, to, slot_ms)
    }

    pub fn total(&self) -> Heard {
        let mut h = Heard::default();
        for s in self.sources.values() {
            h.add(*s);
        }
        h
    }

    /// Sources, most heard first.
    pub fn ranked(&self) -> Vec<(&String, &Heard)> {
        let mut v: Vec<_> = self.sources.iter().collect();
        v.sort_by_key(|(n, h)| (std::cmp::Reverse(h.seconds), (*n).clone()));
        v
    }

    /// "Spotify 2 h 10 min, YouTube 40 min".
    pub fn sentence(&self, n: usize) -> String {
        self.ranked()
            .into_iter()
            .filter(|(_, h)| h.minutes() > 0)
            .take(n)
            .map(|(s, h)| format!("{} {}", name(s), dur(h.minutes())))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn json(&self, titles_per_source: usize) -> Value {
        let total = self.total();
        let sources: Vec<Value> = self
            .ranked()
            .into_iter()
            .map(|(s, h)| {
                let mut top: Vec<(&String, &u32)> = self
                    .titles
                    .iter()
                    .filter(|((src, _), _)| src == s)
                    .map(|((_, t), n)| (t, n))
                    .collect();
                top.sort_by_key(|(t, n)| (std::cmp::Reverse(**n), (*t).clone()));
                json!({
                    "source": s,
                    "name": name(s),
                    "minutes": h.minutes(),
                    "away_from_keyboard_minutes": h.away_minutes(),
                    "top": top.into_iter().take(titles_per_source)
                        .map(|(t, n)| json!({"title": t, "minutes": minutes(*n)}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "minutes": total.minutes(),
            "away_from_keyboard_minutes": total.away_minutes(),
            "sources": sources,
        })
    }
}

/// The listening events among `events`.
pub fn of(events: &[Envelope]) -> impl Iterator<Item = (&str, Millis, &Listening)> {
    events.iter().filter_map(|e| match &e.event {
        Event::Listening(l) => Some((e.device.as_str(), e.start_ms, l)),
        _ => None,
    })
}

/// How a source reads on the page: `spotify` -> Spotify, `youtube.com` ->
/// YouTube, `web (zen)` -> "Zen (other sites)".
pub fn name(source: &str) -> String {
    let known = [
        ("spotify", "Spotify"),
        ("youtube.com", "YouTube"),
        ("music.youtube.com", "YouTube Music"),
        ("netflix.com", "Netflix"),
        ("twitch.tv", "Twitch"),
        ("x.com", "X"),
        ("hbomax.com", "HBO Max"),
        ("play.hbomax.com", "HBO Max"),
        ("svtplay.se", "SVT Play"),
        ("disneyplus.com", "Disney+"),
        ("primevideo.com", "Prime Video"),
        ("soundcloud.com", "SoundCloud"),
        ("mpv", "mpv"),
        ("vlc", "VLC"),
        ("blocked", "Private"),
    ];
    if let Some((_, n)) = known.iter().find(|(k, _)| *k == source) {
        return (*n).to_owned();
    }
    if let Some(app) = source
        .strip_prefix("web (")
        .and_then(|s| s.strip_suffix(')'))
    {
        return format!("{} (other sites)", capitalise(app));
    }
    if source.contains('.') {
        return source.trim_start_matches("www.").to_owned();
    }
    capitalise(source)
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

pub fn dur(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, r) => format!("{r} min"),
        (h, 0) => format!("{h} h"),
        (h, r) => format!("{h} h {r} min"),
    }
}

/// What is audible right now: the newest minute each device reported
/// sound in, if it is one of the last two.
pub fn audible_now(events: &[Envelope], now: Millis) -> Vec<(String, Listening)> {
    let recent = now - now.rem_euclid(MINUTE_MS) - 2 * MINUTE_MS;
    let mut newest: BTreeMap<&str, Millis> = BTreeMap::new();
    for (d, t, _) in of(events) {
        if t >= recent {
            let e = newest.entry(d).or_insert(t);
            *e = (*e).max(t);
        }
    }
    let mut out: Vec<(String, Listening)> = of(events)
        .filter(|(d, t, _)| newest.get(d) == Some(t))
        .map(|(d, _, l)| (d.to_owned(), l.clone()))
        .collect();
    out.sort_by_key(|(_, l)| std::cmp::Reverse(l.audible_seconds));
    out
}

/// `time_now`'s part: JSON rows and a sentence ("Audible now: Spotify,
/// Green Day – Boulevard of Broken Dreams (pc).").
pub fn now_json_text(events: &[Envelope], now: Millis) -> (Value, String) {
    let heard = audible_now(events, now);
    let rows: Vec<Value> = heard
        .iter()
        .map(|(d, l)| {
            json!({
                "device": d,
                "source": l.source,
                "name": name(&l.source),
                "app": l.app,
                "title": (!l.title.is_empty()).then(|| l.title.clone()),
                "artist": (!l.artist.is_empty()).then(|| l.artist.clone()),
                "audible_seconds": l.audible_seconds,
            })
        })
        .collect();
    let text = if heard.is_empty() {
        " Nothing is audible on the computers.".to_owned()
    } else {
        let parts: Vec<String> = heard
            .iter()
            .map(|(d, l)| {
                let what = match (l.artist.is_empty(), l.title.is_empty()) {
                    (_, true) => String::new(),
                    (true, false) => format!(", \"{}\"", clip(&l.title, 60)),
                    (false, false) => format!(", {} – {}", l.artist, clip(&l.title, 60)),
                };
                format!("{}{what} ({d})", name(&l.source))
            })
            .collect();
        format!(" Audible now: {}.", parts.join("; "))
    };
    (Value::Array(rows), text)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

/// `time_listening` / `/api/listening`: a window's listening by source.
pub fn digest(
    w: &crate::digest::Window,
    labels: &[MinuteLabel],
    events: &[Envelope],
    now: Millis,
) -> crate::digest::Digest {
    let t = Tally::from_events(events, labels, w.from_ms, w.to_ms, 3_600_000);
    let total = t.total();
    let (audible, now_text) = now_json_text(events, now);
    let mut json = t.json(5);
    json["window"] = json!(w.label);
    json["audible_now"] = audible;
    let text = if total.minutes() == 0 {
        format!("{}: nothing audible recorded.{now_text}", w.label)
    } else {
        let afk = if total.away_minutes() > 0 {
            format!(
                ", {} of it away from the keyboard",
                dur(total.away_minutes())
            )
        } else {
            String::new()
        };
        format!(
            "{}: {} of audible sound{afk}. {}.{now_text}",
            capitalise(&w.label),
            dur(total.minutes()),
            t.sentence(6)
        )
    };
    crate::digest::Digest { json, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::label::{Activity, Decider};

    fn heard(source: &str, start: Millis, secs: u32) -> Envelope {
        Envelope::new(
            "pc",
            "time-agent",
            start,
            start + MINUTE_MS,
            Event::Listening(Listening {
                app: "spotify".into(),
                source: source.into(),
                title: "Boulevard of Broken Dreams".into(),
                artist: "Green Day".into(),
                audible_seconds: secs,
                peak: 0.3,
                mpris_status: Some("Playing".into()),
            }),
        )
    }

    fn label(ts: Millis, place: Place) -> MinuteLabel {
        MinuteLabel {
            ts,
            place,
            device: Some("pc".into()),
            activity: (place != Place::Away).then_some(Activity::Work),
            project: None,
            background: None,
            background_device: None,
            rule: String::new(),
            decider: Decider::Rule,
            complete: true,
            location: None,
            what: None,
            background_what: None,
        }
    }

    #[test]
    fn minutes_per_source_with_away_from_keyboard_split() {
        let m = MINUTE_MS;
        let mut events: Vec<Envelope> = (0..130).map(|i| heard("spotify", i * m, 60)).collect();
        events.extend((0..40).map(|i| heard("youtube.com", (200 + i) * m, 60)));
        // Half a minute heard, and a minute outside the window.
        events.push(heard("youtube.com", 300 * m, 30));
        events.push(heard("youtube.com", 2000 * m, 60));
        let labels: Vec<MinuteLabel> = (0..130)
            .map(|i| {
                label(
                    i * m,
                    if i >= 100 {
                        Place::Away
                    } else {
                        Place::Computer
                    },
                )
            })
            .collect();
        let t = Tally::from_events(&events, &labels, 0, 1440 * m, 3_600_000);
        assert_eq!(t.sources["spotify"].minutes(), 130);
        assert_eq!(t.sources["spotify"].away_minutes(), 30);
        assert_eq!(t.sources["youtube.com"].minutes(), 41);
        assert_eq!(t.sentence(5), "Spotify 2 h 10 min, YouTube 41 min");
        assert_eq!(t.slots["spotify"][0], 3600);
        assert_eq!(t.slots["spotify"][2], 10 * 60);
        let j = t.json(3);
        assert_eq!(
            j["sources"][0]["top"][0]["title"],
            "Green Day – Boulevard of Broken Dreams"
        );
    }

    #[test]
    fn now_names_what_is_audible_and_only_if_recent() {
        let m = MINUTE_MS;
        let now = 100 * m + 5_000;
        let events = vec![
            heard("spotify", 98 * m, 60),
            heard("netflix.com", 90 * m, 60),
        ];
        let (rows, text) = now_json_text(&events, now);
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(
            text,
            " Audible now: Spotify, Green Day – Boulevard of Broken Dreams (pc)."
        );
        let (rows, text) = now_json_text(&events, 200 * m);
        assert!(rows.as_array().unwrap().is_empty());
        assert_eq!(text, " Nothing is audible on the computers.");
    }

    #[test]
    fn sources_read_like_names() {
        assert_eq!(name("spotify"), "Spotify");
        assert_eq!(name("youtube.com"), "YouTube");
        assert_eq!(name("web (zen)"), "Zen (other sites)");
        assert_eq!(name("www.dn.se"), "dn.se");
        assert_eq!(name("mpv"), "mpv");
    }
}
