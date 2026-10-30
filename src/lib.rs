//! Shared library for the Linux audio focus daemon (`lafd`).
//!
//! The daemon watches MPRIS players on the session bus and applies
//! Android-style audio focus: a player that starts playing pauses the others,
//! and a trigger (call, alarm, ...) pauses everything until it ends.

pub mod config;
pub mod focus;
pub mod mpris;
pub mod protocol;
