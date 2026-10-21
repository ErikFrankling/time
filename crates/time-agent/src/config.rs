//! Agent configuration (`~/.config/time/agent.toml`).

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// This machine's name, sent as `host.name`: `pc`, `framework`.
    pub device: String,
    /// OTLP/HTTP logs endpoint. The local Alloy when present, otherwise the
    /// homelab gateway.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Where unsent events wait.
    pub spool_dir: PathBuf,
    /// Case-insensitive substrings of window class, title or browser host.
    /// A match sends a minute marked `blocked` with nothing else in it.
    #[serde(default = "default_blocklist")]
    pub blocklist: Vec<String>,
    /// Window classes that are browsers, whose front tab is looked up.
    #[serde(default = "default_browsers")]
    pub browsers: Vec<String>,
    /// Heartbeat every this many minutes.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_minutes: u32,
}

fn default_endpoint() -> String {
    "http://127.0.0.1:4318/v1/logs".into()
}

fn default_blocklist() -> Vec<String> {
    ["keepassxc", "bitwarden", "1password", "lastpass", "signal"]
        .map(String::from)
        .to_vec()
}

fn default_browsers() -> Vec<String> {
    [
        "firefox",
        "zen",
        "zen-beta",
        "chromium",
        "google-chrome",
        "helium",
        "brave-browser",
    ]
    .map(String::from)
    .to_vec()
}

fn default_heartbeat() -> u32 {
    5
}

impl AgentConfig {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn is_browser(&self, class: &str) -> bool {
        self.browsers.iter().any(|b| b.eq_ignore_ascii_case(class))
    }

    pub fn is_blocked(&self, hay: &str) -> bool {
        let hay = hay.to_lowercase();
        self.blocklist
            .iter()
            .any(|n| !n.is_empty() && hay.contains(&n.to_lowercase()))
    }
}
