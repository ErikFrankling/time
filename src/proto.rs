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
