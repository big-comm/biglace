//! Fallback for targets we haven't ported yet (today: macOS and friends).
//! `spawn()` returns `None`, so the rest of the app silently runs without
//! a tray indicator.

use std::sync::mpsc;

use super::Command;

pub struct HandleImpl;

impl HandleImpl {
    pub fn update(&self, _connected: bool, _peer_count: usize) {}
}

pub fn spawn() -> Option<(mpsc::Receiver<Command>, HandleImpl)> {
    None
}
