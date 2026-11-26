//! Linux backend: StatusNotifierItem (D-Bus) via `ksni`.
//!
//! Runs on a dedicated std::thread (via `ksni`'s blocking adapter) and pushes
//! user actions back to the GTK main loop through an async channel.
//! The GTK side owns the widgets, so the tray never touches them directly —
//! it only emits commands and lets the main loop translate them into the
//! existing connect/disconnect/show flows.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::{cairo, glib};
use ksni::{
    blocking::{Handle as KsniHandle, TrayMethods},
    menu::StandardItem,
    Icon, MenuItem, ToolTip, Tray,
};

use super::Command;
use crate::{tr, trf};

/// A new message on the tray icon: it travels from one node through the
/// center to another. Symbolic icons, recolored by the panel like the icon.
const MESSAGE_FRAMES: [&str; 8] = [
    "org.communitybig.biglace-tray-message-1-symbolic",
    "org.communitybig.biglace-tray-message-2-symbolic",
    "org.communitybig.biglace-tray-message-3-symbolic",
    "org.communitybig.biglace-tray-message-4-symbolic",
    "org.communitybig.biglace-tray-message-5-symbolic",
    "org.communitybig.biglace-tray-message-6-symbolic",
    "org.communitybig.biglace-tray-message-7-symbolic",
    "org.communitybig.biglace-tray-message-8-symbolic",
];
const FRAME_TIME: Duration = Duration::from_millis(90);
/// Side of the unread badge drawn over the icon, in pixels.
const BADGE_SIDE: i32 = 32;

pub struct BigLaceTray {
    pub connected: bool,
    pub peer_count: usize,
    /// Only the local network is on (BigScale left for it).
    local: bool,
    /// In a voice room: its group and whether we're muted.
    pub room: Option<(String, bool)>,
    /// People in this computer (terminal, files, screen): a line each, and the icon for it.
    visits: Vec<(String, &'static str)>,
    /// The new-message animation's frame on show, if it plays.
    frame: Option<&'static str>,
    /// The unread count over the icon (none when nothing is unread).
    badge: Vec<Icon>,
    sender: async_channel::Sender<Command>,
}

impl BigLaceTray {
    fn send(&self, cmd: Command) {
        // The receiver lives on the GTK main loop; if it's gone, the app is
        // shutting down and a dropped command is fine.
        let _ = self.sender.try_send(cmd);
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
        // monochrome indicator on light and dark panels alike. Not named
        // "<app id>-symbolic": docks like XFCE's docklike prefer that name
        // over the app icon and would show the bare glyph in the dock.
        if let Some((_, icon)) = self.visits.first() {
            // Someone is in this computer: that matters most.
            return (*icon).into();
        }
        match &self.room {
            // In a room: a microphone, so it's obvious at a glance.
            Some((_, false)) => "audio-input-microphone-symbolic".into(),
            Some((_, true)) => "microphone-disabled-symbolic".into(),
            None => self.frame.unwrap_or("org.communitybig.biglace-tray-symbolic").into(),
        }
    }

    fn overlay_icon_pixmap(&self) -> Vec<Icon> {
        self.badge.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if self.connected {
            match self.peer_count {
                0 => tr!("Connected over the internet"),
                1 => tr!("Over the internet · 1 peer"),
                n => trf!("Over the internet · {n} peers", "n" => n),
            }
        } else if self.local {
            tr!("Local network")
        } else {
            tr!("Disconnected")
        };
        let description = match &self.room {
            Some((group, muted)) => {
                let line = if *muted {
                    trf!("In the room {group} · muted", "group" => group)
                } else {
                    trf!("In the room {group}", "group" => group)
                };
                format!("{line}\n{description}")
            }
            None => description,
        };
        let description = if self.visits.is_empty() {
            description
        } else {
            let lines: Vec<&str> = self.visits.iter().map(|(line, _)| line.as_str()).collect();
            format!("{}\n{description}", lines.join("\n"))
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

        let mut items: Vec<MenuItem<Self>> = vec![StandardItem {
            label: tr!("Show BigLace"),
            activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Show)),
            ..Default::default()
        }
        .into()];
        if !self.visits.is_empty() {
            items.push(MenuItem::Separator);
            for (line, icon) in &self.visits {
                items.push(
                    StandardItem {
                        label: line.clone(),
                        icon_name: (*icon).into(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.push(
                StandardItem {
                    label: tr!("End remote access"),
                    icon_name: "process-stop-symbolic".into(),
                    activate: Box::new(|t: &mut BigLaceTray| t.send(Command::EndVisits)),
                    ..Default::default()
                }
                .into(),
            );
        }
        if let Some((title, muted)) = &self.room {
            items.push(MenuItem::Separator);
            items.push(
                StandardItem {
                    label: title.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: if *muted { tr!("Unmute (Ctrl+Alt+M)") } else { tr!("Mute (Ctrl+Alt+M)") },
                    icon_name: if *muted { "microphone-sensitivity-high-symbolic" } else { "microphone-disabled-symbolic" }.into(),
                    activate: Box::new(|t: &mut BigLaceTray| t.send(Command::ToggleMute)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: tr!("Leave the room"),
                    icon_name: "call-stop-symbolic".into(),
                    activate: Box::new(|t: &mut BigLaceTray| t.send(Command::LeaveRoom)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
            MenuItem::Separator,
            connect_or_disconnect,
            MenuItem::Separator,
            StandardItem {
                label: tr!("Quit"),
                activate: Box::new(|t: &mut BigLaceTray| t.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

/// Linux-specific handle wrapper. Holds the ksni `Handle` and exposes only
/// the cross-platform `update()` method our facade wants.
pub struct HandleImpl {
    handle: KsniHandle<BigLaceTray>,
    /// The unread count last shown (none before the first).
    unread: Cell<Option<u32>>,
    animating: Rc<Cell<bool>>,
}

impl HandleImpl {
    pub fn update(&self, connected: bool, peer_count: usize, local: bool) {
        self.handle.update(|t| {
            t.connected = connected;
            t.peer_count = peer_count;
            t.local = local;
        });
    }

    pub fn update_room(&self, room: Option<(String, bool)>) {
        self.handle.update(|t| t.room = room);
    }

    pub fn update_visits(&self, visits: Vec<(String, &'static str)>) {
        self.handle.update(|t| t.visits = visits);
    }

    /// Show `count` over the icon; a count that grew plays the new-message
    /// animation (not the first one shown, which only says what's waiting).
    pub fn update_unread(&self, count: u32) {
        let grew = self.unread.replace(Some(count)).is_some_and(|before| count > before);
        let badge = match unread_badge(count) {
            Ok(badge) => badge,
            Err(e) => {
                crate::tailscale::dbg(&format!("tray: could not draw the unread count: {e}"));
                vec![]
            }
        };
        self.handle.update(|t| t.badge = badge);
        if grew && !self.animating.replace(true) {
            let (handle, animating) = (self.handle.clone(), self.animating.clone());
            let mut frames = MESSAGE_FRAMES.iter();
            glib::timeout_add_local(FRAME_TIME, move || {
                let frame = frames.next().copied();
                handle.update(|t| t.frame = frame);
                if frame.is_some() {
                    return glib::ControlFlow::Continue;
                }
                animating.set(false);
                glib::ControlFlow::Break
            });
        }
    }
}

/// `count` in a red disc (`99+` past 99), as the overlay icon; nothing for 0.
fn unread_badge(count: u32) -> Result<Vec<Icon>, cairo::Error> {
    if count == 0 {
        return Ok(vec![]);
    }
    let text = if count > 99 { "99+".to_string() } else { count.to_string() };
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, BADGE_SIDE, BADGE_SIDE)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let mid = f64::from(BADGE_SIDE) / 2.0;
        cr.set_source_rgb(0.89, 0.11, 0.14);
        cr.arc(mid, mid, mid, 0.0, std::f64::consts::TAU);
        cr.fill()?;
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(match text.len() {
            1 => 22.0,
            2 => 18.0,
            _ => 13.0,
        });
        let ext = cr.text_extents(&text)?;
        cr.move_to(mid - ext.width() / 2.0 - ext.x_bearing(), mid - ext.height() / 2.0 - ext.y_bearing());
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.show_text(&text)?;
    }
    surface.flush();
    let pixels = surface.data().map_err(|_| cairo::Error::SurfaceFinished)?;
    Ok(vec![Icon { width: BADGE_SIDE, height: BADGE_SIDE, data: to_sni_argb(&pixels) }])
}

/// Cairo's premultiplied, native-endian ARGB32 to the straight, big-endian
/// ARGB32 a StatusNotifierItem carries.
fn to_sni_argb(cairo_pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cairo_pixels.len());
    for px in cairo_pixels.as_chunks::<4>().0 {
        let [a, r, g, b] = u32::from_ne_bytes(*px).to_be_bytes();
        let straight = |c: u8| if a == 0 { 0 } else { (u16::from(c) * 255 / u16::from(a)) as u8 };
        out.extend_from_slice(&[a, straight(r), straight(g), straight(b)]);
    }
    out
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
pub fn spawn() -> Option<(async_channel::Receiver<Command>, HandleImpl)> {
    let (tx, rx) = async_channel::unbounded();
    let tray = BigLaceTray {
        connected: false,
        peer_count: 0,
        local: false,
        room: None,
        visits: Vec::new(),
        frame: None,
        badge: vec![],
        sender: tx,
    };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some((rx, HandleImpl { handle, unread: Cell::new(None), animating: Rc::default() })),
        Err(e) => {
            eprintln!("[biglace] tray: failed to register StatusNotifierItem: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cairo_pixels_become_sni_argb() {
        // Half-transparent red, premultiplied: a = 0x80, r = 0x80 (0xFF scaled by a).
        let cairo_px = 0x8080_0000u32.to_ne_bytes();
        assert_eq!(to_sni_argb(&cairo_px), [0x80, 0xFF, 0x00, 0x00]);
        assert_eq!(to_sni_argb(&[0, 0, 0, 0]), [0, 0, 0, 0]);
    }

    #[test]
    fn unread_badge_draws_count_in_red() {
        assert!(unread_badge(0).unwrap().is_empty());
        for count in [7, 42, 150] {
            let icons = unread_badge(count).unwrap();
            let px = icons[0].data.as_chunks::<4>().0;
            assert_eq!(px.len(), (BADGE_SIDE * BADGE_SIDE) as usize);
            assert_eq!(px[0][0], 0, "corner outside the disc is transparent");
            let red = px.iter().filter(|p| p[0] == 255 && p[1] > 200 && p[2] < 60).count();
            let white = px.iter().filter(|p| p[0] == 255 && p[1] > 240 && p[2] > 240 && p[3] > 240).count();
            assert!(red > 300, "{count}: {red} red pixels");
            assert!(white > 15, "{count}: {white} white pixels");
        }
    }
}
