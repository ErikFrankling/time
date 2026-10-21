//! Server configuration, read once at start from a TOML file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

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

fn default_listen() -> String {
    "0.0.0.0:7373".into()
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
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
    fn devices_parse() {
        let c = Config::parse(
            "data_dir = '/x'\n[devices.pc]\nkind = 'desktop'\n[devices.phone]\nkind = 'phone'",
        )
        .unwrap();
        assert_eq!(c.devices["phone"].kind, DeviceKind::Phone);
        assert_eq!(c.listen, "0.0.0.0:7373");
    }
}
