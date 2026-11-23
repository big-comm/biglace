//! Noto's animated emoji (reactions/, CC BY 4.0, see reactions/NOTICE.txt),
//! played while shown, for the reactions offered on a message.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk4::gdk_pixbuf::prelude::*;
use gtk4::gdk_pixbuf::PixbufAnimation;
use gtk4::{gdk, gio, glib, prelude::*};

/// The reactions offered on a message, in order; each has its animation below.
pub const REACTIONS: [&str; 10] = ["❤️", "👍", "👀", "🙏", "🤓", "😢", "😂", "🔥", "🎉", "👏"];

/// The WebP animation of [`REACTIONS`]' emoji (gdk-pixbuf decodes it through glycin).
fn source(emoji: &str) -> Option<&'static [u8]> {
    Some(match emoji {
        "❤️" => include_bytes!("reactions/2764_fe0f.webp"),
        "👍" => include_bytes!("reactions/1f44d.webp"),
        "👀" => include_bytes!("reactions/1f440.webp"),
        "🙏" => include_bytes!("reactions/1f64f.webp"),
        "🤓" => include_bytes!("reactions/1f913.webp"),
        "😢" => include_bytes!("reactions/1f622.webp"),
        "😂" => include_bytes!("reactions/1f602.webp"),
        "🔥" => include_bytes!("reactions/1f525.webp"),
        "🎉" => include_bytes!("reactions/1f389.webp"),
        "👏" => include_bytes!("reactions/1f44f.webp"),
        _ => return None,
    })
}

thread_local! {
    /// Decoded once per emoji: every menu opened afterwards reuses it.
    static DECODED: RefCell<HashMap<&'static str, Option<PixbufAnimation>>> = RefCell::default();
}

fn animation(emoji: &'static str) -> Option<PixbufAnimation> {
    DECODED.with_borrow_mut(|decoded| {
        decoded
            .entry(emoji)
            .or_insert_with(|| {
                let bytes = glib::Bytes::from_static(source(emoji)?);
                let stream = gio::MemoryInputStream::from_bytes(&bytes);
                // allow-noisy-log: a reaction left static says why, as the rest of the app reports failures.
                PixbufAnimation::from_stream(&stream, gio::Cancellable::NONE)
                    .inspect_err(|e| eprintln!("[biglace] reaction {emoji}: {e}"))
                    .ok()
            })
            .clone()
    })
}

/// `emoji` at `size` pixels: animated while mapped, as text where it has no
/// animation or it can't be decoded.
pub fn build(emoji: &'static str, size: i32) -> gtk4::Widget {
    let Some(animation) = animation(emoji) else {
        return gtk4::Label::new(Some(emoji)).upcast();
    };
    let picture = gtk4::Image::builder().pixel_size(size).build();
    picture.update_property(&[gtk4::accessible::Property::Label(emoji)]);
    picture.connect_map(move |picture| play(picture, &animation));
    picture.upcast()
}

/// Show each frame for its delay until `picture` is unmapped: nothing runs while it is hidden.
fn play(picture: &gtk4::Image, animation: &PixbufAnimation) {
    let frames = animation.iter(None);
    let weak = picture.downgrade();
    let step = move || -> Option<std::time::Duration> {
        let picture = weak.upgrade().filter(WidgetExt::is_mapped)?;
        frames.advance(std::time::SystemTime::now());
        picture.set_paintable(Some(&gdk::Texture::for_pixbuf(&frames.pixbuf())));
        frames.delay_time()
    };
    fn schedule(step: std::rc::Rc<dyn Fn() -> Option<std::time::Duration>>) {
        if let Some(delay) = step() {
            glib::timeout_add_local_once(delay, move || schedule(step));
        }
    }
    schedule(std::rc::Rc::new(step));
}
