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
    /// Host of the focused browser tab this minute, never a full URL.
    pub domain: Option<String>,
    pub phash: i64,
    pub model: Option<String>,
    pub keys: u32,
    pub mouse: u32,
    pub idle_secs: Option<u32>,
    pub apps: Vec<String>,
    pub workspaces: u16,
    pub classified: bool,
    /// Waiting on the background classifier. Distinct from `!classified`,
    /// which is also true of blocked and idle-skipped minutes -- those were
    /// decided without a model and are finished, not owed an answer.
    pub pending: bool,
    /// Everything active this minute, including the primary category.
    pub tags: Vec<String>,
    /// Where this minute's screenshot lives, relative to the data dir
    /// ("frames/<device>/<ts>.jpg"). None for devices that cannot capture and
    /// for every row from before screenshots were kept.
    pub image_path: Option<String>,
    /// The agent's free-text note about its machine, as it read when the frame
    /// arrived. Kept so a reclassify run can hand the model the same context
    /// the original call had.
    pub note: Option<String>,
    /// Capture was suppressed by the client-side blocklist.
    pub blocked: bool,
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

/// One model call for the audit trail: which minutes went out, what came back
/// (or why nothing did), and what the endpoint said it cost.
#[derive(Debug, Default)]
pub struct LlmCall {
    pub created: i64,
    pub device: String,
    pub ts_from: i64,
    pub ts_to: i64,
    pub n: i64,
    pub model: String,
    pub endpoint: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub raw_response: Option<String>,
    pub error: Option<String>,
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
    pub domain: Option<String>,
}

impl Filter {
    pub fn is_empty_pub(&self) -> bool {
        self.category.is_none()
            && self.device.is_none()
            && self.app.is_none()
            && self.domain.is_none()
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
        if let Some(d) = &self.domain {
            sql.push_str(" AND domain = ?");
            args.push(Box::new(d.clone()));
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
                    keys, mouse, idle_secs, apps, workspaces, classified, tags, domain, \
                    pending, image_path, note, blocked";

impl Db {
    pub fn open() -> Result<Self> {
        Self::open_at(crate::config::db_path()?)
    }

    /// Split from `open` so a test can point at a throwaway file instead of
    /// the real database under $HOME.
    pub fn open_at(path: std::path::PathBuf) -> Result<Self> {
        let conn = Connection::open(path)?;
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
            ("domain", "TEXT"),
            ("pending", "INTEGER NOT NULL DEFAULT 0"),
            ("attempts", "INTEGER NOT NULL DEFAULT 0"),
            ("image_path", "TEXT"),
            ("note", "TEXT"),
            ("blocked", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            let exists: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('minute') WHERE name = ?1")?
                .exists([col])?;
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE minute ADD COLUMN {col} {decl};"))?;
            }
        }

        // Created after the migration loop rather than with the table, because
        // the column it indexes is one of the ones added there. The sweep runs
        // on every start and must not scan the whole history to find a handful
        // of rows.
        conn.execute_batch("CREATE INDEX IF NOT EXISTS minute_pending ON minute(pending, ts);")?;

        // Output, kept well away from the minute table. Different grain (a day,
        // not a minute), different truth (retrospective and re-derivable rather
        // than a one-shot observation), so a collection run can be re-run over
        // the same window as often as it likes without disturbing anything.
        //
        // `source` is part of the key because git and GitHub both count commits
        // and neither is wrong -- only local clones have line counts and private
        // repositories, only GitHub has pull requests and reviews.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS code_day (
                day           TEXT NOT NULL,
                source        TEXT NOT NULL,
                repo          TEXT NOT NULL,
                commits       INTEGER NOT NULL DEFAULT 0,
                added         INTEGER NOT NULL DEFAULT 0,
                removed       INTEGER NOT NULL DEFAULT 0,
                files         INTEGER NOT NULL DEFAULT 0,
                prs_opened    INTEGER NOT NULL DEFAULT 0,
                prs_merged    INTEGER NOT NULL DEFAULT 0,
                issues_opened INTEGER NOT NULL DEFAULT 0,
                issues_closed INTEGER NOT NULL DEFAULT 0,
                reviews       INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day, source, repo)
             );
             CREATE INDEX IF NOT EXISTS code_day_day ON code_day(day);",
        )?;

        // What the agents did, kept as far away from the minute table as the
        // code rows are and for the same reason: re-derivable totals over a
        // whole day, not a one-shot observation of a minute.
        //
        // `tool` is part of the key because three tools count tokens three
        // different ways, and a schema that let them merge would invite exactly
        // the blind sum the collector refuses to do.
        //
        // The minute rows are per (ts, device, tool) rather than resolved down
        // to one row: two tools running in the same minute is the interesting
        // case, not a conflict to be broken.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_day (
                day            TEXT NOT NULL,
                device         TEXT NOT NULL,
                tool           TEXT NOT NULL,
                project        TEXT NOT NULL,
                sessions       INTEGER NOT NULL DEFAULT 0,
                prompts        INTEGER NOT NULL DEFAULT 0,
                prompt_chars   INTEGER NOT NULL DEFAULT 0,
                prompt_p50     INTEGER NOT NULL DEFAULT 0,
                prompt_p90     INTEGER NOT NULL DEFAULT 0,
                tokens_in      INTEGER NOT NULL DEFAULT 0,
                tokens_out     INTEGER NOT NULL DEFAULT 0,
                cache_read     INTEGER NOT NULL DEFAULT 0,
                cache_write    INTEGER NOT NULL DEFAULT 0,
                active_minutes INTEGER NOT NULL DEFAULT 0,
                peak_parallel  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day, device, tool, project)
             );
             CREATE INDEX IF NOT EXISTS agent_day_day ON agent_day(day);
             CREATE TABLE IF NOT EXISTS agent_minute (
                ts       INTEGER NOT NULL,
                device   TEXT NOT NULL,
                tool     TEXT NOT NULL,
                sessions INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (ts, device, tool)
             );
             CREATE INDEX IF NOT EXISTS agent_minute_ts ON agent_minute(ts);",
        )?;

        // The audit trail: one row per model call, raw reply included. Labels
        // used to be the only thing that survived a call; keeping the reply
        // (and the screenshots, under frames/) is what makes it possible to
        // re-judge old minutes with a better model later instead of trusting
        // whatever the model of the day said.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS llm_call (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                created           INTEGER NOT NULL,
                device            TEXT NOT NULL,
                ts_from           INTEGER NOT NULL,
                ts_to             INTEGER NOT NULL,
                n                 INTEGER NOT NULL,
                model             TEXT NOT NULL,
                endpoint          TEXT NOT NULL,
                prompt_tokens     INTEGER,
                completion_tokens INTEGER,
                raw_response      TEXT,
                error             TEXT
             );
             CREATE INDEX IF NOT EXISTS llm_call_created ON llm_call(created);",
        )?;

        // Dry-run reclassifications land here rather than in `minute`, so a
        // backtest of a candidate model can be compared against the live labels
        // without touching them. Keyed per run so several trials coexist.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS minute_trial (
                run_id   TEXT NOT NULL,
                device   TEXT NOT NULL,
                ts       INTEGER NOT NULL,
                category TEXT NOT NULL,
                project  TEXT,
                detail   TEXT,
                tags     TEXT,
                model    TEXT,
                created  INTEGER NOT NULL,
                PRIMARY KEY (run_id, device, ts)
             );",
        )?;
        Ok(Self { conn })
    }

    pub fn insert(&self, m: &Minute) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO minute
               (ts, device, category, project, detail, window, phash, model,
                keys, mouse, idle_secs, apps, workspaces, classified, tags, domain,
                pending, image_path, note, blocked)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,
                     ?18,?19,?20)",
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
                m.domain,
                m.pending as i32,
                m.image_path,
                m.note,
                m.blocked as i32,
            ],
        )?;
        Ok(())
    }

    /// Write the label a background worker eventually came back with.
    ///
    /// An UPDATE rather than an INSERT: ingest already stored everything it
    /// observed about the minute, and re-inserting from the queued job would
    /// overwrite it with a stale copy. Only the judgment changes here.
    pub fn label(
        &self,
        device: &str,
        ts: i64,
        label: &crate::classify::Label,
        model: &str,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE minute SET category = ?3, project = ?4, detail = ?5, tags = ?6,
                    model = ?7, classified = 1, pending = 0
             WHERE device = ?1 AND ts = ?2",
            rusqlite::params![
                device,
                ts,
                label.category,
                label.project,
                label.detail,
                serde_json::to_string(&label.tags).unwrap_or_else(|_| "[]".into()),
                model,
            ],
        )?)
    }

    /// Minutes still owed a label, newest first so a truncated sweep picks up
    /// the ones a person is most likely to be looking at.
    ///
    /// Rows that have already been offered `max_attempts` times are skipped. A
    /// minute the model will not label is not rare -- a truncated array, a
    /// dropped entry, an unparseable answer -- and without this it stays
    /// pending, gets re-sent every sweep, and is billed again every ten minutes
    /// for as long as the process runs.
    pub fn pending_since(
        &self,
        since: i64,
        limit: usize,
        max_attempts: u32,
    ) -> Result<Vec<Minute>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM minute WHERE pending = 1 AND ts >= ?1 \
             AND attempts < ?3 ORDER BY ts DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![since, limit as i64, max_attempts],
            row_to_minute,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record that these minutes were sent to the model and did not come back
    /// with a label. Counted per row rather than per batch: a batch that half
    /// succeeds should not penalise the half that worked.
    pub fn bump_attempts(&self, device: &str, timestamps: &[i64]) -> Result<()> {
        for ts in timestamps {
            self.conn.execute(
                "UPDATE minute SET attempts = attempts + 1 WHERE device = ?1 AND ts = ?2",
                rusqlite::params![device, ts],
            )?;
        }
        Ok(())
    }

    /// Record one model call, verdict or failure, raw reply and all. The
    /// insert must never take the minutes down with it, so callers log a
    /// failure here rather than propagating it.
    pub fn record_llm_call(&self, c: &LlmCall) -> Result<()> {
        self.conn.execute(
            "INSERT INTO llm_call
               (created, device, ts_from, ts_to, n, model, endpoint,
                prompt_tokens, completion_tokens, raw_response, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                c.created,
                c.device,
                c.ts_from,
                c.ts_to,
                c.n,
                c.model,
                c.endpoint,
                c.prompt_tokens,
                c.completion_tokens,
                c.raw_response,
                c.error,
            ],
        )?;
        Ok(())
    }

    /// Store one reclassified minute in the trial table, leaving the live
    /// label alone. Replace-on-conflict so a re-run of the same run_id
    /// converges instead of erroring halfway.
    pub fn put_trial(
        &self,
        run_id: &str,
        device: &str,
        ts: i64,
        label: &crate::classify::Label,
        model: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO minute_trial
               (run_id, device, ts, category, project, detail, tags, model, created)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                run_id,
                device,
                ts,
                label.category,
                label.project,
                label.detail,
                serde_json::to_string(&label.tags).unwrap_or_else(|_| "[]".into()),
                model,
                chrono::Utc::now().timestamp(),
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

    /// Foreground minutes per website host. "Firefox" as an app answers
    /// nothing -- two hours in it could be work or dn.se, and this is the
    /// column that tells them apart.
    ///
    /// Idle minutes are left out. A browser parked on a page overnight is not
    /// eight hours spent on that site, and this list exists to answer "where
    /// did the evening go", which only counts while someone was there.
    pub fn by_domain(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<(String, i64)>> {
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for m in self
            .resolved(from, to, f)?
            .into_iter()
            .filter(|m| m.category != "idle")
        {
            if let Some(d) = m.domain.filter(|d| !d.trim().is_empty()) {
                *counts.entry(d).or_default() += 1;
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
        tags: r
            .get::<_, Option<String>>(14)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        domain: r.get(15).unwrap_or(None),
        pending: r.get::<_, i32>(16).unwrap_or(0) != 0,
        image_path: r.get(17).unwrap_or(None),
        note: r.get(18).unwrap_or(None),
        blocked: r.get::<_, i32>(19).unwrap_or(0) != 0,
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

impl Db {}

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
            let mut with: Vec<String> = m
                .tags
                .iter()
                .filter(|t| **t != m.category)
                .cloned()
                .collect();
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

impl Db {
    /// Rewrite the minutes leading up to a detected absence.
    ///
    /// Idle is only noticed once the timeout expires, so by the time a minute
    /// is marked idle the preceding `idle_secs` were already written as
    /// whatever was last happening. Left alone, every break silently credits
    /// that much phantom work to the last category -- six breaks a day is half
    /// an hour of fiction.
    ///
    /// Only minutes with no input are rewritten: if there were keypresses the
    /// person was demonstrably still there, whatever the screen was doing.
    pub fn backdate_idle(&self, device: &str, until_ts: i64, idle_secs: u32) -> Result<usize> {
        let from = until_ts - (idle_secs as i64).min(3600);
        let mut stmt = self.conn.prepare(
            "UPDATE minute SET category = 'idle', project = NULL,
                    detail = 'backdated: no input before this was noticed'
             WHERE device = ?1 AND ts >= ?2 AND ts < ?3
               AND keys = 0 AND mouse = 0 AND category != 'idle'",
        )?;
        Ok(stmt.execute(rusqlite::params![device, from, until_ts])?)
    }
}

const CODE_COLS: &str = "day, source, repo, commits, added, removed, files, \
                         prs_opened, prs_merged, issues_opened, issues_closed, reviews";

impl Db {
    /// Record a collection run. Replacing rather than adding is what makes a
    /// run idempotent: the collector re-reads a whole window every night, and
    /// yesterday's numbers must not double when it does.
    pub fn put_code(&self, rows: &[crate::code::CodeDay]) -> Result<usize> {
        for r in rows {
            self.conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO code_day ({CODE_COLS}) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"
                ),
                rusqlite::params![
                    r.day,
                    r.source,
                    r.repo,
                    r.commits,
                    r.added,
                    r.removed,
                    r.files,
                    r.prs_opened,
                    r.prs_merged,
                    r.issues_opened,
                    r.issues_closed,
                    r.reviews,
                ],
            )?;
        }
        Ok(rows.len())
    }

    /// Every row between two local dates, inclusive.
    pub fn code_between(&self, from: &str, to: &str) -> Result<Vec<crate::code::CodeDay>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CODE_COLS} FROM code_day WHERE day >= ?1 AND day <= ?2 ORDER BY day, repo"
        ))?;
        let rows = stmt.query_map([from, to], |r| {
            Ok(crate::code::CodeDay {
                day: r.get(0)?,
                source: r.get(1)?,
                repo: r.get(2)?,
                commits: r.get(3)?,
                added: r.get(4)?,
                removed: r.get(5)?,
                files: r.get(6)?,
                prs_opened: r.get(7)?,
                prs_merged: r.get(8)?,
                issues_opened: r.get(9)?,
                issues_closed: r.get(10)?,
                reviews: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

const AGENT_COLS: &str = "day, device, tool, project, sessions, prompts, prompt_chars, \
                          prompt_p50, prompt_p90, tokens_in, tokens_out, cache_read, \
                          cache_write, active_minutes, peak_parallel";

impl Db {
    /// Record an agent-telemetry run. Replacing rather than adding, exactly as
    /// `put_code` does: a nightly job re-reads a window that overlaps the last
    /// one, and re-reading a transcript must not double the tokens it reports.
    pub fn put_agents(&self, r: &crate::proto::AgentReport) -> Result<usize> {
        for d in &r.days {
            self.conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO agent_day ({AGENT_COLS}) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"
                ),
                rusqlite::params![
                    d.day,
                    d.device,
                    d.tool,
                    d.project,
                    d.sessions,
                    d.prompts,
                    d.prompt_chars,
                    d.prompt_p50,
                    d.prompt_p90,
                    d.tokens_in,
                    d.tokens_out,
                    d.cache_read,
                    d.cache_write,
                    d.active_minutes,
                    d.peak_parallel,
                ],
            )?;
        }
        for m in &r.minutes {
            self.conn.execute(
                "INSERT OR REPLACE INTO agent_minute (ts, device, tool, sessions) \
                 VALUES (?1,?2,?3,?4)",
                rusqlite::params![m.ts, m.device, m.tool, m.sessions],
            )?;
        }
        Ok(r.days.len() + r.minutes.len())
    }

    /// Every agent day row between two local dates, inclusive.
    pub fn agent_days(&self, from: &str, to: &str) -> Result<Vec<crate::agents::AgentDay>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {AGENT_COLS} FROM agent_day WHERE day >= ?1 AND day <= ?2 \
             ORDER BY day, tool, project"
        ))?;
        let rows = stmt.query_map([from, to], |r| {
            Ok(crate::agents::AgentDay {
                day: r.get(0)?,
                device: r.get(1)?,
                tool: r.get(2)?,
                project: r.get(3)?,
                sessions: r.get(4)?,
                prompts: r.get(5)?,
                prompt_chars: r.get(6)?,
                prompt_p50: r.get(7)?,
                prompt_p90: r.get(8)?,
                tokens_in: r.get(9)?,
                tokens_out: r.get(10)?,
                cache_read: r.get(11)?,
                cache_write: r.get(12)?,
                active_minutes: r.get(13)?,
                peak_parallel: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Agent minutes in a unix-second range. One row per (minute, device, tool),
    /// so the caller can both count distinct minutes and add up concurrency.
    pub fn agent_minutes(&self, from: i64, to: i64) -> Result<Vec<crate::agents::AgentMinute>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, device, tool, sessions FROM agent_minute \
             WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map([from, to], |r| {
            Ok(crate::agents::AgentMinute {
                ts: r.get(0)?,
                device: r.get(1)?,
                tool: r.get(2)?,
                sessions: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// A run of consecutive minutes in one category.
#[derive(Debug, Clone)]
pub struct Block {
    pub start: i64,
    pub minutes: i64,
    pub category: String,
}

/// What the day looked like as blocks rather than totals.
#[derive(Debug, Default)]
pub struct Focus {
    pub longest: i64,
    pub blocks_25: i64,
    pub blocks_50: i64,
    /// Minutes from the first tracked activity to the start of the first
    /// block of 25+ deep minutes. None if there never was one.
    pub time_to_first_deep: Option<i64>,
    pub switches: i64,
    pub median_block: i64,
}

impl Db {
    /// Merge consecutive minutes of the same category into blocks.
    ///
    /// A single stray minute would otherwise shatter every block, so up to
    /// `tolerance` interrupting minutes are absorbed. Without that the metric
    /// reads as zero forever, which is both false and demoralising.
    pub fn blocks(&self, from: i64, to: i64, f: &Filter, tolerance: i64) -> Result<Vec<Block>> {
        let minutes = self.resolved(from, to, f)?;
        let mut out: Vec<Block> = Vec::new();
        let mut interruptions = 0i64;

        for m in minutes {
            match out.last_mut() {
                Some(b)
                    if b.category == m.category
                        && m.ts - (b.start + b.minutes * 60) <= 60 * (tolerance + 1) =>
                {
                    b.minutes = (m.ts - b.start) / 60 + 1;
                    interruptions = 0;
                }
                Some(b) if interruptions < tolerance && m.ts - (b.start + b.minutes * 60) <= 60 => {
                    // A brief dip into something else: hold the block open.
                    interruptions += 1;
                    let _ = b;
                    out.push(Block {
                        start: m.ts,
                        minutes: 1,
                        category: m.category,
                    });
                }
                _ => {
                    interruptions = 0;
                    out.push(Block {
                        start: m.ts,
                        minutes: 1,
                        category: m.category,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Fragmentation metrics. Two days with identical pie charts can differ
    /// fivefold here, which is the whole reason to compute it.
    pub fn focus(&self, from: i64, to: i64, f: &Filter, deep: &[String]) -> Result<Focus> {
        let blocks = self.blocks(from, to, f, 2)?;
        let real: Vec<&Block> = blocks.iter().filter(|b| b.category != "idle").collect();
        let deep_blocks: Vec<&&Block> = real
            .iter()
            .filter(|b| deep.iter().any(|d| *d == b.category))
            .collect();

        let mut lengths: Vec<i64> = real.iter().map(|b| b.minutes).collect();
        lengths.sort_unstable();

        let first_activity = real.first().map(|b| b.start);
        Ok(Focus {
            longest: deep_blocks.iter().map(|b| b.minutes).max().unwrap_or(0),
            blocks_25: deep_blocks.iter().filter(|b| b.minutes >= 25).count() as i64,
            blocks_50: deep_blocks.iter().filter(|b| b.minutes >= 50).count() as i64,
            time_to_first_deep: deep_blocks
                .iter()
                .find(|b| b.minutes >= 25)
                .zip(first_activity)
                .map(|(b, start)| (b.start - start) / 60),
            switches: real.len().saturating_sub(1) as i64,
            median_block: lengths.get(lengths.len() / 2).copied().unwrap_or(0),
        })
    }

    /// One row per day for the strip chart: (day_start, minute-of-day buckets).
    /// Each bucket is the dominant category in that slice of the day.
    pub fn day_strips(
        &self,
        days: i64,
        bucket_mins: i64,
        f: &Filter,
    ) -> Result<Vec<(i64, Vec<Option<String>>)>> {
        use chrono::{Duration, Local, TimeZone};
        let per_day = (1440 / bucket_mins) as usize;
        let mut out = Vec::new();

        for d in (0..days).rev() {
            let day = Local::now().date_naive() - Duration::days(d);
            let Some(start) = Local
                .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|x| x.timestamp())
            else {
                continue;
            };

            let mut tally: Vec<std::collections::HashMap<String, i64>> =
                vec![Default::default(); per_day];
            for m in self.resolved(start, start + 86_400, f)? {
                let idx = (((m.ts - start) / 60 / bucket_mins) as usize).min(per_day - 1);
                *tally[idx].entry(m.category).or_default() += 1;
            }

            let row = tally
                .into_iter()
                .map(|t| {
                    t.into_iter()
                        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
                        .map(|(c, _)| c)
                })
                .collect();
            out.push((start, row));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("time-db-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn minute(ts: i64) -> Minute {
        Minute {
            ts,
            device: "pc".into(),
            category: "other".into(),
            image_path: Some(format!("frames/pc/{ts}.jpg")),
            note: Some("runs agents".into()),
            blocked: false,
            ..Default::default()
        }
    }

    /// The new columns must round-trip, and -- since they arrive via the
    /// additive ALTER loop -- must survive the database being opened again.
    #[test]
    fn raw_columns_survive_a_reopen() {
        let path = tmp_db("raw-cols");
        {
            let db = Db::open_at(path.clone()).unwrap();
            let mut m = minute(60);
            m.blocked = true;
            db.insert(&m).unwrap();
        }
        let db = Db::open_at(path.clone()).unwrap();
        let m = db.get("pc", 60).unwrap().expect("row still there");
        assert_eq!(m.image_path.as_deref(), Some("frames/pc/60.jpg"));
        assert_eq!(m.note.as_deref(), Some("runs agents"));
        assert!(m.blocked);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn llm_call_and_trial_tables_survive_a_reopen() {
        let path = tmp_db("audit");
        {
            let db = Db::open_at(path.clone()).unwrap();
            db.record_llm_call(&LlmCall {
                created: 1,
                device: "pc".into(),
                ts_from: 60,
                ts_to: 120,
                n: 2,
                model: "time-vision".into(),
                endpoint: "http://local".into(),
                prompt_tokens: Some(1000),
                completion_tokens: Some(200),
                raw_response: Some("[{}]".into()),
                error: None,
            })
            .unwrap();
            let label = crate::classify::Label {
                category: "idle".into(),
                project: None,
                detail: Some("away".into()),
                tags: vec!["idle".into()],
            };
            db.put_trial("run-1", "pc", 60, &label, "time-vision")
                .unwrap();
            // Same key again must replace, not error.
            db.put_trial("run-1", "pc", 60, &label, "time-vision")
                .unwrap();
        }
        let db = Db::open_at(path.clone()).unwrap();
        let calls: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM llm_call", [], |r| r.get(0))
            .unwrap();
        assert_eq!(calls, 1);
        let (trials, cat): (i64, String) = db
            .conn
            .query_row(
                "SELECT COUNT(*), MAX(category) FROM minute_trial WHERE run_id = 'run-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(trials, 1, "replaced, not duplicated");
        assert_eq!(cat, "idle");
        let _ = std::fs::remove_file(&path);
    }
}
