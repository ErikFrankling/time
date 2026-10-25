//! Server configuration, read once at start from a TOML file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use time_core::label::Activity;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the database lives.
    pub data_dir: PathBuf,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Every device Time expects to hear from. A device not listed here is
    /// still stored, but the status screen only judges the ones it knows.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
    /// Channels switched off on purpose. The status screen shows them as
    /// "off" in words, never as merely quiet.
    #[serde(default)]
    pub channels_off: Vec<String>,
    /// v1's database: imported once, when this one has no events yet.
    #[serde(default)]
    pub import_v1_from: Option<PathBuf>,
    /// A rules file; the rules shipped with Time when absent.
    #[serde(default)]
    pub rules: Option<PathBuf>,
    /// GitHub `owner/repo` whose releases carry the Android APK served at `/app`.
    #[serde(default = "default_apk_repo")]
    pub apk_repo: String,
    /// Serve this APK file as is instead of fetching releases.
    #[serde(default)]
    pub apk_path: Option<PathBuf>,
    /// Where the website is reached; nudges link to it.
    #[serde(default = "default_public_url")]
    pub public_url: String,
    /// Networks that name a place (home, KTH). Defaults: home by the home
    /// router, KTH by eduroam or the kth.se domain.
    #[serde(default = "time_core::places::defaults", rename = "place")]
    pub places: Vec<time_core::places::PlaceSpec>,
    /// Focus nudges. On by default with the values in `time_core::nudge`.
    #[serde(default)]
    pub nudge: time_core::nudge::NudgeConfig,
    /// Which activities count as productive or unproductive in the MCP
    /// tools and the hourly summary line. Everything else is neutral.
    #[serde(default)]
    pub mcp: McpConfig,
    /// IANA zone for days and clock times on the website.
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Seconds between classifier passes.
    #[serde(default = "default_classify_every")]
    pub classify_every_secs: u64,
    /// Daily step goal for the steps ring and bars.
    #[serde(default = "default_step_goal")]
    pub step_goal: u32,
}

/// How the agent-facing summaries judge a minute. Erik's call (2026-10-23):
/// video and feeds are unproductive, gaming is fine (neutral), and
/// this is meant to be tuned, so it lives in `config.toml`:
///
/// ```toml
/// [mcp]
/// productive = ["work", "study"]
/// unproductive = ["video", "feeds"]
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    pub productive: Vec<Activity>,
    pub unproductive: Vec<Activity>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            productive: vec![Activity::Work, Activity::Study],
            unproductive: vec![Activity::Video, Activity::Feeds],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Desktop,
    Laptop,
    Phone,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub kind: DeviceKind,
}

fn default_apk_repo() -> String {
    "ErikFrankling/time".into()
}

fn default_public_url() -> String {
    "https://time.erikfrankling.duckdns.org/".into()
}

fn default_timezone() -> String {
    "Europe/Stockholm".into()
}

fn default_step_goal() -> u32 {
    10_000
}

fn default_classify_every() -> u64 {
    10
}

fn default_listen() -> String {
    "0.0.0.0:7373".into()
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let c: Self = toml::from_str(text)?;
        c.tz()?;
        if let Some(a) = c
            .mcp
            .productive
            .iter()
            .find(|a| c.mcp.unproductive.contains(a))
        {
            anyhow::bail!("[mcp]: {a} is both productive and unproductive");
        }
        Ok(c)
    }

    pub fn tz(&self) -> anyhow::Result<chrono_tz::Tz> {
        self.timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("timezone {:?}: {e}", self.timezone))
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_an_error() {
        // The v1 config silently ignored keys it no longer read; a typo then
        // looked like a setting that did nothing.
        assert!(Config::parse("data_dir = '/x'\nclasify = false").is_err());
    }

    #[test]
    fn mcp_groups_default_to_games_being_fine() {
        let c = Config::parse("data_dir = '/x'").unwrap();
        assert!(!c.mcp.unproductive.contains(&Activity::Games));
        assert!(c.mcp.unproductive.contains(&Activity::Video));
        let c = Config::parse("data_dir = '/x'\n[mcp]\nunproductive = ['games']").unwrap();
        assert_eq!(c.mcp.unproductive, vec![Activity::Games]);
        assert!(Config::parse("data_dir = '/x'\n[mcp]\nunproductive = ['work']").is_err());
    }

    #[test]
    fn devices_parse() {
        let c = Config::parse(
            "data_dir = '/x'\n[devices.pc]\nkind = 'desktop'\n[devices.phone]\nkind = 'phone'",
        )
        .unwrap();
        assert_eq!(c.devices["phone"].kind, DeviceKind::Phone);
        assert_eq!(c.listen, "0.0.0.0:7373");
    }
}
