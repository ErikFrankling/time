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

    /// Free-text note about this machine, passed to the model as context.
    #[serde(default)]
    pub note: Option<String>,
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
