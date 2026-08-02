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

    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_port")]
    pub port: u16,

    /// dHash Hamming distance below which a minute counts as "screen unchanged".
    /// Seconds without human input after which an unchanged screen counts as
    /// idle outright, instead of carrying the previous label forward.
    #[serde(default = "default_idle_after_secs")]
    pub idle_after_secs: u32,

    #[serde(default = "default_idle_distance")]
    pub idle_distance: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            server: default_server_url(),
            device: default_device(),
            width: default_width(),
            blocklist: default_blocklist(),
            note: None,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            categories: default_categories(),
            model: default_model(),
            endpoint: default_endpoint(),
            port: default_port(),
            idle_after_secs: default_idle_after_secs(),
            idle_distance: default_idle_distance(),
        }
    }
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

fn default_endpoint() -> String {
    "https://opencode.ai/zen/go/v1/chat/completions".into()
}

fn default_width() -> u32 {
    1024
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

pub const DEFAULT_CONFIG: &str = r#"# The agent runs on each machine you use. It screenshots, and posts. That's all
# it does -- no API key, no database, no model calls.
[agent]
server = "http://127.0.0.1:7373"
device = "pc"

# Downscale width before sending. The cost dial.
width = 1024

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

model = "qwen3.6-plus"
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"
port = 7373
idle_distance = 3

# Seconds without human input after which an unchanged screen is called idle
# outright rather than inheriting the previous label.
idle_after_secs = 300
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
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if !cfg.server.categories.iter().any(|c| c == "other") {
            cfg.server.categories.push("other".into());
        }
        Ok(cfg)
    }
}

/// Model API key. Server-side only, and never read from the config file -- it
/// comes from the environment so it can be a Kubernetes secret.
pub fn api_key() -> Result<String> {
    let k = std::env::var("TIME_API_KEY")
        .context("TIME_API_KEY is not set (on the cluster this comes from the time-secrets secret)")?;
    let k = k.trim().to_string();
    anyhow::ensure!(!k.is_empty(), "TIME_API_KEY is empty");
    Ok(k)
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
