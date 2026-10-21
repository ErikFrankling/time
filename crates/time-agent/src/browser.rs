//! Front tabs, reported by the browsers themselves.
//!
//! Nothing outside a browser knows its URL on Wayland, so the agent answers
//! the three endpoints ActivityWatch's `aw-watcher-web` extension posts to
//! on 127.0.0.1:5600 (ported from v1). What changed from v1: every tab
//! reported in the last few minutes is kept, and the one used is the one
//! whose *title* is in the focused window's title.
//! Firefox and its forks all report as "firefox", so v1, which kept only the
//! newest beat from anyone, billed a Firefox window to a Zen tab.
//!
//! Only the host is ever kept from a URL, and only in memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::probe::{match_tab, Tab, Tabs, Window};

/// Fixed by the extension's host permissions.
pub const PORT: u16 = 5600;
/// The extension beats every minute and on every tab switch.
const MAX_AGE: Duration = Duration::from_secs(150);

#[derive(Debug, Clone)]
struct Beat {
    tab: Option<Tab>,
    at: Instant,
}

#[derive(Clone, Default)]
pub struct Watcher {
    beats: Arc<Mutex<HashMap<String, Beat>>>,
    bind_error: Option<String>,
}

impl Watcher {
    /// Listen on the extension's port. Failing to bind (a real aw-server owns
    /// it) disables host reporting rather than the agent.
    pub fn start(device: &str) -> Self {
        let mut w = Self::default();
        let addr = format!("127.0.0.1:{PORT}");
        match tiny_http::Server::http(&addr) {
            Ok(server) => {
                let (beats, hostname) = (w.beats.clone(), device.to_string());
                std::thread::spawn(move || {
                    for req in server.incoming_requests() {
                        serve(req, &beats, &hostname);
                    }
                });
            }
            Err(e) => w.bind_error = Some(format!("browser tabs: binding {addr}: {e}")),
        }
        w
    }

    /// Record a heartbeat. Public so tests can feed the same path the HTTP
    /// handler uses.
    pub fn beat(&self, bucket: &str, event: &serde_json::Value) {
        let data = &event["data"];
        let incognito = data["incognito"].as_bool().unwrap_or(false);
        let host = data["url"]
            .as_str()
            .filter(|_| !incognito)
            .and_then(host_of);
        let title = data["title"].as_str().unwrap_or_default().to_owned();
        let tab = host.map(|host| Tab { host, title });
        if let Ok(mut beats) = self.beats.lock() {
            // Keyed by bucket *and* title: Firefox and Zen both report into
            // "aw-watcher-web-firefox_<host>", so a bucket alone would let one
            // browser overwrite the other. Old entries age out.
            beats.retain(|_, b| b.at.elapsed() <= MAX_AGE);
            let key = format!(
                "{bucket}\u{1f}{}",
                tab.as_ref().map(|t| t.title.as_str()).unwrap_or("")
            );
            beats.insert(
                key,
                Beat {
                    tab,
                    at: Instant::now(),
                },
            );
        }
    }
}

impl Tabs for Watcher {
    fn problem(&self) -> Option<String> {
        self.bind_error.clone()
    }

    fn front_tab(&mut self, focused: &Window) -> Option<Tab> {
        let beats = self.beats.lock().ok()?;
        let fresh: Vec<Tab> = beats
            .values()
            .filter(|b| b.at.elapsed() <= MAX_AGE)
            .filter_map(|b| b.tab.clone())
            .collect();
        match_tab(focused, &fresh)
    }
}

fn serve(mut req: tiny_http::Request, beats: &Arc<Mutex<HashMap<String, Beat>>>, hostname: &str) {
    // DNS rebinding: any page can resolve a name to 127.0.0.1, so only
    // requests addressed to loopback by name are answered.
    let host_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .is_some_and(|h| {
            h.value.as_str().starts_with("127.0.0.1") || h.value.as_str().starts_with("localhost")
        });
    if !host_ok {
        let _ = req.respond(tiny_http::Response::empty(403));
        return;
    }
    let url = req.url().to_string();
    let cors = [
        "Access-Control-Allow-Origin: *",
        "Access-Control-Allow-Methods: GET, POST, OPTIONS",
        "Access-Control-Allow-Headers: Content-Type, Authorization",
        "Access-Control-Max-Age: 86400",
        "Content-Type: application/json",
    ];
    let respond = |req: tiny_http::Request, code: u16, body: String| {
        let mut r = tiny_http::Response::from_string(body).with_status_code(code);
        for h in cors {
            if let Ok(h) = h.parse::<tiny_http::Header>() {
                r.add_header(h);
            }
        }
        let _ = req.respond(r);
    };
    if *req.method() == tiny_http::Method::Options {
        respond(req, 204, String::new());
        return;
    }
    // The homelab's health check for the agent (dotfiles observability).
    if url.starts_with("/health") {
        respond(req, 200, r#"{"status":"ok"}"#.into());
        return;
    }
    if url.starts_with("/api/0/info") {
        let info = serde_json::json!({ "hostname": hostname, "version": concat!("time ", env!("CARGO_PKG_VERSION")), "testing": false, "device_id": hostname });
        respond(req, 200, info.to_string());
        return;
    }
    if let Some(bucket) = heartbeat_bucket(&url) {
        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_err() {
            respond(req, 400, "{}".into());
            return;
        }
        let event: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        Watcher {
            beats: beats.clone(),
            bind_error: None,
        }
        .beat(&bucket, &event);
        // The extension parses the reply as the stored event.
        respond(req, 200, body);
        return;
    }
    // Bucket creation and anything else: succeed, or the extension retries forever.
    respond(req, 200, "{}".into());
}

/// `/api/0/buckets/<bucket>/heartbeat?pulsetime=...` -> `<bucket>`.
fn heartbeat_bucket(url: &str) -> Option<String> {
    let path = url.split('?').next()?;
    let rest = path.strip_prefix("/api/0/buckets/")?;
    let bucket = rest.strip_suffix("/heartbeat")?;
    (!bucket.is_empty()).then(|| bucket.to_owned())
}

/// Host only, lowercased, without `www.`; None for anything not a web page.
pub fn host_of(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_start_matches("www.").to_lowercase();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_only_the_host() {
        assert_eq!(
            host_of("https://www.dn.se/a?b=c#d").as_deref(),
            Some("dn.se")
        );
        assert_eq!(host_of("about:newtab"), None);
        assert_eq!(host_of("file:///x"), None);
    }

    #[test]
    fn bucket_from_path() {
        assert_eq!(
            heartbeat_bucket("/api/0/buckets/aw-watcher-web-firefox_pc/heartbeat?pulsetime=65")
                .as_deref(),
            Some("aw-watcher-web-firefox_pc")
        );
        assert_eq!(heartbeat_bucket("/api/0/buckets/x"), None);
    }

    #[test]
    fn two_browsers_reporting_as_firefox_are_told_apart_by_title() {
        let mut w = Watcher::default();
        let beat = |url: &str, title: &str| json!({ "data": { "url": url, "title": title, "incognito": false } });
        // Both browsers report into the same bucket name.
        w.beat(
            "aw-watcher-web-firefox_pc",
            &beat("https://play.hbomax.com/x", "Angel Has Fallen • HBO Max"),
        );
        w.beat(
            "aw-watcher-web-firefox_pc",
            &beat("https://www.dn.se/k", "Krönika — DN"),
        );
        let focused = Window {
            class: "firefox".into(),
            title: "Krönika — DN — Mozilla Firefox".into(),
            fullscreen: false,
        };
        assert_eq!(w.front_tab(&focused).unwrap().host, "dn.se");
        w.beat(
            "aw-watcher-web-firefox_pc",
            &json!({ "data": { "url": "https://x.com/", "title": "Home / X", "incognito": true } }),
        );
        let private = Window {
            title: "Home / X — Mozilla Firefox".into(),
            ..focused.clone()
        };
        assert_eq!(w.front_tab(&private), None, "incognito tabs report no host");
        assert_eq!(
            w.front_tab(&focused).unwrap().host,
            "dn.se",
            "and do not erase other tabs"
        );
    }
}
