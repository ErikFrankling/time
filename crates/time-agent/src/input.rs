//! Keyboard and pointer activity from evdev, counted, never recorded.
//!
//! Ported from v1, whose two rules still hold:
//!
//! 1. Virtual devices are excluded. Agents driving the desktop (ydotool,
//!    computer use) inject through uinput and would otherwise look exactly
//!    like Erik typing.
//! 2. Key identity is never captured. A key press increments a counter and
//!    the keycode is dropped in the same expression; no code path here can
//!    emit which key was pressed.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::probe::{Input, InputSnapshot};

const VIRTUAL_MARKERS: [&str; 4] = ["ydotool", "virtual", "uinput", "dotool"];

#[derive(Default)]
struct State {
    last_ms: AtomicU64,
    keys: AtomicU32,
    mouse: AtomicU32,
    devices: AtomicU32,
}

pub struct Evdev {
    state: Arc<State>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn is_virtual(name: &str) -> bool {
    let n = name.to_lowercase();
    VIRTUAL_MARKERS.iter().any(|m| n.contains(m))
}

impl Evdev {
    /// Watch every physical keyboard and pointer on a thread each. Devices
    /// that cannot be opened are skipped; none at all means input is
    /// reported as unknown, which the heartbeat says out loud.
    pub fn start() -> Self {
        let state = Arc::new(State::default());
        state.last_ms.store(now_ms(), Relaxed);
        for (path, device) in evdev::enumerate() {
            let name = device.name().unwrap_or("").to_string();
            if is_virtual(&name) {
                continue;
            }
            let ev = device.supported_events();
            let keys = ev.contains(evdev::EventType::KEY);
            let motion =
                ev.contains(evdev::EventType::RELATIVE) || ev.contains(evdev::EventType::ABSOLUTE);
            if !keys && !motion {
                continue;
            }
            state.devices.fetch_add(1, Relaxed);
            let state = state.clone();
            std::thread::spawn(move || {
                let Ok(mut device) = evdev::Device::open(&path) else {
                    state.devices.fetch_sub(1, Relaxed);
                    return;
                };
                while let Ok(events) = device.fetch_events() {
                    for ev in events {
                        match ev.event_type() {
                            evdev::EventType::KEY if ev.value() == 1 => {
                                state.keys.fetch_add(1, Relaxed);
                            }
                            evdev::EventType::RELATIVE | evdev::EventType::ABSOLUTE => {
                                state.mouse.fetch_add(1, Relaxed);
                            }
                            _ => continue,
                        }
                        state.last_ms.store(now_ms(), Relaxed);
                    }
                }
                state.devices.fetch_sub(1, Relaxed);
            });
        }
        Self { state }
    }

    pub fn devices(&self) -> u32 {
        self.state.devices.load(Relaxed)
    }
}

impl Input for Evdev {
    fn problem(&self) -> Option<String> {
        (self.devices() == 0).then(|| {
            "input: no readable input devices (is the user in the input group?)".to_owned()
        })
    }

    fn take(&mut self) -> InputSnapshot {
        if self.devices() == 0 {
            return InputSnapshot::default();
        }
        let idle_ms = now_ms().saturating_sub(self.state.last_ms.load(Relaxed));
        InputSnapshot {
            idle_seconds: Some((idle_ms / 1000) as u32),
            keys: self.state.keys.swap(0, Relaxed),
            mouse: self.state.mouse.swap(0, Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn virtual_devices_are_recognised() {
        assert!(super::is_virtual("ydotoold virtual device"));
        assert!(!super::is_virtual("Logitech USB Receiver"));
    }
}
