//! What the agent can observe, as traits.
//!
//! Production implementations talk to Hyprland, evdev, MPRIS and the browser
//! extension; the simulator implements the same traits from a scripted day.

use anyhow::Result;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Window {
    pub class: String,
    pub title: String,
    pub fullscreen: bool,
}

/// The compositor: what has focus.
pub trait Desktop: Send {
    /// `Ok(None)` when nothing has focus (an empty workspace) -- normal, not
    /// an error. `Err` means the compositor could not be asked at all.
    fn active_window(&mut self) -> Result<Option<Window>>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputSnapshot {
    pub keys: u32,
    pub mouse: u32,
    pub clicks: u32,
    /// Seconds with any input since the previous call; None when unknown.
    pub active_seconds: Option<u32>,
    /// None when no input device could be read: unknown, not idle.
    pub idle_seconds: Option<u32>,
}

/// Input counts since the previous call.
pub trait Input: Send {
    fn take(&mut self) -> InputSnapshot;
    /// Why this channel is not working, if it is not.
    fn problem(&self) -> Option<String> {
        None
    }
}

/// What was audible since the previous call.
pub trait Audio: Send {
    fn take(&mut self) -> crate::audio::Heard;
    fn problem(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub host: String,
    pub title: String,
}

/// The browser extension's view of front tabs.
pub trait Tabs: Send {
    /// The front tab of whichever browser window currently has focus.
    ///
    /// Several browsers report through the same extension and some (Firefox
    /// and its forks) report under the same name, so a tab is matched to the
    /// focused window by its title, which every browser puts in the window
    /// title. Matching by browser name instead is how v1 billed a Firefox
    /// window showing dn.se to play.hbomax.com open in Zen.
    fn front_tab(&mut self, focused: &Window) -> Option<Tab>;
    fn problem(&self) -> Option<String> {
        None
    }
}

/// Nothing to observe. For channels not available on a machine.
pub struct Nothing;

impl Input for Nothing {
    fn take(&mut self) -> InputSnapshot {
        InputSnapshot::default()
    }
}

impl Audio for Nothing {
    fn take(&mut self) -> crate::audio::Heard {
        crate::audio::Heard::default()
    }
}

impl Tabs for Nothing {
    fn front_tab(&mut self, _: &Window) -> Option<Tab> {
        None
    }
}

/// Pick the reported tab whose title appears in the focused window's title.
/// Longest title wins, so "YouTube" never beats "Some video - YouTube".
pub fn match_tab<'a>(focused: &Window, tabs: impl IntoIterator<Item = &'a Tab>) -> Option<Tab> {
    tabs.into_iter()
        .filter(|t| !t.title.is_empty() && focused.title.contains(&t.title))
        .max_by_key(|t| t.title.len())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_match_the_focused_window_by_title() {
        let w = Window {
            class: "firefox".into(),
            title: "Krönika — DN — Mozilla Firefox".into(),
            fullscreen: false,
        };
        let tabs = [
            Tab {
                host: "play.hbomax.com".into(),
                title: "Angel Has Fallen • HBO Max".into(),
            },
            Tab {
                host: "dn.se".into(),
                title: "Krönika — DN".into(),
            },
            Tab {
                host: "x.com".into(),
                title: "DN".into(),
            },
        ];
        assert_eq!(match_tab(&w, &tabs).unwrap().host, "dn.se");
        let other = Window {
            title: "Terminal".into(),
            ..w
        };
        assert!(match_tab(&other, &tabs).is_none());
    }
}
