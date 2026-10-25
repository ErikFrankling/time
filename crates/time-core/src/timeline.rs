//! One timeline across devices: what each minute was.
//!
//! Inputs are stored events; output is one [`MinuteLabel`] per minute.
//! Pure and deterministic: the same events and rules always give the same
//! labels, so history is recomputed whenever rules change instead of
//! carrying stale judgements forward (which is what v1's dashboard did for
//! two months).
//!
//! Which device had Erik's attention, in order:
//! 1. a computer with typing,
//! 2. the phone in the foreground,
//! 3. a computer with pointer movement or very recent input,
//! 4. a computer playing video or audio nobody has touched for less than
//!    [`PRESENT_IDLE_SECS`] (watching),
//! 5. otherwise away.
//!
//! A phone app only counts while the phone is unlocked with its screen on.
//! A computer nobody has touched for longer than [`PRESENT_IDLE_SECS`] is
//! nobody there, whatever it is playing: Netflix left running for four
//! hours while Erik was out at dinner is not four hours of video.
//!
//! An away minute says why when the data does: `presence.moving` when the
//! phone counted steps (out, walking), `presence.out` when the phone was on
//! a different network from the computers (see [`locate`]), otherwise
//! `presence.away` (nobody at any device) or `presence.no-data` (nothing
//! reported: untracked). Away is never an activity.
//!
//! What another device was doing in the same minute becomes the minute's
//! `background` and `background_device`: video or music playing elsewhere
//! first, otherwise another device in use. That is Erik's parallel time,
//! like YouTube on the phone while typing on the pc.

use std::collections::{BTreeMap, HashMap};

use crate::clock::{Millis, MINUTE_MS};
use crate::event::{Envelope, Event, Origin, ScreenMinute};
use crate::label::{Activity, Decider, MinuteLabel, Place};
use crate::rules::{Facts, RuleSet, Source};

/// Bump whenever a change here can change a label. Stored labels carry it,
/// and the server relabels all history when it differs, exactly as it does
/// when the rules file changes.
pub const LOGIC_VERSION: u32 = 5;

/// Input this recent at the end of a minute means someone was there.
const RECENT_INPUT_SECS: u32 = 90;
/// No keyboard or mouse input for this long and nobody is at that computer,
/// even with a video playing or a window fullscreen. From Erik's data
/// (2026-10-21..25): of 72 stretches without input while something played
/// that he ended by touching the machine, 65 ended within 15 minutes and 70
/// within 30; the only longer ones were 4-5 hours with him out of the house.
pub const PRESENT_IDLE_SECS: u32 = 15 * 60;
/// Steps per minute, averaged over a Health Connect bucket, that mean
/// moving. Erik's hours split cleanly: under 300 steps an hour at home,
/// over 1000 when out; this is 480 an hour.
pub const MOVING_STEPS_PER_MIN: f64 = 8.0;
/// A phone app must hold the foreground this long to own a minute.
const PHONE_MIN_OVERLAP_MS: Millis = 30_000;
/// A locked phone reports its state only every few minutes; its last
/// network stands for this long.
pub const PHONE_STATE_HOLD_MS: Millis = 15 * MINUTE_MS;

#[derive(Debug, Clone)]
pub struct DeviceSpec {
    pub name: String,
    pub source: Source,
}

#[derive(Debug, Clone)]
struct Observation<'e> {
    device: &'e str,
    source: Source,
    typing: u32,
    active: bool,
    passive: bool,
    activity: Activity,
    rule: String,
    project: Option<String>,
    class: &'e str,
}

/// Label every minute in `[from, to)`.
///
/// `covered` is how far each device is known to have reported; a minute is
/// `complete` only when every configured device has reported past it.
pub fn label_minutes(
    rules: &RuleSet,
    devices: &[DeviceSpec],
    events: &[Envelope],
    covered: &HashMap<String, Millis>,
    from: Millis,
    to: Millis,
) -> Vec<MinuteLabel> {
    let source_of = |d: &str| devices.iter().find(|s| s.name == d).map(|s| s.source);

    let mut screens: HashMap<(&str, Millis), &ScreenMinute> = HashMap::new();
    let mut spans: Vec<&Envelope> = Vec::new();
    let mut focus: Vec<&Envelope> = Vec::new();
    let mut prompts: BTreeMap<Millis, &str> = BTreeMap::new();
    let mut moving: Vec<(Millis, Millis)> = Vec::new();
    // Per phone and minute: was it ever unlocked with the screen on?
    let mut phone_usable: HashMap<(&str, Millis), bool> = HashMap::new();
    for e in events {
        match &e.event {
            Event::ScreenMinute(m) => {
                screens.insert(
                    (
                        e.device.as_str(),
                        e.start_ms - e.start_ms.rem_euclid(MINUTE_MS),
                    ),
                    m,
                );
            }
            Event::PhoneApp(_) => spans.push(e),
            Event::PhoneState(s) => {
                let usable = !s.locked && s.interactive;
                *phone_usable
                    .entry((
                        e.device.as_str(),
                        e.start_ms - e.start_ms.rem_euclid(MINUTE_MS),
                    ))
                    .or_insert(false) |= usable;
            }
            Event::HealthSteps(h) if e.end_ms > e.start_ms => {
                let per_min = h.steps as f64 * MINUTE_MS as f64 / (e.end_ms - e.start_ms) as f64;
                if per_min >= MOVING_STEPS_PER_MIN {
                    moving.push((e.start_ms, e.end_ms));
                }
            }
            Event::FocusSpan(f) if f.focused && f.project.is_some() => focus.push(e),
            Event::PromptSent(p) if p.origin == Origin::Human => {
                if let Some(project) = &p.project {
                    prompts.insert(e.start_ms, project);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    let mut ts = from - from.rem_euclid(MINUTE_MS);
    while ts < to {
        let end = ts + MINUTE_MS;
        let mut obs: Vec<Observation> = Vec::new();

        for ((device, _), m) in screens.iter().filter(|((_, t), _)| *t == ts) {
            let source = source_of(device).unwrap_or(Source::Desktop);
            obs.push(observe_screen(rules, device, source, m));
        }
        if let Some((env, _)) = spans
            .iter()
            // A locked phone or a dark screen is in a pocket, whatever
            // app it last showed.
            .filter(|e| phone_usable.get(&(e.device.as_str(), ts)) != Some(&false))
            .map(|e| (*e, e.end_ms.min(end) - e.start_ms.max(ts)))
            .filter(|(_, o)| *o >= PHONE_MIN_OVERLAP_MS)
            .max_by_key(|(_, o)| *o)
        {
            if let Event::PhoneApp(app) = &env.event {
                let facts = Facts {
                    source: Some(Source::Phone),
                    package: Some(&app.package),
                    title: &app.label,
                    ..Default::default()
                };
                let (activity, rule, project) = decide(rules, &facts);
                obs.push(Observation {
                    device: &env.device,
                    source: Source::Phone,
                    typing: 0,
                    active: true,
                    passive: app.media_playing,
                    activity,
                    rule,
                    project,
                    class: "",
                });
            }
        }
        // Stable order so ties always break the same way.
        obs.sort_by(|a, b| a.device.cmp(b.device));

        let primary = pick(&obs);
        let complete = devices
            .iter()
            .all(|d| covered.get(&d.name).is_some_and(|c| *c >= end));

        let label = match primary {
            None => MinuteLabel {
                ts,
                place: Place::Away,
                device: None,
                activity: None,
                project: None,
                background: None,
                background_device: None,
                rule: if moving.iter().any(|(a, b)| *a < end && *b > ts) {
                    "presence.moving".into()
                } else if obs.is_empty() {
                    "presence.no-data".into()
                } else {
                    "presence.away".into()
                },
                decider: Decider::Presence,
                complete,
                location: None,
            },
            Some(p) => {
                let mut project = p.project.clone();
                if project.is_none() && p.source == Source::Desktop && p.activity.is_productive() {
                    project = focus
                        .iter()
                        .filter(|f| f.start_ms < end && f.end_ms > ts && f.device == p.device)
                        .find_map(|f| match &f.event {
                            Event::FocusSpan(s) => s.project.clone(),
                            _ => None,
                        })
                        .or_else(|| {
                            // The latest human prompt in the last 10 minutes.
                            prompts
                                .range(ts - 10 * MINUTE_MS..end)
                                .next_back()
                                .map(|(_, p)| (*p).to_owned())
                        });
                }
                // Parallel time: something playing elsewhere first, otherwise
                // another device in use (the phone in hand while the pc types).
                let others = || obs.iter().filter(|o| o.device != p.device);
                let parallel = others()
                    .find(|o| o.passive && matches!(o.activity, Activity::Video | Activity::Music))
                    .or_else(|| {
                        others()
                            .filter(|o| {
                                o.active && (o.source == Source::Phone || !o.class.is_empty())
                            })
                            .max_by_key(|o| o.typing)
                    });
                let background = parallel.map(|o| o.activity);
                let background_device = parallel.map(|o| o.device.to_owned());
                MinuteLabel {
                    ts,
                    place: if p.source == Source::Phone {
                        Place::Phone
                    } else {
                        Place::Computer
                    },
                    device: Some(p.device.to_owned()),
                    activity: Some(p.activity),
                    project,
                    background,
                    background_device,
                    rule: p.rule.clone(),
                    decider: Decider::Rule,
                    complete,
                    location: None,
                }
            }
        };
        out.push(label);
        ts = end;
    }
    out
}

fn observe_screen<'e>(
    rules: &RuleSet,
    device: &'e str,
    source: Source,
    m: &'e ScreenMinute,
) -> Observation<'e> {
    let touched = m.keys > 0 || m.mouse > 0;
    let recent = m.idle_seconds.is_some_and(|s| s < RECENT_INPUT_SECS);
    // A locked screen or a closed lid is nobody at that machine, whatever
    // the input counters and players say; so is one nobody has touched for
    // a quarter of an hour. Agents that do not report idle time are trusted.
    let away = m.locked == Some(true)
        || m.lid_closed == Some(true)
        || (!touched && m.idle_seconds.is_some_and(|s| s >= PRESENT_IDLE_SECS));
    let active = !away && (touched || recent);
    let passive = !away && (m.audio_playing || m.fullscreen);
    let (activity, rule, project) = if m.blocked {
        (Activity::Other, "blocked".to_owned(), None)
    } else if m.window_class.is_empty() {
        (Activity::Other, "no-window".to_owned(), None)
    } else {
        let facts = Facts {
            source: Some(source),
            class: &m.window_class,
            title: &m.window_title,
            host: m.browser_host.as_deref(),
            package: None,
        };
        decide(rules, &facts)
    };
    Observation {
        device,
        source,
        typing: m.keys,
        active,
        passive,
        activity,
        rule,
        project,
        class: &m.window_class,
    }
}

fn decide(rules: &RuleSet, f: &Facts) -> (Activity, String, Option<String>) {
    match rules.classify(f) {
        Some(m) => {
            let project = m
                .project
                .map(str::to_owned)
                .or_else(|| rules.project_for_title(f.title).map(str::to_owned));
            (m.activity, m.rule.to_owned(), project)
        }
        None => (Activity::Unknown, "unmatched".to_owned(), None),
    }
}

fn pick<'a, 'e>(obs: &'a [Observation<'e>]) -> Option<&'a Observation<'e>> {
    let desktops = || obs.iter().filter(|o| o.source == Source::Desktop);
    desktops()
        .filter(|o| o.typing > 0)
        .max_by_key(|o| o.typing)
        .or_else(|| obs.iter().find(|o| o.source == Source::Phone && o.active))
        .or_else(|| desktops().find(|o| o.active && !o.class.is_empty()))
        .or_else(|| {
            desktops()
                .find(|o| o.passive && matches!(o.activity, Activity::Video | Activity::Music))
        })
}

/// Set each label's `location` from the network its devices reported: the
/// device in use first, then the phone, then any computer.
///
/// An away minute goes where the phone is, since Erik is where his phone
/// is, not where his idle pc is. When the phone reported a network that is
/// not the computers' place (mobile data, another Wi-Fi), the minute is
/// `presence.out` unless steps already made it `presence.moving`.
pub fn locate(
    places: &[crate::places::PlaceSpec],
    labels: &mut [MinuteLabel],
    events: &[Envelope],
) {
    use crate::event::Network;
    let known = |n: &Network| n.ssid.is_some() || n.gateway.is_some() || n.domain.is_some();
    let mut nets: HashMap<(Millis, &str), &Network> = HashMap::new();
    // The phone's network per minute, even an empty one: on mobile data the
    // phone reports no Wi-Fi, router or domain, and that is the signal.
    let mut phone_nets: BTreeMap<Millis, &Network> = BTreeMap::new();
    for e in events {
        let ts = e.start_ms - e.start_ms.rem_euclid(MINUTE_MS);
        let net = match &e.event {
            Event::ScreenMinute(m) => &m.network,
            Event::PhoneState(s) => {
                phone_nets.insert(ts, &s.network);
                &s.network
            }
            _ => continue,
        };
        if known(net) {
            nets.insert((ts, e.device.as_str()), net);
        }
    }
    let computers: HashMap<(Millis, &str), &Network> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ScreenMinute(m) if known(&m.network) => Some((
                (
                    e.start_ms - e.start_ms.rem_euclid(MINUTE_MS),
                    e.device.as_str(),
                ),
                &m.network,
            )),
            _ => None,
        })
        .collect();
    for l in labels.iter_mut() {
        if l.place == Place::Away {
            let computer_places: Vec<Option<String>> = computers
                .iter()
                .filter(|((t, _), _)| *t == l.ts)
                .map(|(_, n)| crate::places::resolve(places, n))
                .collect();
            // The phone's latest state: this minute's, or one held over from
            // the last few minutes, which only ever says "out" (it does not
            // stretch a place past the phone's last word).
            let phone = phone_nets
                .range(l.ts - PHONE_STATE_HOLD_MS + MINUTE_MS..=l.ts)
                .next_back()
                .and_then(|(t, n)| {
                    let here = crate::places::resolve(places, n);
                    let elsewhere = match &here {
                        None => true,
                        Some(p) => {
                            !computer_places.is_empty()
                                && !computer_places.iter().any(|c| c.as_deref() == Some(p))
                        }
                    };
                    (*t == l.ts || elsewhere).then_some((here, elsewhere))
                });
            match phone {
                Some((here, elsewhere)) => {
                    if elsewhere && l.rule != "presence.moving" {
                        l.rule = "presence.out".into();
                    }
                    l.location = here;
                }
                // No word from the phone: fall back to the computers.
                None => {
                    if let Some(n) = nets
                        .iter()
                        .filter(|((t, _), _)| *t == l.ts)
                        .map(|(_, n)| *n)
                        .find(|n| crate::places::resolve(places, n).is_some())
                    {
                        l.location = crate::places::resolve(places, n);
                    }
                }
            }
            continue;
        }
        let in_use = l.device.as_deref().and_then(|d| nets.get(&(l.ts, d)));
        let any = || {
            nets.iter()
                .filter(|((t, _), _)| *t == l.ts)
                .map(|(_, n)| *n)
                .find(|n| crate::places::resolve(places, n).is_some())
        };
        if let Some(n) = in_use.copied().or_else(any) {
            l.location = crate::places::resolve(places, n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FocusSpan, HealthSteps, Network, PhoneApp, PhoneState};
    use crate::label::AwayKind;

    const RULES: &str = r#"
        [[rule]]
        id = "video.youtube"
        activity = "video"
        host = ["youtube.com"]
        [[rule]]
        id = "video.netflix"
        activity = "video"
        host = ["netflix.com"]
        [[rule]]
        id = "phone.x"
        activity = "social"
        package = ["com.twitter.android"]
        [[rule]]
        id = "work.t3"
        activity = "work"
        class = ["com.t3tools.T3Code"]
    "#;

    const T: Millis = 1_800_000_000_000 - 1_800_000_000_000 % MINUTE_MS;

    fn devices() -> Vec<DeviceSpec> {
        vec![
            DeviceSpec {
                name: "pc".into(),
                source: Source::Desktop,
            },
            DeviceSpec {
                name: "phone".into(),
                source: Source::Phone,
            },
        ]
    }

    fn screen(ts: Millis, m: ScreenMinute) -> Envelope {
        Envelope::new(
            "pc",
            "time-agent",
            ts,
            ts + MINUTE_MS,
            Event::ScreenMinute(m),
        )
    }

    fn youtube() -> ScreenMinute {
        ScreenMinute {
            window_class: "firefox".into(),
            window_title: "Cats - YouTube".into(),
            browser_host: Some("youtube.com".into()),
            audio_playing: true,
            idle_seconds: Some(600),
            ..Default::default()
        }
    }

    fn label(events: &[Envelope]) -> MinuteLabel {
        let rules = RuleSet::parse(RULES).unwrap();
        let covered = HashMap::from([
            ("pc".to_string(), T + MINUTE_MS),
            ("phone".to_string(), T + MINUTE_MS),
        ]);
        label_minutes(&rules, &devices(), events, &covered, T, T + MINUTE_MS).remove(0)
    }

    #[test]
    fn watching_without_touching_is_video_not_away() {
        let l = label(&[screen(T, youtube())]);
        assert_eq!(
            (l.place, l.activity, l.rule.as_str()),
            (Place::Computer, Some(Activity::Video), "video.youtube")
        );
        assert!(l.complete);
    }

    #[test]
    fn phone_in_hand_beats_a_video_playing_and_keeps_it_as_background() {
        let phone = Envelope::new(
            "phone",
            "time-android",
            T - 5 * MINUTE_MS,
            T + 2 * MINUTE_MS,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                ..Default::default()
            }),
        );
        let l = label(&[screen(T, youtube()), phone]);
        assert_eq!(
            (l.place, l.activity),
            (Place::Phone, Some(Activity::Social))
        );
        assert_eq!(l.background, Some(Activity::Video));
    }

    #[test]
    fn typing_beats_the_phone() {
        let typing = ScreenMinute {
            window_class: "com.t3tools.T3Code".into(),
            keys: 50,
            ..Default::default()
        };
        let phone = Envelope::new(
            "phone",
            "time-android",
            T,
            T + MINUTE_MS,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                ..Default::default()
            }),
        );
        let l = label(&[screen(T, typing), phone]);
        assert_eq!(
            (l.place, l.activity),
            (Place::Computer, Some(Activity::Work))
        );
        // ...and the phone in hand is the minute's parallel activity.
        assert_eq!(
            (l.background, l.background_device.as_deref()),
            (Some(Activity::Social), Some("phone"))
        );
    }

    #[test]
    fn a_brief_phone_glance_does_not_own_the_minute() {
        let phone = Envelope::new(
            "phone",
            "time-android",
            T + 50_000,
            T + 70_000,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                ..Default::default()
            }),
        );
        let l = label(&[phone]);
        assert_eq!(l.place, Place::Away);
    }

    #[test]
    fn nobody_and_nothing_playing_is_away_and_missing_data_is_incomplete() {
        let empty = ScreenMinute {
            idle_seconds: Some(3600),
            ..Default::default()
        };
        let l = label(&[screen(T, empty)]);
        assert_eq!((l.place, l.rule.as_str()), (Place::Away, "presence.away"));

        let rules = RuleSet::parse(RULES).unwrap();
        let l = label_minutes(&rules, &devices(), &[], &HashMap::new(), T, T + MINUTE_MS).remove(0);
        assert_eq!(
            (l.place, l.rule.as_str(), l.complete),
            (Place::Away, "presence.no-data", false)
        );
    }

    #[test]
    fn unmatched_windows_are_unknown_never_guessed() {
        let m = ScreenMinute {
            window_class: "weird-app".into(),
            mouse: 3,
            ..Default::default()
        };
        let l = label(&[screen(T, m)]);
        assert_eq!(
            (l.activity, l.rule.as_str()),
            (Some(Activity::Unknown), "unmatched")
        );
    }

    #[test]
    fn t3_minutes_take_the_focused_threads_project() {
        let typing = ScreenMinute {
            window_class: "com.t3tools.T3Code".into(),
            keys: 50,
            ..Default::default()
        };
        let focus = Envelope::new(
            "pc",
            "t3code",
            T - MINUTE_MS,
            T + 5 * MINUTE_MS,
            Event::FocusSpan(FocusSpan {
                client: "web".into(),
                thread_id: "t".into(),
                project: Some("husk".into()),
                focused: true,
            }),
        );
        let l = label(&[screen(T, typing), focus]);
        assert_eq!(l.project.as_deref(), Some("husk"));
    }

    /// Netflix fullscreen and playing, last touched 25 minutes ago: the
    /// evening Erik was out at dinner (2026-10-25).
    fn netflix_left_running(idle: u32) -> ScreenMinute {
        ScreenMinute {
            window_class: "zen-beta".into(),
            window_title: "Netflix — Zen Browser".into(),
            browser_host: Some("netflix.com".into()),
            fullscreen: true,
            audio_playing: true,
            idle_seconds: Some(idle),
            network: Network {
                gateway: Some("192.168.50.1".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn steps(per_hour: u32) -> Envelope {
        let hour = T - T.rem_euclid(60 * MINUTE_MS);
        Envelope::new(
            "phone",
            "time-android",
            hour,
            hour + 60 * MINUTE_MS,
            Event::HealthSteps(HealthSteps {
                steps: per_hour,
                distance_m: per_hour as f64 * 0.7,
            }),
        )
    }

    fn phone_state(locked: bool, network: Network) -> Envelope {
        Envelope::new(
            "phone",
            "time-android",
            T + 10_000,
            T + 10_000,
            Event::PhoneState(PhoneState {
                locked,
                interactive: !locked,
                network,
                ..Default::default()
            }),
        )
    }

    #[test]
    fn media_playing_with_no_input_for_a_quarter_hour_is_away_not_video() {
        let l = label(&[screen(T, netflix_left_running(25 * 60))]);
        assert_eq!(
            (l.place, l.activity, l.rule.as_str()),
            (Place::Away, None, "presence.away")
        );
        assert_eq!(l.away_kind(), Some(AwayKind::Idle));
        // ...but ten minutes into an episode without touching is watching.
        let l = label(&[screen(T, netflix_left_running(10 * 60))]);
        assert_eq!(
            (l.place, l.activity),
            (Place::Computer, Some(Activity::Video))
        );
    }

    #[test]
    fn a_video_left_playing_is_not_parallel_time_either() {
        let phone = Envelope::new(
            "phone",
            "time-android",
            T - 5 * MINUTE_MS,
            T + 2 * MINUTE_MS,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                ..Default::default()
            }),
        );
        let l = label(&[screen(T, netflix_left_running(3 * 3600)), phone]);
        assert_eq!(l.place, Place::Phone);
        assert_eq!(l.background, None);
    }

    #[test]
    fn no_input_and_steps_on_the_phone_is_out_walking() {
        let l = label(&[screen(T, netflix_left_running(2 * 3600)), steps(2107)]);
        assert_eq!(
            (l.place, l.activity, l.rule.as_str()),
            (Place::Away, None, "presence.moving")
        );
        assert_eq!(l.away_kind(), Some(AwayKind::Walking));
        // Pottering about at home is not walking.
        let l = label(&[screen(T, netflix_left_running(2 * 3600)), steps(134)]);
        assert_eq!(l.away_kind(), Some(AwayKind::Idle));
        // Steps say nothing about a minute someone is typing in.
        let typing = ScreenMinute {
            window_class: "com.t3tools.T3Code".into(),
            keys: 40,
            ..Default::default()
        };
        let l = label(&[screen(T, typing), steps(2107)]);
        assert_eq!(l.activity, Some(Activity::Work));
    }

    #[test]
    fn a_locked_phone_does_not_own_a_minute_whatever_app_it_last_showed() {
        let app = Envelope::new(
            "phone",
            "time-android",
            T - 5 * MINUTE_MS,
            T + 2 * MINUTE_MS,
            Event::PhoneApp(PhoneApp {
                package: "com.twitter.android".into(),
                ..Default::default()
            }),
        );
        let l = label(&[app.clone(), phone_state(true, Network::default())]);
        assert_eq!(l.place, Place::Away);
        let l = label(&[app, phone_state(false, Network::default())]);
        assert_eq!(l.place, Place::Phone);
    }

    #[test]
    fn away_with_the_phone_on_mobile_data_is_out_and_located_by_the_phone() {
        let events = [
            screen(T, netflix_left_running(2 * 3600)),
            phone_state(true, Network::default()),
        ];
        let mut labels = vec![label(&events)];
        locate(&crate::places::defaults(), &mut labels, &events);
        assert_eq!(
            (labels[0].rule.as_str(), labels[0].location.as_deref()),
            ("presence.out", None)
        );
        assert_eq!(labels[0].away_kind(), Some(AwayKind::Out));

        // Phone on the home Wi-Fi beside the idle pc: just away, at home.
        let home = Network {
            gateway: Some("192.168.50.1".into()),
            ..Default::default()
        };
        let events = [
            screen(T, netflix_left_running(2 * 3600)),
            phone_state(true, home),
        ];
        let mut labels = vec![label(&events)];
        locate(&crate::places::defaults(), &mut labels, &events);
        assert_eq!(
            (labels[0].rule.as_str(), labels[0].location.as_deref()),
            ("presence.away", Some("home"))
        );

        // A locked phone reports every few minutes; its last word holds.
        let earlier = Envelope::new(
            "phone",
            "time-android",
            T - 6 * MINUTE_MS,
            T - 6 * MINUTE_MS,
            Event::PhoneState(PhoneState {
                locked: true,
                ..Default::default()
            }),
        );
        let events = [screen(T, netflix_left_running(2 * 3600)), earlier];
        let mut labels = vec![label(&events)];
        locate(&crate::places::defaults(), &mut labels, &events);
        assert_eq!(labels[0].away_kind(), Some(AwayKind::Out));

        // Walking beats out.
        let events = [
            screen(T, netflix_left_running(2 * 3600)),
            phone_state(true, Network::default()),
            steps(3000),
        ];
        let mut labels = vec![label(&events)];
        locate(&crate::places::defaults(), &mut labels, &events);
        assert_eq!(labels[0].away_kind(), Some(AwayKind::Walking));
    }
}
