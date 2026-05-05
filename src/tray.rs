//! StatusNotifierItem (system tray) integration.
//!
//! Runs on a dedicated std::thread (via `ksni`'s blocking adapter) and pushes
//! user actions back to the GTK main loop through an std::sync::mpsc channel.
//! The GTK side owns the widgets, so the tray never touches them directly —
//! it only emits commands and lets the main loop translate them into the
//! existing connect/disconnect/show flows.

use std::sync::mpsc;

use ksni::{
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
    MenuItem, ToolTip, Tray,
};

use crate::{tr, trf};

#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    /// Bring the main window to the foreground (or unhide it).
    Show,
    /// User asked to (re)connect from the tray menu.
    Connect,
    /// User asked to disconnect from the tray menu.
    Disconnect,
    /// User asked to quit the application from the tray menu.
    Quit,
}

pub struct BigLaceTray {
    pub connected:  bool,
    pub peer_count: usize,
    sender: mpsc::Sender<TrayCommand>,
}

impl BigLaceTray {
    fn send(&self, cmd: TrayCommand) {
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
        self.send(TrayCommand::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let connect_or_disconnect: MenuItem<Self> = if self.connected {
            StandardItem {
                label: tr!("Disconnect"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(TrayCommand::Disconnect)),
                ..Default::default()
            }
            .into()
        } else {
            StandardItem {
                label: tr!("Connect"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(TrayCommand::Connect)),
                ..Default::default()
            }
            .into()
        };

        vec![
            StandardItem {
                label: tr!("Show BigLace"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(TrayCommand::Show)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            connect_or_disconnect,
            MenuItem::Separator,
            StandardItem {
                label: tr!("Quit"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the SNI service on a background thread. Returns the receiver the GTK
/// main loop must poll for user actions, plus a handle that lets the GTK side
/// push status updates back into the indicator (tooltip, menu label).
///
/// Returns `None` if the D-Bus session bus is unreachable — common on
/// headless systems and in some sandboxed flatpak runs. The app keeps
/// working without a tray in that case.
pub fn spawn() -> Option<(mpsc::Receiver<TrayCommand>, Handle<BigLaceTray>)> {
    let (tx, rx) = mpsc::channel();
    let tray = BigLaceTray {
        connected:  false,
        peer_count: 0,
        sender:     tx,
    };
    match tray.spawn() {
        Ok(handle) => Some((rx, handle)),
        Err(e) => {
            eprintln!("[biglace] tray: failed to register StatusNotifierItem: {e}");
            None
        }
    }
}
