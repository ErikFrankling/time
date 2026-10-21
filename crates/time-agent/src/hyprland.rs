//! Hyprland's IPC socket, spoken directly.
//!
//! v1 forked `hyprctl` three or four times a minute. The socket protocol is
//! one line in, one JSON document out, connection closed -- simpler than
//! the subprocess, and it is what the simulator fakes.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::probe::{Desktop, Window};

pub struct Hyprland {
    socket: PathBuf,
}

impl Hyprland {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// The running session's socket, found the way `hyprctl` finds it.
    pub fn from_env() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .context("HYPRLAND_INSTANCE_SIGNATURE is not set (is Hyprland running?)")?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let modern = PathBuf::from(&runtime)
            .join("hypr")
            .join(&sig)
            .join(".socket.sock");
        let legacy = PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock");
        Ok(Self::new(if modern.exists() || !legacy.exists() {
            modern
        } else {
            legacy
        }))
    }

    /// Send `j/<command>` and parse the JSON reply.
    pub fn request(&self, command: &str) -> Result<Value> {
        let mut s = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.set_write_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(format!("j/{command}").as_bytes())?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        if buf.is_empty() {
            bail!("hyprland sent an empty reply to {command}");
        }
        serde_json::from_slice(&buf)
            .with_context(|| format!("hyprland reply to {command} is not JSON"))
    }
}

impl Desktop for Hyprland {
    fn active_window(&mut self) -> Result<Option<Window>> {
        Ok(parse_active_window(&self.request("activewindow")?))
    }
}

/// `{}` means nothing has focus. `fullscreen` is a bool in older Hyprland and
/// a mode number (0 none, 1 maximised, 2 fullscreen) in newer.
pub fn parse_active_window(v: &Value) -> Option<Window> {
    let class = v.get("class").and_then(Value::as_str)?;
    let title = v.get("title").and_then(Value::as_str).unwrap_or_default();
    let fullscreen = match v.get("fullscreen") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64() == Some(2),
        _ => false,
    };
    Some(Window {
        class: class.to_owned(),
        title: title.to_owned(),
        fullscreen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_fullscreen_encodings_and_empty() {
        assert_eq!(parse_active_window(&json!({})), None);
        let w =
            parse_active_window(&json!({"class": "mpv", "title": "x", "fullscreen": 2})).unwrap();
        assert!(w.fullscreen);
        let w =
            parse_active_window(&json!({"class": "kitty", "title": "x", "fullscreen": 1})).unwrap();
        assert!(!w.fullscreen, "maximised is not fullscreen");
        assert!(
            parse_active_window(&json!({"class": "a", "fullscreen": true}))
                .unwrap()
                .fullscreen
        );
    }
}
