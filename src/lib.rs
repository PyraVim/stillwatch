//! `stillwatch` — a watchdog for processes that run unattended.
//!
//! The binary is a thin wrapper around this library so that the integration
//! tests in `tests/` can drive the same code the binary runs.

pub mod config;
pub mod evaluate;
pub mod fmt;
pub mod notify;
pub mod receiver;
pub mod state;
