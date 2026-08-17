use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

/// The agent is deliberately dumb: screenshot, active window, POST. It holds no
/// API key, makes no model calls, and keeps no database. Everything that costs
/// money or needs a secret lives on the server.
#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// Base URL of the server, e.g. "https://time.example.org".
    #[serde(default = "default_server_url")]
    pub server: String,

    /// Name this machine reports as.
    #[serde(default = "default_device")]
    pub device: String,

    /// Width to downscale screenshots to before sending.
    #[serde(default = "default_width")]
    pub width: u32,

    /// Window classes/titles that suppress capture entirely (case-insensitive
    /// substring match). Enforced on the client so a blocked screen is never
    /// encoded, never leaves the machine, and never reaches the server.
    #[serde(default = "default_blocklist")]
    pub blocklist: Vec<String>,

    /// Window classes that count as a browser, matched case-insensitively as
    /// substrings. The browser extension keeps reporting its front tab even
    /// while minimised, so this list is what decides when that tab is what the
    /// person was actually looking at.
    #[serde(default = "default_browsers")]
    pub browsers: Vec<String>,

    /// Free-text note about this machine, handed to the model as context.
    /// Use it for things the screen alone would mislead on, e.g. that this
    /// box runs AI computer-use sessions that repaint the screen unattended.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// Categories the model must choose from. `other` is appended automatically
    /// if missing -- without a legal escape hatch the model forces bad fits into
    /// real categories, which quietly poisons the chart.
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,

    #[serde(default = "default_model")]
    pub model: String,

    /// Model for batches that carry no screenshot -- a phone, or any minute the
    /// sweep picks up after its image is gone. Split from `model` because vision
    /// is the expensive half of the plan and most minutes do not need it: a
    /// text-only model is a third of the input price and a tenth of the output.
    #[serde(default = "default_model_text")]
    pub model_text: String,

    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Output-token budget per minute in a batch. None falls back to a
    /// name-substring guess (reasoning models get more), which is wrong the
    /// moment the endpoint is a local llama-swap alias whose name says nothing
    /// about the model behind it -- this is the override for that.
    #[serde(default)]
    pub max_tokens_per_minute: Option<u32>,

    /// Ask the endpoint to enforce the label schema via an OpenAI-style
    /// `response_format` of type `json_schema`. llama.cpp's llama-server
    /// supports this (it compiles the schema to a grammar); hosted gateways
    /// mostly ignore or reject it, hence off by default. The prompt still
    /// spells out the shape either way, so nothing depends on this being on.
    #[serde(default)]
    pub json_schema: bool,

    /// Whether the model may think before answering. None -- the default --
    /// leaves the model's own default alone; `false` asks the chat template
    /// to skip the reasoning block entirely (llama-server's
    /// `chat_template_kwargs.enable_thinking`). Live classification wants
    /// the thinking: it is what buys the presence judgment (see DESIGN.md).
    /// This switch exists for backfills where the GPU bill outweighs that,
    /// and `time reclassify --no-think` flips it per run.
    #[serde(default)]
    pub thinking: Option<bool>,

    /// Estimated input tokens a batch may reach before the collector closes
    /// it early -- the guard that keeps an image-dense batch inside one
    /// llama-server slot. Deployment, not code, because it tracks the GPU's
    /// context size: with `-c 40960 --parallel 2` (two 20480-token slots)
    /// 13000 input leaves the slot room for thinking and the reply.
    #[serde(default = "default_batch_token_budget")]
    pub batch_token_budget: u32,

    #[serde(default = "default_port")]
    pub port: u16,

    /// dHash Hamming distance below which a minute counts as "screen unchanged".
    /// Seconds without human input after which an unchanged screen counts as
    /// idle outright, instead of carrying the previous label forward.
    #[serde(default = "default_idle_after_secs")]
    pub idle_after_secs: u32,

    #[serde(default = "default_idle_distance")]
    pub idle_distance: u32,

    /// Minutes labelled per model call. One call per minute per device does not
    /// fit a weekly allowance that is shared with real work, and a run of
    /// minutes is also a better prompt than an isolated screenshot: the model
    /// can see that a build started four minutes ago and is still running.
    #[serde(default = "default_batch_minutes")]
    pub batch_minutes: usize,

    /// How long the first minute of a batch waits for company before the call
    /// goes out anyway. Devices report one minute at a time, so this is the
    /// knob that decides whether a batch ever forms -- at the cost of labels
    /// lagging the minute they describe by up to this long.
    #[serde(default = "default_batch_wait_secs")]
    pub batch_wait_secs: u64,

    /// Directories searched for git repositories. The search stops at the first
    /// `.git` on a path, so listing a parent of everything is the intent.
    #[serde(default = "default_code_roots")]
    pub code_roots: Vec<String>,

    /// Substrings matched against commit author name and email to decide which
    /// commits are yours. Empty means "whatever `user.email` each repository is
    /// configured with", which is right on a personal machine.
    #[serde(default)]
    pub code_authors: Vec<String>,

    /// GitHub login, for pull requests, issues and reviews. None disables that
    /// half; the local git scan is unaffected.
    #[serde(default)]
    pub github_user: Option<String>,

    /// How far back a collection run looks. Commits are retrospective and a
    /// laptop may be shut for a week, so a run overlaps well past the last one.
    #[serde(default = "default_code_days")]
    pub code_days: i64,

    /// Which coding agents to read telemetry from. Two of the three sources are
    /// private on-disk formats with no stability promise, so a tool whose parser
    /// has started lying can be dropped from the run here rather than by
    /// shipping a new binary.
    #[serde(default = "default_agent_tools")]
    pub agent_tools: Vec<String>,

    /// How far back an agent run looks. Separate from `code_days` because the
    /// windows cost very different amounts: a month of git log is a few seconds,
    /// a month of transcripts is well over a gigabyte of JSONL.
    #[serde(default = "default_agent_days")]
    pub agent_days: i64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            server: default_server_url(),
            device: default_device(),
            width: default_width(),
            blocklist: default_blocklist(),
            browsers: default_browsers(),
            note: None,
        }
    }
}

impl AgentConfig {
    pub fn is_browser(&self, class: &str) -> bool {
        crate::capture::matches(class, &self.browsers)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            categories: default_categories(),
            model: default_model(),
            model_text: default_model_text(),
            endpoint: default_endpoint(),
            max_tokens_per_minute: None,
            json_schema: false,
            thinking: None,
            batch_token_budget: default_batch_token_budget(),
            port: default_port(),
            idle_after_secs: default_idle_after_secs(),
            idle_distance: default_idle_distance(),
            batch_minutes: default_batch_minutes(),
            batch_wait_secs: default_batch_wait_secs(),
            code_roots: default_code_roots(),
            code_authors: Vec::new(),
            github_user: None,
            code_days: default_code_days(),
            agent_tools: default_agent_tools(),
            agent_days: default_agent_days(),
        }
    }
}

fn default_code_roots() -> Vec<String> {
    vec!["~/projects".into()]
}

fn default_code_days() -> i64 {
    30
}

fn default_agent_tools() -> Vec<String> {
    [
        crate::agents::TOOL_CLAUDE,
        crate::agents::TOOL_OPENCODE,
        crate::agents::TOOL_CODEX,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_agent_days() -> i64 {
    14
}

fn default_server_url() -> String {
    "http://127.0.0.1:7373".into()
}

fn default_device() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn default_blocklist() -> Vec<String> {
    [
        "vaultwarden",
        "bitwarden",
        "keepass",
        "signal",
        "gnome-keyring",
        "polkit",
        "private",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_browsers() -> Vec<String> {
    ["firefox", "zen", "librewolf", "chromium", "chrome", "brave"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_categories() -> Vec<String> {
    [
        "work_neptune",
        "work_husk",
        "work_personal",
        "kth",
        "comms",
        "browsing",
        "youtube",
        "twitter",
        "idle",
        "other",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_model() -> String {
    "qwen3.6-plus".into()
}

/// `deepseek-v4-flash` is text-only, so it can never be reached from a batch
/// carrying a screenshot -- which is what makes it safe to use here at all.
/// The model documented as training on submitted data is the separate
/// `deepseek-v4-flash-free` ID, which the Go plan does not offer; the paid one
/// is zero-retention like the rest of the plan.
fn default_model_text() -> String {
    "deepseek-v4-flash".into()
}

/// The hosted fallback. The intended setup is a local llama-swap
/// (llama.cpp) endpoint on a machine with a GPU -- free, private, and off any
/// usage allowance -- but a fresh install should classify out of the box, and
/// only the hosted endpoint does that without a model download first.
fn default_endpoint() -> String {
    "https://opencode.ai/zen/go/v1/chat/completions".into()
}

/// Measured rather than guessed. 768, 896 and 1024 labelled the same twelve
/// minutes identically over six runs each -- including the minute whose only
/// evidence of a second activity was a video playing in a window the active
/// window title never mentions. The wider frame was paying a third more input
/// tokens for answers that did not change.
fn default_width() -> u32 {
    768
}

fn default_port() -> u16 {
    7373
}

fn default_idle_after_secs() -> u32 {
    300
}

fn default_idle_distance() -> u32 {
    3
}

fn default_batch_minutes() -> usize {
    20
}

/// Sized for a 20480-token llama-server slot: 13000 of input leaves over
/// 7000 for thinking and the reply. Raise it from config when the slots grow.
fn default_batch_token_budget() -> u32 {
    13_000
}

/// Ten minutes. Long enough that a live desktop accumulates a real run, short
/// enough that today's chart is never badly out of date -- and irrelevant to a
/// phone catching up on a week, which fills a batch instantly.
fn default_batch_wait_secs() -> u64 {
    600
}

pub const DEFAULT_CONFIG: &str = r#"# The agent runs on each machine you use. It screenshots, and posts. That's all
# it does -- no API key, no database, no model calls.
[agent]
server = "http://127.0.0.1:7373"
device = "pc"

# Downscale width before sending. The cost dial, and it bottoms out sooner than
# it looks: 768 and 1024 gave identical labels over six runs of the same twelve
# minutes, so the wider frame was a third more input tokens for the same answers.
width = 768

# Substring match against the active window class and title. Enforced here, on
# the client, so a matching screen is never encoded and never leaves the machine.
blocklist = [
  "vaultwarden",
  "bitwarden",
  "keepass",
  "signal",
  "gnome-keyring",
  "polkit",
  "private",
]

# Also matched against the site in the front browser tab, so a bank listed here
# suppresses the screenshot as well as the domain.

# Time per website comes from ActivityWatch's browser extension, which posts the
# front tab to 127.0.0.1:5600 -- the agent answers there. Install it once from
# https://addons.mozilla.org/firefox/addon/aw-watcher-web/ and accept the
# consent prompt. Only the host is ever kept: dn.se, never the article.
#
# The extension reports its tab whether or not the browser is in front, so a
# window class from this list must have focus before the tab counts.
browsers = ["firefox", "zen", "librewolf", "chromium", "chrome", "brave"]

# The server runs in the cluster. It holds the API key, calls the model, owns
# the database, and serves the UI.
[server]

# Categories for the chart. Edit freely -- this list is the whole point.
#
# `idle` and `other` are always available and always render grey; they are
# absence-of-activity rather than things worth telling apart. Everything else
# gets one of 8 distinct, colourblind-checked colours, in the order listed
# here. A 9th real category still works, it just shares the grey -- so keep
# this to 8 if you want the chart to stay readable.
categories = [
  "work_neptune",
  "work_husk",
  "work_personal",
  "kth",
  "comms",
  "browsing",
  "youtube",
  "twitter",
  "idle",
  "other",
]

# Two models, chosen per call by whether the batch carries screenshots.
# `model` must have vision; `model_text` need not, and is much cheaper, which
# matters because the phone -- which cannot screenshot itself at all -- reports
# far more minutes than the desktops do.
#
# Do not point `model_text` at a "-free" model ID. Those are the ones whose
# submitted data may be used for training; the paid IDs are zero-retention.
model = "qwen3.6-plus"
model_text = "deepseek-v4-flash"

# Any OpenAI-compatible chat-completions URL. The default is the hosted
# OpenCode endpoint so a fresh install works out of the box, but the intended
# setup is a local llama-swap (llama.cpp) instance serving a Qwen VL model --
# free, private, and off any usage allowance. With llama-swap, point both
# model names above at your aliases, leave TIME_API_KEY unset, and consider:
#
#   max_tokens_per_minute = 1600   # explicit output budget per minute
#   json_schema = true             # llama-server enforces the label schema
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"
port = 7373
idle_distance = 3

# Seconds without human input after which an unchanged screen is called idle
# outright rather than inheriting the previous label.
idle_after_secs = 300

# Minutes per model call, and how long the first minute of a batch waits for
# the next one. Together these are what a day of tracking costs: 20 minutes in
# one call sends the system prompt once instead of twenty times, and the model
# sees the run rather than twenty unrelated screenshots. Labels lag by up to
# batch_wait_secs; the row is written immediately either way.
batch_minutes = 20
batch_wait_secs = 600

# `time collect` reads how much you actually produced -- commits, diffs, pull
# requests -- and joins it to the timeline. Run it nightly; commit timestamps
# are retrospective, so there is nothing to gain from running it more often.
#
# Repositories are found by walking these roots and stopping at the first .git.
code_roots = ["~/projects"]

# Substrings matched against commit author name and email. Leave empty to use
# whatever user.email each repository is configured with.
code_authors = []

# For pull requests, issues and reviews. The token is never read from this
# file: it comes from TIME_GITHUB_TOKEN, or from `gh auth token` locally.
# github_user = "your-login"

# How far back each run looks. Overlap is free -- rows are replaced, not added.
code_days = 30

# `time agents` reads what the coding agents did -- sessions, prompts, tokens,
# how many ran at once -- from their own records, on the machine they ran on.
# Also nightly, and also idempotent.
#
# Only opencode keeps a real database. claude and codex are read out of private
# on-disk JSONL that upstream can change without notice, so drop a name from
# this list if its numbers start looking wrong.
agent_tools = ["claude", "opencode", "codex"]

# Shorter than code_days on purpose: a month of transcripts is over a gigabyte
# to re-read, where a month of git log is seconds.
agent_days = 14
"#;

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, DEFAULT_CONFIG)?;
            eprintln!("wrote default config to {}", path.display());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if !cfg.server.categories.iter().any(|c| c == "other") {
            cfg.server.categories.push("other".into());
        }
        Ok(cfg)
    }
}

/// Model API key. Server-side only, and never read from the config file -- it
/// comes from the environment so it can be a Kubernetes secret. None when
/// unset or empty, which is the normal state for a local llama-swap endpoint:
/// no key exists, and sending a Bearer header anyway is what some local
/// servers reject.
pub fn api_key() -> Option<String> {
    std::env::var("TIME_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn base_dir(kind: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(kind).join("time"))
}

pub fn config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("TIME_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(base_dir(".config")?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    // In the cluster this is a mounted PVC; locally it's under $HOME.
    let dir = match std::env::var("TIME_DATA_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => base_dir(".local/share")?,
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("time.db"))
}

#[cfg(test)]
mod tests {
    /// One test, not three: they would share the process environment and race.
    #[test]
    fn api_key_is_optional_and_trimmed() {
        std::env::remove_var("TIME_API_KEY");
        assert_eq!(super::api_key(), None);
        std::env::set_var("TIME_API_KEY", "   ");
        assert_eq!(super::api_key(), None, "whitespace-only key is no key");
        std::env::set_var("TIME_API_KEY", " sk-123 ");
        assert_eq!(super::api_key(), Some("sk-123".into()));
        std::env::remove_var("TIME_API_KEY");
    }

    /// `phone_categories` is retired -- the model has full authority now --
    /// but a config file that still carries it must keep parsing.
    #[test]
    fn a_retired_key_does_not_break_an_old_config() {
        let cfg: super::Config =
            toml::from_str("[server]\nphone_categories = [\"com.spotify=music\"]\n").unwrap();
        assert_eq!(cfg.server.port, super::default_port());
    }

    #[test]
    fn new_server_keys_default_off() {
        let cfg: super::Config = toml::from_str("[server]\n").unwrap();
        assert_eq!(cfg.server.max_tokens_per_minute, None);
        assert!(!cfg.server.json_schema);
        assert_eq!(cfg.server.thinking, None, "absent means model default");
        assert_eq!(cfg.server.batch_token_budget, 13_000);
        let cfg: super::Config = toml::from_str(
            "[server]\nmax_tokens_per_minute = 1600\njson_schema = true\n\
             thinking = false\nbatch_token_budget = 27000\n",
        )
        .unwrap();
        assert_eq!(cfg.server.max_tokens_per_minute, Some(1600));
        assert!(cfg.server.json_schema);
        assert_eq!(cfg.server.thinking, Some(false));
        assert_eq!(cfg.server.batch_token_budget, 27_000);
    }
}
