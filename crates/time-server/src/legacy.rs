//! v1's ingest endpoints, so nothing is lost while devices move to v2.
//!
//! When the live route switches to v2, the phone still runs the v1 app (it
//! updates on Erik's tap) and the desktops may still run the v1 agent. They
//! post v1 frames to `/v1/frame` and `/v1/frames`. This turns each frame into
//! the v2 event it describes, with the same stable ids the v1 importer uses,
//! so a frame seen by both paths is stored once. Retire once no v1 client
//! remains (the status screen shows `app.version` per device).

use serde::{Deserialize, Serialize};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, Heartbeat, PhoneApp, ScreenMinute};

use crate::import_v1::SERVICE;

#[derive(Debug, Deserialize)]
pub struct Frame {
    pub ts: i64,
    pub device: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub idle_secs: Option<u32>,
    #[serde(default)]
    pub keys: u32,
    #[serde(default)]
    pub mouse: u32,
}

/// What v1 clients expect back per frame.
#[derive(Debug, Serialize)]
pub struct Ack {
    pub ts: i64,
    pub category: &'static str,
    pub project: Option<String>,
    pub detail: Option<String>,
    pub classified: bool,
    pub pending: bool,
}

/// v1 phones put `Label — package` in `window`; desktops `class — title`.
fn split(window: &str) -> (&str, &str) {
    if window == "unknown" {
        return ("", "");
    }
    window.split_once(" — ").unwrap_or((window, ""))
}

/// The events one v1 frame stands for, and a heartbeat for its device, so a
/// v1 client counts as connected on the status screen.
pub fn convert(frame: &Frame, phones: &[String], now: Millis) -> Vec<Envelope> {
    let ts: Millis = frame.ts * 1000;
    let mut out = Vec::new();
    if phones.contains(&frame.device) {
        if !frame.window.is_empty() {
            let (label, package) = split(&frame.window);
            out.push(Envelope::stable(
                &frame.device,
                SERVICE,
                ts,
                ts + MINUTE_MS,
                Event::PhoneApp(PhoneApp {
                    package: package.to_owned(),
                    label: label.to_owned(),
                    media_playing: false,
                }),
            ));
        }
    } else {
        let (class, title) = if frame.blocked {
            ("", "")
        } else {
            split(&frame.window)
        };
        out.push(Envelope::stable(
            &frame.device,
            SERVICE,
            ts,
            ts + MINUTE_MS,
            Event::ScreenMinute(ScreenMinute {
                window_class: class.to_owned(),
                window_title: title.to_owned(),
                keys: frame.keys,
                mouse: frame.mouse,
                idle_seconds: frame.idle_secs,
                blocked: frame.blocked,
                ..Default::default()
            }),
        ));
    }
    out.push(Envelope::new(
        &frame.device,
        "time-v1-client",
        now,
        now,
        Event::Heartbeat(Heartbeat {
            spool_pending: 0,
            app_version: "v1".into(),
            covered_until_ms: Some(ts + MINUTE_MS),
            missing: vec!["running the v1 client: update to get v2's signals".into()],
        }),
    ));
    out
}

pub fn ack(frame: &Frame) -> Ack {
    Ack {
        ts: frame.ts,
        category: "other",
        project: None,
        detail: None,
        classified: false,
        pending: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_frames_become_v2_events_with_the_importers_ids() {
        let phones = vec!["phone".to_string()];
        let phone = Frame {
            ts: 60,
            device: "phone".into(),
            window: "X — com.twitter.android".into(),
            blocked: false,
            idle_secs: None,
            keys: 0,
            mouse: 0,
        };
        let events = convert(&phone, &phones, 1000);
        assert!(
            matches!(&events[0].event, Event::PhoneApp(a) if a.package == "com.twitter.android" && a.label == "X")
        );
        assert!(matches!(&events[1].event, Event::Heartbeat(h) if h.app_version == "v1"));
        // Same id as import_v1 gives the same minute, so both paths dedupe.
        let again = convert(&phone, &phones, 2000);
        assert_eq!(events[0].id, again[0].id);

        let pc = Frame {
            ts: 120,
            device: "pc".into(),
            window: "kitty — ~/x".into(),
            blocked: false,
            idle_secs: Some(3),
            keys: 9,
            mouse: 1,
        };
        let events = convert(&pc, &phones, 1000);
        assert!(
            matches!(&events[0].event, Event::ScreenMinute(m) if m.window_class == "kitty" && m.keys == 9)
        );
    }
}
