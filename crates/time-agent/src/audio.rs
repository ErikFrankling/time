//! Whether anything is playing, from MPRIS via `playerctl`.
//!
//! Browsers, Spotify and video players all publish MPRIS. One `playerctl`
//! call a minute is cheaper than holding a D-Bus connection open, and a
//! missing `playerctl` is reported, not guessed around.

use std::process::Command;

use crate::probe::Audio;

#[derive(Default)]
pub struct Mpris {
    pub last_error: Option<String>,
}

impl Audio for Mpris {
    fn problem(&self) -> Option<String> {
        self.last_error.clone()
    }

    fn playing(&mut self) -> bool {
        match Command::new("playerctl")
            .args(["--all-players", "status"])
            .output()
        {
            Ok(out) => {
                self.last_error = None;
                any_playing(&String::from_utf8_lossy(&out.stdout))
            }
            Err(e) => {
                self.last_error = Some(format!("playerctl: {e}"));
                false
            }
        }
    }
}

pub fn any_playing(status: &str) -> bool {
    status.lines().any(|l| l.trim() == "Playing")
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_status_lines() {
        assert!(super::any_playing("Paused\nPlaying\n"));
        assert!(!super::any_playing("Paused\nStopped\n"));
        assert!(!super::any_playing("No players found"));
    }
}
