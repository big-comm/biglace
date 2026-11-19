//! The pointer's shape on an X11 host (arrow, hand, text beam…), sent to
//! the viewer whenever it changes, so the viewer's own pointer — which
//! moves with no video delay — looks like the one on the remote screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::Event;

/// Largest cursor sent (bigger ones are themes gone wild; keep the default).
const MAX_SIDE: u16 = 128;

/// A cursor image as it goes on the wire: width, height, hotspot x, y
/// (u16 BE each), then straight (not premultiplied) RGBA rows.
pub fn encode(width: u16, height: u16, xhot: u16, yhot: u16, argb: &[u32]) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let n = usize::from(width) * usize::from(height);
    if argb.len() < n {
        return None;
    }
    let mut out = Vec::with_capacity(8 + n * 4);
    for v in [width, height, xhot.min(width - 1), yhot.min(height - 1)] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for px in &argb[..n] {
        let a = (px >> 24) as u8;
        // X hands out premultiplied colour: undo it for a plain RGBA texture.
        let un = |c: u32| -> u8 {
            let c = (c & 0xff) as u16;
            if a == 0 {
                0
            } else {
                ((c * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8
            }
        };
        out.extend_from_slice(&[un(px >> 16), un(px >> 8), un(*px), a]);
    }
    Some(out)
}

/// Watch the pointer's shape until `stop` is set; `send` gets each new one
/// (the current one first). False when XFixes isn't available.
pub fn watch(send: impl Fn(Vec<u8>) + Send + 'static, stop: Arc<AtomicBool>) -> bool {
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return false };
    if conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_none() {
        return false;
    }
    let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) else { return false };
    if conn
        .xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
        .ok()
        .and_then(|c| c.check().ok())
        .is_none()
    {
        return false;
    }
    let _ = conn.flush();
    std::thread::Builder::new()
        .name("biglace-remote-cursor".into())
        .spawn(move || {
            let mut last_serial = None;
            let mut current = || {
                let Some(img) = conn.xfixes_get_cursor_image().ok().and_then(|c| c.reply().ok()) else { return };
                if last_serial == Some(img.cursor_serial) {
                    return;
                }
                last_serial = Some(img.cursor_serial);
                if let Some(bytes) = encode(img.width, img.height, img.xhot, img.yhot, &img.cursor_image) {
                    send(bytes);
                }
            };
            current();
            while !stop.load(Ordering::Acquire) {
                let mut changed = false;
                while let Ok(Some(event)) = conn.poll_for_event() {
                    if matches!(event, Event::XfixesCursorNotify(_)) {
                        changed = true;
                    }
                }
                if changed {
                    current();
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_are_encoded_straight() {
        // 2×1: opaque red, half-transparent (premultiplied) white.
        let bytes = encode(2, 1, 0, 0, &[0xffff_0000, 0x8080_8080]).unwrap();
        assert_eq!(&bytes[..8], &[0, 2, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..12], &[255, 0, 0, 255]);
        assert_eq!(&bytes[12..16], &[255, 255, 255, 128]);
        assert!(encode(0, 1, 0, 0, &[]).is_none());
        assert!(encode(2, 2, 0, 0, &[0; 3]).is_none(), "too few pixels");
        // A hotspot outside the image is pulled in.
        assert_eq!(&encode(1, 1, 9, 9, &[0]).unwrap()[4..8], &[0, 0, 0, 0]);
    }
}
