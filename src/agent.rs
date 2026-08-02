use anyhow::{Context, Result};

use crate::capture;
use crate::input;
use crate::config::AgentConfig;
use crate::proto::{Frame, FrameAck};

/// Build this minute's frame. The only decision the agent makes on its own is
/// the blocklist -- and it errs toward sending nothing.
pub fn build_frame(cfg: &AgentConfig, input: &input::Monitor) -> Result<Frame> {
    let now = chrono::Local::now().timestamp();
    let ts = now - (now % 60);
    let window = capture::active_window();
    let snap = input.take();

    if window.blocked(&cfg.blocklist) {
        return Ok(Frame {
            ts,
            device: cfg.device.clone(),
            window: String::new(), // the title itself may be sensitive
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
        .timeout(std::time::Duration::from_secs(330))
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

pub fn run(cfg: &AgentConfig) -> Result<()> {
    let input = input::Monitor::start();
    println!("agent {} -> {}", cfg.device, cfg.server);
    loop {
        match build_frame(cfg, &input).and_then(|f| post(cfg, &f)) {
            Ok(ack) => println!(
                "{} [{}]{} {}",
                chrono::Local::now().format("%H:%M"),
                ack.category,
                if ack.classified { "" } else { " (skipped)" },
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
