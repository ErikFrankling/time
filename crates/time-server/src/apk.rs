//! Keeps a copy of the latest Android release on disk and serves it at `/app`.
//!
//! Ported from v1. CI publishes the APK to GitHub Releases and the server
//! pulls it from there (the repo is public, so no credential), so the chain
//! from `git push` to an update notification on the phone runs unattended.
//! A failing fetch never stops the server: it keeps serving what it has and
//! says so on `/app/`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Unauthenticated GitHub API calls are limited to 60 an hour per IP; a phone
/// learning about an update an hour late is not worth more of that budget.
pub const POLL: std::time::Duration = std::time::Duration::from_secs(3600);

/// What the phone and the landing page need to know about the APK on disk.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub version: String,
    pub version_code: i64,
    pub sha256: String,
    /// The release asset this came from. Empty for a hand-dropped file.
    pub url: String,
    pub published: String,
    pub size: u64,
    /// `release` or `debug`. A debug-signed build cannot be upgraded to a
    /// release-signed one, so this is shown rather than inferred.
    pub signing: String,
}

#[derive(Default, Clone)]
pub struct State {
    pub meta: Option<Meta>,
    /// Unix seconds of the last successful check against GitHub.
    pub checked: i64,
    /// Last failure, so `/app/` admits it may be serving something stale.
    pub error: String,
}

pub struct Apk {
    /// The APK on disk.
    pub path: PathBuf,
    /// A file pinned by hand: served as is, never fetched over.
    pub pinned: bool,
    /// GitHub `owner/repo` whose releases carry the APK.
    pub repo: String,
    pub state: Mutex<State>,
}

pub type Shared = Arc<Apk>;

impl Apk {
    /// Whatever a previous run left behind is the starting answer, so `/app`
    /// works from the first request.
    pub fn open(path: PathBuf, pinned: bool, repo: String) -> Shared {
        let apk = Apk {
            path,
            pinned,
            repo,
            state: Mutex::new(State::default()),
        };
        let meta = apk.load_meta().or_else(|| apk.describe_local());
        apk.state.lock().unwrap_or_else(|e| e.into_inner()).meta = meta;
        Arc::new(apk)
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn meta_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.json", self.path.display()))
    }

    /// One check. `Ok(None)` means GitHub's newest APK is the one on disk.
    pub fn poll_now(&self) -> Result<Option<Meta>> {
        let result = self.refresh();
        if let Err(e) = &result {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).error = format!("{e:#}");
        }
        result
    }

    fn refresh(&self) -> Result<Option<Meta>> {
        if self.pinned {
            return Ok(None);
        }
        let release = latest_release(&self.repo)?;
        let now = chrono::Utc::now().timestamp();
        let current = self.snapshot().meta;
        // The asset URL carries the tag: same URL and size means same bytes.
        if let Some(c) = &current {
            if c.url == release.url && c.size == release.size && self.path.exists() {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                s.checked = now;
                s.error.clear();
                return Ok(None);
            }
        }
        let bytes = download(&release.url)?;
        if bytes.len() as u64 != release.size {
            return Err(anyhow!(
                "short download: {} of {} bytes",
                bytes.len(),
                release.size
            ));
        }
        let meta = Meta {
            version: release.version,
            version_code: release.version_code,
            sha256: hex(&Sha256::digest(&bytes)),
            url: release.url,
            published: release.published,
            size: bytes.len() as u64,
            signing: release.signing,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        // Written beside the target and renamed: a request mid-download never
        // sees a truncated APK.
        let tmp = self.path.with_extension("apk.part");
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming into {}", self.path.display()))?;
        std::fs::write(self.meta_path(), serde_json::to_vec_pretty(&meta)?).ok();
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.meta = Some(meta.clone());
        s.checked = now;
        s.error.clear();
        Ok(Some(meta))
    }

    fn load_meta(&self) -> Option<Meta> {
        let m: Meta = serde_json::from_slice(&std::fs::read(self.meta_path()).ok()?).ok()?;
        // A sidecar whose file is gone describes nothing.
        self.path.exists().then_some(m)
    }

    /// A file put there by hand: hashed so the phone can verify it, with the
    /// version from the environment since nothing else knows it.
    fn describe_local(&self) -> Option<Meta> {
        let bytes = std::fs::read(&self.path).ok()?;
        Some(Meta {
            version: std::env::var("TIME_APK_VERSION").unwrap_or_else(|_| "unknown".into()),
            // Zero never exceeds an installed versionCode, so a hand-dropped
            // APK never triggers an update prompt it cannot back up.
            version_code: std::env::var("TIME_APK_VERSION_CODE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            sha256: hex(&Sha256::digest(&bytes)),
            url: String::new(),
            published: modified(&self.path),
            size: bytes.len() as u64,
            signing: std::env::var("TIME_APK_SIGNING").unwrap_or_else(|_| "unknown".into()),
        })
    }
}

/// Poll forever on a blocking thread. Never fails the server.
pub fn spawn_poller(apk: Shared) {
    if apk.pinned {
        eprintln!("apk: pinned to {} (no fetching)", apk.path.display());
        return;
    }
    std::thread::spawn(move || loop {
        match apk.poll_now() {
            Ok(Some(m)) => eprintln!("apk: fetched {} ({})", m.version, m.sha256),
            Ok(None) => {}
            Err(e) => eprintln!("apk: {e:#}"),
        }
        std::thread::sleep(POLL);
    });
}

struct Release {
    version: String,
    version_code: i64,
    url: String,
    published: String,
    size: u64,
    signing: String,
}

/// The newest release that carries an APK. Not `/releases/latest`, which is
/// the newest release of any kind.
fn latest_release(repo: &str) -> Result<Release> {
    let api = std::env::var("TIME_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    let mut req = client()?
        .get(format!("{api}/repos/{repo}/releases?per_page=20"))
        .header("Accept", "application/vnd.github+json");
    // Optional: only lifts the rate limit.
    if let Ok(t) = std::env::var("TIME_GITHUB_TOKEN") {
        if !t.is_empty() {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
    }
    let resp = req.send().context("listing releases")?;
    if !resp.status().is_success() {
        return Err(anyhow!("releases: HTTP {}", resp.status()));
    }
    let releases: Vec<serde_json::Value> = resp.json().context("parsing releases")?;
    for r in releases {
        if r["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some((version, version_code)) = parse_tag(r["tag_name"].as_str().unwrap_or_default())
        else {
            continue;
        };
        let Some(asset) = r["assets"].as_array().and_then(|a| {
            a.iter()
                .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".apk")))
        }) else {
            continue;
        };
        let name = asset["name"].as_str().unwrap_or_default();
        return Ok(Release {
            version,
            version_code,
            url: asset["browser_download_url"]
                .as_str()
                .ok_or_else(|| anyhow!("asset has no download url"))?
                .to_string(),
            published: r["published_at"].as_str().unwrap_or_default().to_string(),
            size: asset["size"].as_u64().unwrap_or(0),
            signing: if name.contains("-debug") {
                "debug"
            } else {
                "release"
            }
            .into(),
        });
    }
    Err(anyhow!("no release with an APK asset in {repo}"))
}

/// `android-v0.1.0-42` -> `("0.1.0", 42)`. The versionCode is not in the
/// releases API, so the tag carries it; the phone compares on it.
fn parse_tag(tag: &str) -> Option<(String, i64)> {
    let rest = tag.strip_prefix("android-v")?;
    let (version, code) = rest.rsplit_once('-')?;
    if version.is_empty() {
        return None;
    }
    Some((version.to_string(), code.parse().ok()?))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let resp = client()?.get(url).send().context("downloading asset")?;
    if !resp.status().is_success() {
        return Err(anyhow!("asset: HTTP {}", resp.status()));
    }
    let mut buf = Vec::new();
    resp.take(64 * 1024 * 1024).read_to_end(&mut buf)?;
    Ok(buf)
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("time/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn modified(p: &Path) -> String {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_tags() {
        assert_eq!(parse_tag("android-v0.1.0-42"), Some(("0.1.0".into(), 42)));
        assert_eq!(parse_tag("android-v2.0.0-7"), Some(("2.0.0".into(), 7)));
        assert_eq!(parse_tag("v0.1.0"), None);
        assert_eq!(parse_tag("android-v0.1.0"), None);
        assert_eq!(parse_tag("android-v-1"), None);
    }

    #[test]
    fn a_pinned_file_is_described_and_never_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("time.apk");
        std::fs::write(&path, b"not really an apk").unwrap();
        let apk = Apk::open(path, true, "x/y".into());
        let meta = apk.snapshot().meta.unwrap();
        assert_eq!(meta.size, 17);
        assert_eq!(meta.version_code, 0);
        assert_eq!(meta.sha256, hex(&Sha256::digest(b"not really an apk")));
        assert_eq!(apk.poll_now().unwrap(), None);
    }
}
