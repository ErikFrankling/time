//! End-to-end: the real server and agent, a scripted morning, a fake world.

use std::path::Path;

use time_core::clock::MINUTE_MS;
use time_core::event::{Event, ScreenMinute};
use time_sim::world::World;

fn morning() -> World {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/morning.toml");
    let mut w = World::load(&path).expect("scenario loads");
    w.run().expect("scenario runs");
    w
}

fn screen_minutes(w: &World) -> Vec<(i64, ScreenMinute)> {
    w.events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            Event::ScreenMinute(m) if e.device == "pc" => Some((e.start_ms, m)),
            _ => None,
        })
        .collect()
}

#[test]
fn every_desktop_minute_arrives_exactly_once_despite_an_outage() {
    let w = morning();
    let minutes = screen_minutes(&w);
    let start = w.scenario.start_ms;
    let expected: Vec<i64> = (0..120).map(|i| start + i * MINUTE_MS).collect();
    let got: Vec<i64> = minutes.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        got, expected,
        "120 minutes, in order, none missing, none doubled"
    );
}

#[test]
fn browser_host_comes_from_the_focused_browsers_tab() {
    let w = morning();
    let at = |hhmm: &str| w.scenario.at(hhmm).unwrap();
    let minutes = screen_minutes(&w);
    let host_at = |t: i64| {
        minutes
            .iter()
            .find(|(m, _)| *m == t)
            .unwrap()
            .1
            .browser_host
            .clone()
    };
    assert_eq!(host_at(at("09:05")).as_deref(), Some("youtube.com"));
    // v1 recorded play.hbomax.com here, from the other browser.
    assert_eq!(host_at(at("10:15")).as_deref(), Some("dn.se"));
    assert_eq!(host_at(at("09:50")), None, "T3 Code is not a browser");
}

#[test]
fn blocked_windows_carry_nothing_but_the_flag() {
    let w = morning();
    let t = w.scenario.at("10:10").unwrap();
    let (_, m) = screen_minutes(&w)
        .into_iter()
        .find(|(m, _)| *m == t)
        .unwrap();
    assert!(m.blocked);
    assert!(m.window_class.is_empty() && m.window_title.is_empty() && m.browser_host.is_none());
}

#[test]
fn an_empty_desk_is_recorded_as_no_window_and_growing_idle() {
    let w = morning();
    let t = w.scenario.at("10:45").unwrap();
    let (_, m) = screen_minutes(&w)
        .into_iter()
        .find(|(m, _)| *m == t)
        .unwrap();
    assert!(m.window_class.is_empty());
    assert_eq!((m.keys, m.mouse), (0, 0));
    assert!(
        m.idle_seconds.unwrap() >= 15 * 60,
        "idle {:?}",
        m.idle_seconds
    );
}

#[test]
fn phone_spans_and_steps_arrive_late_but_once() {
    let w = morning();
    let events = w.events().unwrap();
    let apps: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::PhoneApp(a) => Some((e.start_ms, e.end_ms, a.package.clone())),
            _ => None,
        })
        .collect();
    let at = |hhmm: &str| w.scenario.at(hhmm).unwrap();
    assert_eq!(
        apps,
        vec![
            (at("09:40"), at("09:50"), "com.twitter.android".to_string()),
            // Used while offline, delivered after: present, and not doubled by re-reads.
            (
                at("10:05"),
                at("10:15"),
                "com.google.android.youtube".to_string()
            ),
        ]
    );
    let steps: u32 = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::HealthSteps(s) => Some(s.steps),
            _ => None,
        })
        .sum();
    assert_eq!(steps, 3000);
    // Re-reads reached the server and were absorbed as duplicates.
    assert!(
        w.status().unwrap()["pipeline"]["duplicate"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn status_reports_devices_channels_and_health() {
    let w = morning();
    let s = w.status().unwrap();
    let device = |name: &str| {
        s["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(device("pc")["state"], "connected");
    assert_eq!(device("phone")["state"], "connected");
    assert_eq!(device("pc")["spool_pending"], 0);
    let channel = |id: &str| {
        s["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(channel("screen"), "live");
    assert_eq!(channel("health"), "live");
    assert_eq!(
        channel("dictation"),
        "not_set_up",
        "nothing in the scenario dictates"
    );

    assert_eq!(w.get("/healthz").unwrap(), (200, "ok\n".to_string()));
    let (code, metrics) = w.get("/metrics").unwrap();
    assert_eq!(code, 200);
    assert!(
        metrics.contains("time_device_last_heartbeat_timestamp_seconds{device=\"pc\"}"),
        "{metrics}"
    );
    assert!(
        metrics.contains("time_device_spool_pending{device=\"phone\"}"),
        "{metrics}"
    );
}

#[test]
fn malformed_batches_are_refused_and_partial_ones_partly_accepted() {
    let w = morning();
    let client = reqwest::blocking::Client::new();
    let url = format!("{}/v1/logs", w.base_url);
    let r = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = client
        .post(&url)
        .header("content-type", "application/x-protobuf")
        .body(vec![0u8])
        .send()
        .unwrap();
    assert_eq!(r.status(), 415);

    let partial = serde_json::json!({"resourceLogs": [{
        "resource": {"attributes": [
            {"key": "host.name", "value": {"stringValue": "pc"}},
            {"key": "service.namespace", "value": {"stringValue": "time"}}
        ]},
        "scopeLogs": [{"logRecords": [
            {"attributes": [
                {"key": "event.name", "value": {"stringValue": "time.heartbeat"}},
                {"key": "event.id", "value": {"stringValue": "manual-1"}},
                {"key": "time.start", "value": {"intValue": "1"}}
            ]},
            {"attributes": [{"key": "event.name", "value": {"stringValue": "time.heartbeat"}}]}
        ]}]
    }]});
    let r = client.post(&url).json(&partial).send().unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().unwrap();
    assert_eq!(body["partialSuccess"]["rejectedLogRecords"], "1");
}

#[test]
fn the_morning_is_labelled_minute_by_minute() {
    use time_core::label::{Activity, Place};
    let w = morning();
    let at = |hhmm: &str| w.scenario.at(hhmm).unwrap();
    let labels = w.labels(w.scenario.start_ms, w.scenario.end_ms).unwrap();
    assert_eq!(labels.len(), 120, "every minute labelled");
    let l = |hhmm: &str| labels.iter().find(|l| l.ts == at(hhmm)).unwrap().clone();

    // Watching YouTube without touching anything: video, not away.
    let m = l("09:05");
    assert_eq!(
        (m.place, m.activity, m.rule.as_str()),
        (Place::Computer, Some(Activity::Video), "video.sites")
    );
    // Typing in T3 Code while X is open on the phone: the typing wins.
    let m = l("09:45");
    assert_eq!(
        (m.place, m.activity),
        (Place::Computer, Some(Activity::Work))
    );
    // Still typing while YouTube plays on the phone: work, with video behind it.
    let m = l("10:07");
    assert_eq!(
        (m.activity, m.background),
        (Some(Activity::Work), Some(Activity::Video))
    );
    // The password manager: counted, never described.
    assert_eq!(l("10:11").rule, "blocked");
    // dn.se in Firefox while Zen has HBO Max open: browsing, not video.
    assert_eq!(l("10:20").activity, Some(Activity::Browsing));
    // Nobody there.
    assert_eq!(l("10:45").place, Place::Away);
    assert!(
        labels.iter().all(|l| l.complete),
        "every device reported past every minute"
    );

    let s = w.status().unwrap();
    let c = &s["pipeline"]["classifier"];
    assert_eq!(c["waiting"], 0);
    assert_eq!(c["labelled_total"], 120);
    assert_eq!(c["last_24h_unknown"], 0);
}

#[test]
fn a_late_phone_sync_relabels_minutes_already_labelled() {
    use time_core::label::{Activity, Place};
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/morning.toml");
    let mut w = World::load(&path).unwrap();
    let start = w.scenario.start_ms;
    let at = |hhmm: &str| w.scenario.at(hhmm).unwrap();
    let (t1005, t1035) = (at("10:05"), at("10:35"));
    // Run up to 10:35 with the phone offline since 10:00, and label now.
    for t in (start..t1035).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    w.classify().unwrap();
    let before = w.labels(t1005, t1005 + MINUTE_MS).unwrap().remove(0);
    assert!(!before.complete, "the phone has not reported 10:05 yet");
    // The phone comes back at 10:40 and syncs; the minute is relabelled.
    for t in (t1035..w.scenario.end_ms).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    w.classify().unwrap();
    let after = w.labels(t1005, t1005 + MINUTE_MS).unwrap().remove(0);
    assert!(after.complete);
    assert_eq!(
        (after.place, after.background),
        (Place::Computer, Some(Activity::Video))
    );
}

#[test]
fn the_status_page_answers_is_it_working() {
    let w = morning();
    let (code, html) = w.get("/status").unwrap();
    assert_eq!(code, 200);
    for needle in [
        "Everything is working.",
        "pc",
        "phone",
        "Connected",
        "Phone apps",
        "Recording",
        "Not set up",
        "Labelled",
        "rules on the CPU",
    ] {
        assert!(html.contains(needle), "status page lacks {needle:?}");
    }
    let (code, _) = w.get("/").unwrap();
    assert_eq!(code, 200, "/ redirects to a page that loads");
}

#[test]
fn a_silent_device_is_named_on_the_status_page() {
    use time_core::clock::Clock;
    let w = morning();
    // Three hours later with nobody sending anything.
    w.clock.set(w.clock.now_ms() + 3 * 60 * MINUTE_MS);
    let (_, html) = w.get("/status").unwrap();
    assert!(
        html.contains("Not hearing from pc, phone."),
        "{}",
        &html[..html.len().min(2000)]
    );
    assert!(html.contains("Offline"));
}
