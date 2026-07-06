//! Windows backend: Shell_NotifyIcon via the `tray-icon` crate.
//!
//! tray-icon needs a Windows message loop to dispatch icon clicks and menu
//! events — without one, callbacks are silently swallowed. We can't use the
//! GTK main loop for that (it's a GLib loop, not a Win32 message pump), so
//! we spawn a dedicated worker thread that:
//!   1. Owns the `TrayIcon` (Windows COM/threading rules require it stays
//!      on its creating thread),
//!   2. Pumps Win32 messages with `PeekMessageW` + `DispatchMessageW`,
//!   3. Forwards menu clicks to the GTK loop via mpsc::Sender<Command>,
//!   4. Receives `update(connected, peer_count)` calls from the GTK loop via
//!      a separate channel and applies them on its own thread.
//!
//! The icon is currently a placeholder solid-color square — embedding the
//! real BigLace icon needs either a build-time SVG rasterizer or a Win32
//! resource; either is a follow-up. The square is enough to make the tray
//! visible and exercise the click-through plumbing in CI.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem as TrayMenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

use super::Command;
use crate::{tr, trf};

/// Status update pushed from the GTK loop to the tray worker thread.
#[derive(Debug, Clone, Copy)]
struct Update {
    connected:  bool,
    peer_count: usize,
}

pub struct HandleImpl {
    update_tx: mpsc::Sender<Update>,
}

impl HandleImpl {
    pub fn update(&self, connected: bool, peer_count: usize) {
        // Worker may have exited (e.g. tray init failed and we never started
        // the loop); a dropped update is harmless — there's nothing to refresh.
        let _ = self.update_tx.send(Update { connected, peer_count });
    }
}

pub fn spawn() -> Option<(mpsc::Receiver<Command>, HandleImpl)> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel::<Update>();

    // Probe channel: the worker pings this once it has built the tray
    // icon successfully so we know whether to expose the handle. We block
    // briefly waiting for it; init usually returns in well under a second.
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();

    thread::spawn(move || {
        run_tray_thread(cmd_tx, update_rx, ready_tx);
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(true) => Some((cmd_rx, HandleImpl { update_tx })),
        _ => None,
    }
}

fn run_tray_thread(
    cmd_tx: mpsc::Sender<Command>,
    update_rx: mpsc::Receiver<Update>,
    ready_tx: mpsc::Sender<bool>,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let menu = Menu::new();

    let show_item = TrayMenuItem::new(tr!("Show BigLace"), true, None);
    let toggle_item = TrayMenuItem::new(tr!("Connect"), true, None);
    let quit_item = TrayMenuItem::new(tr!("Quit"), true, None);

    let show_id = show_item.id().clone();
    let toggle_id = toggle_item.id().clone();
    let quit_id = quit_item.id().clone();

    if let Err(e) = (|| -> Result<(), Box<dyn std::error::Error>> {
        menu.append(&show_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&toggle_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_item)?;
        Ok(())
    })() {
        eprintln!("[biglace] tray: failed to build menu: {e}");
        let _ = ready_tx.send(false);
        return;
    }

    let icon = build_default_icon();

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("BigLace")
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[biglace] tray: failed to register tray icon: {e}");
            let _ = ready_tx.send(false);
            return;
        }
    };

    let _ = ready_tx.send(true);

    let mut connected = false;
    let menu_channel = MenuEvent::receiver();

    loop {
        // Pump Win32 messages so tray-icon's hidden window can dispatch
        // events. Without this, click events never fire.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Apply pending state updates from the GTK main loop.
        while let Ok(u) = update_rx.try_recv() {
            connected = u.connected;
            let toggle_label = if connected { tr!("Disconnect") } else { tr!("Connect") };
            toggle_item.set_text(&toggle_label);
            let _ = tray.set_tooltip(Some(tooltip_text(connected, u.peer_count)));
        }

        // Forward menu clicks to the GTK main loop.
        while let Ok(ev) = menu_channel.try_recv() {
            let cmd = if ev.id == show_id {
                Command::Show
            } else if ev.id == toggle_id {
                if connected { Command::Disconnect } else { Command::Connect }
            } else if ev.id == quit_id {
                Command::Quit
            } else {
                continue;
            };
            if cmd_tx.send(cmd).is_err() {
                // GTK loop has dropped the receiver — app is shutting down.
                return;
            }
            // On Quit, return now so `tray` drops here and tray-icon's Drop
            // fires Shell_NotifyIcon(NIM_DELETE) — otherwise the process exits
            // via std::process::exit (no destructors) and the icon lingers as a
            // ghost until the user hovers over it.
            if matches!(cmd, Command::Quit) {
                return;
            }
        }

        // 50 ms is brisk enough that menu clicks feel instant while keeping
        // CPU at idle when nothing's happening.
        thread::sleep(Duration::from_millis(50));
    }
}

fn tooltip_text(connected: bool, peer_count: usize) -> String {
    if connected {
        match peer_count {
            0 => format!("BigLace — {}", tr!("Connected")),
            1 => format!("BigLace — {}", tr!("Connected · 1 peer")),
            // Use the same msgid as the Linux backend so pt-BR (and word
            // orders that differ) translate the whole phrase, not just the
            // prefix with "peers" hardcoded in English.
            n => format!("BigLace — {}", trf!("Connected · {n} peers", "n" => n)),
        }
    } else {
        format!("BigLace — {}", tr!("Disconnected"))
    }
}

/// Placeholder 16x16 RGBA icon (BigLace blue). The real branded icon should
/// land here in a follow-up — likely via a `build.rs` that rasterizes
/// `usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg`
/// into RGBA bytes embedded with `include_bytes!`.
fn build_default_icon() -> Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x29, 0x9F, 0xFC, 0xFF]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("16x16 RGBA buffer is always valid")
}
