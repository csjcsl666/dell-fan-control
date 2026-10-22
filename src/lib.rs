//! Dell Fan Control
//!
//! Library part of the `dell-fan-control` binary: hardware access, the
//! control core, persistence, IPC, the background service and the GUI

pub mod config;
pub mod control;
pub mod daemon;
pub mod gui;
pub mod hw;
pub mod icon;
pub mod ipc;
pub mod profile;
pub mod state;
pub mod sys;
pub mod tray;
