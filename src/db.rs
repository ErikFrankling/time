use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, Default)]
pub struct Minute {
    pub ts: i64,
    pub device: String,
    pub category: String,
    pub project: Option<String>,
    pub detail: Option<String>,
    pub window: Option<String>,
    pub phash: i64,
    pub model: Option<String>,
    pub keys: u32,
    pub mouse: u32,
    pub idle_secs: Option<u32>,
    pub apps: Vec<String>,
    pub workspaces: u16,
    pub classified: bool,
    /// Everything active this minute, including the primary category.
    pub tags: Vec<String>,
}

impl Minute {
    /// The active window is stored as "class — title". The class alone is what
    /// aggregates usefully; titles are near-unique and would make a top-apps
    /// list with one entry per row.
    pub fn app(&self) -> Option<&str> {
        self.window
            .as_deref()
            .map(|w| w.split(" — ").next().unwrap_or(w).trim())
            .filter(|s| !s.is_empty())
    }
}

/// Totals over a day, split however the caller asked.
#[derive(Debug, Default)]
pub struct Stats {
    pub tracked: i64,
    pub active: i64,
    pub idle: i64,
    pub classified: i64,
    pub keys: i64,
    pub mouse: i64,
    pub devices: i64,
}

/// Filters applied to every query behind the drill-down.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub category: Option<String>,
    pub device: Option<String>,
    pub app: Option<String>,
}

impl Filter {
    pub fn is_empty_pub(&self) -> bool {
        self.category.is_none() && self.device.is_none() && self.app.is_none()
    }

    /// Build the SQL tail and its bound values together, so a filter can never
    /// be added in one place and forgotten in the other.
    fn sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut sql = String::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(c) = &self.category {
            sql.push_str(" AND category = ?");
            args.push(Box::new(c.clone()));
        }
        if let Some(d) = &self.device {
            sql.push_str(" AND device = ?");
            args.push(Box::new(d.clone()));
        }
        if let Some(a) = &self.app {
            // Anchored to the start so it matches the class, not a title.
            sql.push_str(" AND window LIKE ?");
            args.push(Box::new(format!("{a}%")));
        }
        (sql, args)
    }
}

fn params<'a>(
    base: &'a [&'a dyn rusqlite::ToSql],
    extra: &'a [Box<dyn rusqlite::ToSql>],
) -> Vec<&'a dyn rusqlite::ToSql> {
    let mut v: Vec<&dyn rusqlite::ToSql> = base.to_vec();
    v.extend(extra.iter().map(|b| b.as_ref()));
    v
}

const COLS: &str = "ts, device, category, project, detail, window, phash, model, \
                    keys, mouse, idle_secs, apps, workspaces, classified, tags";

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

        // Added after the first rows existed, so bring old databases forward
        // rather than requiring a wipe. Every one is nullable or defaulted.
        for (col, decl) in [
            ("keys", "INTEGER NOT NULL DEFAULT 0"),
            ("mouse", "INTEGER NOT NULL DEFAULT 0"),
            ("idle_secs", "INTEGER"),
            ("apps", "TEXT"),
            ("workspaces", "INTEGER NOT NULL DEFAULT 0"),
            ("classified", "INTEGER NOT NULL DEFAULT 0"),
            ("tags", "TEXT"),
        ] {
            let exists: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('minute') WHERE name = ?1")?
                .exists([col])?;
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE minute ADD COLUMN {col} {decl};"))?;
            }
        }
        Ok(Self { conn })
    }

    pub fn insert(&self, m: &Minute) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO minute
               (ts, device, category, project, detail, window, phash, model,
                keys, mouse, idle_secs, apps, workspaces, classified, tags)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                m.ts,
                m.device,
                m.category,
                m.project,
                m.detail,
                m.window,
                m.phash,
                m.model,
                m.keys,
                m.mouse,
                m.idle_secs,
                serde_json::to_string(&m.apps).unwrap_or_else(|_| "[]".into()),
                m.workspaces,
                m.classified as i32,
                serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into()),
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, device: &str, ts: i64) -> Result<Option<Minute>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM minute WHERE device = ?1 AND ts = ?2"),
                rusqlite::params![device, ts],
                row_to_minute,
            )
            .optional()?)
    }

    pub fn last(&self, device: &str) -> Result<Option<Minute>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM minute WHERE device = ?1 ORDER BY ts DESC LIMIT 1"),
                [device],
                row_to_minute,
            )
            .optional()?)
    }

    pub fn range(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<Minute>> {
        let (tail, args) = f.sql();
        let sql = format!("SELECT {COLS} FROM minute WHERE ts >= ? AND ts < ?{tail} ORDER BY ts");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params(&[&from as &dyn rusqlite::ToSql, &to], &args).as_slice(),
            row_to_minute,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One row per minute, choosing between machines that disagree.
    ///
    /// You are one person with several computers. If the laptop says browsing
    /// while the desktop sits idle, you were browsing -- the idle machine is
    /// not a second thing you were doing, it is the absence of you. Counting
    /// both inflates the day past wall-clock and buries real activity under
    /// idle time from whatever you happened to leave switched on.
    ///
    /// Precedence: real human input wins, because that is where you physically
    /// were. Failing that, any activity beats idle. Ties break on device name
    /// so the same minute always resolves the same way.
    pub fn resolved(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<Minute>> {
        let rows = self.range(from, to, f)?;
        let mut by_ts: std::collections::BTreeMap<i64, Minute> = Default::default();
        for m in rows {
            match by_ts.get(&m.ts) {
                None => {
                    by_ts.insert(m.ts, m);
                }
                Some(cur) => {
                    // Union the tags before choosing a winner: doing
                    // one thing on each machine is still two things
                    // happening, even though only one is the primary.
                    let mut merged = cur.tags.clone();
                    merged.extend(m.tags.iter().cloned());
                    merged.sort();
                    merged.dedup();
                    let win = beats(&m, cur);
                    let mut keep = if win { m } else { cur.clone() };
                    keep.tags = merged;
                    by_ts.insert(keep.ts, keep);
                }
            }
        }
        Ok(by_ts.into_values().collect())
    }

    /// Minutes per category, after resolving machines that disagree.
    pub fn by_category(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            *counts.entry(m.category).or_default() += 1;
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }

    /// Which machine the person was actually at, minute by minute --
    /// not how long each box was merely powered on and reporting.
    pub fn by_device(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            *counts.entry(m.device).or_default() += 1;
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }


    /// Foreground minutes per application, derived from the active window
    /// class. This is time actually spent in an app, not merely having it open.
    pub fn by_app(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            if let Some(app) = m.app() {
                *counts.entry(app.to_string()).or_default() += 1;
            }
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }

    /// Minutes each app was *open*, whether or not it had focus. Answers "what
    /// was running", which the foreground list cannot.
    pub fn open_apps(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            for app in m.apps {
                *counts.entry(app).or_default() += 1;
            }
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }

    pub fn by_project(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            if let Some(p) = m.project.filter(|p| !p.trim().is_empty()) {
                *counts.entry(p).or_default() += 1;
            }
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }

    /// Day totals over resolved minutes, so several machines cannot
    /// push a day past wall-clock. Keys and pointer events are summed
    /// across every device, since those are raw activity counts rather
    /// than time and do not double-count.
    pub fn stats(&self, from: i64, to: i64, f: &Filter) -> Result<Stats> {
        let resolved = self.resolved(from, to, f)?;
        let raw = self.range(from, to, f)?;
        let devices: std::collections::HashSet<&str> =
            raw.iter().map(|m| m.device.as_str()).collect();
        Ok(Stats {
            tracked: resolved.len() as i64,
            active: resolved.iter().filter(|m| m.keys + m.mouse > 0).count() as i64,
            idle: resolved.iter().filter(|m| m.category == "idle").count() as i64,
            classified: raw.iter().filter(|m| m.classified).count() as i64,
            keys: raw.iter().map(|m| m.keys as i64).sum(),
            mouse: raw.iter().map(|m| m.mouse as i64).sum(),
            devices: devices.len() as i64,
        })
    }

    /// Minutes per (hour, category) for the stacked hourly chart, plus input
    /// counts per hour for the activity sparkline.
    pub fn hourly(
        &self,
        from: i64,
        to: i64,
        f: &Filter,
    ) -> Result<(Vec<Vec<(String, i64)>>, Vec<(i64, i64)>)> {
        let mut buckets: Vec<Vec<(String, i64)>> = vec![Vec::new(); 24];
        let mut input = vec![(0i64, 0i64); 24];
        for m in self.resolved(from, to, f)? {
            let h = (((m.ts - from) / 3600).clamp(0, 23)) as usize;
            let slot = &mut buckets[h];
            match slot.iter_mut().find(|(c, _)| *c == m.category) {
                Some(e) => e.1 += 1,
                None => slot.push((m.category.clone(), 1)),
            }
            input[h].0 += m.keys as i64;
            input[h].1 += m.mouse as i64;
        }
        Ok((buckets, input))
    }


    pub fn all_devices(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT device FROM minute ORDER BY 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn row_to_minute(r: &rusqlite::Row) -> rusqlite::Result<Minute> {
    let apps: Option<String> = r.get(11).unwrap_or(None);
    Ok(Minute {
        ts: r.get(0)?,
        device: r.get(1)?,
        category: r.get(2)?,
        project: r.get(3)?,
        detail: r.get(4)?,
        window: r.get(5)?,
        phash: r.get(6).unwrap_or(0),
        model: r.get(7)?,
        keys: r.get(8).unwrap_or(0),
        mouse: r.get(9).unwrap_or(0),
        idle_secs: r.get(10).unwrap_or(None),
        apps: apps
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        workspaces: r.get(12).unwrap_or(0),
        classified: r.get::<_, i32>(13).unwrap_or(0) != 0,
        tags: r.get::<_, Option<String>>(14).ok().flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
    })
}

/// True when `a` better represents what the person was doing than `b`.
fn beats(a: &Minute, b: &Minute) -> bool {
    let (ai, bi) = (a.keys + a.mouse, b.keys + b.mouse);
    // Input is the strongest evidence of where the person physically is.
    if ai != bi {
        return ai > bi;
    }
    let (aidle, bidle) = (a.category == "idle", b.category == "idle");
    if aidle != bidle {
        return bidle;
    }
    // Same standing: keep it deterministic so a minute never flickers.
    a.device < b.device
}

impl Db {
}

impl Db {
    /// Two-level breakdown for the sunburst: each primary category, and within
    /// it what else was going on at the same time.
    ///
    /// Each minute lands in exactly one inner slice and exactly one outer
    /// segment, so the rings line up. A minute with several companions becomes
    /// one combined segment ("youtube + music") rather than being counted
    /// twice, which would make the outer ring wider than its parent and the
    /// chart a lie.
    pub fn layered(
        &self,
        from: i64,
        to: i64,
        f: &Filter,
    ) -> Result<Vec<(String, i64, Vec<(Vec<String>, i64)>)>> {
        let mut outer: std::collections::HashMap<(String, Vec<String>), i64> = Default::default();
        let mut totals: std::collections::HashMap<String, i64> = Default::default();
        for m in self.resolved(from, to, f)? {
            *totals.entry(m.category.clone()).or_default() += 1;
            let mut with: Vec<String> =
                m.tags.iter().filter(|t| **t != m.category).cloned().collect();
            with.sort();
            *outer.entry((m.category.clone(), with)).or_default() += 1;
        }

        let mut cats: Vec<(String, i64)> = totals.into_iter().collect();
        cats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        Ok(cats
            .into_iter()
            .map(|(cat, total)| {
                let mut segs: Vec<(Vec<String>, i64)> = outer
                    .iter()
                    .filter(|((c, _), _)| *c == cat)
                    .map(|((_, w), n)| (w.clone(), *n))
                    .collect();
                // Undivided time first, then companions by size.
                segs.sort_by(|a, b| {
                    a.0.is_empty()
                        .cmp(&b.0.is_empty())
                        .reverse()
                        .then(b.1.cmp(&a.1))
                });
                (cat, total, segs)
            })
            .collect())
    }
}
