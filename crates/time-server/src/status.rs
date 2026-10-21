//! "Is it working?": the status screen's data, also the source of `/metrics`.
//!
//! v1's worst failure was silent: classification was switched off for two
//! months and nothing on screen said so. Every state here therefore has a
//! word for it, including "off" for things switched off on purpose.

use serde::Serialize;
use time_core::clock::{Millis, MINUTE_MS};

use crate::config::DeviceKind;
use crate::AppState;

const HOUR_MS: Millis = 60 * MINUTE_MS;

/// A device counts as connected with a heartbeat this recent. The phone syncs
/// every 15 minutes; 20 leaves room for one slow sync.
pub const CONNECTED_WITHIN_MS: Millis = 20 * MINUTE_MS;
pub const LATE_WITHIN_MS: Millis = 2 * HOUR_MS;

/// One signal Time records, and how often it should be heard from while the
/// thing it measures is happening.
pub struct Channel {
    pub id: &'static str,
    pub label: &'static str,
    pub event: &'static str,
    /// Silence longer than this, while devices are connected, is "quiet".
    pub live_within_ms: Millis,
}

pub const CHANNELS: &[Channel] = &[
    Channel {
        id: "screen",
        label: "Screen and typing (pc, laptop)",
        event: "time.screen.minute",
        live_within_ms: 5 * MINUTE_MS,
    },
    Channel {
        id: "phone_apps",
        label: "Phone apps",
        event: "time.phone.app",
        live_within_ms: 45 * MINUTE_MS,
    },
    Channel {
        id: "health",
        label: "Steps and distance",
        event: "time.health.steps",
        live_within_ms: 2 * HOUR_MS,
    },
    Channel {
        id: "prompts",
        label: "T3 Code prompts",
        event: "t3.prompt.sent",
        live_within_ms: HOUR_MS,
    },
    Channel {
        id: "focus",
        label: "T3 Code focused thread",
        event: "t3.focus.span",
        live_within_ms: HOUR_MS,
    },
    Channel {
        id: "dictation",
        label: "Dictation",
        event: "stt.dictation.finished",
        live_within_ms: 4 * HOUR_MS,
    },
    Channel {
        id: "read_aloud",
        label: "Read-aloud listening",
        event: "t3.readaloud.played",
        live_within_ms: 4 * HOUR_MS,
    },
    Channel {
        id: "voice_mode",
        label: "Voice mode",
        event: "t3.voicemode.span",
        live_within_ms: 24 * HOUR_MS,
    },
    Channel {
        id: "tokens",
        label: "Token usage",
        event: "llm.tokens.used",
        live_within_ms: HOUR_MS,
    },
    Channel {
        id: "code",
        label: "Commits (GitHub, local git)",
        event: "github.commit",
        live_within_ms: 24 * HOUR_MS,
    },
];

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Connected,
    Late,
    Offline,
    /// Configured but never heard from.
    Never,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    /// Heard from within its expected interval.
    Live,
    /// Heard from in the last day, not recently.
    Quiet,
    /// Heard from before, but not in a day: something broke.
    Missing,
    /// Never heard from: no source sends this yet.
    NotSetUp,
    /// Switched off in config, on purpose.
    Off,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub now_ms: Millis,
    pub devices: Vec<DeviceStatus>,
    pub channels: Vec<ChannelStatus>,
    pub pipeline: Pipeline,
    pub system: System,
}

#[derive(Debug, Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub kind: Option<DeviceKind>,
    pub state: DeviceState,
    pub last_heartbeat_ms: Option<Millis>,
    pub last_event_ms: Option<Millis>,
    /// How far behind real time the device's reporting is.
    pub behind_ms: Option<Millis>,
    pub spool_pending: Option<u32>,
    pub app_version: Option<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ChannelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub state: ChannelState,
    pub last_ms: Option<Millis>,
    pub last_hour: u64,
}

#[derive(Debug, Serialize)]
pub struct Pipeline {
    pub events_total: u64,
    pub accepted: u64,
    pub duplicate: u64,
    pub rejected: u64,
    pub classifier: ClassifierStatus,
}

#[derive(Debug, Serialize)]
pub struct ClassifierStatus {
    pub every_secs: i64,
    pub last_run_ms: Option<Millis>,
    pub next_run_ms: Option<Millis>,
    /// Minutes queued for (re)labelling, and the oldest of them.
    pub waiting: u64,
    pub oldest_waiting_ms: Option<Millis>,
    pub labelled_total: u64,
    pub last_24h_minutes: u64,
    pub last_24h_unknown: u64,
    /// Labelled, but a device had not reported yet; relabelled when it does.
    pub last_24h_incomplete: u64,
    pub rules_version: String,
    pub rules: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct System {
    pub version: &'static str,
    pub schema: u32,
    pub db_bytes: u64,
    pub started_ms: Millis,
}

pub fn device_state(now: Millis, last_heartbeat: Option<Millis>) -> DeviceState {
    match last_heartbeat {
        None => DeviceState::Never,
        Some(t) if now - t <= CONNECTED_WITHIN_MS => DeviceState::Connected,
        Some(t) if now - t <= LATE_WITHIN_MS => DeviceState::Late,
        Some(_) => DeviceState::Offline,
    }
}

pub fn channel_state(
    now: Millis,
    last: Option<Millis>,
    live_within: Millis,
    off: bool,
) -> ChannelState {
    if off {
        return ChannelState::Off;
    }
    match last {
        Some(t) if now - t <= live_within => ChannelState::Live,
        Some(t) if now - t <= 24 * HOUR_MS => ChannelState::Quiet,
        Some(_) => ChannelState::Missing,
        None => ChannelState::NotSetUp,
    }
}

pub fn collect(state: &AppState) -> anyhow::Result<Status> {
    use std::sync::atomic::Ordering::Relaxed;

    let now = state.clock.now_ms();
    let rows = state.store.devices()?;

    let mut devices: Vec<DeviceStatus> = Vec::new();
    for (name, cfg) in &state.config.devices {
        let row = rows.iter().find(|r| &r.name == name);
        devices.push(device_status(now, name, Some(cfg.kind), row));
    }
    for row in rows
        .iter()
        .filter(|r| !state.config.devices.contains_key(&r.name))
    {
        devices.push(device_status(now, &row.name, None, Some(row)));
    }

    let activity = state.store.channel_activity(now - HOUR_MS)?;
    let channels = CHANNELS
        .iter()
        .map(|c| {
            let (last_ms, last_hour) = activity
                .iter()
                .find(|(name, _, _)| name == c.event)
                .map(|(_, last, n)| (*last, *n))
                .unwrap_or((None, 0));
            let off = state.config.channels_off.iter().any(|o| o == c.id);
            ChannelStatus {
                id: c.id,
                label: c.label,
                state: channel_state(now, last_ms, c.live_within_ms, off),
                last_ms,
                last_hour,
            }
        })
        .collect();

    Ok(Status {
        now_ms: now,
        devices,
        channels,
        pipeline: Pipeline {
            events_total: state.store.event_count()?,
            accepted: state.counters.records_accepted.load(Relaxed),
            duplicate: state.counters.records_duplicate.load(Relaxed),
            rejected: state.counters.records_rejected.load(Relaxed),
            classifier: classifier_status(state, now)?,
        },
        system: System {
            version: env!("CARGO_PKG_VERSION"),
            schema: state.store.schema_version()?,
            db_bytes: state.store.size_bytes()?,
            started_ms: state.started_ms,
        },
    })
}

fn classifier_status(state: &AppState, now: Millis) -> anyhow::Result<ClassifierStatus> {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &state.classifier;
    let (waiting, oldest_waiting_ms) = state.store.dirty_stats()?;
    let stats = state.store.label_stats(now - 24 * HOUR_MS)?;
    let at = |v: Millis| (v > 0).then_some(v);
    Ok(ClassifierStatus {
        every_secs: c.every_ms / 1000,
        last_run_ms: at(c.last_run_ms.load(Relaxed)),
        next_run_ms: at(c.next_run_ms.load(Relaxed)),
        waiting,
        oldest_waiting_ms,
        labelled_total: stats.total,
        last_24h_minutes: stats.recent,
        last_24h_unknown: stats.unknown,
        last_24h_incomplete: stats.incomplete,
        rules_version: c.rules.version.clone(),
        rules: c.rules.rules.len(),
        last_error: c
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    })
}

fn device_status(
    now: Millis,
    name: &str,
    kind: Option<DeviceKind>,
    row: Option<&crate::store::DeviceRow>,
) -> DeviceStatus {
    let hb = row.and_then(|r| r.last_heartbeat_ms);
    let covered = row.and_then(|r| r.covered_until_ms.or(r.last_event_ms));
    DeviceStatus {
        name: name.to_owned(),
        kind,
        state: device_state(now, hb),
        last_heartbeat_ms: hb,
        last_event_ms: row.and_then(|r| r.last_event_ms),
        behind_ms: covered.map(|c| (now - c).max(0)),
        spool_pending: row.and_then(|r| r.spool_pending),
        app_version: row.and_then(|r| r.app_version.clone()),
        missing: row.map(|r| r.missing.clone()).unwrap_or_default(),
    }
}

/// Prometheus text exposition of the same numbers.
pub fn metrics(status: &Status) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# HELP time_device_last_heartbeat_timestamp_seconds Newest time.heartbeat from each device.");
    let _ = writeln!(
        out,
        "# TYPE time_device_last_heartbeat_timestamp_seconds gauge"
    );
    for d in &status.devices {
        if let Some(t) = d.last_heartbeat_ms {
            let _ = writeln!(
                out,
                "time_device_last_heartbeat_timestamp_seconds{{device=\"{}\"}} {}",
                esc(&d.name),
                t / 1000
            );
        }
    }
    let _ = writeln!(
        out,
        "# HELP time_device_spool_pending Events a device reports it has not sent yet."
    );
    let _ = writeln!(out, "# TYPE time_device_spool_pending gauge");
    for d in &status.devices {
        if let Some(p) = d.spool_pending {
            let _ = writeln!(
                out,
                "time_device_spool_pending{{device=\"{}\"}} {p}",
                esc(&d.name)
            );
        }
    }
    let _ = writeln!(
        out,
        "# HELP time_last_event_timestamp_seconds Newest arrival per channel."
    );
    let _ = writeln!(out, "# TYPE time_last_event_timestamp_seconds gauge");
    for c in &status.channels {
        if let Some(t) = c.last_ms {
            let _ = writeln!(
                out,
                "time_last_event_timestamp_seconds{{channel=\"{}\"}} {}",
                c.id,
                t / 1000
            );
        }
    }
    let _ = writeln!(
        out,
        "# HELP time_events_ingested_total Log records by outcome since start."
    );
    let _ = writeln!(out, "# TYPE time_events_ingested_total counter");
    for (outcome, n) in [
        ("accepted", status.pipeline.accepted),
        ("duplicate", status.pipeline.duplicate),
        ("rejected", status.pipeline.rejected),
    ] {
        let _ = writeln!(
            out,
            "time_events_ingested_total{{outcome=\"{outcome}\"}} {n}"
        );
    }
    let c = &status.pipeline.classifier;
    let _ = writeln!(
        out,
        "# HELP time_classifier_waiting_minutes Minutes queued for labelling."
    );
    let _ = writeln!(out, "# TYPE time_classifier_waiting_minutes gauge");
    let _ = writeln!(out, "time_classifier_waiting_minutes {}", c.waiting);
    if let Some(t) = c.last_run_ms {
        let _ = writeln!(
            out,
            "# TYPE time_classifier_last_run_timestamp_seconds gauge"
        );
        let _ = writeln!(
            out,
            "time_classifier_last_run_timestamp_seconds {}",
            t / 1000
        );
    }
    let _ = writeln!(out, "# TYPE time_minutes_unknown_24h gauge");
    let _ = writeln!(out, "time_minutes_unknown_24h {}", c.last_24h_unknown);
    let _ = writeln!(out, "# TYPE time_db_bytes gauge");
    let _ = writeln!(out, "time_db_bytes {}", status.system.db_bytes);
    out
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_states_have_sharp_edges() {
        let now = 10 * HOUR_MS;
        assert_eq!(device_state(now, None), DeviceState::Never);
        assert_eq!(
            device_state(now, Some(now - CONNECTED_WITHIN_MS)),
            DeviceState::Connected
        );
        assert_eq!(
            device_state(now, Some(now - CONNECTED_WITHIN_MS - 1)),
            DeviceState::Late
        );
        assert_eq!(
            device_state(now, Some(now - LATE_WITHIN_MS - 1)),
            DeviceState::Offline
        );
    }

    #[test]
    fn off_beats_everything() {
        assert_eq!(channel_state(0, Some(0), 1, true), ChannelState::Off);
        assert_eq!(channel_state(0, None, 1, false), ChannelState::NotSetUp);
        assert_eq!(
            channel_state(100 * HOUR_MS, Some(0), 1, false),
            ChannelState::Missing
        );
    }
}
