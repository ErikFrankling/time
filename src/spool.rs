use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::config;
use crate::proto::Frame;

/// How long a minute is worth keeping while the server refuses to take it.
///
/// Seven days is what the phone can already promise -- Android keeps roughly a
/// week of usage events -- so the two clients lose their history at the same
/// point, and a chart that is missing a day is missing it from both. It is also
/// past the longest the laptop is realistically shut without being opened.
const MAX_AGE_SECS: i64 = 7 * 24 * 3600;

/// A hard stop on how much disk an outage may cost.
///
/// A spooled frame is metadata only (see `push`), which is a few hundred bytes;
/// 32 MiB is therefore on the order of 60,000 minutes, or six weeks of
/// continuous failure. So this budget never binds during a normal outage --
/// `MAX_AGE_SECS` gets there first -- and that is deliberate. It exists to bound
/// the damage from a pathological frame: a runaway process with a megabyte of
/// window title, a session with a thousand open apps. Sizing it to bind on
/// ordinary outages would mean throwing away minutes we could easily have kept.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// A frame the server has not yet admitted to having.
#[derive(Debug, Clone)]
pub struct Entry {
    pub ts: i64,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Frames written to disk before they are sent and deleted only once the server
/// says it has them.
///
/// A directory of one JSON file per minute, rather than a queue crate or a
/// second SQLite database. The access pattern is the whole argument: append one
/// small record a minute, read them back in timestamp order, delete the ones
/// that landed. A directory does all three natively -- ordering is a filename
/// sort, deletion is `unlink`, atomicity is `rename`, and a crash can only ever
/// leave a stray `.tmp` that nothing reads. A database would add a schema, a
/// migration, and a corruption-recovery path to buy transactions this has no
/// use for, and the result would no longer be something `ls` can explain.
pub struct Spool {
    dir: PathBuf,
    max_bytes: u64,
    max_age: i64,
}

impl Spool {
    pub fn open() -> Result<Self> {
        Self::at(config::data_dir()?.join("spool"))
    }

    pub fn at(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating spool at {}", dir.display()))?;
        Ok(Self {
            dir,
            max_bytes: MAX_BYTES,
            max_age: MAX_AGE_SECS,
        })
    }

    /// Smaller caps, so eviction can be tested without writing 32 MiB.
    #[cfg(test)]
    fn caps(mut self, max_bytes: u64, max_age: i64) -> Self {
        self.max_bytes = max_bytes;
        self.max_age = max_age;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record a frame on disk, without its screenshot.
    ///
    /// The pixels are dropped on purpose. `DESIGN.md` promises a screenshot is
    /// never written to disk longer than the moment between capture and the API
    /// call, and the blocklist exists because that promise is load-bearing; a
    /// spool sitting on hours of screenshots in `$HOME` would quietly retract
    /// it. It is also what makes the size cap stop mattering: metadata is a few
    /// hundred bytes against ~150 KB for a JPEG, so a week-long outage costs
    /// single-digit megabytes and evicts nothing. Spooling the pixels would mean
    /// discarding whole minutes to make room for them -- which is the bug this
    /// file exists to fix, reintroduced one level down.
    ///
    /// What a stranded minute loses is precision, not existence. The server
    /// already treats an image-less frame as a first-class case, because it is
    /// the only kind the phone can send: it classifies from the window, the open
    /// apps, the domain and the previous label. Minutes sent while the server is
    /// reachable still carry their screenshot -- only the ones that were
    /// actually stranded are degraded.
    pub fn push(&self, frame: &Frame) -> Result<Entry> {
        let mut v = serde_json::to_value(frame)?;
        v["image"] = serde_json::Value::Null;
        let body = serde_json::to_vec(&v)?;

        let path = self.path(frame.ts);
        // Written under a temporary name and renamed into place. A kill halfway
        // through a write then leaves a `.tmp` nothing looks at, rather than a
        // truncated frame the drain would have to reason about.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;

        Ok(Entry {
            ts: frame.ts,
            path,
            bytes: body.len() as u64,
        })
    }

    /// Everything still owed to the server, oldest first.
    ///
    /// The order is not cosmetic. Each minute is classified with the previous
    /// one as context, so a backlog that arrives shuffled is a backlog that is
    /// labelled wrong.
    pub fn pending(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)
            .with_context(|| format!("reading {}", self.dir.display()))?
        {
            let e = e?;
            let name = e.file_name();
            let Some(ts) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|s| s.parse::<i64>().ok())
            else {
                continue;
            };
            out.push(Entry {
                ts,
                path: e.path(),
                bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
        out.sort_by_key(|e| e.ts);
        Ok(out)
    }

    pub fn remove(&self, e: &Entry) {
        let _ = std::fs::remove_file(&e.path);
    }

    /// Enforce both caps, oldest first, and say what went.
    ///
    /// Loudly, because a spool that silently stops keeping things is
    /// indistinguishable from one that is working.
    pub fn evict(&self, now: i64) -> Result<usize> {
        let pending = self.pending()?;
        let mut total: u64 = pending.iter().map(|e| e.bytes).sum();
        let floor = now - self.max_age;

        let mut dropped = Vec::new();
        for e in &pending {
            let too_old = e.ts < floor;
            let too_big = total > self.max_bytes;
            if !too_old && !too_big {
                break;
            }
            total -= e.bytes;
            self.remove(e);
            dropped.push((e.ts, too_old));
        }

        if let (Some(first), Some(last)) = (dropped.first(), dropped.last()) {
            let why = if dropped.iter().all(|(_, old)| *old) {
                format!("older than {} days", self.max_age / 86_400)
            } else {
                format!("over the {} MiB cap", self.max_bytes / 1024 / 1024)
            };
            eprintln!(
                "spool: dropped {} minute(s) {why} — {} .. {}",
                dropped.len(),
                stamp(first.0),
                stamp(last.0)
            );
        }
        Ok(dropped.len())
    }

    fn path(&self, ts: i64) -> PathBuf {
        // Zero-padded so a lexical directory listing is already chronological,
        // and wide enough that it stays that way past the year 5000.
        self.dir.join(format!("{ts:011}.json"))
    }
}

fn stamp(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts: i64) -> Frame {
        Frame {
            ts,
            device: "pc".into(),
            window: "kitty — nvim".into(),
            domain: None,
            image: Some("AAAA".repeat(50_000)),
            blocked: false,
            idle_secs: Some(0),
            keys: 12,
            mouse: 3,
            note: None,
            apps: vec!["kitty".into()],
            workspaces: 2,
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("time-spool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn drains_oldest_first_regardless_of_write_order() {
        let s = Spool::at(tmpdir("order")).unwrap();
        for ts in [180, 60, 120, 0] {
            s.push(&frame(ts)).unwrap();
        }
        let got: Vec<i64> = s.pending().unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, vec![0, 60, 120, 180]);
    }

    #[test]
    fn the_screenshot_never_reaches_the_disk() {
        let s = Spool::at(tmpdir("pixels")).unwrap();
        let e = s.push(&frame(60)).unwrap();
        let text = std::fs::read_to_string(&e.path).unwrap();
        assert!(!text.contains("AAAA"), "spooled frame still carries pixels");
        // Everything the server can still classify from must survive.
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["image"], serde_json::Value::Null);
        assert_eq!(v["window"], "kitty — nvim");
        assert_eq!(v["keys"], 12);
        assert!(text.len() < 1000, "spooled frame is {} bytes", text.len());
    }

    #[test]
    fn age_eviction_takes_the_oldest_and_leaves_the_rest() {
        let s = Spool::at(tmpdir("age")).unwrap();
        let now = 10 * 24 * 3600;
        for ts in [60, 120, now - 60, now] {
            s.push(&frame(ts)).unwrap();
        }
        assert_eq!(s.evict(now).unwrap(), 2);
        let got: Vec<i64> = s.pending().unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, vec![now - 60, now]);
    }

    #[test]
    fn the_byte_cap_evicts_the_oldest_and_keeps_the_newest() {
        // Equal-length timestamps so "three frames" is an exact byte figure.
        let all = [100_060, 100_120, 100_180, 100_240];
        let one = Spool::at(tmpdir("measure"))
            .unwrap()
            .push(&frame(all[0]))
            .unwrap()
            .bytes;

        // A budget of three against four written: the oldest has to go, and
        // only the oldest.
        let s = Spool::at(tmpdir("bytes")).unwrap().caps(one * 3, MAX_AGE_SECS);
        for ts in all {
            s.push(&frame(ts)).unwrap();
        }
        assert_eq!(s.evict(all[3]).unwrap(), 1);
        let got: Vec<i64> = s.pending().unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, all[1..]);
    }

    #[test]
    fn a_survivor_is_readable_after_the_process_is_gone() {
        let dir = tmpdir("restart");
        {
            let s = Spool::at(&dir).unwrap();
            s.push(&frame(60)).unwrap();
            s.push(&frame(120)).unwrap();
        }
        // A fresh Spool over the same directory is what a restart looks like.
        let s = Spool::at(&dir).unwrap();
        assert_eq!(s.pending().unwrap().len(), 2);
        let body = std::fs::read(&s.pending().unwrap()[0].path).unwrap();
        let f: Frame = serde_json::from_slice(&body).unwrap();
        assert_eq!(f.ts, 60);
        assert_eq!(f.window, "kitty — nvim");
    }

    #[test]
    fn a_half_written_frame_is_not_offered_to_the_server() {
        let s = Spool::at(tmpdir("torn")).unwrap();
        s.push(&frame(60)).unwrap();
        std::fs::write(s.dir().join("00000000120.tmp"), b"{\"ts\":120,").unwrap();
        let got: Vec<i64> = s.pending().unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, vec![60]);
    }
}
