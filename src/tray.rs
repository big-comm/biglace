//! System tray indicator (cross-platform façade).
//!
//! Linux uses StatusNotifierItem via `ksni`; Windows uses Shell_NotifyIcon
//! via `tray-icon`. Both backends push user actions into a single mpsc
//! channel that the GTK main loop polls, so the rest of the codebase doesn't
//! need to know which backend is active — `tray::spawn()` returns a
//! `(Receiver<Command>, Handle)` pair on every platform.
//!
//! On targets we don't ship for (today: anything other than Linux/Windows),
//! `spawn()` returns `None` so the app silently runs without a tray icon.

use std::sync::mpsc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod stub;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "windows")]
use windows as imp;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use stub as imp;

/// User-initiated actions emitted by the tray menu / icon clicks.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    /// Bring the main window to the foreground (or unhide it).
    Show,
    /// User asked to (re)connect from the tray menu.
    Connect,
    /// User asked to disconnect from the tray menu.
    Disconnect,
    /// User asked to quit the application from the tray menu.
    Quit,
}

/// Cross-platform handle that lets the GTK main loop push status updates
/// into the tray indicator (tooltip, connect/disconnect label flip).
pub struct Handle {
    inner: imp::HandleImpl,
}

impl Handle {
    /// Reflect the current connection state on the tray icon. Cheap and
    /// idempotent — call from `refresh_state` regardless of whether
    /// anything actually changed.
    pub fn update(&self, connected: bool, peer_count: usize) {
        self.inner.update(connected, peer_count);
    }
}

/// Spawn the platform-appropriate tray service. Returns `None` when the
/// backend can't initialize (no D-Bus session on Linux, tray subsystem
/// failure on Windows, unsupported target). Callers must keep the returned
/// `Handle` alive for as long as they want the icon visible.
pub fn spawn() -> Option<(mpsc::Receiver<Command>, Handle)> {
    imp::spawn().map(|(rx, inner)| (rx, Handle { inner }))
}
