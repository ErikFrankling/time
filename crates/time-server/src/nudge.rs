//! Focus nudges, delivered: the rule from `time_core::nudge` applied to the
//! stored timeline every few minutes, sent to Erik's phone (ntfy) and to his
//! personal agent (a webhook), and recorded as events so their effect can be
//! measured.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use chrono::{Datelike, TimeZone, Timelike};
use time_core::clock::{floor_minute, Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, NudgeOutcome, NudgeSent, Origin};
use time_core::nudge::{self, Focus, NudgeConfig};

use crate::store::Store;
use crate::AppState;

pub const SERVICE: &str = "time-server";
pub const DEVICE: &str = "server";
/// Ten minutes after a nudge, did the unproductive streak stop?
const OUTCOME_AFTER_MS: Millis = 10 * MINUTE_MS;

/// Somewhere a nudge goes.
pub trait Sink: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(&self, title: &str, body: &str, facts: &serde_json::Value) -> Result<()>;
}

/// Erik's phone, through the homelab ntfy server (k8s secret `time-ntfy`).
pub struct Ntfy {
    pub url: String,
    pub topic: String,
    pub token: String,
    /// What a tap opens: the day view.
    pub click: String,
}

impl Sink for Ntfy {
    fn name(&self) -> &'static str {
        "phone"
    }

    fn send(&self, title: &str, body: &str, _facts: &serde_json::Value) -> Result<()> {
        let payload = serde_json::json!({
            "topic": self.topic,
            "title": title,
            "message": body,
            // Normal: a sound, no pop-over. Nudges must never look like alerts.
            "priority": 3,
            "tags": ["time"],
            "click": self.click,
            // A follow-up replaces the earlier nudge instead of stacking.
            "sequence_id": "time-focus-nudge",
        });
        let resp = client()?
            .post(format!("{}/", self.url.trim_end_matches('/')))
            .bearer_auth(&self.token)
            .json(&payload)
            .send()
            .context("ntfy")?;
        anyhow::ensure!(resp.status().is_success(), "ntfy: HTTP {}", resp.status());
        Ok(())
    }
}

/// The personal agent: a T3 webhook that starts a run in its thread. Facts
/// only; the agent decides what to say.
pub struct Webhook {
    pub url: String,
}

impl Sink for Webhook {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn send(&self, title: &str, body: &str, facts: &serde_json::Value) -> Result<()> {
        let payload = serde_json::json!({ "kind": "time.focus_nudge", "title": title, "message": body, "facts": facts });
        let resp = client()?
            .post(&self.url)
            .json(&payload)
            .send()
            .context("agent webhook")?;
        anyhow::ensure!(
            resp.status().is_success(),
            "agent webhook: HTTP {}",
            resp.status()
        );
        Ok(())
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?)
}

pub struct Nudger {
    pub config: NudgeConfig,
    pub sinks: RwLock<Vec<Arc<dyn Sink>>>,
    pub last_check_ms: AtomicI64,
    /// Why the last check did not nudge, in words, for the status screen.
    pub last_result: Mutex<String>,
}

impl Nudger {
    /// Sinks from the environment: `NTFY_URL`, `NTFY_TOPIC`, `NTFY_TOKEN`
    /// (the `time-ntfy` secret) and `TIME_AGENT_WEBHOOK`. None configured is
    /// allowed and shown on the status screen.
    pub fn from_env(config: NudgeConfig, public_url: &str) -> Self {
        let mut sinks: Vec<Arc<dyn Sink>> = Vec::new();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let (Some(url), Some(token)) = (env("NTFY_URL"), env("NTFY_TOKEN")) {
            let topic = env("NTFY_TOPIC").unwrap_or_else(|| "erik".into());
            sinks.push(Arc::new(Ntfy {
                url,
                topic,
                token,
                click: public_url.to_owned(),
            }));
        }
        if let Some(url) = env("TIME_AGENT_WEBHOOK") {
            sinks.push(Arc::new(Webhook { url }));
        }
        Self {
            config,
            sinks: RwLock::new(sinks),
            last_check_ms: AtomicI64::new(0),
            last_result: Mutex::new("not checked yet".into()),
        }
    }

    pub fn add_sink(&self, sink: Arc<dyn Sink>) {
        self.sinks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(sink);
    }

    pub fn sink_names(&self) -> Vec<&'static str> {
        self.sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| s.name())
            .collect()
    }

    fn set_result(&self, s: impl Into<String>) {
        *self.last_result.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
    }
}

pub fn focus(store: &Store, now: Millis) -> Result<Option<Focus>> {
    Ok(match store.meta("focus")? {
        Some(json) => serde_json::from_str::<Focus>(&json)
            .ok()
            .filter(|f| f.until_ms > now),
        None => None,
    })
}

/// One check. Returns the nudge event if one was sent.
pub fn check(state: &AppState, now: Millis) -> Result<Option<Envelope>> {
    let n = &state.nudger;
    n.last_check_ms.store(now, Relaxed);
    record_outcomes(state, now)?;

    let cfg = &n.config;
    let focus = focus(&state.store, now)?;
    let to = floor_minute(now);
    let from = to - i64::from(cfg.window_minutes) * MINUTE_MS;
    let labels = state.store.labels_between(from, to)?;
    let events = state.store.events_between(from, to)?;
    let signals: BTreeSet<Millis> = events
        .iter()
        .filter(|e| match &e.event {
            Event::PromptSent(p) => p.origin == Origin::Human,
            Event::DictationFinished(_) | Event::Commit(_) => true,
            _ => false,
        })
        .map(|e| floor_minute(e.start_ms))
        .collect();
    let a = nudge::assess(cfg, focus.as_ref(), &labels, &signals);
    if !a.over {
        n.set_result(format!(
            "no nudge: {} unproductive and {} productive minutes in the last {}",
            a.unproductive_minutes,
            a.productive_minutes,
            nudge::minutes_text(a.window_minutes)
        ));
        return Ok(None);
    }

    let local = state
        .tz
        .timestamp_millis_opt(now)
        .single()
        .context("local time")?;
    let (day_start, _) = crate::day::bounds(state.tz, local.date_naive())?;
    let sent: Vec<Envelope> = state
        .store
        .events_between(day_start, now + 1)?
        .into_iter()
        .filter(|e| matches!(e.event, Event::NudgeSent(_)))
        .collect();
    let last = sent.iter().map(|e| e.start_ms).max();
    if let Err(hold) = nudge::may_send(
        cfg,
        focus.as_ref(),
        now,
        local.hour(),
        local.weekday().num_days_from_monday(),
        last,
        sent.len() as u32,
    ) {
        n.set_result(format!(
            "would nudge ({} unproductive minutes) but held: {hold:?}",
            a.unproductive_minutes
        ));
        return Ok(None);
    }

    let sources = top_sources(&labels, &events, cfg);
    let (title, body) = nudge::message(&a, &sources, focus.as_ref());
    let facts = serde_json::json!({
        "window_minutes": a.window_minutes,
        "unproductive_minutes": a.unproductive_minutes,
        "productive_minutes": a.productive_minutes,
        "top_sources": sources,
        "focus": focus,
        "nudges_today": sent.len() + 1,
        "day_view": format!("{}day/{}", state.config.public_url, local.date_naive()),
    });
    let mut delivered = Vec::new();
    let sinks: Vec<Arc<dyn Sink>> = n.sinks.read().unwrap_or_else(|e| e.into_inner()).clone();
    for s in &sinks {
        match s.send(&title, &body, &facts) {
            Ok(()) => delivered.push(s.name().to_owned()),
            Err(e) => eprintln!("nudge: {} failed: {e:#}", s.name()),
        }
    }
    let env = Envelope::new(
        DEVICE,
        SERVICE,
        now,
        now,
        Event::NudgeSent(NudgeSent {
            window_minutes: a.window_minutes,
            unproductive_minutes: a.unproductive_minutes,
            productive_minutes: a.productive_minutes,
            top_sources: sources,
            focus_reason: focus.map(|f| f.reason),
            nudges_today: sent.len() as u32 + 1,
            delivered: delivered.clone(),
        }),
    );
    state.store.insert(std::slice::from_ref(&env), now)?;
    n.set_result(format!(
        "nudged: \"{title}\" → {}",
        if delivered.is_empty() {
            "nowhere (no sink configured)".into()
        } else {
            delivered.join(", ")
        }
    ));
    Ok(Some(env))
}

/// The two biggest unproductive sources in the window, by name: the site,
/// the app, or the window class.
fn top_sources(
    labels: &[time_core::label::MinuteLabel],
    events: &[Envelope],
    cfg: &NudgeConfig,
) -> Vec<String> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for l in labels
        .iter()
        .filter(|l| l.activity.is_some_and(|a| cfg.unproductive.contains(&a)))
    {
        let name = events.iter().find_map(|e| {
            if l.device.as_deref() != Some(e.device.as_str())
                || e.start_ms > l.ts
                || e.end_ms <= l.ts
            {
                return None;
            }
            match &e.event {
                Event::ScreenMinute(m) => Some(
                    m.browser_host
                        .clone()
                        .map(|h| pretty_host(&h))
                        .unwrap_or_else(|| m.window_class.clone()),
                ),
                Event::PhoneApp(a) => Some(if a.label.is_empty() {
                    a.package.clone()
                } else {
                    a.label.clone()
                }),
                _ => None,
            }
        });
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(2).map(|(n, _)| n).collect()
}

fn pretty_host(h: &str) -> String {
    match h {
        "youtube.com" | "m.youtube.com" | "youtu.be" => "YouTube".into(),
        "x.com" | "twitter.com" => "X".into(),
        "reddit.com" => "Reddit".into(),
        "netflix.com" => "Netflix".into(),
        "play.hbomax.com" => "HBO Max".into(),
        "disneyplus.com" => "Disney+".into(),
        other => other.to_owned(),
    }
}

/// For each nudge at least ten minutes old without an outcome: did the
/// unproductive minutes stop?
fn record_outcomes(state: &AppState, now: Millis) -> Result<()> {
    let since = now - 24 * 60 * MINUTE_MS;
    let events = state.store.events_between(since, now + 1)?;
    let done: BTreeSet<&str> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::NudgeOutcome(o) => Some(o.nudge_id.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for e in events
        .iter()
        .filter(|e| matches!(e.event, Event::NudgeSent(_)))
    {
        if done.contains(e.id.as_str()) || now - e.start_ms < OUTCOME_AFTER_MS {
            continue;
        }
        let after = state.store.labels_between(
            floor_minute(e.start_ms),
            floor_minute(e.start_ms) + OUTCOME_AFTER_MS,
        )?;
        let unproductive = after
            .iter()
            .filter(|l| {
                l.activity
                    .is_some_and(|a| state.nudger.config.unproductive.contains(&a))
            })
            .count() as u32;
        out.push(Envelope::new(
            DEVICE,
            SERVICE,
            e.start_ms + OUTCOME_AFTER_MS,
            e.start_ms + OUTCOME_AFTER_MS,
            Event::NudgeOutcome(NudgeOutcome {
                nudge_id: e.id.clone(),
                switched: unproductive < 3,
                unproductive_minutes_after: unproductive,
            }),
        ));
    }
    if !out.is_empty() {
        state.store.insert(&out, now)?;
    }
    Ok(())
}

/// Check every five minutes, forever.
pub async fn run_forever(state: Arc<AppState>) {
    loop {
        let st = state.clone();
        let r = tokio::task::spawn_blocking(move || check(&st, st.clock.now_ms())).await;
        match r {
            Ok(Err(e)) => state.nudger.set_result(format!("check failed: {e:#}")),
            Err(e) => state.nudger.set_result(format!("check failed: {e}")),
            Ok(Ok(_)) => {}
        }
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
}

/// Activities counted as unproductive, for the status screen.
pub fn unproductive_names(cfg: &NudgeConfig) -> String {
    cfg.unproductive
        .iter()
        .map(|a| a.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
