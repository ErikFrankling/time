use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Minute {
    pub ts: i64,
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
            "CREATE TABLE IF NOT EXISTS minute (
                ts       INTEGER PRIMARY KEY,
                category TEXT NOT NULL,
                project  TEXT,
                detail   TEXT,
                window   TEXT,
                phash    INTEGER,
                model    TEXT
             );
             CREATE INDEX IF NOT EXISTS minute_category ON minute(category);",
        )?;
        Ok(Self { conn })
    }

    pub fn insert(&self, m: &Minute) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO minute (ts, category, project, detail, window, phash, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![m.ts, m.category, m.project, m.detail, m.window, m.phash, m.model],
        )?;
        Ok(())
    }

    pub fn last(&self) -> Result<Option<Minute>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts, category, project, detail, window, phash, model
                 FROM minute ORDER BY ts DESC LIMIT 1",
                [],
                row_to_minute,
            )
            .optional()?)
    }

    /// All minutes in a half-open [from, to) unix-second range.
    pub fn range(&self, from: i64, to: i64) -> Result<Vec<Minute>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, category, project, detail, window, phash, model
             FROM minute WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map([from, to], row_to_minute)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Minute counts per category for a range, largest first.
    pub fn totals(&self, from: i64, to: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT category, COUNT(*) FROM minute
             WHERE ts >= ?1 AND ts < ?2
             GROUP BY category ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map([from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn row_to_minute(r: &rusqlite::Row) -> rusqlite::Result<Minute> {
    Ok(Minute {
        ts: r.get(0)?,
        category: r.get(1)?,
        project: r.get(2)?,
        detail: r.get(3)?,
        window: r.get(4)?,
        phash: r.get(5).unwrap_or(0),
        model: r.get(6)?,
    })
}
