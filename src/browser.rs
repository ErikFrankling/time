//! Which site the front tab is on, straight from the browser itself.
//!
//! Nothing outside the browser knows this on Wayland. The window title is the
//! page title and never the host; asking the vision model to read the URL bar
//! off the screenshot fails outright on a browser configured to hide the
//! toolbar, which Zen is here, and guesses at 1024px wide when it does not;
//! `places.sqlite` is a history log rather than a statement about now, and is
//! held open by the running browser; and the remote debugging protocols mean
//! opening an unauthenticated port that can drive the browser and read its
//! cookies -- a far worse trade than an extension.
//!
//! So the browser has to volunteer it, and ActivityWatch's `aw-watcher-web`
//! already does exactly that: signed, on AMO, maintained, and installable in
//! Firefox and every Firefox fork in one click. Rather than run their whole
//! server to receive it, the agent answers the three endpoints that extension
//! talks to. That is the entire integration.
//!
//! Two things the extension does NOT do, which are handled here:
//!
//! - It keeps heartbeating the last active tab forever, minimised or not, and
//!   knows nothing about OS focus. Upstream intersects it with a window watcher
//!   at query time; the agent already has the focused window from Hyprland and
//!   does the same intersection in `agent::build_frame`. Without that, a
//!   backgrounded browser would quietly bill every minute of the day to
//!   whatever tab was last open.
//! - It reports the URL. Only the host is ever kept, and only in memory --
//!   paths and query strings are the sensitive half of a URL, and they would
//!   aggregate into a list one row long anyway.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hardcoded in the extension's manifest host permissions, so it is not
/// negotiable: the Firefox build can only reach 127.0.0.1:5600.
pub const PORT: u16 = 5600;

struct Beat {
    /// None for a tab with no host worth recording -- about:blank, an
    /// extension page, or private browsing. Explicitly absent rather than
    /// missing, so an incognito tab clears the last known site instead of
    /// leaving it to be billed for the minute.
    domain: Option<String>,
    at: Instant,
}

#[derive(Clone)]
pub struct Tabs {
    latest: Arc<Mutex<Option<Beat>>>,
}

impl Tabs {
    /// Start listening. A failure to bind is not fatal: it means something else
    /// owns the port (a real aw-server, or a second agent), and an activity
    /// tracker that refuses to start over a missing browser extension is worse
    /// than one that records no domains.
    pub fn start(device: &str) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let addr = format!("127.0.0.1:{PORT}");
        match tiny_http::Server::http(&addr) {
            Ok(server) => {
                let (latest, hostname) = (latest.clone(), device.to_string());
                std::thread::spawn(move || {
                    for req in server.incoming_requests() {
                        serve(req, &latest, &hostname);
                    }
                });
                println!("browser watcher on {addr}");
            }
            Err(e) => eprintln!("browser watcher disabled: binding {addr}: {e}"),
        }
        Self { latest }
    }

    /// The host of the front tab, if the browser said so recently enough.
    ///
    /// The extension heartbeats once a minute and on every tab switch, so a
    /// beat older than a couple of those means the browser is gone, asleep, or
    /// never had the extension -- and a stale host is worse than none.
    pub fn domain(&self, max_age: Duration) -> Option<String> {
        let beat = self.latest.lock().ok()?;
        let beat = beat.as_ref()?;
        if beat.at.elapsed() > max_age {
            return None;
        }
        beat.domain.clone()
    }
}

/// The three endpoints `aw-watcher-web` uses, and nothing else.
fn serve(mut req: tiny_http::Request, latest: &Mutex<Option<Beat>>, hostname: &str) {
    // A browser can be told to resolve any name to 127.0.0.1, so binding to
    // loopback alone does not keep a hostile page out -- only the Host header
    // does. Same check aw-server carries for the same advisory.
    let host_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| {
            let v = h.value.as_str();
            v.starts_with("127.0.0.1") || v.starts_with("localhost")
        })
        .unwrap_or(false);
    if !host_ok {
        let _ = req.respond(tiny_http::Response::empty(403));
        return;
    }

    let url = req.url().to_string();
    let method = req.method().clone();

    // Firefox gives every install of an extension its own origin, so there is
    // no fixed value to allow -- upstream wildcards it for the same reason.
    // Credentials are never used, so `*` is the honest answer.
    let cors = [
        "Access-Control-Allow-Origin: *",
        "Access-Control-Allow-Methods: GET, POST, OPTIONS",
        "Access-Control-Allow-Headers: Content-Type, Authorization",
        "Access-Control-Max-Age: 86400",
    ];
    let with_cors = |mut r: tiny_http::Response<std::io::Empty>| {
        for h in cors {
            if let Ok(h) = h.parse::<tiny_http::Header>() {
                r.add_header(h);
            }
        }
        r
    };
    let json = |body: String| {
        let mut r = tiny_http::Response::from_string(body);
        for h in cors.iter().chain(["Content-Type: application/json"].iter()) {
            if let Ok(h) = h.parse::<tiny_http::Header>() {
                r.add_header(h);
            }
        }
        r
    };

    // `Content-Type: application/json` is not CORS-safelisted, so every POST
    // is preceded by a preflight.
    if method == tiny_http::Method::Options {
        let _ = req.respond(with_cors(tiny_http::Response::empty(204)));
        return;
    }

    // How the extension finds out a server is there at all. It reads only
    // `hostname`, which it then bakes into the bucket name.
    if url.starts_with("/api/0/info") {
        let _ = req.respond(json(
            serde_json::json!({
                "hostname": hostname,
                "version": concat!("time ", env!("CARGO_PKG_VERSION")),
                "testing": false,
                "device_id": hostname,
            })
            .to_string(),
        ));
        return;
    }

    if url.contains("/heartbeat") {
        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_err() {
            let _ = req.respond(tiny_http::Response::empty(400));
            return;
        }
        let event: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let incognito = event["data"]["incognito"].as_bool().unwrap_or(false);
        let domain = event["data"]["url"]
            .as_str()
            .filter(|_| !incognito)
            .and_then(domain_of);

        if let Ok(mut slot) = latest.lock() {
            *slot = Some(Beat {
                domain,
                at: Instant::now(),
            });
        }

        // The client parses the response as the stored event and will throw on
        // an empty body, so hand its own event back.
        let _ = req.respond(json(body));
        return;
    }

    // Bucket creation. There is nothing to create -- only the last beat is
    // ever kept -- but the extension retries forever on anything but success.
    let _ = req.respond(json("{}".into()));
}

/// Host only, lowercased, `www.` dropped so one site is one row.
///
/// Anything that is not a real web page -- about:, moz-extension:, file: --
/// is not a site and returns None.
fn domain_of(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_start_matches("www.").to_lowercase();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::domain_of;

    #[test]
    fn keeps_the_host_and_nothing_else() {
        assert_eq!(
            domain_of("https://www.dn.se/sverige/artikel?utm_source=x#frag"),
            Some("dn.se".into())
        );
        assert_eq!(domain_of("https://X.com/erik"), Some("x.com".into()));
        assert_eq!(domain_of("http://localhost:3000/admin"), Some("localhost".into()));
    }

    #[test]
    fn ignores_what_is_not_a_website() {
        assert_eq!(domain_of("about:newtab"), None);
        assert_eq!(domain_of("moz-extension://abc/options.html"), None);
        assert_eq!(domain_of("file:///home/erik/notes.md"), None);
        assert_eq!(domain_of("not a url"), None);
    }
}
