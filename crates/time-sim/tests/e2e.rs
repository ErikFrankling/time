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

#[test]
fn the_day_page_and_api_show_the_morning() {
    let w = morning();
    let (code, json) = w.get("/api/day/2026-10-21").unwrap();
    assert_eq!(code, 200, "{json}");
    let day: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(day["steps"], 3000);
    let total = |name: &str| {
        day["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["activity"] == name)
            .map(|a| a["computer"].as_u64().unwrap() + a["phone"].as_u64().unwrap())
            .unwrap_or(0)
    };
    // 09:00-09:40 YouTube on the pc, and 10:12-10:15 on the phone while the
    // pc only had mouse movement.
    assert_eq!(total("video"), 43);
    // 09:40-10:10 typing in T3 Code beats the phone.
    assert_eq!(total("work"), 30);

    let (code, html) = w.get("/day/2026-10-21").unwrap();
    assert_eq!(code, 200);
    for needle in [
        "Wednesday 21 October",
        "Hour by hour",
        "Where the time went",
        "3\u{2009}000",
        "steps · 2.1 km",
        "09:00–09:40 · video on the computer · video.sites",
    ] {
        assert!(html.contains(needle), "day page lacks {needle:?}");
    }
    assert_eq!(w.get("/day/yesterday").unwrap().0, 400);
}

#[test]
fn answering_the_inbox_writes_a_rule_and_relabels_history() {
    use time_core::label::Activity;
    let scenario = time_sim::scenario::Scenario::parse(
        r#"
        name = "inbox"
        start = "2026-10-21T09:00:00+02:00"
        end = "09:30"
        desktops = ["pc"]
        [[desktop]]
        device = "pc"
        from = "09:00"
        to = "09:30"
        class = "org.weird.App"
        title = "Weird — \"quoted\" title"
        keys = 20
        "#,
    )
    .unwrap();
    let mut w = World::start(scenario).unwrap();
    w.run().unwrap();
    let all = |w: &World| w.labels(w.scenario.start_ms, w.scenario.end_ms).unwrap();
    assert!(all(&w)
        .iter()
        .all(|l| l.activity == Some(Activity::Unknown)));

    let (code, html) = w.get("/inbox").unwrap();
    assert_eq!(code, 200);
    assert!(html.contains("30 min across 1 windows"), "{html}");
    assert!(html.contains("All of org.weird.App"));

    // Answer it the way the form does.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let url = format!("{}/inbox/rule", w.base_url);
    let r = client
        .post(&url)
        .form(&[
            ("on", "class"),
            ("value", "org.weird.App"),
            ("activity", "work"),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().unwrap_or_default());
    w.classify().unwrap();
    assert!(all(&w)
        .iter()
        .all(|l| l.activity == Some(Activity::Work) && l.rule.starts_with("local.")));
    let (_, html) = w.get("/inbox").unwrap();
    assert!(html.contains("Nothing unknown"));

    // Another site cannot post rules.
    let r = client
        .post(&url)
        .header("origin", "https://evil.example")
        .form(&[("on", "class"), ("value", "x"), ("activity", "work")])
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[test]
fn the_week_page_sums_the_days() {
    let w = morning();
    let (code, html) = w.get("/week/2026-10-23").unwrap();
    assert_eq!(code, 200);
    // 2026-10-23 is a Friday; its week starts Monday 19 October.
    assert!(html.contains("Week of 19 October"), "{html}");
    assert!(html.contains(r#"href="/day/2026-10-21""#));
    assert!(html.contains("3\u{2009}000"), "steps from Wednesday");
}

#[test]
fn the_app_endpoints_answer_honestly_without_an_apk() {
    let w = morning();
    assert_eq!(w.get("/app/version").unwrap().0, 404);
    assert_eq!(w.get("/app").unwrap().0, 404);
    let (code, html) = w.get("/app/").unwrap();
    assert_eq!(code, 200);
    assert!(html.contains("No APK has been fetched yet."));
}

#[test]
fn an_hour_of_youtube_and_x_gets_one_nudge_then_an_outcome() {
    use time_core::clock::Clock;
    use time_core::event::Event;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/doomscroll.toml");
    let mut w = World::load(&path).unwrap();
    let sent = w.capture_nudges();
    let at = |w: &World, hhmm: &str| w.scenario.at(hhmm).unwrap();
    let start = w.scenario.start_ms;

    // Run to 11:00: an hour of YouTube, then X on the phone.
    for t in (start..at(&w, "11:00")).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    let nudge = w.nudge_check().unwrap().expect("a nudge at 11:00");
    {
        let got = sent.0.lock().unwrap();
        assert_eq!(got.len(), 1);
        let (title, body, facts) = &got[0];
        // The X session on the phone is still open at 11:00, and a phone span is
        // only sent once it ends, so this nudge counts the YouTube alone.
        assert_eq!(title, "40 min on YouTube");
        assert!(body.contains("Time to get back to it?"));
        assert_eq!(facts["nudges_today"], 1);
    }

    // Five minutes later, still scrolling: held by the cooldown.
    for t in (at(&w, "11:00")..at(&w, "11:05")).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    assert!(w.nudge_check().unwrap().is_none());
    assert_eq!(sent.0.lock().unwrap().len(), 1);
    let (_, status) = w.get("/status").unwrap();
    assert!(
        status.contains("held: Cooldown"),
        "status explains why nothing was sent"
    );

    // Back to work from 11:05. The 10-minute outcome says he switched.
    for t in (at(&w, "11:05")..at(&w, "11:20")).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    w.nudge_check().unwrap();
    let outcome = w
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.event {
            Event::NudgeOutcome(o) if o.nudge_id == nudge.id => Some(o),
            _ => None,
        })
        .expect("an outcome is recorded");
    assert!(
        !outcome.switched,
        "10:50-11:00 he was still on X, the window right after the nudge"
    );
    let _ = w.clock.now_ms();
}

#[test]
fn a_focus_day_set_by_the_agent_nudges_after_hours() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/doomscroll.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("2026-10-21T10:00:00+02:00", "2026-10-24T20:00:00+02:00")
        .replace("\"10:", "\"20:")
        .replace("\"11:", "\"21:");
    let mut w = World::start(time_sim::scenario::Scenario::parse(&text).unwrap()).unwrap();
    let sent = w.capture_nudges();
    let start = w.scenario.start_ms;
    let nine = w.scenario.at("21:00").unwrap();
    for t in (start..nine).step_by(MINUTE_MS as usize) {
        w.step(t).unwrap();
    }
    // Saturday evening: no nudge on its own.
    assert!(w.nudge_check().unwrap().is_none());
    // The personal agent sets a focus day.
    let r = reqwest::blocking::Client::new()
        .post(format!("{}/v1/focus", w.base_url))
        .json(&serde_json::json!({ "until_ms": nine + 3 * 3_600_000, "reason": "the DA2210 essay due tomorrow", "threshold_minutes": 20 }))
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(w.nudge_check().unwrap().is_some());
    let got = sent.0.lock().unwrap();
    assert!(
        got[0]
            .1
            .contains("Today's focus: the DA2210 essay due tomorrow."),
        "{}",
        got[0].1
    );
}

#[test]
fn commits_and_pull_requests_show_on_the_day() {
    use time_core::event::{Commit, Envelope, Event, PullRequest};
    let w = morning();
    let t = w.scenario.at("10:30").unwrap();
    let ev = |e| Envelope::new("pc", "github-collector", t, t, e);
    let batch = time_core::otlp::encode(
        &[
            ev(Event::Commit(Commit {
                repo: "time".into(),
                sha: "abc".into(),
                path: None,
                lines_added: 1200,
                lines_removed: 40,
                files: 9,
            })),
            ev(Event::PullRequest(PullRequest {
                repo: "time".into(),
                number: 7,
                action: "merged".into(),
            })),
        ],
        "test",
    );
    let r = reqwest::blocking::Client::new()
        .post(format!("{}/v1/logs", w.base_url))
        .json(&batch)
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    let (_, html) = w.get("/day/2026-10-21").unwrap();
    assert!(html.contains("+1\u{2009}200 −40 lines"), "{html}");
    assert!(html.contains("0 opened · 1 merged · 0 reviews"));
    let s = w.status().unwrap();
    let code = s["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "code")
        .unwrap()
        .clone();
    assert_eq!(code["state"], "live");
}

#[test]
fn the_v1_phone_app_keeps_working_against_v2() {
    let w = morning();
    let ts = w.scenario.at("10:50").unwrap() / 1000;
    let frames = serde_json::json!([
        {"ts": ts, "device": "phone", "window": "X — com.twitter.android", "blocked": false, "keys": 0, "mouse": 0, "apps": ["com.twitter.android"], "workspaces": 0}
    ]);
    let r = reqwest::blocking::Client::new().post(format!("{}/v1/frames", w.base_url)).json(&frames).send().unwrap();
    assert_eq!(r.status(), 200);
    let acks: serde_json::Value = r.json().unwrap();
    assert_eq!(acks[0]["ts"], ts);
    let s = w.status().unwrap();
    let phone = s["devices"].as_array().unwrap().iter().find(|d| d["name"] == "phone").unwrap().clone();
    assert_eq!(phone["state"], "connected");
    assert_eq!(phone["app_version"], "v1");
}
