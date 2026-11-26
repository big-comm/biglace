//! Fallback for targets we haven't ported yet (today: macOS and friends).
//! `spawn()` returns `None`, so the rest of the app silently runs without
//! a tray indicator.

use super::Command;

pub struct HandleImpl;

impl HandleImpl {
    pub fn update(&self, _connected: bool, _peer_count: usize, _local: bool) {}
    pub fn update_room(&self, _room: Option<(String, bool)>) {}
    pub fn update_unread(&self, _count: u32) {}
    pub fn update_visits(&self, _visits: Vec<(String, &'static str)>) {}
}

pub fn spawn() -> Option<(async_channel::Receiver<Command>, HandleImpl)> {
    None
}
