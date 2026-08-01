use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Categories the model must choose from. `other` is appended automatically
    /// if missing -- without a legal escape hatch the model forces bad fits into
    /// real categories, which quietly poisons the pie chart.
    pub categories: Vec<String>,

    /// Window classes/titles that suppress capture entirely (case-insensitive
    /// substring match). These minutes are recorded, but no screenshot is taken
    /// and nothing is sent anywhere.
    #[serde(default = "default_blocklist")]
    pub blocklist: Vec<String>,

    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Width to downscale screenshots to before sending. The cost dial.
    #[serde(default = "default_width")]
    pub width: u32,

    #[serde(default = "default_port")]
    pub port: u16,

    /// dHash Hamming distance below which a minute counts as "screen unchanged".
    #[serde(default = "default_idle_distance")]
    pub idle_distance: u32,
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

fn default_model() -> String {
    "mimo-v2-omni".into()
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

fn default_idle_distance() -> u32 {
    3
}

pub const DEFAULT_CONFIG: &str = r#"# Categories for the daily chart. Edit freely -- this list is the whole point.
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

# Substring match against the active window class and title. Matching windows
# are never screenshotted and never leave the machine.
blocklist = [
  "vaultwarden",
  "bitwarden",
  "keepass",
  "signal",
  "gnome-keyring",
  "polkit",
  "private",
]

model = "mimo-v2-omni"
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"

# Downscale width before sending. Lower = cheaper, less legible small text.
width = 1024

port = 7373
idle_distance = 3
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
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;

        if !cfg.categories.iter().any(|c| c == "other") {
            cfg.categories.push("other".into());
        }
        Ok(cfg)
    }

    pub fn api_key(&self) -> Result<String> {
        // Env var first so it can come from a systemd credential or sops file
        // without ever being written into the config.
        if let Ok(k) = std::env::var("TIME_API_KEY") {
            if !k.trim().is_empty() {
                return Ok(k.trim().to_string());
            }
        }
        let path = key_path()?;
        let k = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no API key: set TIME_API_KEY or write it to {}",
                path.display()
            )
        })?;
        Ok(k.trim().to_string())
    }
}

fn base_dir(kind: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(kind).join("time"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(base_dir(".config")?.join("config.toml"))
}

pub fn key_path() -> Result<PathBuf> {
    Ok(base_dir(".config")?.join("api-key"))
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = base_dir(".local/share")?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("time.db"))
}
