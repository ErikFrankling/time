//! Keeps a copy of the latest Android release on disk.
//!
//! The APK used to arrive by `kubectl cp`, which meant every phone update
//! started with a human at a terminal. CI publishes to GitHub Releases instead
//! and the server pulls from there: the repo is public, so the asset URL needs
//! no credential and the whole chain from `git push` to a notification on the
//! phone runs unattended.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How often to ask GitHub for a newer release. Unauthenticated API calls are
/// limited to 60/hour per IP and a phone that learns about an update an hour
/// late is not a problem worth spending that budget on.
const POLL: std::time::Duration = std::time::Duration::from_secs(3600);

/// What the phone and the landing page need to know about the APK on disk.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub version: String,
    pub version_code: i64,
    pub sha256: String,
    /// The release asset this came from. Empty for a hand-dropped file.
    pub url: String,
    /// RFC3339, straight from the release.
    pub published: String,
    pub size: u64,
    /// Which key signed it: `release` or `debug`. A debug-signed build cannot
    /// be upgraded to a release-signed one, so this has to be visible rather
    /// than inferred.
    pub signing: String,
}

/// Live state shared with the request handlers.
#[derive(Default)]
pub struct State {
    pub meta: Option<Meta>,
    /// Unix seconds of the last *successful* check against GitHub.
    pub checked: i64,
    /// Last failure, retained so the landing page can admit it is serving
    /// something possibly out of date rather than quietly looking healthy.
    pub error: String,
}

pub type Shared = Arc<Mutex<State>>;

/// Where the APK lives. Defaults beside the database — the PVC in the cluster,
/// `~/.local/share/time` on a laptop — so the fetched copy survives a restart
/// and a local run does not try to write into a `/data` that isn't there.
/// Setting it explicitly turns the fetcher off and pins the server to whatever
/// file is at that path.
pub fn path() -> PathBuf {
    if let Ok(p) = std::env::var("TIME_APK_PATH") {
        return PathBuf::from(p);
    }
    crate::config::data_dir()
        .unwrap_or_else(|_| PathBuf::from("/data"))
        .join("time.apk")
}

/// True when the operator pinned a file by hand and we must not overwrite it.
fn pinned() -> bool {
    std::env::var_os("TIME_APK_PATH").is_some()
}

pub fn repo() -> String {
    std::env::var("TIME_APK_REPO").unwrap_or_else(|_| "ErikFrankling/time".into())
}

/// Sidecar next to the APK, so a restart knows the version and hash of the
/// bytes already on the volume without re-downloading them.
fn meta_path() -> PathBuf {
    PathBuf::from(format!("{}.json", path().display()))
}

/// Start the poller and hand back the state it writes into.
///
/// Never returns an error: a server that refuses to boot because GitHub is
/// having a bad morning is worse than one serving last week's APK.
pub fn start() -> Shared {
    let shared: Shared = Arc::new(Mutex::new(State::default()));

    // Whatever a previous run left behind is the starting answer, so /app works
    // from the first request rather than after the first successful fetch.
    if let Some(m) = load_meta() {
        shared.lock().unwrap().meta = Some(m);
    } else if let Some(m) = describe_local() {
        shared.lock().unwrap().meta = Some(m);
    }

    if pinned() {
        println!("apk: pinned to {} (TIME_APK_PATH set, no fetching)", path().display());
        return shared;
    }

    let out = shared.clone();
    std::thread::spawn(move || loop {
        match poll_now(&out) {
            Ok(Some(m)) => println!("apk: fetched {} ({})", m.version, m.sha256),
            Ok(None) => {}
            Err(e) => eprintln!("apk: {e:#}"),
        }
        std::thread::sleep(POLL);
    });
    shared
}

/// One poll. `Ok(None)` means the release on GitHub is the one already on disk.
fn refresh(shared: &Shared) -> Result<Option<Meta>> {
    let release = latest_release(&repo())?;
    let now = chrono::Utc::now().timestamp();

    let current = shared.lock().unwrap().meta.clone();
    // The asset URL carries the tag, so an unchanged URL and an unchanged size
    // mean unchanged bytes. Re-downloading 25 MB hourly to prove that would be
    // the only real cost this whole mechanism has.
    if let Some(c) = &current {
        if c.url == release.url && c.size == release.size && path().exists() {
            let mut s = shared.lock().unwrap();
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

    let apk = path();
    if let Some(dir) = apk.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    // Write beside the target and rename, so a request landing mid-download
    // never sees a truncated APK. Same filesystem, so the rename is atomic.
    let tmp = apk.with_extension("apk.part");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &apk).with_context(|| format!("renaming into {}", apk.display()))?;
    std::fs::write(meta_path(), serde_json::to_vec_pretty(&meta)?).ok();

    let mut s = shared.lock().unwrap();
    s.meta = Some(meta.clone());
    s.checked = now;
    s.error.clear();
    Ok(Some(meta))
}

/// Force a check now, reporting the outcome. Used by the poller's first pass
/// and available for a manual poke.
pub fn poll_now(shared: &Shared) -> Result<Option<Meta>> {
    match refresh(shared) {
        Ok(v) => Ok(v),
        Err(e) => {
            let mut s = shared.lock().unwrap();
            s.error = format!("{e:#}");
            Err(e)
        }
    }
}

struct Release {
    version: String,
    version_code: i64,
    url: String,
    published: String,
    size: u64,
    signing: String,
}

/// The newest release that actually carries an APK.
///
/// Not `/releases/latest`: that is the newest non-prerelease of *any* kind, so
/// the day this repo cuts a server release it would start handing the phone a
/// release with no APK in it.
fn latest_release(repo: &str) -> Result<Release> {
    // Overridable so the fetch chain can be exercised against a stub, and so a
    // GitHub Enterprise host would work without a code change.
    let api = std::env::var("TIME_GITHUB_API")
        .unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{api}/repos/{repo}/releases?per_page=20");
    let mut req = client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    // Optional: only lifts the rate limit. The repo is public, so the whole
    // path works without it.
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
        let tag = r["tag_name"].as_str().unwrap_or_default();
        let Some((version, version_code)) = parse_tag(tag) else {
            continue;
        };
        let Some(asset) = r["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|a| {
                a["name"].as_str().is_some_and(|n| n.ends_with(".apk"))
            }))
        else {
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
            // CI falls back to the debug build when no keystore secret exists,
            // and marks the filename so nobody has to guess later.
            signing: if name.contains("-debug") { "debug" } else { "release" }.into(),
        });
    }
    Err(anyhow!("no release with an APK asset in {repo}"))
}

/// `android-v0.1.0-42` -> `("0.1.0", 42)`.
///
/// The versionCode is not in the releases API and reading it out of the APK
/// would mean an AXML parser for one integer, so the tag carries it. The phone
/// compares on it, so getting it wrong is the one thing that breaks updates.
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
        // GitHub rejects requests without one.
        .user_agent(concat!("time/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn load_meta() -> Option<Meta> {
    let m: Meta = serde_json::from_slice(&std::fs::read(meta_path()).ok()?).ok()?;
    // The sidecar describes a file; if that file is gone the sidecar is a lie.
    path().exists().then_some(m)
}

/// A file someone put there by hand — `TIME_APK_PATH` pointing at a local
/// build, or a leftover from before this existed. Hash it so the phone can
/// still verify the download, and take the version from the environment since
/// nothing else knows it.
fn describe_local() -> Option<Meta> {
    let apk = path();
    let bytes = std::fs::read(&apk).ok()?;
    Some(Meta {
        version: std::env::var("TIME_APK_VERSION").unwrap_or_else(|_| "unknown".into()),
        // Zero never exceeds an installed versionCode, so a hand-dropped APK
        // never triggers an update prompt it cannot substantiate.
        version_code: std::env::var("TIME_APK_VERSION_CODE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        sha256: hex(&Sha256::digest(&bytes)),
        url: String::new(),
        published: modified(&apk),
        size: bytes.len() as u64,
        signing: std::env::var("TIME_APK_SIGNING").unwrap_or_else(|_| "unknown".into()),
    })
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
        assert_eq!(parse_tag("android-v1.2.3-7"), Some(("1.2.3".into(), 7)));
        // Server releases and anything else must not be mistaken for an APK.
        assert_eq!(parse_tag("v0.1.0"), None);
        assert_eq!(parse_tag("android-v0.1.0"), None);
        assert_eq!(parse_tag("android-v-1"), None);
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
