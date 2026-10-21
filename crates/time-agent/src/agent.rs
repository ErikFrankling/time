//! One minute of observation, and getting it to the server.

use std::sync::Arc;

use anyhow::Result;
use time_core::clock::{floor_minute, Clock, Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, Heartbeat, ScreenMinute};

use crate::config::AgentConfig;
use crate::probe::{Audio, Desktop, Input, Tabs};
use crate::sender::{Delivery, Sender};
use crate::spool::Spool;

pub const SERVICE: &str = "time-agent";

/// Events per request when draining a backlog.
const BATCH: usize = 500;
/// Requests per tick, so a week-long backlog cannot eat the minute.
const MAX_BATCHES_PER_TICK: usize = 20;

pub struct Agent {
    pub config: AgentConfig,
    clock: Arc<dyn Clock>,
    desktop: Box<dyn Desktop>,
    input: Box<dyn Input>,
    audio: Box<dyn Audio>,
    tabs: Box<dyn Tabs>,
    sender: Box<dyn Sender>,
    spool: Spool,
    /// The last minute already recorded, so a late or doubled tick never
    /// records a minute twice.
    last_minute: Option<Millis>,
    ticks: u64,
    /// Why the compositor could not be read, for the heartbeat.
    desktop_error: Option<String>,
}

/// What one tick did, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub recorded: bool,
    pub sent: usize,
    pub dropped: usize,
    pub pending: usize,
    pub retry: Option<String>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AgentConfig,
        clock: Arc<dyn Clock>,
        desktop: Box<dyn Desktop>,
        input: Box<dyn Input>,
        audio: Box<dyn Audio>,
        tabs: Box<dyn Tabs>,
        sender: Box<dyn Sender>,
    ) -> Result<Self> {
        let spool = Spool::open(&config.spool_dir, 20_000)?;
        Ok(Self {
            config,
            clock,
            desktop,
            input,
            audio,
            tabs,
            sender,
            spool,
            last_minute: None,
            ticks: 0,
            desktop_error: None,
        })
    }

    /// Call once at (or just after) each minute boundary. Records the minute
    /// that just ended, then sends what the spool holds.
    pub fn tick(&mut self) -> Result<TickReport> {
        let now = self.clock.now_ms();
        let minute = floor_minute(now) - MINUTE_MS;
        let mut report = TickReport::default();

        if self.last_minute.is_none_or(|m| minute > m) {
            let event = self.observe();
            self.spool.push(&Envelope::new(
                &self.config.device,
                SERVICE,
                minute,
                minute + MINUTE_MS,
                Event::ScreenMinute(event),
            ))?;
            self.last_minute = Some(minute);
            report.recorded = true;
        }

        if self
            .ticks
            .is_multiple_of(u64::from(self.config.heartbeat_minutes.max(1)))
        {
            let pending = self.spool.len()? as u32;
            let missing = self
                .desktop_error
                .iter()
                .map(|e| format!("compositor: {e}"))
                .chain(self.input.problem())
                .chain(self.audio.problem())
                .chain(self.tabs.problem())
                .collect();
            let hb = Heartbeat {
                spool_pending: pending,
                app_version: env!("CARGO_PKG_VERSION").into(),
                covered_until_ms: self.last_minute.map(|m| m + MINUTE_MS),
                missing,
            };
            self.spool.push(&Envelope::new(
                &self.config.device,
                SERVICE,
                now,
                now,
                Event::Heartbeat(hb),
            ))?;
        }
        self.ticks += 1;

        self.flush(&mut report)?;
        report.pending = self.spool.len()?;
        Ok(report)
    }

    fn observe(&mut self) -> ScreenMinute {
        let input = self.input.take();
        let audio_playing = self.audio.playing();
        let window = match self.desktop.active_window() {
            Ok(w) => {
                self.desktop_error = None;
                w
            }
            Err(e) => {
                self.desktop_error = Some(format!("{e:#}"));
                None
            }
        };
        let mut minute = ScreenMinute {
            keys: input.keys,
            mouse: input.mouse,
            idle_seconds: input.idle_seconds,
            audio_playing,
            ..Default::default()
        };
        let Some(window) = window else {
            return minute;
        };
        let tab = if self.config.is_browser(&window.class) {
            self.tabs.front_tab(&window)
        } else {
            None
        };
        let hay = format!(
            "{} {} {}",
            window.class,
            window.title,
            tab.as_ref().map(|t| t.host.as_str()).unwrap_or("")
        );
        if self.config.is_blocked(&hay) {
            // The title or host may itself be the sensitive part.
            minute.blocked = true;
            return minute;
        }
        minute.window_class = window.class;
        minute.window_title = window.title;
        minute.fullscreen = window.fullscreen;
        minute.browser_host = tab.map(|t| t.host);
        minute
    }

    fn flush(&mut self, report: &mut TickReport) -> Result<()> {
        for _ in 0..MAX_BATCHES_PER_TICK {
            let batch = self.spool.oldest(BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            let (paths, envs): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
            match self.sender.send(&envs) {
                Delivery::Accepted => {
                    self.spool.remove(&paths)?;
                    report.sent += envs.len();
                }
                Delivery::Refused(why) => {
                    eprintln!(
                        "agent: receiver refused {} event(s), dropping them: {why}",
                        envs.len()
                    );
                    self.spool.remove(&paths)?;
                    report.dropped += envs.len();
                }
                Delivery::Retry(why) => {
                    report.retry = Some(why);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
