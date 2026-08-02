use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Minute {
    pub ts: i64,
    pub device: String,
    pub category: String,
    pub project: Option<String>,
    pub detail: Option<String>,
    pub window: Option<String>,
    pub phash: i64,
    pub model: Option<String>,
}

impl Db {
    pub fn open() -> Result<Self> {
        let conn = Connection::open(crate::config::db_path()?)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            // Keyed by (device, ts) so several machines can report the same
            // minute without overwriting each other.
            "CREATE TABLE IF NOT EXISTS minute (
                ts       INTEGER NOT NULL,
                device   TEXT NOT NULL,
                category TEXT NOT NULL,
                project  TEXT,
                detail   TEXT,
                window   TEXT,
                phash    INTEGER,
                model    TEXT,
                PRIMARY KEY (device, ts)
             );
             CREATE INDEX IF NOT EXISTS minute_ts ON minute(ts);
             CREATE INDEX IF NOT EXISTS minute_category ON minute(category);",
        )?;
        Ok(Self { conn })
    }

    pub fn insert(&self, m: &Minute) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO minute
               (ts, device, category, project, detail, window, phash, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                m.ts, m.device, m.category, m.project, m.detail, m.window, m.phash, m.model
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, device: &str, ts: i64) -> Result<Option<Minute>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts, device, category, project, detail, window, phash, model
                 FROM minute WHERE device = ?1 AND ts = ?2",
                rusqlite::params![device, ts],
                row_to_minute,
            )
            .optional()?)
    }

    pub fn last(&self, device: &str) -> Result<Option<Minute>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts, device, category, project, detail, window, phash, model
                 FROM minute WHERE device = ?1 ORDER BY ts DESC LIMIT 1",
                [device],
                row_to_minute,
            )
            .optional()?)
    }

    /// All minutes in a half-open [from, to) unix-second range.
    pub fn range(&self, from: i64, to: i64) -> Result<Vec<Minute>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, device, category, project, detail, window, phash, model
             FROM minute WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map([from, to], row_to_minute)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Distinct minutes per category. Counted over distinct timestamps so two
    /// machines reporting the same minute don't inflate the day past 24 hours.
    pub fn totals(&self, from: i64, to: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT category, COUNT(DISTINCT ts) FROM minute
             WHERE ts >= ?1 AND ts < ?2
             GROUP BY category ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn row_to_minute(r: &rusqlite::Row) -> rusqlite::Result<Minute> {
    Ok(Minute {
        ts: r.get(0)?,
        device: r.get(1)?,
        category: r.get(2)?,
        project: r.get(3)?,
        detail: r.get(4)?,
        window: r.get(5)?,
        phash: r.get(6).unwrap_or(0),
        model: r.get(7)?,
    })
}
