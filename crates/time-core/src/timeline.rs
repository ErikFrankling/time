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
//! 4. a computer playing video or audio nobody is touching (watching),
//! 5. otherwise away.
//!
//! Something playing on a device that is not the one in use becomes the
//! minute's `background`.

use std::collections::{BTreeMap, HashMap};

use crate::clock::{Millis, MINUTE_MS};
use crate::event::{Envelope, Event, Origin, ScreenMinute};
use crate::label::{Activity, Decider, MinuteLabel, Place};
use crate::rules::{Facts, RuleSet, Source};

/// Input this recent at the end of a minute means someone was there.
const RECENT_INPUT_SECS: u32 = 90;
/// A phone app must hold the foreground this long to own a minute.
const PHONE_MIN_OVERLAP_MS: Millis = 30_000;

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
                rule: if obs.is_empty() {
                    "presence.no-data".into()
                } else {
                    "presence.away".into()
                },
                decider: Decider::Presence,
                complete,
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
                let background = obs
                    .iter()
                    .filter(|o| o.device != p.device && o.passive)
                    .map(|o| o.activity)
                    .find(|a| matches!(a, Activity::Video | Activity::Music));
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
                    rule: p.rule.clone(),
                    decider: Decider::Rule,
                    complete,
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
    let recent = m.idle_seconds.is_some_and(|s| s < RECENT_INPUT_SECS);
    let active = m.keys > 0 || m.mouse > 0 || recent;
    let passive = m.audio_playing || m.fullscreen;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FocusSpan, PhoneApp};

    const RULES: &str = r#"
        [[rule]]
        id = "video.youtube"
        activity = "video"
        host = ["youtube.com"]
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
}
