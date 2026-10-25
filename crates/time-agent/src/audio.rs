//! What was really audible, per source, from PipeWire, with track names
//! from MPRIS.
//!
//! MPRIS alone says "Playing" for a paused-looking tab, and a browser holds
//! an output stream open (state `running`) for a page that makes no sound.
//! So every few seconds a sampler asks PipeWire (`pw-dump`) which output
//! streams are running, unmuted and linked to an unmuted sink, then
//! measures each one's actual signal with a short passive capture
//! (`pw-record` on the stream itself, which is after the stream's own
//! volume) and scales it by the sink's volume. Only a level above
//! [`SILENCE`] counts. MPRIS (`playerctl`) names the track and, for a
//! browser, sometimes the page.
//!
//! No existing tool does this: scrobblers (rescrobbled, mpris-scrobbler)
//! and aw-watcher-media-player trust MPRIS status, and level meters
//! (pwvucontrol, Quickshell's peak monitor) record nothing.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use time_core::event::Listening;

use crate::browser::Watcher;
use crate::probe::Audio;

/// Peak below this (of full scale, after stream and sink volume) is
/// silence: -60 dBFS.
pub const SILENCE: f64 = 0.001;
/// How often the sampler looks.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// How long each level measurement listens.
const CAPTURE: Duration = Duration::from_millis(400);
/// Seconds of a minute that make the minute "audio playing": two samples,
/// so one notification sound does not.
pub const AUDIBLE_MINUTE_SECS: u32 = 20;

/// What was heard since the previous call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heard {
    pub sources: Vec<Listening>,
}

impl Heard {
    /// Something was audible for a real part of the minute.
    pub fn audible(&self) -> bool {
        self.sources.iter().map(|s| s.audible_seconds).sum::<u32>() >= AUDIBLE_MINUTE_SECS
    }
}

/// An output stream as PipeWire describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub serial: u64,
    /// The program, lowercased binary name: `spotify`, `zen`, `firefox`.
    pub app: String,
    /// `media.name`: a browser's tab title, a player's track, or the app.
    pub media: String,
    pub running: bool,
    /// Product of the stream's own mute and volume.
    pub gain: f64,
    /// The volume of what it plays into: 0 when the sink is muted, None
    /// when it is not linked to anything (so it cannot be heard).
    pub sink_gain: Option<f64>,
}

impl Stream {
    /// Worth measuring: it could be heard if it carried signal.
    pub fn could_be_heard(&self) -> bool {
        self.running && self.gain > 0.0 && self.sink_gain.is_some_and(|g| g > 0.0)
    }
}

fn props_gain(node: &Value) -> f64 {
    let p = &node["info"]["params"]["Props"][0];
    if p["mute"].as_bool() == Some(true) {
        return 0.0;
    }
    let channels = p["channelVolumes"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_f64).fold(0.0, f64::max))
        .unwrap_or(1.0);
    channels * p["volume"].as_f64().unwrap_or(1.0)
}

/// Output streams from `pw-dump` JSON, with the gain of the sink each is
/// linked to.
pub fn parse_streams(dump: &str) -> Vec<Stream> {
    let Ok(Value::Array(objects)) = serde_json::from_str::<Value>(dump) else {
        return vec![];
    };
    let class = |o: &Value| {
        o["info"]["props"]["media.class"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let nodes: BTreeMap<u64, &Value> = objects
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Node")
        .filter_map(|o| Some((o["id"].as_u64()?, o)))
        .collect();
    // stream node id -> the node ids it feeds.
    let mut links: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for l in objects
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Link")
    {
        let (Some(out), Some(inp)) = (
            l["info"]["output-node-id"].as_u64(),
            l["info"]["input-node-id"].as_u64(),
        ) else {
            continue;
        };
        links.entry(out).or_default().push(inp);
    }
    nodes
        .iter()
        .filter(|(_, n)| class(n) == "Stream/Output/Audio")
        .filter_map(|(id, n)| {
            let p = &n["info"]["props"];
            let app = p["application.process.binary"]
                .as_str()
                .or(p["application.name"].as_str())
                .unwrap_or("unknown")
                .trim_start_matches('.')
                .trim_end_matches("-wrapped")
                .to_lowercase();
            if app == "time-agent" || app == "pw-record" {
                return None;
            }
            // Linked to a sink: its volume. Linked to a filter or loopback
            // (an equaliser): assume it passes sound on.
            let sink_gain = links.get(id).and_then(|targets| {
                targets
                    .iter()
                    .filter_map(|t| nodes.get(t))
                    .map(|t| {
                        if class(t) == "Audio/Sink" {
                            props_gain(t)
                        } else {
                            1.0
                        }
                    })
                    .reduce(f64::max)
            });
            Some(Stream {
                serial: p["object.serial"].as_u64().unwrap_or(*id),
                app,
                media: p["media.name"].as_str().unwrap_or_default().to_owned(),
                running: n["info"]["state"] == "running",
                gain: props_gain(n),
                sink_gain,
            })
        })
        .collect()
}

/// One MPRIS player.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Player {
    /// `spotify`, `firefox.instance_1_5827`.
    pub name: String,
    pub status: String,
    pub artist: String,
    pub title: String,
    pub url: String,
}

pub const PLAYERCTL_FORMAT: &str =
    "{{playerName}}\t{{status}}\t{{xesam:artist}}\t{{xesam:title}}\t{{xesam:url}}";

pub fn parse_players(out: &str) -> Vec<Player> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?.trim().to_owned();
            if name.is_empty() || name.contains(' ') {
                return None;
            }
            Some(Player {
                name,
                status: f.next().unwrap_or_default().trim().to_owned(),
                artist: f.next().unwrap_or_default().trim().to_owned(),
                title: f.next().unwrap_or_default().trim().to_owned(),
                url: f.next().unwrap_or_default().trim().to_owned(),
            })
        })
        .collect()
}

/// Programs that publish MPRIS under another name: Zen and LibreWolf as
/// `firefox.instance_…`, every Chromium as `chromium.instance…`.
fn family(name: &str) -> &str {
    let base = name.split('.').next().unwrap_or(name);
    match base {
        "zen" | "zen-beta" | "librewolf" | "floorp" | "firefox" => "firefox",
        "chromium" | "chrome" | "google-chrome" | "brave" | "brave-browser" | "helium" => {
            "chromium"
        }
        other => other,
    }
}

fn is_browser(app: &str) -> bool {
    matches!(family(app), "firefox" | "chromium")
}

/// The MPRIS player that is this stream: same program, and the title
/// matching when the program has several.
pub fn match_player<'a>(stream: &Stream, players: &'a [Player]) -> Option<&'a Player> {
    let same: Vec<&Player> = players
        .iter()
        .filter(|p| family(&p.name) == family(&stream.app))
        .collect();
    let by_title = same.iter().copied().find(|p| {
        !p.title.is_empty()
            && !stream.media.is_empty()
            && (stream.media.contains(&p.title) || p.title.contains(&stream.media))
    });
    match (by_title, same.as_slice()) {
        (Some(p), _) => Some(p),
        (None, [only]) if !is_browser(&stream.app) => Some(only),
        _ => None,
    }
}

/// A site from a page title, for when no tab report or URL says.
pub fn site_from_title(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    let known: [(&str, &str); 9] = [
        ("youtube", "youtube.com"),
        ("netflix", "netflix.com"),
        ("twitch", "twitch.tv"),
        ("hbo max", "hbomax.com"),
        ("svt play", "svtplay.se"),
        ("disney+", "disneyplus.com"),
        ("prime video", "primevideo.com"),
        ("soundcloud", "soundcloud.com"),
        (" / x", "x.com"),
    ];
    known.iter().find(|(k, _)| t.contains(k)).map(|(_, s)| *s)
}

/// What a stream is, for totals: the site for a browser, else the program.
pub fn source_of(stream: &Stream, player: Option<&Player>, tab_host: Option<String>) -> String {
    if !is_browser(&stream.app) {
        return stream.app.clone();
    }
    tab_host
        .or_else(|| {
            player
                .and_then(|p| crate::browser::host_of(&p.url))
                .filter(|h| !h.is_empty())
        })
        .or_else(|| site_from_title(&stream.media).map(str::to_owned))
        .or_else(|| player.and_then(|p| site_from_title(&p.title).map(str::to_owned)))
        .unwrap_or_else(|| format!("web ({})", stream.app))
}

/// A stream name that says nothing about what is playing.
pub fn is_generic(media: &str, app: &str) -> bool {
    let m = media.trim().to_lowercase();
    m.is_empty()
        || m == app
        || [
            "audiostream",
            "audio stream",
            "playback",
            "playback stream",
            "output",
            "spotify",
            "chromium",
            "firefox",
        ]
        .contains(&m.as_str())
}

/// Largest absolute sample of signed 16-bit little-endian audio, 0..1.
pub fn peak_s16(bytes: &[u8]) -> f64 {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
        .max()
        .map_or(0.0, |m| f64::from(m) / 32768.0)
}

/// Measure one stream's level by listening to it briefly (passively, so it
/// never keeps anything awake).
fn measure(serial: u64) -> Result<f64, String> {
    let mut child = Command::new("pw-record")
        .args([
            "--target",
            &serial.to_string(),
            "--format",
            "s16",
            "--rate",
            "48000",
            "--channels",
            "2",
            "-P",
            "{ node.passive=true application.name=time-agent media.name=level }",
            "--raw",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("pw-record: {e}"))?;
    let mut out = child.stdout.take().ok_or("pw-record: no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    std::thread::sleep(CAPTURE);
    let _ = child.kill();
    let _ = child.wait();
    let buf = reader.join().unwrap_or_default();
    Ok(peak_s16(&buf))
}

/// (app, source, title, artist): what one source is.
pub type Key = (String, String, String, String);

#[derive(Default)]
struct Acc {
    /// Key -> (seconds, peak, mpris status)
    heard: BTreeMap<Key, (u32, f64, Option<String>)>,
    error: Option<String>,
}

/// The production sampler: a thread that looks every [`SAMPLE_EVERY`].
pub struct PipeWire {
    acc: Arc<Mutex<Acc>>,
}

impl PipeWire {
    /// `tabs` names the site of a browser stream from the tab reports.
    pub fn start(tabs: Option<Watcher>) -> Self {
        let acc = Arc::new(Mutex::new(Acc::default()));
        let shared = acc.clone();
        std::thread::spawn(move || loop {
            let started = Instant::now();
            let result = sample(tabs.as_ref());
            if let Ok(mut a) = shared.lock() {
                match result {
                    Ok(found) => {
                        a.error = None;
                        let secs = SAMPLE_EVERY.as_secs() as u32;
                        for (key, peak, status) in found {
                            let e = a.heard.entry(key).or_insert((0, 0.0, None));
                            e.0 = (e.0 + secs).min(60);
                            e.1 = e.1.max(peak);
                            e.2 = status.or(e.2.take());
                        }
                    }
                    Err(e) => a.error = Some(e),
                }
            }
            std::thread::sleep(SAMPLE_EVERY.saturating_sub(started.elapsed()));
        });
        Self { acc }
    }
}

type Found = (Key, f64, Option<String>);

/// One look: the sources audible right now.
pub fn sample(tabs: Option<&Watcher>) -> Result<Vec<Found>, String> {
    let dump = Command::new("pw-dump")
        .output()
        .map_err(|e| format!("pw-dump: {e}"))?;
    let streams = parse_streams(&String::from_utf8_lossy(&dump.stdout));
    let candidates: Vec<&Stream> = streams.iter().filter(|s| s.could_be_heard()).collect();
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    let players = Command::new("playerctl")
        .args(["--all-players", "metadata", "--format", PLAYERCTL_FORMAT])
        .output()
        .map(|o| parse_players(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let mut found = Vec::new();
    for s in candidates {
        let peak = measure(s.serial)? * s.sink_gain.unwrap_or(0.0);
        if peak < SILENCE {
            continue;
        }
        let player = match_player(s, &players);
        let tab_host = tabs.and_then(|t| {
            t.host_for_title(&s.media)
                .or_else(|| player.and_then(|p| t.host_for_title(&p.title)))
        });
        let source = source_of(s, player, tab_host);
        let (title, artist) = match player {
            Some(p) if !p.title.is_empty() => (p.title.clone(), p.artist.clone()),
            _ if is_generic(&s.media, &s.app) => (String::new(), String::new()),
            _ => (s.media.clone(), String::new()),
        };
        found.push((
            (s.app.clone(), source, title, artist),
            peak,
            player.map(|p| p.status.clone()),
        ));
    }
    Ok(found)
}

impl Audio for PipeWire {
    fn problem(&self) -> Option<String> {
        self.acc.lock().ok()?.error.clone()
    }

    fn take(&mut self) -> Heard {
        let Ok(mut a) = self.acc.lock() else {
            return Heard::default();
        };
        let sources = std::mem::take(&mut a.heard)
            .into_iter()
            .map(
                |((app, source, title, artist), (secs, peak, status))| Listening {
                    app,
                    source,
                    title,
                    artist,
                    audible_seconds: secs,
                    peak: (peak * 1000.0).round() / 1000.0,
                    mpris_status: status,
                },
            )
            .collect();
        Heard { sources }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `pw-dump` on the pc: Spotify playing into the
    /// FiiO, a Zen tab (Netflix) paused, a Firefox tab holding a stream
    /// open, mpv muted, and a stream into a muted sink.
    const DUMP: &str = r#"[
      {"id":72,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Audio/Sink","object.serial":72},
        "params":{"Props":[{"mute":false,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":80,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Audio/Sink","object.serial":80},
        "params":{"Props":[{"mute":true,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":90,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Stream/Output/Audio","object.serial":7001,
          "application.name":"spotify","application.process.binary":"spotify","media.name":"Spotify"},
        "params":{"Props":[{"mute":false,"channelVolumes":[0.75,0.75],"volume":1.0}]}}},
      {"id":83,"type":"PipeWire:Interface:Node","info":{"state":"idle",
        "props":{"media.class":"Stream/Output/Audio","object.serial":7326,
          "application.name":"Zen","application.process.binary":"zen","media.name":"Netflix"},
        "params":{"Props":[{"mute":false,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":99,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Stream/Output/Audio","object.serial":7263,
          "application.name":"Firefox","application.process.binary":"firefox","media.name":"(1) Home / X"},
        "params":{"Props":[{"mute":false,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":100,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Stream/Output/Audio","object.serial":7400,
          "application.name":"mpv","application.process.binary":"mpv","media.name":"film.mkv"},
        "params":{"Props":[{"mute":true,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":101,"type":"PipeWire:Interface:Node","info":{"state":"running",
        "props":{"media.class":"Stream/Output/Audio","object.serial":7500,
          "application.name":"Chromium","application.process.binary":"chromium","media.name":"Lecture - YouTube"},
        "params":{"Props":[{"mute":false,"channelVolumes":[1.0,1.0],"volume":1.0}]}}},
      {"id":110,"type":"PipeWire:Interface:Link","info":{"output-node-id":90,"input-node-id":72}},
      {"id":111,"type":"PipeWire:Interface:Link","info":{"output-node-id":83,"input-node-id":72}},
      {"id":112,"type":"PipeWire:Interface:Link","info":{"output-node-id":99,"input-node-id":72}},
      {"id":113,"type":"PipeWire:Interface:Link","info":{"output-node-id":100,"input-node-id":72}},
      {"id":114,"type":"PipeWire:Interface:Link","info":{"output-node-id":101,"input-node-id":80}}
    ]"#;

    fn stream<'a>(all: &'a [Stream], app: &str) -> &'a Stream {
        all.iter().find(|s| s.app == app).unwrap()
    }

    #[test]
    fn only_running_unmuted_streams_into_unmuted_sinks_are_measured() {
        let all = parse_streams(DUMP);
        assert_eq!(all.len(), 5);
        assert!(stream(&all, "spotify").could_be_heard());
        assert_eq!(stream(&all, "spotify").gain, 0.75);
        assert!(!stream(&all, "zen").could_be_heard(), "paused tab: idle");
        assert!(
            stream(&all, "firefox").could_be_heard(),
            "open stream: measured"
        );
        assert!(!stream(&all, "mpv").could_be_heard(), "stream muted");
        assert!(!stream(&all, "chromium").could_be_heard(), "sink muted");
    }

    #[test]
    fn generic_stream_names_are_not_titles() {
        assert!(is_generic("AudioStream", "firefox"));
        assert!(is_generic("Spotify", "spotify"));
        assert!(!is_generic("Lecture - YouTube", "chromium"));
    }

    #[test]
    fn peak_reads_signed_samples() {
        assert_eq!(peak_s16(&[0, 0, 0, 0]), 0.0);
        let half = (-16384i16).to_le_bytes();
        assert_eq!(peak_s16(&[0, 0, half[0], half[1]]), 0.5);
        assert!(
            peak_s16(&3i16.to_le_bytes()) < SILENCE,
            "volume 0.1: silence"
        );
    }

    #[test]
    fn players_are_matched_to_streams() {
        let players = parse_players(
            "firefox.instance_1_5827\tPaused\t\tNetflix\t\n\
             spotify\tPlaying\tGreen Day\tBoulevard of Broken Dreams\thttps://open.spotify.com/track/x\n",
        );
        let all = parse_streams(DUMP);
        let spotify = match_player(stream(&all, "spotify"), &players).unwrap();
        assert_eq!(spotify.artist, "Green Day");
        let zen = match_player(stream(&all, "zen"), &players).unwrap();
        assert_eq!(zen.status, "Paused");
        // A browser with no player of that title gets none, not a guess.
        assert!(match_player(stream(&all, "firefox"), &players).is_none());
    }

    #[test]
    fn browser_streams_are_named_by_site() {
        let all = parse_streams(DUMP);
        assert_eq!(source_of(stream(&all, "spotify"), None, None), "spotify");
        assert_eq!(source_of(stream(&all, "zen"), None, None), "netflix.com");
        assert_eq!(
            source_of(stream(&all, "chromium"), None, None),
            "youtube.com"
        );
        assert_eq!(
            source_of(stream(&all, "firefox"), None, Some("x.com".into())),
            "x.com"
        );
        let mut odd = stream(&all, "firefox").clone();
        odd.media = "Something".into();
        assert_eq!(source_of(&odd, None, None), "web (firefox)");
    }

    #[test]
    fn a_minute_is_audible_from_two_samples() {
        let one = Listening {
            audible_seconds: 10,
            ..Default::default()
        };
        assert!(!Heard {
            sources: vec![one.clone()]
        }
        .audible());
        assert!(Heard {
            sources: vec![one.clone(), one]
        }
        .audible());
    }
}
