//! The desktop agent.
//!
//! Each minute it asks the system what is in front, how much input there
//! was and whether audio is playing, turns that into one
//! `time.screen.minute` event, spools it, and sends the spool. Every system
//! call goes through a trait in [`probe`], so the simulator can stand in for
//! Hyprland, the input devices, the audio stack and the browser.

pub mod agent;
pub mod audio;
pub mod browser;
pub mod config;
pub mod hyprland;
pub mod input;
pub mod probe;
pub mod sender;
pub mod spool;
pub mod system;

pub use agent::Agent;
pub use config::AgentConfig;
