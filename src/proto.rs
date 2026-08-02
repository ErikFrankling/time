use serde::{Deserialize, Serialize};

/// What an agent posts to the server each minute.
///
/// This is the entire client/server contract. The agent contributes facts it
/// can observe cheaply and locally; every judgment about what the minute *was*
/// happens server-side.
#[derive(Debug, Serialize, Deserialize)]
pub struct Frame {
    /// Unix seconds, floored to the minute.
    pub ts: i64,

    /// Which machine this came from.
    pub device: String,

    /// Active window, "class — title".
    pub window: String,

    /// Base64 JPEG of the focused monitor. None when the agent suppressed
    /// capture because the active window matched its blocklist.
    #[serde(default)]
    pub image: Option<String>,

    /// True when capture was suppressed by the blocklist, so the server can
    /// tell "nothing on screen worth sending" apart from "screenshot failed".
    #[serde(default)]
    pub blocked: bool,

    /// Seconds since the last *human* input event on this machine. None when
    /// the agent could not read any input device, which the server must treat
    /// as "unknown" rather than "active" -- silently assuming presence is how
    /// you end up logging an empty room as work.
    #[serde(default)]
    pub idle_secs: Option<u32>,

    /// Human key presses and pointer movements during this minute. Counts only;
    /// which keys were pressed is never captured.
    #[serde(default)]
    pub keys: u32,
    #[serde(default)]
    pub mouse: u32,

    /// Host of the focused browser tab, e.g. "dn.se". Only the host is ever
    /// sent: paths and query strings are where the sensitive half of a URL
    /// lives, and they would aggregate into a list one row long anyway. None
    /// whenever a browser is not in front, or the browser is not reporting.
    #[serde(default)]
    pub domain: Option<String>,

    /// Free-text note about this machine, passed to the model as context.
    #[serde(default)]
    pub note: Option<String>,

    /// Every window class open this minute, deduplicated. The active window
    /// says what is in front; this says what the session is set up to do.
    #[serde(default)]
    pub apps: Vec<String>,

    /// Workspaces in use, a rough measure of how spread out the session is.
    #[serde(default)]
    pub workspaces: u16,
}

/// What a collection run posts to the server.
///
/// Separate from `Frame` because the shape is different in every way that
/// matters: a whole window at once rather than one minute, days rather than
/// seconds, and re-derivable rather than observed-once. The repositories live
/// on the workstation and the database lives in the cluster, so the collector
/// has to come over the wire like everything else.
#[derive(Debug, Serialize, Deserialize)]
pub struct CodeReport {
    pub days: Vec<crate::code::CodeDay>,
}

/// What an agent-telemetry run posts to the server.
///
/// Two grains in one message on purpose: the day rows are the totals worth
/// charting, the minute rows are what lets agent activity be laid over the
/// tracked-time timeline. They are derived from the same read of the same
/// transcripts, so splitting them across two requests could only ever produce
/// a database where one half is newer than the other.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AgentReport {
    pub days: Vec<crate::agents::AgentDay>,
    pub minutes: Vec<crate::agents::AgentMinute>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FrameAck {
    pub category: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// Whether the server spent a model call on this frame.
    pub classified: bool,
}
