//! Is anything changing on the X11 screen? XDamage tells us, so a still
//! screen isn't captured, converted and encoded 30 times a second.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::Event;

/// Watch the root window until `stop` is set; `dirty` is raised on every
/// change. Returns false when XDamage isn't available (the caller then
/// treats the screen as always changing).
pub fn watch(dirty: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> bool {
    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return false;
    };
    if conn
        .damage_query_version(1, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_none()
    {
        return false;
    }
    let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) else {
        return false;
    };
    let Ok(id) = conn.generate_id() else {
        return false;
    };
    if conn
        .damage_create(id, root, damage::ReportLevel::NON_EMPTY)
        .ok()
        .and_then(|c| c.check().ok())
        .is_none()
    {
        return false;
    }
    let _ = conn.flush();
    std::thread::Builder::new()
        .name("biglace-remote-damage".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let mut changed = false;
                while let Ok(Some(event)) = conn.poll_for_event() {
                    if matches!(event, Event::DamageNotify(_)) {
                        changed = true;
                    }
                }
                if changed {
                    dirty.store(true, Ordering::Release);
                    // Re-arm: NON_EMPTY reports again only after this.
                    let _ = conn.damage_subtract(id, x11rb::NONE, x11rb::NONE);
                    let _ = conn.flush();
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            let _ = conn.damage_destroy(id);
            let _ = conn.flush();
        })
        .is_ok()
}
