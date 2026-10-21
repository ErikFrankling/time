//! Bring v1's history into v2 as events.
//!
//! v1 stored one row per device-minute with a model's label on it. The
//! facts in those rows (window, input counts, idle time, phone app) become
//! v2 events; the labels are not imported -- v2 recomputes them with rules,
//! and [`compare`] measures how the two differ.
//!
//! v1's `domain` column is not imported: it was taken from whichever browser
//! reported last, not the focused one, so it is wrong exactly when it
//! matters (a Firefox window on dn.se was recorded as play.hbomax.com).
//! v2's title rules cover the same ground from the window title.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, PhoneApp, ScreenMinute};
use time_core::label::{Activity, Place};

use crate::store::Store;

pub const SERVICE: &str = "v1-import";
const CHUNK: usize = 5_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub screen_minutes: usize,
    pub phone_spans: usize,
    pub inserted: usize,
    pub duplicates: usize,
    pub skipped_devices: BTreeMap<String, usize>,
}

struct Row {
    ts: Millis,
    device: String,
    window: String,
    keys: u32,
    mouse: u32,
    idle_secs: Option<u32>,
    blocked: bool,
}

fn open(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening v1 database {}", path.display()))
}

fn rows(conn: &Connection) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(
        "SELECT ts, device, coalesce(window, ''), keys, mouse, idle_secs, coalesce(blocked, 0)
         FROM minute ORDER BY device, ts",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Row {
            ts: r.get::<_, i64>(0)? * 1000,
            device: r.get(1)?,
            window: r.get(2)?,
            keys: r.get::<_, i64>(3)?.max(0) as u32,
            mouse: r.get::<_, i64>(4)?.max(0) as u32,
            idle_secs: r
                .get::<_, Option<i64>>(5)?
                .map(|v| v.clamp(0, u32::MAX as i64) as u32),
            blocked: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// v1 wrote desktop windows as "class — title" and phone apps as
/// "Label — package". It wrote "unknown" when nothing had focus.
fn split(window: &str) -> (&str, &str) {
    if window == "unknown" {
        return ("", "");
    }
    window.split_once(" — ").unwrap_or((window, ""))
}

/// Convert v1 rows into events. Only devices named in `desktops` or
/// `phones` are imported; v1's test devices are left behind.
pub fn events(
    conn: &Connection,
    desktops: &[String],
    phones: &[String],
) -> Result<(Vec<Envelope>, ImportReport)> {
    let mut report = ImportReport::default();
    let mut out = Vec::new();
    // The open phone span: (device, label, package, start, end).
    let mut span: Option<(String, String, String, Millis, Millis)> = None;
    let close = |span: &mut Option<(String, String, String, Millis, Millis)>,
                 out: &mut Vec<Envelope>,
                 report: &mut ImportReport| {
        if let Some((device, label, package, start, end)) = span.take() {
            out.push(Envelope::stable(
                &device,
                SERVICE,
                start,
                end,
                Event::PhoneApp(PhoneApp {
                    package,
                    label,
                    media_playing: false,
                }),
            ));
            report.phone_spans += 1;
        }
    };

    for row in rows(conn)? {
        if desktops.contains(&row.device) {
            let (class, title) = if row.blocked {
                ("", "")
            } else {
                split(&row.window)
            };
            let m = ScreenMinute {
                window_class: class.to_owned(),
                window_title: title.to_owned(),
                browser_host: None,
                fullscreen: false,
                audio_playing: false,
                keys: row.keys,
                mouse: row.mouse,
                idle_seconds: row.idle_secs,
                blocked: row.blocked,
            };
            out.push(Envelope::stable(
                &row.device,
                SERVICE,
                row.ts,
                row.ts + MINUTE_MS,
                Event::ScreenMinute(m),
            ));
            report.screen_minutes += 1;
        } else if phones.contains(&row.device) {
            if row.window.is_empty() {
                close(&mut span, &mut out, &mut report);
                continue;
            }
            let (label, package) = split(&row.window);
            let extends = span.as_ref().is_some_and(|(d, _, p, _, end)| {
                d == &row.device && p == package && *end == row.ts
            });
            if extends {
                if let Some(s) = span.as_mut() {
                    s.4 = row.ts + MINUTE_MS;
                }
            } else {
                close(&mut span, &mut out, &mut report);
                span = Some((
                    row.device.clone(),
                    label.to_owned(),
                    package.to_owned(),
                    row.ts,
                    row.ts + MINUTE_MS,
                ));
            }
        } else {
            *report.skipped_devices.entry(row.device).or_default() += 1;
        }
    }
    close(&mut span, &mut out, &mut report);
    Ok((out, report))
}

pub fn import(
    path: &Path,
    store: &Store,
    desktops: &[String],
    phones: &[String],
    now: Millis,
) -> Result<ImportReport> {
    let conn = open(path)?;
    let (events, mut report) = events(&conn, desktops, phones)?;
    for chunk in events.chunks(CHUNK) {
        let r = store.insert(chunk, now)?;
        report.inserted += r.new;
        report.duplicates += r.duplicate;
    }
    Ok(report)
}

/// v1's model label, mapped onto v2's vocabulary.
pub fn map_v1_category(c: &str) -> Option<Activity> {
    Some(match c {
        "work_neptune" | "work_husk" | "work_personal" => Activity::Work,
        "kth" => Activity::Study,
        "comms" => Activity::Comms,
        "youtube" | "streaming" | "netflix" => Activity::Video,
        "twitter" => Activity::Social,
        "browsing" => Activity::Browsing,
        "music" => Activity::Music,
        "other" => Activity::Other,
        _ => return None, // idle and anything unexpected
    })
}

#[derive(Debug, Default)]
pub struct Comparison {
    /// v1 minutes the model labelled (non-idle) whose v2 label is known.
    pub compared: usize,
    pub agree: usize,
    /// v2 has no rule for it.
    pub unknown: usize,
    /// (v1 activity, v2 activity) -> minutes, for disagreements.
    pub confusions: BTreeMap<(String, String), usize>,
    /// The most common v2 `unknown` windows, to grow the rules from.
    pub top_unknown: Vec<(String, usize)>,
}

/// How v2's rule labels compare with v1's model labels, minute by minute.
/// v1's labels are noisy (the same Netflix tab labelled three ways), so
/// this is a sanity check, not ground truth.
pub fn compare(path: &Path, store: &Store) -> Result<Comparison> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT ts, device, category, coalesce(window, '') FROM minute
         WHERE classified = 1 AND category != 'idle' ORDER BY ts",
    )?;
    let rows: Vec<(Millis, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)? * 1000, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut c = Comparison::default();
    let mut unknown_windows: BTreeMap<String, usize> = BTreeMap::new();
    for (ts, device, category, window) in rows {
        let Some(v1) = map_v1_category(&category) else {
            continue;
        };
        let Some(label) = store.labels_between(ts, ts + 1)?.pop() else {
            continue;
        };
        // Only compare where v2 also put Erik on that device.
        if label.device.as_deref() != Some(device.as_str()) || label.place == Place::Away {
            continue;
        }
        let Some(v2) = label.activity else { continue };
        if v2 == Activity::Unknown {
            c.unknown += 1;
            *unknown_windows.entry(window).or_default() += 1;
            continue;
        }
        c.compared += 1;
        if v1 == v2 {
            c.agree += 1;
        } else {
            *c.confusions
                .entry((v1.to_string(), v2.to_string()))
                .or_default() += 1;
        }
    }
    let mut top: Vec<_> = unknown_windows.into_iter().collect();
    top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    top.truncate(30);
    c.top_unknown = top;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1_db(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("v1.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE minute (ts INTEGER NOT NULL, device TEXT NOT NULL, category TEXT NOT NULL,
               project TEXT, detail TEXT, window TEXT, phash INTEGER, model TEXT,
               keys INTEGER NOT NULL DEFAULT 0, mouse INTEGER NOT NULL DEFAULT 0, idle_secs INTEGER,
               apps TEXT, workspaces INTEGER NOT NULL DEFAULT 0, classified INTEGER NOT NULL DEFAULT 0,
               tags TEXT, domain TEXT, pending INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
               image_path TEXT, note TEXT, blocked INTEGER NOT NULL DEFAULT 0, inserted INTEGER,
               PRIMARY KEY (device, ts));
             INSERT INTO minute (ts, device, category, window, keys, mouse, idle_secs, classified, domain) VALUES
               (60, 'pc', 'youtube', 'firefox — Cats - YouTube — Mozilla Firefox', 0, 2, 400, 1, 'play.hbomax.com'),
               (120, 'pc', 'work_personal', 'kitty — ~/.dotfiles', 40, 0, 2, 1, NULL),
               (180, 'pc', 'other', 'mystery-app — hello', 3, 0, 1, 1, NULL),
               (60, 'phone', 'twitter', 'X — com.twitter.android', 0, 0, NULL, 1, NULL),
               (120, 'phone', 'twitter', 'X — com.twitter.android', 0, 0, NULL, 1, NULL),
               (240, 'phone', 'comms', 'Snapchat — com.snapchat.android', 0, 0, NULL, 1, NULL),
               (60, 'tagtest', 'other', 'x — y', 0, 0, NULL, 1, NULL);",
        )
        .unwrap();
        path
    }

    #[test]
    fn imports_minutes_merges_phone_spans_and_skips_test_devices() {
        let dir = tempfile::tempdir().unwrap();
        let path = v1_db(dir.path());
        let store = Store::open_in_memory().unwrap();
        let (pc, phone) = (vec!["pc".to_string()], vec!["phone".to_string()]);
        let r = import(&path, &store, &pc, &phone, 0).unwrap();
        assert_eq!((r.screen_minutes, r.phone_spans), (3, 2));
        assert_eq!(r.skipped_devices.get("tagtest"), Some(&1));
        // Importing twice changes nothing.
        let again = import(&path, &store, &pc, &phone, 0).unwrap();
        assert_eq!((again.inserted, again.duplicates), (0, 5));

        let events = store.events_between(0, 10_000_000).unwrap();
        let x = events
            .iter()
            .find(|e| matches!(&e.event, Event::PhoneApp(a) if a.package == "com.twitter.android"))
            .unwrap();
        assert_eq!(
            (x.start_ms, x.end_ms),
            (60_000, 180_000),
            "two minutes merged into one span"
        );
        let cats = events
            .iter()
            .find(|e| e.start_ms == 60_000 && e.device == "pc")
            .unwrap();
        match &cats.event {
            Event::ScreenMinute(m) => {
                assert_eq!(m.window_class, "firefox");
                assert_eq!(m.browser_host, None, "v1's domain is not trusted");
            }
            other => panic!("{other:?}"),
        }
    }
}
