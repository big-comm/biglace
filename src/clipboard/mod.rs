//! The clipboard shared with our phones (over `crate::link`, `{"t":"clip"}`):
//! what is copied here goes to them, what they send lands here.
//!
//! Seeing a copy made in another program depends on the session:
//! - Wayland with ext-data-control (KWin, wlroots): [`data_control`];
//! - GNOME: the BigLace shell extension, over D-Bus ([`gnome`]);
//! - X11, and the rest: GTK's clipboard, which on Wayland only learns of a
//!   copy while a BigLace window has focus.

mod data_control;
pub mod gnome;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use base64::Engine;
use gtk4::{gdk, glib, prelude::*};
use sha2::{Digest, Sha256};

const PNG: &str = "image/png";
const UTF8_TEXT: &str = "text/plain;charset=utf-8";

/// Longest text that crosses, in bytes.
pub const MAX_TEXT: usize = 1_000_000;
/// Largest picture that crosses: base64 keeps it well under a link frame (16 MiB).
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

impl Content {
    fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            Content::Text(t) => {
                h.update(b"t");
                h.update(t.as_bytes());
            }
            Content::Png(p) => {
                h.update(b"i");
                h.update(p);
            }
        }
        h.finalize().into()
    }

    fn fits(&self) -> bool {
        match self {
            Content::Text(t) => !t.is_empty() && t.len() <= MAX_TEXT,
            Content::Png(p) => !p.is_empty() && p.len() <= MAX_IMAGE,
        }
    }
}

/// The setting "Share the clipboard with my phone".
static ENABLED: AtomicBool = AtomicBool::new(false);
/// What last crossed, either way: a copy that echoes back is not sent again.
static LAST: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Records `c` as the latest; false when it already was.
fn fresh(c: &Content) -> bool {
    let d = c.digest();
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *last == Some(d) {
        return false;
    }
    *last = Some(d);
    true
}

/// Something was copied on this computer (any thread): it goes to our phones.
pub fn copied(c: Content) {
    if enabled() && c.fits() && fresh(&c) {
        crate::link::send_clip(&c);
    }
}

/// What a phone copied (link thread), checked; a picture comes back as PNG.
pub fn from_phone(text: Option<String>, image: Option<String>) -> Option<Content> {
    if !enabled() {
        return None;
    }
    let c = match (text, image) {
        (Some(t), None) => Content::Text(t),
        (None, Some(b64)) => {
            if b64.len() > MAX_IMAGE.div_ceil(3) * 4 {
                return None;
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
            let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()?;
            Content::Png(texture.save_to_png_bytes().to_vec())
        }
        _ => return None,
    };
    c.fits().then_some(c)
}

thread_local! {
    /// Wayland's clipboard without focus, when the compositor offers it (main thread).
    static DATA_CONTROL: RefCell<Option<data_control::Clipboard>> = const { RefCell::new(None) };
}

/// Follow this computer's clipboard (main thread, once, at startup).
pub fn watch(display: &gdk::Display) {
    if display.backend().is_wayland() {
        match data_control::Clipboard::start() {
            Ok(c) => {
                DATA_CONTROL.with(|d| *d.borrow_mut() = Some(c));
                return;
            }
            Err(e) => crate::tailscale::dbg(&format!("clipboard: no ext-data-control ({e}), GTK's clipboard instead")),
        }
    }
    display.clipboard().connect_changed(|cb| {
        if !cb.is_local() {
            read_gdk(cb);
        }
    });
}

fn read_gdk(cb: &gdk::Clipboard) {
    let formats = cb.formats();
    if formats.contain_mime_type(PNG) || formats.contains_type(gdk::Texture::static_type()) {
        cb.read_texture_async(gtk4::gio::Cancellable::NONE, |r| {
            if let Ok(Some(t)) = r {
                copied(Content::Png(t.save_to_png_bytes().to_vec()));
            }
        });
    } else if formats.contains_type(glib::Type::STRING) {
        cb.read_text_async(gtk4::gio::Cancellable::NONE, |r| {
            if let Ok(Some(t)) = r {
                copied(Content::Text(t.into()));
            }
        });
    }
}

/// Put what a phone sent on this computer's clipboard (main thread).
pub fn apply(display: &gdk::Display, c: Content) {
    fresh(&c);
    if gnome::set(&c) {
        return;
    }
    let done = DATA_CONTROL.with(|d| d.borrow().as_ref().map(|dc| dc.set(c.clone())));
    match done {
        Some(Ok(())) => {}
        Some(Err(e)) => crate::tailscale::dbg(&format!("clipboard: can't set: {e}")),
        None => match c {
            Content::Text(t) => display.clipboard().set_text(&t),
            Content::Png(p) => match gdk::Texture::from_bytes(&glib::Bytes::from_owned(p)) {
                Ok(t) => display.clipboard().set_texture(&t),
                Err(e) => crate::tailscale::dbg(&format!("clipboard: bad picture: {e}")),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_crosses_once() {
        let a = Content::Text("oi".into());
        assert!(fresh(&a));
        assert!(!fresh(&a));
        assert!(fresh(&Content::Png(b"oi".to_vec())));
        assert!(fresh(&a));
    }

    #[test]
    fn oversized_refused() {
        assert!(Content::Text("x".into()).fits());
        assert!(!Content::Text(String::new()).fits());
        assert!(!Content::Text("x".repeat(MAX_TEXT + 1)).fits());
        assert!(!Content::Png(vec![0; MAX_IMAGE + 1]).fits());
    }
}
