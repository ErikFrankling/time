//! Types shared by the server, the desktop agent and the simulator.
//!
//! Everything that crosses a process boundary is defined here once, so the
//! agent cannot send an event the server does not understand and the server
//! cannot store a label the dashboard cannot draw.

pub mod clock;
pub mod event;
pub mod label;
pub mod nudge;
pub mod otlp;
pub mod rules;
pub mod timeline;
