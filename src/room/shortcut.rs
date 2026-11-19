//! A global shortcut to mute the microphone in a voice room (Ctrl+Alt+M),
//! working while another program has the focus: through the desktop's
//! GlobalShortcuts portal on Wayland (the desktop may ask once, and lets the
//! person change the keys), grabbed on the root window on X11.
//! Set up the first time a room is joined; it does nothing outside a room.

use std::sync::atomic::{AtomicBool, Ordering};

const ID: &str = "toggle-mute";

/// Start listening (once).
pub fn ensure() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new().name("biglace-shortcut".into()).spawn(|| {
        let result = if crate::remote::wayland_session() { portal() } else { x11() };
        if let Err(e) = result {
            crate::tailscale::dbg(&format!("room: no global mute shortcut: {e}"));
        }
    });
}

#[cfg(target_os = "linux")]
fn portal() -> Result<(), String> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use ashpd::zbus::export::futures_core::Stream;
    ashpd::zbus::block_on(async {
        // A connection of its own, registered as the app first: host
        // (non-Flatpak) apps have no app id otherwise, and the portal
        // refuses ("An app id is required").
        let conn = ashpd::zbus::Connection::session().await.map_err(|e| e.to_string())?;
        if let Ok(app_id) = ashpd::AppID::try_from(crate::APP_ID) {
            if let Err(e) = ashpd::register_host_app_with_connection(conn.clone(), app_id).await {
                crate::tailscale::dbg(&format!("room: can't register with the portal: {e}"));
            }
        }
        let gs = GlobalShortcuts::with_connection(conn).await.map_err(|e| e.to_string())?;
        let session = gs.create_session(Default::default()).await.map_err(|e| e.to_string())?;
        let shortcut = NewShortcut::new(ID, crate::tr!("Mute or unmute the microphone in a BigLace room"))
            .preferred_trigger("CTRL+ALT+m");
        gs.bind_shortcuts(&session, &[shortcut], None, Default::default())
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        let activated = gs.receive_activated().await.map_err(|e| e.to_string())?;
        let mut activated = std::pin::pin!(activated);
        while let Some(a) = std::future::poll_fn(|cx| activated.as_mut().poll_next(cx)).await {
            if a.shortcut_id() == ID {
                crate::room::toggle_mute();
            }
        }
        Ok(())
    })
}

#[cfg(target_os = "linux")]
fn x11() -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};
    use x11rb::protocol::Event;
    let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[screen].root;
    // The key that types "m".
    let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
    let map = conn
        .get_keyboard_mapping(min, max - min + 1)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let per = usize::from(map.keysyms_per_keycode.max(1));
    let keycode = map
        .keysyms
        .chunks(per)
        .position(|syms| syms.contains(&0x6d))
        .map(|i| min + i as u8)
        .ok_or("no M key")?;
    let base = ModMask::CONTROL | ModMask::M1;
    // Also with Num Lock / Caps Lock on.
    for extra in [ModMask::from(0u16), ModMask::M2, ModMask::LOCK, ModMask::M2 | ModMask::LOCK] {
        conn.grab_key(true, root, base | extra, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|_| "Ctrl+Alt+M is taken by another program".to_string())?;
    }
    conn.flush().map_err(|e| e.to_string())?;
    loop {
        match conn.wait_for_event().map_err(|e| e.to_string())? {
            Event::KeyPress(k) if k.detail == keycode => crate::room::toggle_mute(),
            _ => {}
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn portal() -> Result<(), String> {
    Err("not available here".into())
}

#[cfg(not(target_os = "linux"))]
fn x11() -> Result<(), String> {
    Err("not available here".into())
}
