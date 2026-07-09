//! Linux backend: StatusNotifierItem (D-Bus) via `ksni`.
//!
//! Runs on a dedicated std::thread (via `ksni`'s blocking adapter) and pushes
//! user actions back to the GTK main loop through an std::sync::mpsc channel.
//! The GTK side owns the widgets, so the tray never touches them directly —
//! it only emits commands and lets the main loop translate them into the
//! existing connect/disconnect/show flows.

use std::sync::mpsc;

use ksni::{
    blocking::{Handle as KsniHandle, TrayMethods},
    menu::StandardItem,
    MenuItem, ToolTip, Tray,
};

use super::Command;
use crate::{tr, trf};

pub struct BigLaceTray {
    pub connected: bool,
    pub peer_count: usize,
    sender: mpsc::Sender<Command>,
}

impl BigLaceTray {
    fn send(&self, cmd: Command) {
        // The receiver lives on the GTK main loop; if it's gone, the app is
        // shutting down and a dropped command is fine.
        let _ = self.sender.send(cmd);
    }
}

impl Tray for BigLaceTray {
    fn id(&self) -> String {
        "org.communitybig.biglace".into()
    }

    fn title(&self) -> String {
        "BigLace".into()
    }

    fn icon_name(&self) -> String {
        // Symbolic SVG ships under usr/share/icons/hicolor/symbolic/apps/.
        // Themes recolor it to the panel's foreground, so we get a true
        // monochrome indicator on light and dark panels alike.
        "org.communitybig.biglace-symbolic".into()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if self.connected {
            match self.peer_count {
                0 => tr!("Connected"),
                1 => tr!("Connected · 1 peer"),
                n => trf!("Connected · {n} peers", "n" => n),
            }
        } else {
            tr!("Disconnected")
        };
        ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: vec![],
            title: "BigLace".into(),
            description,
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-click on the indicator brings the window to the front.
        self.send(Command::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let connect_or_disconnect: MenuItem<Self> = if self.connected {
            StandardItem {
                label: tr!("Disconnect"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Disconnect)),
                ..Default::default()
            }
            .into()
        } else {
            StandardItem {
                label: tr!("Connect"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Connect)),
                ..Default::default()
            }
            .into()
        };

        vec![
            StandardItem {
                label: tr!("Show BigLace"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Show)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            connect_or_disconnect,
            MenuItem::Separator,
            StandardItem {
                label: tr!("Quit"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Linux-specific handle wrapper. Holds the ksni `Handle` and exposes only
/// the cross-platform `update()` method our facade wants.
pub struct HandleImpl(KsniHandle<BigLaceTray>);

impl HandleImpl {
    pub fn update(&self, connected: bool, peer_count: usize) {
        self.0.update(|t| {
            t.connected = connected;
            t.peer_count = peer_count;
        });
    }
}

/// Returns `None` if the D-Bus session bus is unreachable — common on
/// headless systems and in some sandboxed flatpak runs. The app keeps
/// working without a tray in that case.
///
/// `assume_sni_available(true)` is what makes the `--hidden` autostart launch
/// behave at login: when biglace is started by the session before the panel /
/// shell has registered a `StatusNotifierWatcher`, ksni would otherwise return
/// `Error::Watcher`/`Error::WontShow` here, we'd report the tray as inactive,
/// and the window would pop open instead of staying in the tray. With this
/// flag a missing watcher is a soft error: `spawn()` succeeds and a background
/// task re-registers the item automatically once the watcher appears moments
/// later. A genuine hard D-Bus failure (no session bus at all) still returns
/// `Err`, so the headless fallback of presenting the window is preserved.
pub fn spawn() -> Option<(mpsc::Receiver<Command>, HandleImpl)> {
    let (tx, rx) = mpsc::channel();
    let tray = BigLaceTray {
        connected: false,
        peer_count: 0,
        sender: tx,
    };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some((rx, HandleImpl(handle))),
        Err(e) => {
            eprintln!("[biglace] tray: failed to register StatusNotifierItem: {e}");
            None
        }
    }
}
