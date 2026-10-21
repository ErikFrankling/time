//! Events wait on disk until the server has them.
//!
//! One file per event, named so that a directory listing is oldest-first.
//! An event is written before any attempt to send it and deleted only after
//! the receiver accepted it, so a crash, a reboot or a week without the VPN
//! loses nothing up to the cap.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use time_core::event::Envelope;

pub struct Spool {
    dir: PathBuf,
    /// Oldest events are evicted beyond this many, loudly. 20k is two weeks
    /// of minutes from one desktop.
    cap: usize,
}

impl Spool {
    pub fn open(dir: &Path, cap: usize) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating spool {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_owned(),
            cap,
        })
    }

    pub fn push(&self, env: &Envelope) -> Result<()> {
        let name = format!("{:015}-{}.json", env.start_ms.max(0), env.id);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        fs::write(&tmp, serde_json::to_vec(env)?)?;
        // Rename is atomic: a reader never sees half an event.
        fs::rename(&tmp, self.dir.join(name))?;
        self.evict()
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        Ok(files)
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.files()?.len())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The oldest `n` events. A file that no longer parses is moved aside
    /// rather than blocking everything behind it forever.
    pub fn oldest(&self, n: usize) -> Result<Vec<(PathBuf, Envelope)>> {
        let mut out = Vec::new();
        for path in self.files()?.into_iter().take(n) {
            match fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok(serde_json::from_slice(&b)?))
            {
                Ok(env) => out.push((path, env)),
                Err(e) => {
                    eprintln!("spool: setting aside unreadable {}: {e:#}", path.display());
                    let _ = fs::rename(&path, path.with_extension("bad"));
                }
            }
        }
        Ok(out)
    }

    pub fn remove(&self, paths: &[PathBuf]) -> Result<()> {
        for p in paths {
            match fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn evict(&self) -> Result<()> {
        let files = self.files()?;
        if files.len() > self.cap {
            let excess = &files[..files.len() - self.cap];
            eprintln!(
                "spool: over {} events, dropping the {} oldest",
                self.cap,
                excess.len()
            );
            self.remove(excess)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::event::{Event, Heartbeat};

    fn ev(ts: i64) -> Envelope {
        Envelope::new(
            "pc",
            "time-agent",
            ts,
            ts,
            Event::Heartbeat(Heartbeat::default()),
        )
    }

    #[test]
    fn oldest_first_cap_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::open(dir.path(), 3).unwrap();
        for ts in [5, 1, 4, 2] {
            s.push(&ev(ts)).unwrap();
        }
        // Capped at 3: ts=1 evicted.
        let got: Vec<i64> = s
            .oldest(10)
            .unwrap()
            .into_iter()
            .map(|(_, e)| e.start_ms)
            .collect();
        assert_eq!(got, vec![2, 4, 5]);

        std::fs::write(dir.path().join("000000000000000-zzz.json"), b"not json").unwrap();
        let got = s.oldest(10).unwrap();
        assert_eq!(
            got.len(),
            3,
            "the bad file is set aside, the rest still come out"
        );
        assert!(dir.path().join("000000000000000-zzz.bad").exists());

        let paths: Vec<_> = got.into_iter().map(|(p, _)| p).collect();
        s.remove(&paths).unwrap();
        assert!(s.is_empty().unwrap());
    }
}
