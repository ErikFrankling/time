use anyhow::{Context, Result};

use crate::browser;
use crate::capture;
use crate::input;
use crate::config::AgentConfig;
use crate::proto::{Frame, FrameAck};

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

    let img = capture::screenshot(cfg.width)?;
    let jpeg = capture::to_jpeg(&img)?;
    drop(img);

    Ok(Frame {
        ts,
        device: cfg.device.clone(),
        window: window.describe(),
        domain,
        image: Some(base64_encode(&jpeg)),
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
    let client = reqwest::blocking::Client::builder()
        // Ingest no longer waits on the model, so anything past a few seconds
        // of upload is a real fault. Waiting five and a half minutes for it
        // only means the next minute's frame is late too.
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let req = client.post(&url).json(frame);

    let resp = req.send().with_context(|| format!("posting to {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "server returned {status}: {}",
        body.chars().take(300).collect::<String>()
    );
    Ok(serde_json::from_str(&body).with_context(|| format!("parsing ack: {body}"))?)
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

pub fn run(cfg: &AgentConfig) -> Result<()> {
    let input = input::Monitor::start();
    let tabs = browser::Tabs::start(&cfg.device);
    println!("agent {} -> {}", cfg.device, cfg.server);
    loop {
        match build_frame(cfg, &input, &tabs).and_then(|f| post(cfg, &f)) {
            Ok(ack) => println!(
                "{} [{}]{} {}",
                chrono::Local::now().format("%H:%M"),
                ack.category,
                status(&ack),
                ack.detail.as_deref().unwrap_or("")
            ),
            // A daemon meant to run for months must survive a server restart, a
            // dropped VPN, or a compositor hiccup.
            Err(e) => eprintln!("{} error: {e:#}", chrono::Local::now().format("%H:%M")),
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
