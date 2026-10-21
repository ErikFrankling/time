//! Every fact Time records, as typed events.
//!
//! An event is the stored truth; minutes and reports are derived from events
//! and can always be recomputed. Each variant maps to one OTel `event.name`,
//! and each field's serde name is its OTel attribute key, so the schema in
//! PROPOSAL.md and this file cannot drift apart: the OTLP codec in
//! [`crate::otlp`] is derived from these definitions.
//!
//! No free text a person wrote is ever a field here. Prompts, transcripts and
//! read-aloud text are reduced to word counts where they are produced.

use serde::{Deserialize, Serialize};

use crate::clock::Millis;

/// Which machine an event describes, as sent in the OTel `host.name`
/// resource attribute: `pc`, `framework`, `phone`.
pub type Device = String;

/// One event plus where and when it happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Idempotency key. A retried or re-synced event carries the same id and
    /// is stored once.
    pub id: String,
    pub device: Device,
    /// The emitting program, from `service.name`.
    pub service: String,
    /// Start of what the event describes. Equal to `end_ms` for instants.
    pub start_ms: Millis,
    pub end_ms: Millis,
    pub event: Event,
}

impl Envelope {
    pub fn new(
        device: &str,
        service: &str,
        start_ms: Millis,
        end_ms: Millis,
        event: Event,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            device: device.to_owned(),
            service: service.to_owned(),
            start_ms,
            end_ms,
            event,
        }
    }

    /// An envelope whose id is derived from its content, for sources that
    /// re-read the same facts on every run (the phone's usage log, git
    /// history): the second read then dedupes against the first.
    pub fn stable(
        device: &str,
        service: &str,
        start_ms: Millis,
        end_ms: Millis,
        event: Event,
    ) -> Self {
        let key = format!("{device}|{service}|{start_ms}|{end_ms}|{}", event.name());
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes()).to_string();
        Self {
            id,
            ..Self::new(device, service, start_ms, end_ms, event)
        }
    }
}

/// Everything Time knows how to record. The tag is the OTel `event.name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event.name")]
pub enum Event {
    /// One minute of a desktop, from the time agent.
    #[serde(rename = "time.screen.minute")]
    ScreenMinute(ScreenMinute),
    /// A stretch with one app in the foreground on the phone.
    #[serde(rename = "time.phone.app")]
    PhoneApp(PhoneApp),
    /// Steps and distance for a stretch of time, from Health Connect.
    #[serde(rename = "time.health.steps")]
    HealthSteps(HealthSteps),
    /// A device saying it is alive and how much it still has to send.
    #[serde(rename = "time.heartbeat")]
    Heartbeat(Heartbeat),
    #[serde(rename = "t3.prompt.sent")]
    PromptSent(PromptSent),
    #[serde(rename = "t3.focus.span")]
    FocusSpan(FocusSpan),
    #[serde(rename = "t3.readaloud.played")]
    ReadAloudPlayed(ReadAloudPlayed),
    #[serde(rename = "t3.voicemode.span")]
    VoiceModeSpan(VoiceModeSpan),
    #[serde(rename = "stt.dictation.finished")]
    DictationFinished(DictationFinished),
    #[serde(rename = "llm.tokens.used")]
    TokensUsed(TokensUsed),
    #[serde(rename = "github.commit")]
    Commit(Commit),
    #[serde(rename = "github.pr")]
    PullRequest(PullRequest),
    #[serde(rename = "github.review")]
    Review(Review),
    #[serde(rename = "time.nudge.sent")]
    NudgeSent(NudgeSent),
    #[serde(rename = "time.nudge.outcome")]
    NudgeOutcome(NudgeOutcome),
}

impl Event {
    /// The OTel `event.name`.
    pub fn name(&self) -> &'static str {
        match self {
            Event::ScreenMinute(_) => "time.screen.minute",
            Event::PhoneApp(_) => "time.phone.app",
            Event::HealthSteps(_) => "time.health.steps",
            Event::Heartbeat(_) => "time.heartbeat",
            Event::PromptSent(_) => "t3.prompt.sent",
            Event::FocusSpan(_) => "t3.focus.span",
            Event::ReadAloudPlayed(_) => "t3.readaloud.played",
            Event::VoiceModeSpan(_) => "t3.voicemode.span",
            Event::DictationFinished(_) => "stt.dictation.finished",
            Event::TokensUsed(_) => "llm.tokens.used",
            Event::Commit(_) => "github.commit",
            Event::PullRequest(_) => "github.pr",
            Event::Review(_) => "github.review",
            Event::NudgeSent(_) => "time.nudge.sent",
            Event::NudgeOutcome(_) => "time.nudge.outcome",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenMinute {
    /// Window class of the focused window; empty when nothing has focus.
    #[serde(rename = "window.class", default)]
    pub window_class: String,
    #[serde(rename = "window.title", default)]
    pub window_title: String,
    /// Host of the front tab of *this* browser, only while a browser is
    /// focused. Never a path.
    #[serde(
        rename = "browser.host",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub browser_host: Option<String>,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(rename = "audio.playing", default)]
    pub audio_playing: bool,
    /// Key presses this minute. Which keys is never recorded.
    #[serde(default)]
    pub keys: u32,
    #[serde(default)]
    pub mouse: u32,
    /// Seconds since the last input at the end of the minute; absent when
    /// the agent could not read input devices, which is "unknown", not
    /// "active".
    #[serde(
        rename = "idle.seconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_seconds: Option<u32>,
    /// The window matched the blocklist: nothing else in this event is real.
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhoneApp {
    #[serde(rename = "app.package")]
    pub package: String,
    #[serde(rename = "app.label", default)]
    pub label: String,
    #[serde(rename = "media.playing", default)]
    pub media_playing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthSteps {
    pub steps: u32,
    #[serde(rename = "distance.meters", default)]
    pub distance_m: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Events waiting on the device to be sent.
    #[serde(rename = "spool.pending", default)]
    pub spool_pending: u32,
    #[serde(rename = "app.version", default)]
    pub app_version: String,
    /// Newest instant the device has fully reported. For the phone this lags
    /// the heartbeat itself, because Android hands out usage history in bulk.
    #[serde(
        rename = "covered.until",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub covered_until_ms: Option<i64>,
    /// Permissions or capabilities that are missing, by name, so the status
    /// screen can say why a channel is quiet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    Typed,
    Dictated,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Agent,
    Scheduled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptSent {
    #[serde(rename = "thread.id")]
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub client: String,
    #[serde(rename = "words.typed", default)]
    pub words_typed: u32,
    #[serde(rename = "words.dictated", default)]
    pub words_dictated: u32,
    pub input_mode: InputMode,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusSpan {
    #[serde(default)]
    pub client: String,
    #[serde(rename = "thread.id")]
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadAloudPlayed {
    #[serde(rename = "thread.id", default)]
    pub thread_id: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub voice: String,
    #[serde(rename = "seconds.played")]
    pub seconds_played: f64,
    #[serde(rename = "words.heard", default)]
    pub words_heard: u32,
    #[serde(default)]
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceModeSpan {
    #[serde(rename = "thread.id", default)]
    pub thread_id: String,
    #[serde(default)]
    pub turns: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DictationFinished {
    #[serde(rename = "recording.id", default)]
    pub recording_id: String,
    pub client: String,
    #[serde(rename = "audio.seconds")]
    pub audio_seconds: f64,
    pub words: u32,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokensUsed {
    #[serde(rename = "gen_ai.provider.name")]
    pub provider: String,
    #[serde(rename = "gen_ai.request.model")]
    pub model: String,
    #[serde(default)]
    pub account: String,
    #[serde(rename = "gen_ai.usage.input_tokens", default)]
    pub input_tokens: u64,
    #[serde(rename = "gen_ai.usage.output_tokens", default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(rename = "thread.id", default)]
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub repo: String,
    /// Full hash; the event id is derived from it, so rescans dedupe.
    #[serde(default)]
    pub sha: String,
    /// The local checkout, so the server can map it to a project.
    #[serde(rename = "repo.path", default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "lines.added", default)]
    pub lines_added: u32,
    #[serde(rename = "lines.removed", default)]
    pub lines_removed: u32,
    #[serde(default)]
    pub files: u32,
}

/// A focus nudge went out (or was attempted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NudgeSent {
    #[serde(rename = "window.minutes")]
    pub window_minutes: u32,
    #[serde(rename = "unproductive.minutes")]
    pub unproductive_minutes: u32,
    #[serde(rename = "productive.minutes")]
    pub productive_minutes: u32,
    #[serde(default)]
    pub top_sources: Vec<String>,
    #[serde(
        rename = "focus.reason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub focus_reason: Option<String>,
    #[serde(rename = "nudges.today")]
    pub nudges_today: u32,
    /// Where it was delivered: `phone`, `agent`, both, or neither.
    #[serde(default)]
    pub delivered: Vec<String>,
}

/// Did the nudge change anything? Recorded ten minutes after it went out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NudgeOutcome {
    #[serde(rename = "nudge.id")]
    pub nudge_id: String,
    pub switched: bool,
    #[serde(rename = "unproductive.minutes.after")]
    pub unproductive_minutes_after: u32,
}

/// A pull request Erik opened or that was merged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: String,
    pub number: u32,
    /// `opened` or `merged`.
    pub action: String,
}

/// A review Erik gave.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub repo: String,
    pub number: u32,
}
