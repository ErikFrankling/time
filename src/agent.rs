use anyhow::{Context, Result};

use crate::browser;
use crate::capture;
use crate::input;
use crate::config::AgentConfig;
use crate::proto::{Frame, FrameAck};
use crate::spool::Spool;

/// Frames per batch when draining a backlog. Well under the server's cap of
/// 2000, matching what the phone sends, and small enough that a request that
/// fails costs one round trip rather than the whole backlog.
const CHUNK: usize = 500;

/// How long one tick may spend on the backlog.
///
/// A week-long outage is thousands of minutes and cannot go in one tick without
/// the loop missing the minute it is standing in. Half the tick goes to the
/// backlog and the rest stays for the present; the remainder drains next minute.
const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// How stale a tab report may be before it is discarded. The extension beats
/// once a minute and on every tab switch, so anything older than a couple of
/// beats means the browser is closed, asleep, or has no extension -- and a
/// site nobody is looking at any more is worse than no site at all.
const TAB_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(150);

/// Build this minute's frame. The only decision the agent makes on its own is
/// the blocklist -- and it errs toward sending nothing.
pub fn build_frame(cfg: &AgentConfig, input: &input::Monitor, tabs: &browser::Tabs) -> Result<Frame> {
    let now = chrono::Local::now().timestamp();
    let ts = now - (now % 60);
    let window = capture::active_window();
    let snap = input.take();

    // The extension reports its last active tab whether or not the browser is
    // anywhere near the front, so the tab only counts when the compositor says
    // a browser has focus. Otherwise a browser left open behind a terminal
    // would bill the whole day to whatever was last read.
    let domain = if cfg.is_browser(&window.class) {
        tabs.domain(TAB_MAX_AGE)
    } else {
        None
    };

    // A site can be as sensitive as a window title, and often more so: the
    // bank tab is titled after the bank, but so is the tab you meant to keep.
    // Matching the blocklist against the host too means visiting one suppresses
    // the screenshot as well, rather than merely dropping the domain.
    let blocked_domain = domain
        .as_deref()
        .is_some_and(|d| capture::matches(d, &cfg.blocklist));

    if window.blocked(&cfg.blocklist) || blocked_domain {
        return Ok(Frame {
            ts,
            device: cfg.device.clone(),
            window: String::new(), // the title itself may be sensitive
            domain: None,
            image: None,
            blocked: true,
            idle_secs: snap.idle_secs,
            keys: snap.keys,
            mouse: snap.mouse,
            note: cfg.note.clone(),
            apps: Vec::new(),
            workspaces: 0,
        });
    }

    // A compositor hiccup or a missing `grim` must not cost the whole minute.
    // The window, the open apps and the input counts are still true, and the
    // server treats an image-less frame as a first-class case already -- it is
    // the only kind the phone can send.
    let image = match capture::screenshot(cfg.width).and_then(|img| capture::to_jpeg(&img)) {
        Ok(jpeg) => Some(base64_encode(&jpeg)),
        Err(e) => {
            eprintln!("screenshot failed, sending metadata only: {e:#}");
            None
        }
    };

    Ok(Frame {
        ts,
        device: cfg.device.clone(),
        window: window.describe(),
        domain,
        image,
        blocked: false,
        idle_secs: snap.idle_secs,
        keys: snap.keys,
        mouse: snap.mouse,
        note: cfg.note.clone(),
        apps: capture::open_apps(),
        workspaces: capture::workspace_count(),
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn post(cfg: &AgentConfig, frame: &Frame) -> Result<FrameAck> {
    let url = format!("{}/v1/frame", cfg.server.trim_end_matches('/'));
    let body = send(&url, frame)?;
    Ok(serde_json::from_str(&body).with_context(|| format!("parsing ack: {body}"))?)
}

/// Post a backlog to the batch endpoint. One request, one reply per frame.
fn post_batch(cfg: &AgentConfig, frames: &[serde_json::Value]) -> Result<Vec<FrameAck>> {
    let url = format!("{}/v1/frames", cfg.server.trim_end_matches('/'));
    let body = send(&url, frames)?;
    Ok(serde_json::from_str(&body).with_context(|| format!("parsing acks: {body}"))?)
}

fn send(url: &str, body: &(impl serde::Serialize + ?Sized)) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        // Ingest no longer waits on the model, so anything past a few seconds
        // of upload is a real fault. Waiting five and a half minutes for it
        // only means the next minute's frame is late too.
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .post(url)
        .json(body)
        .send()
        .with_context(|| format!("posting to {url}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "server returned {status}: {}",
        text.chars().take(300).collect::<String>()
    );
    Ok(text)
}

/// How much of the backlog went, and the ack for `want` if it was in it.
pub struct Drained {
    pub sent: usize,
    pub left: usize,
    pub ack: Option<FrameAck>,
}

/// Hand the server everything it has not acknowledged, oldest first.
///
/// Oldest first because the classifier reads each minute with the previous one
/// as context; a backlog that arrives shuffled is a backlog that is labelled
/// wrong. Files are deleted only after the server has answered 2xx for the
/// batch they were in, so a failure anywhere leaves the rest exactly where they
/// were and the next tick starts from the same place.
pub fn drain(cfg: &AgentConfig, spool: &Spool, want: i64) -> Result<Drained> {
    let start = std::time::Instant::now();
    let pending = spool.pending()?;
    let mut out = Drained {
        sent: 0,
        left: pending.len(),
        ack: None,
    };

    for chunk in pending.chunks(CHUNK) {
        if out.sent > 0 && start.elapsed() >= DRAIN_BUDGET {
            break;
        }

        let mut body = Vec::with_capacity(chunk.len());
        let mut queued = Vec::with_capacity(chunk.len());
        for e in chunk {
            match std::fs::read(&e.path)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            {
                Some(v) => {
                    body.push(v);
                    queued.push(e);
                }
                // It will not parse next minute either, and leaving it in place
                // would wedge every later minute behind it forever.
                None => {
                    eprintln!("spool: discarding unreadable {}", e.path.display());
                    spool.remove(e);
                    out.left -= 1;
                }
            }
        }
        if body.is_empty() {
            continue;
        }

        let acks = post_batch(cfg, &body)?;
        for e in queued {
            spool.remove(e);
        }
        out.sent += body.len();
        out.left -= body.len();
        if out.ack.is_none() {
            out.ack = acks.into_iter().find(|a| a.ts == want);
        }
    }
    Ok(out)
}

/// What became of the minute, in the two characters a log line can spare.
///
/// The category printed next to "queued" is the previous minute's, carried
/// forward until the classifier says otherwise -- so the line reads as a guess
/// rather than as an answer, which is what it is.
pub fn status(ack: &FrameAck) -> &'static str {
    match (ack.classified, ack.pending) {
        (true, _) => "",
        (false, true) => " (queued)",
        (false, false) => " (skipped)",
    }
}

/// One minute: capture it, make it durable, then get it and anything owed
/// before it to the server.
///
/// The write to disk comes before the wire deliberately. A frame that exists
/// only in memory is lost to a kill, a panic, or a lid closing during the
/// request -- and unlike the phone, this client has no system log to rebuild
/// the minute from afterwards.
fn tick(cfg: &AgentConfig, spool: &Spool, frame: &Frame) -> Result<Option<FrameAck>> {
    let entry = spool.push(frame)?;
    let pending = spool.pending()?;

    // Nothing older is waiting, so this minute can go on its own -- and this is
    // the only path that still holds the screenshot, since the spooled copy
    // deliberately does not.
    if pending.len() == 1 && pending[0].ts == entry.ts {
        let ack = post(cfg, frame)?;
        spool.remove(&entry);
        return Ok(Some(ack));
    }

    let d = drain(cfg, spool, frame.ts)?;
    if d.left > 0 {
        println!(
            "{} sent {} of {} spooled minute(s), {} still waiting",
            chrono::Local::now().format("%H:%M"),
            d.sent,
            d.sent + d.left,
            d.left
        );
    } else {
        println!(
            "{} sent {} spooled minute(s), spool empty",
            chrono::Local::now().format("%H:%M"),
            d.sent
        );
    }
    Ok(d.ack)
}

pub fn run(cfg: &AgentConfig) -> Result<()> {
    let input = input::Monitor::start();
    let tabs = browser::Tabs::start(&cfg.device);
    let spool = Spool::open()?;
    println!(
        "agent {} -> {} (spool {})",
        cfg.device,
        cfg.server,
        spool.dir().display()
    );
    // Whatever the last run could not deliver is still owed, and is now older
    // than anything this run will capture.
    if let Ok(n) = spool.pending().map(|p| p.len()) {
        if n > 0 {
            println!("{n} minute(s) carried over from the last run");
        }
    }

    loop {
        let now = chrono::Local::now();
        // Before capture, not after: a spool already over budget must not be
        // allowed to grow by another minute first.
        if let Err(e) = spool.evict(now.timestamp()) {
            eprintln!("{} spool evict: {e:#}", now.format("%H:%M"));
        }

        match build_frame(cfg, &input, &tabs).and_then(|f| tick(cfg, &spool, &f)) {
            Ok(Some(ack)) => println!(
                "{} [{}]{} {}",
                now.format("%H:%M"),
                ack.category,
                status(&ack),
                ack.detail.as_deref().unwrap_or("")
            ),
            // Delivered in a batch whose reply did not cover this minute. It is
            // on the server; there is simply nothing more to say about it.
            Ok(None) => {}
            // A daemon meant to run for months must survive a server restart, a
            // dropped VPN, or a compositor hiccup. Nothing is lost by any of
            // them any more -- the frame is on disk until the server takes it.
            Err(e) => eprintln!(
                "{} error: {e:#} ({} minute(s) spooled)",
                now.format("%H:%M"),
                spool.pending().map(|p| p.len()).unwrap_or(0)
            ),
        }
        sleep_to_next_minute();
    }
}

fn sleep_to_next_minute() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let wait = 60 - (now % 60);
    std::thread::sleep(std::time::Duration::from_secs(wait.max(1)));
}
