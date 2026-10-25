//! SQLite storage: events, devices, and what is derived from them.
//!
//! One connection behind a mutex is the single writer. SQLite allows one
//! writer at a time anyway, and v1's `SQLITE_BUSY` retries came from having
//! several. Handlers reach it through `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use time_core::clock::{floor_minute, Millis, MINUTE_MS};
use time_core::event::{Envelope, Event};
use time_core::label::MinuteLabel;

/// Numbered migrations. Each runs once, in order, inside a transaction, and
/// `PRAGMA user_version` records how far a database has come. Never edit a
/// published entry; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: events and devices.
    "CREATE TABLE event (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        device      TEXT NOT NULL,
        service     TEXT NOT NULL,
        start_ms    INTEGER NOT NULL,
        end_ms      INTEGER NOT NULL,
        received_ms INTEGER NOT NULL,
        body        TEXT NOT NULL
     );
     CREATE INDEX event_start ON event(start_ms);
     CREATE INDEX event_device_start ON event(device, start_ms);
     CREATE INDEX event_name_received ON event(name, received_ms);
     CREATE TABLE device (
        name              TEXT PRIMARY KEY,
        last_event_ms     INTEGER,
        last_heartbeat_ms INTEGER,
        covered_until_ms  INTEGER,
        spool_pending     INTEGER,
        app_version       TEXT,
        missing           TEXT
     );",
    // 2: derived minute labels, and the queue of minutes to (re)compute.
    "CREATE TABLE minute (
        ts            INTEGER PRIMARY KEY,
        place         TEXT NOT NULL,
        device        TEXT,
        activity      TEXT,
        project       TEXT,
        background    TEXT,
        rule          TEXT NOT NULL,
        decider       TEXT NOT NULL,
        complete      INTEGER NOT NULL,
        rules_version TEXT NOT NULL,
        computed_ms   INTEGER NOT NULL
     );
     CREATE TABLE dirty (ts INTEGER PRIMARY KEY);
     CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    // 3: where Erik was, per minute.
    "ALTER TABLE minute ADD COLUMN location TEXT;",
    // 4: which device the parallel activity was on.
    "ALTER TABLE minute ADD COLUMN background_device TEXT;",
    // 5: what filled the minute (site, program, phone app), for drill-downs.
    "ALTER TABLE minute ADD COLUMN what TEXT;
     ALTER TABLE minute ADD COLUMN background_what TEXT;",
    // 6: what the rules and the decision model derived on top of minutes:
    // gaps filled with the activity around them, a night's sleep, and every
    // question put to the model with its answer (the cache and the log).
    "CREATE TABLE decision (
        id            INTEGER PRIMARY KEY,
        kind          TEXT NOT NULL,
        key           TEXT NOT NULL,
        input_hash    TEXT NOT NULL,
        model         TEXT NOT NULL,
        prompt        TEXT NOT NULL,
        answer        TEXT,
        error         TEXT,
        input_tokens  INTEGER,
        output_tokens INTEGER,
        created_ms    INTEGER NOT NULL
     );
     CREATE INDEX decision_key ON decision(kind, key, input_hash);
     CREATE TABLE fill (
        start_ms    INTEGER PRIMARY KEY,
        end_ms      INTEGER NOT NULL,
        place       TEXT NOT NULL,
        device      TEXT NOT NULL,
        activity    TEXT NOT NULL,
        what        TEXT,
        project     TEXT,
        confidence  REAL NOT NULL,
        source      TEXT NOT NULL,
        reason      TEXT NOT NULL,
        decision_id INTEGER,
        computed_ms INTEGER NOT NULL
     );
     CREATE TABLE sleep (
        night              TEXT PRIMARY KEY,
        fell_asleep_ms     INTEGER,
        woke_up_ms         INTEGER,
        interruptions      TEXT NOT NULL,
        confidence         REAL,
        reason             TEXT NOT NULL,
        source             TEXT NOT NULL,
        candidate_start_ms INTEGER,
        candidate_end_ms   INTEGER,
        decision_id        INTEGER,
        final              INTEGER NOT NULL,
        computed_ms        INTEGER NOT NULL
     );",
];

/// One computer minute of input counts (never which keys).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct InputMinute {
    pub ts: Millis,
    /// Key presses, mouse buttons included.
    pub keys: u32,
    pub clicks: u32,
    /// Pointer motion events.
    pub mouse: u32,
    /// Seconds with any input; None from agents that did not count them.
    pub active_seconds: Option<u32>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// What one insert did, for the ingest response and the counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Inserted {
    pub new: usize,
    pub duplicate: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub name: String,
    pub last_event_ms: Option<Millis>,
    pub last_heartbeat_ms: Option<Millis>,
    pub covered_until_ms: Option<Millis>,
    pub spool_pending: Option<u32>,
    pub app_version: Option<String>,
    pub missing: Vec<String>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite itself consistent (the
        // transaction rolls back), so the poison flag carries no information.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Store events, skipping ids already present, and update each device's
    /// liveness from what arrived. All or nothing per call.
    pub fn insert(&self, envelopes: &[Envelope], received_ms: Millis) -> Result<Inserted> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut out = Inserted::default();
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO event (id, name, device, service, start_ms, end_ms, received_ms, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(id) DO NOTHING",
            )?;
            let mut seen = tx.prepare_cached(
                "INSERT INTO device (name, last_event_ms) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET last_event_ms = max(coalesce(last_event_ms, 0), excluded.last_event_ms)",
            )?;
            let mut dirty = tx.prepare_cached("INSERT OR IGNORE INTO dirty (ts) VALUES (?1)")?;
            let mut beat = tx.prepare_cached(
                "UPDATE device SET last_heartbeat_ms = ?2, spool_pending = ?3, app_version = ?4,
                        covered_until_ms = coalesce(?5, covered_until_ms), missing = ?6
                 WHERE name = ?1 AND coalesce(last_heartbeat_ms, 0) <= ?2",
            )?;
            for env in envelopes {
                let body = serde_json::to_string(&env.event)?;
                let n = ins.execute(params![
                    env.id,
                    env.event.name(),
                    env.device,
                    env.service,
                    env.start_ms,
                    env.end_ms,
                    received_ms,
                    body
                ])?;
                if n == 0 {
                    out.duplicate += 1;
                    continue;
                }
                out.new += 1;
                seen.execute(params![env.device, env.end_ms])?;
                if !matches!(env.event, Event::Heartbeat(_)) {
                    // A phone's state places the minutes after it too (the
                    // ones that have happened: never label the future).
                    let end = match env.event {
                        Event::PhoneState(_) => (env.end_ms.max(env.start_ms)
                            + time_core::timeline::PHONE_STATE_HOLD_MS
                            - MINUTE_MS)
                            .min(received_ms)
                            .max(env.end_ms),
                        _ => env.end_ms,
                    };
                    for ts in minutes_touched(env.start_ms, end) {
                        dirty.execute([ts])?;
                    }
                }
                if let Event::Heartbeat(hb) = &env.event {
                    beat.execute(params![
                        env.device,
                        env.start_ms,
                        hb.spool_pending,
                        hb.app_version,
                        hb.covered_until_ms,
                        serde_json::to_string(&hb.missing)?,
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(out)
    }

    pub fn devices(&self) -> Result<Vec<DeviceRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, last_event_ms, last_heartbeat_ms, covered_until_ms, spool_pending, app_version, missing
             FROM device ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            let missing: Option<String> = r.get(6)?;
            Ok(DeviceRow {
                name: r.get(0)?,
                last_event_ms: r.get(1)?,
                last_heartbeat_ms: r.get(2)?,
                covered_until_ms: r.get(3)?,
                spool_pending: r.get(4)?,
                app_version: r.get(5)?,
                missing: missing
                    .and_then(|m| serde_json::from_str(&m).ok())
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest arrival time and count since `since_ms`, per event name.
    pub fn channel_activity(&self, since_ms: Millis) -> Result<Vec<(String, Option<Millis>, u64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, max(received_ms), sum(received_ms >= ?1) FROM event GROUP BY name",
        )?;
        let rows = stmt.query_map([since_ms], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn event_count(&self) -> Result<u64> {
        Ok(self
            .conn()
            .query_row("SELECT count(*) FROM event", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Events overlapping `[from, to)`, oldest first.
    pub fn events_between(&self, from: Millis, to: Millis) -> Result<Vec<Envelope>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, device, service, start_ms, end_ms, body FROM event
             WHERE start_ms < ?2 AND end_ms >= ?1 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, device, service, start_ms, end_ms, body) = row?;
            let event = serde_json::from_str(&body).with_context(|| format!("event {id} body"))?;
            out.push(Envelope {
                id,
                device,
                service,
                start_ms,
                end_ms,
                event,
            });
        }
        Ok(out)
    }

    /// A consistent copy of the live database at `dest`, through SQLite's
    /// online backup API: safe while the server keeps writing.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        let conn = self.conn();
        let mut out =
            Connection::open(dest).with_context(|| format!("opening {}", dest.display()))?;
        let b = rusqlite::backup::Backup::new(&conn, &mut out)?;
        b.run_to_completion(1000, std::time::Duration::from_millis(5), None)?;
        Ok(())
    }

    /// `PRAGMA integrity_check` and row counts: what a restore test checks.
    pub fn verify(&self) -> Result<(String, u64, u64)> {
        let conn = self.conn();
        let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let events: i64 = conn.query_row("SELECT count(*) FROM event", [], |r| r.get(0))?;
        let minutes: i64 = conn.query_row("SELECT count(*) FROM minute", [], |r| r.get(0))?;
        Ok((ok, events as u64, minutes as u64))
    }

    /// Prove the database accepts a write, without leaving one behind.
    pub fn check_writable(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
        Ok(())
    }

    pub fn size_bytes(&self) -> Result<u64> {
        let conn = self.conn();
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((pages * size) as u64)
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Up to `limit` minutes waiting to be (re)labelled, oldest first.
    pub fn dirty_batch(&self, limit: usize) -> Result<Vec<Millis>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT ts FROM dirty ORDER BY ts LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn dirty_stats(&self) -> Result<(u64, Option<Millis>)> {
        Ok(self
            .conn()
            .query_row("SELECT count(*), min(ts) FROM dirty", [], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))
            })?)
    }

    pub fn mark_dirty(&self, from: Millis, to: Millis) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO dirty (ts) VALUES (?1)")?;
            for ts in minutes_touched(from, to) {
                stmt.execute([ts])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Queue every minute that has a label or any event, e.g. after the
    /// rules changed. Returns the queue length.
    pub fn mark_all_dirty(&self) -> Result<u64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO dirty (ts) SELECT ts FROM minute", [])?;
        let spans: Vec<(Millis, Millis)> = {
            let mut stmt =
                tx.prepare("SELECT start_ms, end_ms FROM event WHERE name != 'time.heartbeat'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        {
            let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO dirty (ts) VALUES (?1)")?;
            for (a, b) in spans {
                for ts in minutes_touched(a, b) {
                    stmt.execute([ts])?;
                }
            }
        }
        let n: i64 = tx.query_row("SELECT count(*) FROM dirty", [], |r| r.get(0))?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Store labels and take their minutes off the queue, in one transaction.
    pub fn write_labels(
        &self,
        labels: &[MinuteLabel],
        rules_version: &str,
        computed_ms: Millis,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO minute (ts, place, device, activity, project, background, rule, decider, complete, rules_version, computed_ms, location, background_device, what, background_what)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(ts) DO UPDATE SET place = excluded.place, device = excluded.device,
                    activity = excluded.activity, project = excluded.project, background = excluded.background,
                    rule = excluded.rule, decider = excluded.decider, complete = excluded.complete,
                    rules_version = excluded.rules_version, computed_ms = excluded.computed_ms,
                    location = excluded.location, background_device = excluded.background_device,
                    what = excluded.what, background_what = excluded.background_what",
            )?;
            let mut done = tx.prepare_cached("DELETE FROM dirty WHERE ts = ?1")?;
            for l in labels {
                up.execute(params![
                    l.ts,
                    enum_str(&l.place),
                    l.device,
                    l.activity.map(|a| a.as_str()),
                    l.project,
                    l.background.map(|a| a.as_str()),
                    l.rule,
                    enum_str(&l.decider),
                    l.complete,
                    rules_version,
                    computed_ms,
                    l.location,
                    l.background_device,
                    l.what,
                    l.background_what,
                ])?;
                done.execute([l.ts])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Steps and metres from Health Connect records starting in `[from, to)`.
    pub fn steps_between(&self, from: Millis, to: Millis) -> Result<(u32, f64)> {
        let conn = self.conn();
        let (steps, meters): (i64, f64) = conn.query_row(
            "SELECT COALESCE(SUM(json_extract(body, '$.steps')), 0),
                    COALESCE(SUM(json_extract(body, '$.\"distance.meters\"')), 0.0)
             FROM event WHERE name = 'time.health.steps' AND start_ms >= ?1 AND start_ms < ?2",
            [from, to],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((steps.max(0) as u32, meters))
    }

    /// Audible-sound events starting in `[from, to)`: (device, start, what).
    pub fn listening_between(
        &self,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<(String, Millis, time_core::event::Listening)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT device, start_ms, body FROM event
             WHERE name = 'time.audio.listening' AND start_ms >= ?1 AND start_ms < ?2
             ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Millis>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (device, start, body) = row?;
            if let Ok(Event::Listening(l)) = serde_json::from_str(&body) {
                out.push((device, start, l));
            }
        }
        Ok(out)
    }

    /// Keyboard and pointer counts per computer minute starting in
    /// `[from, to)`, read straight out of the JSON so a month stays cheap.
    pub fn input_between(&self, from: Millis, to: Millis) -> Result<Vec<InputMinute>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT start_ms,
                    COALESCE(json_extract(body, '$.keys'), 0),
                    COALESCE(json_extract(body, '$.clicks'), 0),
                    COALESCE(json_extract(body, '$.mouse'), 0),
                    json_extract(body, '$.\"active.seconds\"')
             FROM event WHERE name = 'time.screen.minute' AND start_ms >= ?1 AND start_ms < ?2
             ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok(InputMinute {
                ts: r.get(0)?,
                keys: r.get::<_, i64>(1)?.max(0) as u32,
                clicks: r.get::<_, i64>(2)?.max(0) as u32,
                mouse: r.get::<_, i64>(3)?.max(0) as u32,
                active_seconds: r.get::<_, Option<i64>>(4)?.map(|s| s.clamp(0, 60) as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Events by name overlapping `[from, to)`, for the small kinds (voice,
    /// code) that a period view needs without the whole event table.
    pub fn named_between(&self, names: &[&str], from: Millis, to: Millis) -> Result<Vec<Envelope>> {
        let conn = self.conn();
        let marks = vec!["?"; names.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, name, device, service, start_ms, end_ms, body FROM event
             WHERE name IN ({marks}) AND start_ms < ?{} AND end_ms >= ?{}
             ORDER BY start_ms",
            names.len() + 1,
            names.len() + 2
        ))?;
        let mut args: Vec<rusqlite::types::Value> = names
            .iter()
            .map(|n| rusqlite::types::Value::Text((*n).to_owned()))
            .collect();
        args.push(rusqlite::types::Value::Integer(to));
        args.push(rusqlite::types::Value::Integer(from));
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Millis>(4)?,
                r.get::<_, Millis>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, device, service, start_ms, end_ms, body) = row?;
            if let Ok(event) = serde_json::from_str::<Event>(&body) {
                out.push(Envelope {
                    id,
                    device,
                    service,
                    start_ms,
                    end_ms,
                    event,
                });
            }
        }
        Ok(out)
    }

    /// What each minute was, as the website and the agents see it: the
    /// labels with gaps filled and the night's sleep marked. The raw
    /// labels are [`Self::raw_labels_between`].
    pub fn labels_between(&self, from: Millis, to: Millis) -> Result<Vec<MinuteLabel>> {
        let mut labels = self.raw_labels_between(from, to)?;
        time_core::fill::overlay(&mut labels, &self.fills_between(from, to)?);
        for (a, b) in self.sleep_between(from, to)? {
            for l in labels.iter_mut().filter(|l| l.ts >= a && l.ts < b) {
                if l.place == time_core::label::Place::Away && l.rule != "presence.moving" {
                    l.rule = "presence.sleep".into();
                }
            }
        }
        Ok(labels)
    }

    /// Gap fills overlapping `[from, to)`.
    pub fn fills_between(&self, from: Millis, to: Millis) -> Result<Vec<time_core::fill::Fill>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT start_ms, end_ms, place, device, activity, what, project, confidence, source, reason
             FROM fill WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok((
                r.get::<_, Millis>(0)?,
                r.get::<_, Millis>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, f64>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                start_ms,
                end_ms,
                place,
                device,
                activity,
                what,
                project,
                confidence,
                source,
                reason,
            ) = row?;
            out.push(time_core::fill::Fill {
                start_ms,
                end_ms,
                place: parse_enum(&place)?,
                device,
                activity: activity.parse().map_err(anyhow::Error::msg)?,
                what,
                project,
                confidence: confidence as f32,
                source: parse_enum(&source)?,
                reason,
            });
        }
        Ok(out)
    }

    /// Sleep `[fell asleep, woke up)` overlapping `[from, to)`.
    pub fn sleep_between(&self, from: Millis, to: Millis) -> Result<Vec<(Millis, Millis)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT fell_asleep_ms, woke_up_ms FROM sleep
             WHERE fell_asleep_ms IS NOT NULL AND woke_up_ms > ?1 AND fell_asleep_ms < ?2
             ORDER BY fell_asleep_ms",
        )?;
        let rows = stmt.query_map([from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Run `f` on the connection: for the derived tables in `derive`.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        f(&mut self.conn())
    }

    /// The labels exactly as the rules made them, minute by minute.
    pub fn raw_labels_between(&self, from: Millis, to: Millis) -> Result<Vec<MinuteLabel>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT ts, place, device, activity, project, background, rule, decider, complete, location, background_device, what, background_what
             FROM minute WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok((
                (
                    r.get::<_, Millis>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, bool>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ),
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                (
                    ts,
                    place,
                    device,
                    activity,
                    project,
                    background,
                    rule,
                    decider,
                    complete,
                    location,
                ),
                background_device,
                what,
                background_what,
            ) = row?;
            out.push(MinuteLabel {
                ts,
                place: parse_enum(&place)?,
                device,
                activity: activity
                    .map(|a| a.parse())
                    .transpose()
                    .map_err(anyhow::Error::msg)?,
                project,
                background: background
                    .map(|a| a.parse())
                    .transpose()
                    .map_err(anyhow::Error::msg)?,
                background_device,
                rule,
                decider: parse_enum(&decider)?,
                complete,
                location,
                what,
                background_what,
            });
        }
        Ok(out)
    }

    /// Labelled minutes overall, and since `since`: how many, how many
    /// unknown, how many still waiting on a late device.
    pub fn label_stats(&self, since: Millis) -> Result<LabelStats> {
        let conn = self.conn();
        let total: i64 = conn.query_row("SELECT count(*) FROM minute", [], |r| r.get(0))?;
        let (recent, unknown, incomplete): (i64, i64, i64) = conn.query_row(
            "SELECT count(*), coalesce(sum(activity = 'unknown'), 0), coalesce(sum(NOT complete), 0)
             FROM minute WHERE ts >= ?1",
            [since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let rules_version: Option<String> =
            conn.query_row("SELECT max(rules_version) FROM minute", [], |r| r.get(0))?;
        Ok(LabelStats {
            total: total as u64,
            recent: recent as u64,
            unknown: unknown as u64,
            incomplete: incomplete as u64,
            rules_version,
        })
    }

    /// How far each device is known to have reported, as of `now`.
    ///
    /// A device silent for longer than `asleep_after` (a laptop in a bag, a
    /// phone in Doze) is treated as covered up to now: it is off, not late,
    /// and waiting on it would keep days of minutes "incomplete" and being
    /// relabelled. If it does send something late, the insert queues those
    /// minutes again anyway.
    pub fn coverage(
        &self,
        now: Millis,
        asleep_after: Millis,
    ) -> Result<std::collections::HashMap<String, Millis>> {
        Ok(self
            .devices()?
            .into_iter()
            .filter_map(|d| {
                let heard = d.last_heartbeat_ms.or(d.last_event_ms);
                let covered = d.covered_until_ms.or(d.last_event_ms);
                if heard.is_some_and(|h| now - h > asleep_after) {
                    return Some((d.name, now));
                }
                covered.map(|c| (d.name, c))
            })
            .collect())
    }

    /// Minutes not yet complete, newer than `since`: recomputed as late
    /// devices catch up.
    pub fn incomplete_since(&self, since: Millis, limit: usize) -> Result<Vec<Millis>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT ts FROM minute WHERE NOT complete AND ts >= ?1 ORDER BY ts LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelStats {
    pub total: u64,
    pub recent: u64,
    pub unknown: u64,
    pub incomplete: u64,
    pub rules_version: Option<String>,
}

/// Every minute start an interval touches. Instants touch their own minute;
/// anything longer than a day is clipped to its first day (no real event
/// is that long, and a corrupt one must not queue millions of rows).
pub fn minutes_touched(start: Millis, end: Millis) -> impl Iterator<Item = Millis> {
    let first = floor_minute(start);
    let end = end.max(start + 1).min(start + 24 * 60 * MINUTE_MS);
    (first..end).step_by(MINUTE_MS as usize)
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("label enums serialize as strings"),
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: usize =
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if current > MIGRATIONS.len() {
        anyhow::bail!(
            "database is at schema {current}, this build only knows {}; refusing to run an older binary against it",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::event::{Heartbeat, ScreenMinute};

    fn minute(device: &str, ts: Millis) -> Envelope {
        Envelope::new(
            device,
            "time-agent",
            ts,
            ts + 60_000,
            Event::ScreenMinute(ScreenMinute::default()),
        )
    }

    #[test]
    fn duplicates_are_counted_not_stored() {
        let s = Store::open_in_memory().unwrap();
        let a = minute("pc", 0);
        assert_eq!(
            s.insert(std::slice::from_ref(&a), 1).unwrap(),
            Inserted {
                new: 1,
                duplicate: 0
            }
        );
        assert_eq!(
            s.insert(&[a, minute("pc", 60_000)], 2).unwrap(),
            Inserted {
                new: 1,
                duplicate: 1
            }
        );
        assert_eq!(s.event_count().unwrap(), 2);
    }

    #[test]
    fn heartbeats_update_the_device_and_never_go_backwards() {
        let s = Store::open_in_memory().unwrap();
        let hb = |ts, pending| {
            Envelope::new(
                "phone",
                "time-android",
                ts,
                ts,
                Event::Heartbeat(Heartbeat {
                    spool_pending: pending,
                    app_version: "1".into(),
                    missing: vec!["health".into()],
                    ..Default::default()
                }),
            )
        };
        s.insert(&[hb(1_000, 5)], 1_000).unwrap();
        // A late, older heartbeat arriving after a newer one changes nothing.
        s.insert(&[hb(2_000, 0)], 2_000).unwrap();
        s.insert(&[hb(1_500, 9)], 3_000).unwrap();
        let d = s.devices().unwrap().remove(0);
        assert_eq!(d.last_heartbeat_ms, Some(2_000));
        assert_eq!(d.spool_pending, Some(0));
        assert_eq!(d.missing, vec!["health".to_string()]);
    }

    #[test]
    fn migrations_are_idempotent_and_refuse_newer_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        Store::open(&path).unwrap();
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap() as usize, MIGRATIONS.len());
        drop(s);
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        drop(conn);
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn events_between_returns_overlaps() {
        let s = Store::open_in_memory().unwrap();
        s.insert(
            &[minute("pc", 0), minute("pc", 60_000), minute("pc", 120_000)],
            0,
        )
        .unwrap();
        let got = s.events_between(60_000, 120_000).unwrap();
        // [0,60k) ends exactly at 60k and counts as touching; [120k,..) starts at the bound and does not.
        assert_eq!(got.len(), 2);
        s.check_writable().unwrap();
    }
}
