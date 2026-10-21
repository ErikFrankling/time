//! A Unix socket that answers like Hyprland's IPC socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use time_agent::probe::Window;

/// What the fake compositor currently shows. Shared with the world, which
/// changes it minute by minute.
pub type Focus = Arc<Mutex<Option<Window>>>;

pub struct FakeHyprland {
    pub socket: PathBuf,
    pub focus: Focus,
}

impl FakeHyprland {
    /// Listen at `dir/.socket.sock` on a background thread.
    pub fn start(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let socket = dir.join(".socket.sock");
        let listener = UnixListener::bind(&socket)?;
        let focus: Focus = Arc::new(Mutex::new(None));
        let state = focus.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                // Hyprland reads one request and does not wait for EOF.
                let mut buf = [0u8; 256];
                let n = s.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = match request.trim() {
                    "j/activewindow" => match &*state.lock().unwrap() {
                        None => json!({}),
                        Some(w) => json!({
                            "class": w.class,
                            "title": w.title,
                            "fullscreen": if w.fullscreen { 2 } else { 0 },
                        }),
                    },
                    other => json!({ "error": format!("unknown request {other}") }),
                };
                let _ = s.write_all(reply.to_string().as_bytes());
            }
        });
        Ok(Self { socket, focus })
    }
}
