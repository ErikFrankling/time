//! SQLite storage: events, devices, and what is derived from them.
//!
//! One connection behind a mutex is the single writer. SQLite allows one
//! writer at a time anyway, and v1's `SQLITE_BUSY` retries came from having
//! several. Handlers reach it through `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use time_core::clock::Millis;
use time_core::event::{Envelope, Event};

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
];

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
