//! Human input detection.
//!
//! The point of this module is one question the screenshot cannot answer: **is
//! the person actually here?** A screen that is changing proves nothing --- a
//! build, a video, or an AI computer-use agent all repaint it while the user is
//! in another room.
//!
//! Two rules make the signal trustworthy:
//!
//! 1. **Virtual devices are excluded.** Tools like `ydotool` inject through
//!    uinput, so agent-driven input shows up in `/dev/input` looking exactly
//!    like a human. Filtering those out is the whole human/agent boundary.
//! 2. **Key identity is never captured.** Events are counted and the keycode is
//!    dropped immediately. There is no code path here that can emit which key
//!    was pressed --- that is a property of the code, not a policy.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Substrings marking a device as synthetic rather than human-operated.
const VIRTUAL_MARKERS: [&str; 4] = ["ydotool", "virtual", "uinput", "dotool"];

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    /// Seconds since the last human input event. None when no device could be
    /// monitored at all, which must be reported honestly rather than as zero.
    pub idle_secs: Option<u32>,
    pub keys: u32,
    pub mouse: u32,
}

#[derive(Default)]
struct State {
    last_ms: AtomicU64,
    keys: AtomicU32,
    mouse: AtomicU32,
    devices: AtomicU32,
}

pub struct Monitor {
    state: Arc<State>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_virtual(name: &str) -> bool {
    let n = name.to_lowercase();
    VIRTUAL_MARKERS.iter().any(|m| n.contains(m))
}

impl Monitor {
    /// Start watching every physical keyboard and pointer. Failing to open a
    /// device is not fatal --- permissions vary, and partial coverage still
    /// answers the question.
    pub fn start() -> Self {
        let state = Arc::new(State::default());
        state.last_ms.store(now_ms(), Ordering::Relaxed);

        for (path, device) in evdev::enumerate() {
            let name = device.name().unwrap_or("").to_string();
            if is_virtual(&name) {
                eprintln!("input: ignoring virtual device {name:?}");
                continue;
            }

            let events = device.supported_events();
            let has_keys = events.contains(evdev::EventType::KEY);
            let has_motion =
                events.contains(evdev::EventType::RELATIVE) || events.contains(evdev::EventType::ABSOLUTE);
            if !has_keys && !has_motion {
                continue;
            }

            state.devices.fetch_add(1, Ordering::Relaxed);
            let state = state.clone();
            std::thread::spawn(move || {
                let mut device = match evdev::Device::open(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("input: cannot open {}: {e}", path.display());
                        return;
                    }
                };
                loop {
                    match device.fetch_events() {
                        Ok(events) => {
                            for ev in events {
                                match ev.event_type() {
                                    // Count the press only, and drop the code
                                    // without ever storing or forwarding it.
                                    evdev::EventType::KEY if ev.value() == 1 => {
                                        state.keys.fetch_add(1, Ordering::Relaxed);
                                    }
                                    evdev::EventType::RELATIVE | evdev::EventType::ABSOLUTE => {
                                        state.mouse.fetch_add(1, Ordering::Relaxed);
                                    }
                                    _ => continue,
                                }
                                state.last_ms.store(now_ms(), Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            eprintln!("input: {} closed: {e}", path.display());
                            return;
                        }
                    }
                }
            });
        }

        let n = state.devices.load(Ordering::Relaxed);
        if n == 0 {
            eprintln!(
                "input: no readable input devices — presence detection is off. \
                 Add this user to the 'input' group."
            );
        } else {
            eprintln!("input: monitoring {n} device(s)");
        }

        Self { state }
    }

    /// Read the counters and reset them for the next minute.
    pub fn take(&self) -> Snapshot {
        if self.state.devices.load(Ordering::Relaxed) == 0 {
            return Snapshot::default();
        }
        let idle_ms = now_ms().saturating_sub(self.state.last_ms.load(Ordering::Relaxed));
        Snapshot {
            idle_secs: Some((idle_ms / 1000) as u32),
            keys: self.state.keys.swap(0, Ordering::Relaxed),
            mouse: self.state.mouse.swap(0, Ordering::Relaxed),
        }
    }
}
