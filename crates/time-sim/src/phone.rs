//! A simulated phone that sends exactly what the Android app sends.
//!
//! The real app reads Android's usage log in bulk at each sync, so a span
//! only exists once it has ended and arrives up to a sync interval late --
//! or hours late after an outage. This reproduces that, including the
//! app's habit of re-reading the same history (which stable event ids must
//! absorb).

use time_agent::sender::{Delivery, Sender};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::{Envelope, Event, HealthSteps, Heartbeat, PhoneApp};

use crate::scenario::Resolved;

pub const DEVICE: &str = "phone";
pub const SERVICE: &str = "time-android";
pub const SYNC_EVERY: Millis = 15 * MINUTE_MS;

pub struct PhoneSim {
    sender: Box<dyn Sender>,
    /// The foreground app's span still in progress: (package, label, media, start).
    open: Option<(String, String, bool, Millis)>,
    /// Events read but not yet acknowledged.
    pending: Vec<Envelope>,
    /// Steps in the current hour, flushed at the hour boundary.
    hour_steps: (Millis, u32, f64),
    last_sync: Option<Millis>,
    /// What the last successful sync sent, re-sent once on the next sync the
    /// way a re-read of the usage log would.
    last_sent: Vec<Envelope>,
    pub reread: bool,
    /// Reported in every heartbeat, as the real app reports missing grants.
    pub missing: Vec<String>,
    pub syncs: u32,
}

impl PhoneSim {
    pub fn new(sender: Box<dyn Sender>) -> Self {
        Self {
            sender,
            open: None,
            pending: Vec::new(),
            hour_steps: (0, 0, 0.0),
            last_sync: None,
            last_sent: Vec::new(),
            reread: true,
            missing: Vec::new(),
            syncs: 0,
        }
    }

    /// Advance through the minute starting at `t`, then sync if it is time
    /// and the phone is online at the end of the minute.
    pub fn minute(&mut self, scenario: &Resolved, t: Millis) {
        let now = scenario
            .phone_at(t)
            .map(|s| (s.package.clone(), s.label.clone(), s.media));
        let same = match (&self.open, &now) {
            (Some((p, _, m, _)), Some((q, _, n))) => p == q && m == n,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.close(t);
            if let Some((pkg, label, media)) = now {
                self.open = Some((pkg, label, media, t));
            }
        }

        let hour = t - t.rem_euclid(60 * MINUTE_MS);
        if hour != self.hour_steps.0 {
            self.flush_steps();
            self.hour_steps = (hour, 0, 0.0);
        }
        let (s, m) = scenario.steps_in_minute(t);
        self.hour_steps.1 += s;
        self.hour_steps.2 += m;

        let end = t + MINUTE_MS;
        let due = self.last_sync.is_none_or(|l| end - l >= SYNC_EVERY);
        if due && !scenario.offline(DEVICE, t) {
            self.sync(end);
        }
    }

    fn close(&mut self, at: Millis) {
        if let Some((package, label, media_playing, start)) = self.open.take() {
            self.pending.push(Envelope::stable(
                DEVICE,
                SERVICE,
                start,
                at,
                Event::PhoneApp(PhoneApp {
                    package,
                    label,
                    media_playing,
                }),
            ));
        }
    }

    fn flush_steps(&mut self) {
        let (hour, steps, meters) = self.hour_steps;
        if steps > 0 {
            self.pending.push(Envelope::stable(
                DEVICE,
                SERVICE,
                hour,
                hour + 60 * MINUTE_MS,
                Event::HealthSteps(HealthSteps {
                    steps,
                    distance_m: meters,
                }),
            ));
        }
    }

    /// Everything not yet acknowledged, plus a heartbeat.
    pub fn sync(&mut self, now: Millis) {
        let covered = self.open.as_ref().map(|o| o.3).unwrap_or(now);
        let mut batch = self.pending.clone();
        if self.reread {
            batch.extend(self.last_sent.iter().cloned());
        }
        batch.push(Envelope::new(
            DEVICE,
            SERVICE,
            now,
            now,
            Event::Heartbeat(Heartbeat {
                // Everything pending rides in this batch: nothing is left behind.
                spool_pending: 0,
                app_version: "sim".into(),
                covered_until_ms: Some(covered),
                missing: self.missing.clone(),
            }),
        ));
        match self.sender.send(&batch) {
            Delivery::Accepted | Delivery::Refused(_) => {
                self.last_sent = std::mem::take(&mut self.pending);
                self.last_sync = Some(now);
                self.syncs += 1;
            }
            Delivery::Retry(_) => {}
        }
    }

    /// End of the simulation: close the open span and send what is left, as
    /// the app would on its next sync.
    pub fn finish(&mut self, now: Millis) {
        self.close(now);
        self.flush_steps();
        self.hour_steps = (0, 0, 0.0);
        self.sync(now);
    }
}
