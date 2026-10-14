//! System tray indicator (cross-platform façade).
//!
//! Linux uses StatusNotifierItem via `ksni`; Windows uses Shell_NotifyIcon
//! via `tray-icon`. Both backends push user actions into a single async
//! channel that a GTK main-loop task awaits, so the rest of the codebase
//! doesn't need to know which backend is active — `tray::spawn()` returns a
//! `(Receiver<Command>, Handle)` pair on every platform.
//!
//! On targets we don't ship for (today: anything other than Linux/Windows),
//! `spawn()` returns `None` so the app silently runs without a tray icon.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod stub;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use stub as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// The BigLace icon as raw 32×32 RGBA, for backends that take pixels instead
/// of an icon-theme name (Windows). Rasterized from
/// `usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg`:
/// `rsvg-convert -w 32 -h 32 … | magick - -depth 8 rgba:src/tray/icon-32.rgba`
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const ICON_RGBA_32: &[u8] = include_bytes!("tray/icon-32.rgba");

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
pub fn spawn() -> Option<(async_channel::Receiver<Command>, Handle)> {
    imp::spawn().map(|(rx, inner)| (rx, Handle { inner }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_tray_icon_is_a_32px_rgba_image() {
        assert_eq!(super::ICON_RGBA_32.len(), 32 * 32 * 4);
        // Not blank: a good share of pixels are visible.
        let visible = super::ICON_RGBA_32
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] > 0)
            .count();
        assert!(visible > 32 * 32 / 4, "only {visible} visible pixels");
    }
}
