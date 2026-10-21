//! A simulated world for end-to-end tests.
//!
//! A [`scenario::Scenario`] scripts a day: what is on each desktop, what the
//! phone does, when devices are offline. The [`world::World`] then runs the
//! *real* Time server and the *real* desktop agent against it on a virtual
//! clock -- the agent reads a fake Hyprland socket and simulated input,
//! audio and browser tabs; a simulated phone sends what the Android app
//! sends -- and tests assert on what the server ends up knowing.
//!
//! Nothing here touches the real compositor, the real phone or the live
//! server.

pub mod fake_hyprland;
pub mod phone;
pub mod scenario;
pub mod world;
