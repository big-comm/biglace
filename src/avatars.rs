//! Profile photos: kept on the panel (one small image per account) and
//! cached here, so chats, rooms and groups show the person's face. Widgets
//! ask through [`bind`]; a photo that arrives later fills them in.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk4::{gdk, glib, prelude::*};

/// How often known photos are checked for changes.
const RECHECK: Duration = Duration::from_secs(10 * 60);

fn cache_dir() -> PathBuf {
    glib::user_cache_dir().join("biglace").join("avatars")
}

fn name_ok(user: &str) -> bool {
    !user.is_empty() && user.len() <= 64 && user.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// What we know per account: the photo version (None: no photo) and when.
type Known = Mutex<HashMap<String, (Option<String>, Instant)>>;

fn known() -> &'static Known {
    static K: OnceLock<Known> = OnceLock::new();
    K.get_or_init(Default::default)
}

fn cached_path(user: &str, hash: &str) -> PathBuf {
    cache_dir().join(format!("{user}-{hash}"))
}

/// The cached photo file for `user`, if any (whatever its version).
fn cached_any(user: &str) -> Option<PathBuf> {
    let prefix = format!("{user}-");
    std::fs::read_dir(cache_dir())
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

// ─── Widgets waiting for photos (main thread only) ──────────────────────────

thread_local! {
    static BOUND: std::cell::RefCell<Vec<(String, glib::WeakRef<libadwaita::Avatar>)>> = Default::default();
    static TEXTURES: std::cell::RefCell<HashMap<String, Option<gdk::Texture>>> = Default::default();
}

fn texture_for(user: &str) -> Option<gdk::Texture> {
    if let Some(t) = TEXTURES.with(|t| t.borrow().get(user).cloned()) {
        return t;
    }
    let t = cached_any(user).and_then(|p| gdk::Texture::from_filename(&p).ok());
    TEXTURES.with(|m| m.borrow_mut().insert(user.to_string(), t.clone()));
    t
}

/// Show `user`'s photo in `avatar` (now if cached, else when it arrives).
pub fn bind(avatar: &libadwaita::Avatar, user: &str) {
    unbind(avatar);
    if !name_ok(user) {
        return;
    }
    if let Some(t) = texture_for(user) {
        avatar.set_custom_image(Some(&t));
    }
    BOUND.with(|b| b.borrow_mut().push((user.to_string(), avatar.downgrade())));
    request(vec![user.to_string()]);
}

/// `avatar` shows nobody's photo anymore (it's reused for someone else, or
/// for a group).
pub fn unbind(avatar: &libadwaita::Avatar) {
    avatar.set_custom_image(None::<&gdk::Paintable>);
    BOUND.with(|b| {
        b.borrow_mut().retain(|(_, w)| w.upgrade().is_some_and(|a| &a != avatar));
    });
}

/// A photo changed (or went away): update every widget showing `user`.
fn refresh_widgets(user: &str) {
    TEXTURES.with(|t| t.borrow_mut().remove(user));
    let tex = texture_for(user);
    BOUND.with(|b| {
        for (u, w) in b.borrow().iter() {
            if u == user {
                if let Some(a) = w.upgrade() {
                    a.set_custom_image(tex.as_ref());
                }
            }
        }
    });
}

// ─── Fetching (background) ──────────────────────────────────────────────────

type Updates = (async_channel::Sender<String>, async_channel::Receiver<String>);

fn updates() -> &'static Updates {
    static U: OnceLock<Updates> = OnceLock::new();
    U.get_or_init(async_channel::unbounded)
}

/// Start applying fetched photos on the main loop (call once at startup).
pub fn start() {
    let rx = updates().1.clone();
    glib::spawn_future_local(async move {
        while let Ok(user) = rx.recv().await {
            refresh_widgets(&user);
        }
    });
}

fn pending() -> &'static Mutex<Vec<String>> {
    static P: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// Make sure these people's photos are current (batched, in the background).
pub fn request(users: Vec<String>) {
    let now = Instant::now();
    let fresh = |u: &String| {
        known()
            .lock()
            .ok()
            .and_then(|k| k.get(u).map(|(_, at)| now.duration_since(*at) < RECHECK))
            .unwrap_or(false)
    };
    let wanted: Vec<String> = users.into_iter().filter(|u| name_ok(u) && !fresh(u)).collect();
    if wanted.is_empty() {
        return;
    }
    let start_worker = {
        let Ok(mut p) = pending().lock() else { return };
        let was_empty = p.is_empty();
        for u in wanted {
            if !p.contains(&u) {
                p.push(u);
            }
        }
        was_empty
    };
    if start_worker {
        // A short pause gathers everyone a page asks for into one request.
        let _ = std::thread::Builder::new().name("biglace-avatars".into()).spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            let batch: Vec<String> = pending().lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
            fetch(&batch);
        });
    }
}

fn fetch(users: &[String]) {
    if users.is_empty() {
        return;
    }
    let Ok(hashes) = crate::invite::avatar_hashes(users) else { return };
    let now = Instant::now();
    let _ = std::fs::create_dir_all(cache_dir());
    for u in users {
        let hash = hashes.get(u).cloned().filter(|h| h.len() <= 64 && h.bytes().all(|c| c.is_ascii_hexdigit()));
        let before = known().lock().ok().and_then(|k| k.get(u).map(|(h, _)| h.clone()));
        if let Ok(mut k) = known().lock() {
            k.insert(u.clone(), (hash.clone(), now));
        }
        match &hash {
            Some(h) if !cached_path(u, h).exists() => {
                if let Ok(bytes) = crate::invite::fetch_avatar(u) {
                    if is_image(&bytes) {
                        remove_cached(u);
                        let _ = std::fs::write(cached_path(u, h), &bytes);
                        let _ = updates().0.try_send(u.clone());
                    }
                }
            }
            None if before.as_ref().is_some_and(Option::is_some) || cached_any(u).is_some() => {
                remove_cached(u);
                let _ = updates().0.try_send(u.clone());
            }
            _ => {
                if before.is_none() && cached_any(u).is_some() {
                    let _ = updates().0.try_send(u.clone());
                }
            }
        }
    }
}

fn remove_cached(user: &str) {
    let prefix = format!("{user}-");
    if let Ok(entries) = std::fs::read_dir(cache_dir()) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// JPEG or PNG — the only formats decoded from the network.
fn is_image(b: &[u8]) -> bool {
    b.starts_with(&[0x89, b'P', b'N', b'G']) || b.starts_with(&[0xff, 0xd8, 0xff])
}

// ─── Our own photo ──────────────────────────────────────────────────────────

/// A picture file → a centred square JPEG of at most 256 px that fits the
/// panel's limit.
pub fn prepare(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use gtk4::gdk_pixbuf::{InterpType, Pixbuf};
    let src = Pixbuf::from_file(path).map_err(|e| e.to_string())?;
    let src = src.apply_embedded_orientation().unwrap_or(src);
    let (w, h) = (src.width(), src.height());
    let side = w.min(h);
    let square = src.new_subpixbuf((w - side) / 2, (h - side) / 2, side, side);
    let size = side.min(256);
    let small = square.scale_simple(size, size, InterpType::Bilinear).ok_or("could not resize the picture")?;
    for quality in ["88", "78", "65", "50"] {
        let bytes = small.save_to_bufferv("jpeg", &[("quality", quality)]).map_err(|e| e.to_string())?;
        if bytes.len() <= 44 * 1024 {
            return Ok(bytes);
        }
    }
    Err("the picture is too detailed to fit".into())
}

/// Our photo was changed here: show it right away.
pub fn set_own(user: &str, bytes: Option<&[u8]>) {
    if !name_ok(user) {
        return;
    }
    remove_cached(user);
    if let Some(b) = bytes {
        let _ = std::fs::create_dir_all(cache_dir());
        let _ = std::fs::write(cached_path(user, "local"), b);
    }
    if let Ok(mut k) = known().lock() {
        k.remove(user);
    }
    refresh_widgets(user);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_jpeg_and_png() {
        assert!(is_image(&[0x89, b'P', b'N', b'G', 13, 10]));
        assert!(is_image(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(!is_image(b"GIF89a"));
        assert!(!is_image(b"<svg"));
        assert!(!name_ok("../x"));
        assert!(name_ok("bigrato-2"));
    }

    /// Any picture becomes a square JPEG small enough for the panel.
    #[test]
    fn prepares_a_small_square_jpeg() {
        use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
        let pb = Pixbuf::new(Colorspace::Rgb, false, 8, 1200, 800).unwrap();
        // Noise: the worst case for JPEG size.
        let mut seed = 7u32;
        for y in 0..800 {
            for x in 0..1200 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let v = (seed >> 16) as u8;
                pb.put_pixel(x, y, v, v.wrapping_mul(3), v.wrapping_mul(7), 255);
            }
        }
        let path = std::env::temp_dir().join(format!("biglace-avatar-test-{}.png", std::process::id()));
        pb.savev(&path, "png", &[]).unwrap();
        let jpeg = prepare(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        assert!(jpeg.len() <= 44 * 1024, "{} bytes", jpeg.len());
        let back = gtk4::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(jpeg)).unwrap();
        assert_eq!((back.width(), back.height()), (256, 256));
    }
}
