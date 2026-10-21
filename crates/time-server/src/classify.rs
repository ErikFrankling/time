//! Turning stored events into minute labels, continuously.
//!
//! Any event marks the minutes it touches as dirty; a pass takes the oldest
//! dirty minutes, plus recent minutes still waiting on a late device, and
//! recomputes them from the events. Rules are cheap, so this runs every few
//! seconds on the CPU and there is no switch to turn it off: v1's switch
//! was how classification stopped for two months without anyone noticing.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use time_core::clock::{Millis, MINUTE_MS};
use time_core::rules::{RuleSet, Source};
use time_core::timeline::{label_minutes, DeviceSpec, LOGIC_VERSION};

use crate::config::{Config, DeviceKind};
use crate::store::Store;
use crate::AppState;

/// The rules shipped with Time, used when the config names no file.
pub const DEFAULT_RULES: &str = include_str!("../../../config/rules.toml");

/// Incomplete minutes this recent are retried each pass.
const RECHECK_INCOMPLETE_MS: Millis = 48 * 60 * MINUTE_MS;
/// Minutes per pass. Rules label tens of thousands of minutes a second; this
/// bounds how long a pass holds the database.
pub const PASS_LIMIT: usize = 5_000;
/// A prompt names the project of the minutes up to this long after it.
const LOOKBACK_MS: Millis = 10 * MINUTE_MS;

/// Rules Erik adds from the inbox, kept in the data directory beside the
/// database. They come first, so they win over the shipped rules.
pub const LOCAL_RULES_FILE: &str = "rules.local.toml";

/// The rules in force, swapped whole when Erik adds one.
pub struct Loaded {
    pub rules: RuleSet,
    /// Rules version plus the labelling logic version.
    pub version: String,
}

pub struct Classifier {
    loaded: RwLock<Arc<Loaded>>,
    base_rules: String,
    local_path: PathBuf,
    pub devices: Vec<DeviceSpec>,
    pub every_ms: Millis,
    pub last_run_ms: AtomicI64,
    pub next_run_ms: AtomicI64,
    pub last_labelled: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

impl Classifier {
    pub fn from_config(config: &Config) -> Result<Self> {
        let base_rules = match &config.rules {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading rules {}: {e}", path.display()))?,
            None => DEFAULT_RULES.to_owned(),
        };
        let local_path = config.data_dir.join(LOCAL_RULES_FILE);
        let devices = config
            .devices
            .iter()
            .map(|(name, d)| DeviceSpec {
                name: name.clone(),
                source: match d.kind {
                    DeviceKind::Phone => Source::Phone,
                    DeviceKind::Desktop | DeviceKind::Laptop => Source::Desktop,
                },
            })
            .collect();
        let loaded = Self::load(&base_rules, &local_path)?;
        Ok(Self {
            loaded: RwLock::new(Arc::new(loaded)),
            base_rules,
            local_path,
            devices,
            every_ms: (config.classify_every_secs.max(1) * 1000) as Millis,
            last_run_ms: AtomicI64::new(0),
            next_run_ms: AtomicI64::new(0),
            last_labelled: AtomicU64::new(0),
            last_error: Mutex::new(None),
        })
    }

    fn load(base: &str, local_path: &std::path::Path) -> Result<Loaded> {
        let local = match std::fs::read_to_string(local_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => anyhow::bail!("reading {}: {e}", local_path.display()),
        };
        Self::load_text(base, &local)
    }

    /// The rules in force now.
    pub fn current(&self) -> Arc<Loaded> {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn version(&self) -> String {
        self.current().version.clone()
    }

    /// Add one rule to the local file, prove the result parses, and switch
    /// to it. History is relabelled by [`Self::requeue_if_rules_changed`].
    pub fn add_local_rule(&self, rule_toml: &str) -> Result<()> {
        let mut local = std::fs::read_to_string(&self.local_path).unwrap_or_default();
        local.push_str(rule_toml);
        let candidate = Self::load_text(&self.base_rules, &local)?;
        let tmp = self.local_path.with_extension("toml.tmp");
        std::fs::write(&tmp, &local)?;
        std::fs::rename(&tmp, &self.local_path)?;
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(candidate);
        Ok(())
    }

    fn load_text(base: &str, local: &str) -> Result<Loaded> {
        // TOML arrays of tables concatenate, so local rules simply come first.
        let rules = RuleSet::parse(&format!("{local}\n{base}"))
            .map_err(|e| anyhow::anyhow!("rules: {e:#}"))?;
        Ok(Loaded {
            version: format!("{}.{LOGIC_VERSION}", rules.version),
            rules,
        })
    }

    /// When the rules differ from the ones the stored labels came from,
    /// queue all of history: labels always reflect the current rules.
    pub fn requeue_if_rules_changed(&self, store: &Store) -> Result<Option<u64>> {
        let version = self.version();
        if store.meta("rules_version")?.as_deref() == Some(version.as_str()) {
            return Ok(None);
        }
        let n = store.mark_all_dirty()?;
        store.set_meta("rules_version", &version)?;
        Ok(Some(n))
    }

    /// One pass. Returns how many minutes were labelled.
    pub fn run_once(&self, store: &Store, now: Millis) -> Result<usize> {
        self.last_run_ms.store(now, Relaxed);
        let loaded = self.current();
        let mut todo: BTreeSet<Millis> = store.dirty_batch(PASS_LIMIT)?.into_iter().collect();
        todo.extend(store.incomplete_since(now - RECHECK_INCOMPLETE_MS, PASS_LIMIT)?);
        if todo.is_empty() {
            return Ok(0);
        }
        // Two hours of silence: off or asleep, not late.
        let covered = store.coverage(now, 2 * 60 * MINUTE_MS)?;
        let mut labelled = 0;
        for (a, b) in runs(&todo) {
            let events = store.events_between(a - LOOKBACK_MS, b + MINUTE_MS)?;
            let labels: Vec<_> = label_minutes(
                &loaded.rules,
                &self.devices,
                &events,
                &covered,
                a,
                b + MINUTE_MS,
            )
            .into_iter()
            .filter(|l| todo.contains(&l.ts))
            .collect();
            store.write_labels(&labels, &loaded.version, now)?;
            labelled += labels.len();
        }
        Ok(labelled)
    }
}

/// Contiguous runs of minutes, so each run loads its events once.
fn runs(minutes: &BTreeSet<Millis>) -> Vec<(Millis, Millis)> {
    let mut out: Vec<(Millis, Millis)> = Vec::new();
    for &t in minutes {
        match out.last_mut() {
            Some((_, b)) if t - *b <= MINUTE_MS => *b = t,
            _ => out.push((t, t)),
        }
    }
    out
}

/// Run passes forever on the server's runtime.
pub async fn run_forever(state: Arc<AppState>) {
    loop {
        let st = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            let now = st.clock.now_ms();
            let mut total = 0;
            // Drain a backlog in one go; each pass is bounded.
            loop {
                let n = st.classifier.run_once(&st.store, now)?;
                total += n;
                if n < PASS_LIMIT {
                    break;
                }
            }
            anyhow::Ok(total)
        })
        .await;
        let error = match result {
            Ok(Ok(n)) => {
                state.classifier.last_labelled.store(n as u64, Relaxed);
                None
            }
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = &error {
            eprintln!("classifier: pass failed: {e}");
        }
        *state
            .classifier
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = error;
        let every = state.classifier.every_ms;
        state
            .classifier
            .next_run_ms
            .store(state.clock.now_ms() + every, Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(every as u64)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_parse() {
        let r = RuleSet::parse(DEFAULT_RULES).unwrap();
        assert!(r.rules.len() > 20);
    }

    #[test]
    fn runs_split_on_gaps() {
        let m = |i: i64| i * MINUTE_MS;
        let set: BTreeSet<_> = [m(1), m(2), m(3), m(10), m(11)].into();
        assert_eq!(runs(&set), vec![(m(1), m(3)), (m(10), m(11))]);
    }

    #[test]
    fn a_device_silent_for_hours_is_asleep_not_late() {
        use time_core::event::{Envelope, Event, Heartbeat, ScreenMinute};
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(&format!(
            "data_dir = {:?}\n[devices.pc]\nkind = 'desktop'\n[devices.framework]\nkind = 'laptop'",
            dir.path()
        ))
        .unwrap();
        let c = Classifier::from_config(&config).unwrap();
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000_000 - 1_800_000_000_000 % MINUTE_MS;
        let minute = |t| {
            Envelope::new(
                "pc",
                "time-agent",
                t,
                t + MINUTE_MS,
                Event::ScreenMinute(ScreenMinute {
                    window_class: "kitty".into(),
                    keys: 5,
                    ..Default::default()
                }),
            )
        };
        let beat = |device: &str, t| {
            Envelope::new(
                device,
                "time-agent",
                t,
                t,
                Event::Heartbeat(Heartbeat {
                    covered_until_ms: Some(t),
                    ..Default::default()
                }),
            )
        };
        let t = now - 10 * MINUTE_MS;
        store.insert(&[minute(t), beat("pc", now)], now).unwrap();

        // The laptop reported an hour ago: late, so the minute waits for it.
        store
            .insert(&[beat("framework", now - 60 * MINUTE_MS)], now)
            .unwrap();
        c.run_once(&store, now).unwrap();
        assert!(!store.labels_between(t, t + MINUTE_MS).unwrap()[0].complete);

        // Three days ago instead: asleep, so nothing waits for it.
        let store = Store::open_in_memory().unwrap();
        store
            .insert(
                &[
                    minute(t),
                    beat("pc", now),
                    beat("framework", now - 3 * 24 * 60 * MINUTE_MS),
                ],
                now,
            )
            .unwrap();
        c.run_once(&store, now).unwrap();
        assert!(store.labels_between(t, t + MINUTE_MS).unwrap()[0].complete);
    }
}
