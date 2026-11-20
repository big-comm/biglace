//! Is anything changing on the X11 screen? XDamage tells us, so a still
//! screen isn't captured, converted and encoded 30 times a second.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::Event;

/// The thread watching the screen; it stops when this is dropped.
pub struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Watch the root window; `dirty` is raised on every change. None when
/// XDamage isn't available (the caller then treats the screen as always
/// changing).
pub fn watch(dirty: Arc<super::host::Damage>, track_pointer: bool) -> Option<Watch> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    conn.damage_query_version(1, 1).ok()?.reply().ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    let id = conn.generate_id().ok()?;
    conn.damage_create(id, root, damage::ReportLevel::NON_EMPTY).ok()?.check().ok()?;
    let _ = conn.flush();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let thread = std::thread::Builder::new()
        .name("biglace-remote-damage".into())
        .spawn(move || {
            let mut last_pointer = (0i16, 0i16);
            while !stopped.load(Ordering::Acquire) {
                let mut changed = false;
                // The pointer drawn in the frames: its moves are changes too
                // (XDamage doesn't report them).
                if track_pointer {
                    use x11rb::protocol::xproto::ConnectionExt as _;
                    if let Some(p) = conn.query_pointer(root).ok().and_then(|c| c.reply().ok()) {
                        if (p.root_x, p.root_y) != last_pointer {
                            last_pointer = (p.root_x, p.root_y);
                            changed = true;
                        }
                    }
                }
                while let Ok(Some(event)) = conn.poll_for_event() {
                    if matches!(event, Event::DamageNotify(_)) {
                        changed = true;
                    }
                }
                if changed {
                    dirty.raise();
                    // Re-arm: NON_EMPTY reports again only after this.
                    let _ = conn.damage_subtract(id, x11rb::NONE, x11rb::NONE);
                    let _ = conn.flush();
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            let _ = conn.damage_destroy(id);
            let _ = conn.flush();
        })
        .ok()?;
    Some(Watch { stop, thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only with `DISPLAY` set and no `WAYLAND_DISPLAY`, an X server of
    /// its own (`env -u WAYLAND_DISPLAY xvfb-run cargo test`); it skips otherwise.
    #[test]
    fn dropping_the_watch_ends_its_thread() {
        if std::env::var_os("DISPLAY").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("no X server of its own; skipping");
            return;
        }
        let watch = watch(Arc::default(), true).expect("XDamage on this display");
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(watch);
            let _ = done.send(());
        });
        assert!(ended.recv_timeout(Duration::from_secs(2)).is_ok(), "the damage thread kept running");
    }
}
