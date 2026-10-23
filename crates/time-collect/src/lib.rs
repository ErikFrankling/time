//! What Erik shipped: commits from the local clones, pull requests and
//! reviews from GitHub, as Time events.
//!
//! Every event's id is derived from what it describes (a commit hash, a PR
//! number and action), so the collector can rescan the same window every run
//! and the server stores each thing once. Ported from v1's `time collect`,
//! which posted daily totals instead.

pub mod git;
pub mod github;
pub mod t3;

use serde::Deserialize;

pub const SERVICE: &str = "github-collector";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectConfig {
    /// This machine, sent as `host.name`.
    pub device: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Directories walked for git repositories.
    #[serde(default = "default_roots")]
    pub roots: Vec<String>,
    /// Substrings of author name or email that are Erik. Empty: each
    /// repository's own `user.email`.
    #[serde(default)]
    pub authors: Vec<String>,
    /// How far back each run looks.
    #[serde(default = "default_days")]
    pub days: i64,
    /// Ask GitHub for pull requests and reviews (token from TIME_GITHUB_TOKEN
    /// or the `gh` CLI).
    #[serde(default = "default_true")]
    pub github: bool,
    /// T3 Code's state directory: prompts, tokens and dictations are read
    /// from it when it exists.
    #[serde(default = "default_t3")]
    pub t3: String,
}

fn default_endpoint() -> String {
    "http://127.0.0.1:4318/v1/logs".into()
}

fn default_roots() -> Vec<String> {
    vec!["~/projects".into()]
}

fn default_t3() -> String {
    "~/.t3/userdata".into()
}

fn default_days() -> i64 {
    7
}

fn default_true() -> bool {
    true
}

/// A stable UUID for something identified by `key`.
pub fn stable_id(key: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, key.as_bytes()).to_string()
}
