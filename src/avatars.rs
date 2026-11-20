//! Profile photos, with no server involved: each person's photo lives on
//! their own computer, and their BigLace hands it to whoever asks over the
//! tunnel (`/v1/avatar` on the agent — the network's rules already decide
//! who can reach whom). Others keep a copy, so the face still shows while
//! its owner is offline. Widgets ask through [`bind`]; a photo that
//! arrives later fills them in.
//!
//! A group's photo works the same way: it lives on the devices of the person
//! who created the group (only they set it) and is handed to the group's
//! members at `/v1/group-avatar/<group>`. Photos are kept by key: an account
//! name, or `group.<name>` (account names never hold a dot).

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

const GROUP_KEY: &str = "group.";

fn group_key(group: &str) -> String {
    format!("{GROUP_KEY}{group}")
}

/// An account name, or `group.<name>` for a group's photo.
fn key_ok(key: &str) -> bool {
    match key.strip_prefix(GROUP_KEY) {
        Some(group) => crate::invite::group_name_ok(group),
        None => name_ok(key),
    }
}

/// `file` is a cached photo of `key`: `<key>-<16 hex digits>`, so `big`
/// never takes `big-plasma`'s photo.
fn is_cached_of(file: &str, key: &str) -> bool {
    file.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
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
    std::fs::read_dir(cache_dir())
        .ok()?
        .flatten()
        .filter(|e| is_cached_of(&e.file_name().to_string_lossy(), user))
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
    bind_key(avatar, user);
}

/// Show group `group`'s photo in `avatar` (now if cached, else when it arrives).
pub fn bind_group(avatar: &libadwaita::Avatar, group: &str) {
    bind_key(avatar, &group_key(group));
}

fn bind_key(avatar: &libadwaita::Avatar, user: &str) {
    unbind(avatar);
    if !key_ok(user) {
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
    let wanted: Vec<String> = users.into_iter().filter(|u| key_ok(u) && !fresh(u)).collect();
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

/// Largest photo accepted from another computer.
const MAX_PHOTO: u64 = 64 * 1024;
/// Agent paths: the photo, and just its version (cheap to check).
pub const PATH: &str = "/v1/avatar";
pub const HASH_PATH: &str = "/v1/avatar/hash";

/// Agent path prefix of a group's photo: `<prefix><group>`, and `/hash` after it.
pub const GROUP_PREFIX: &str = "/v1/group-avatar/";

/// Our own photo (set here, served to the others).
fn own_path() -> PathBuf {
    glib::user_config_dir().join("biglace").join("avatar.jpg")
}

/// The photo of a group we created (set here, served to its members).
fn own_group_path(group: &str) -> PathBuf {
    glib::user_config_dir().join("biglace").join("groups").join(format!("{group}.jpg"))
}

fn hash_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// What another computer says about someone's photo.
enum Remote {
    Photo(String),
    NoPhoto,
    Unreachable,
}

fn agent_url(ip: &str, path: &str) -> String {
    format!("http://{ip}:{}{path}", crate::agent::PORT)
}

fn ask_hash(ip: &str, hash_path: &str) -> Remote {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(4)).build();
    match agent.get(&agent_url(ip, hash_path)).call() {
        Ok(r) => match r.into_string() {
            Ok(h) if !h.is_empty() && h.len() <= 64 && h.bytes().all(|c| c.is_ascii_hexdigit()) => Remote::Photo(h),
            _ => Remote::Unreachable,
        },
        Err(ureq::Error::Status(404, _)) => Remote::NoPhoto,
        Err(_) => Remote::Unreachable,
    }
}

fn download(ip: &str, photo_path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build();
    let r = agent.get(&agent_url(ip, photo_path)).call().ok()?;
    let mut body = Vec::new();
    r.into_reader().take(MAX_PHOTO + 1).read_to_end(&mut body).ok()?;
    (body.len() as u64 <= MAX_PHOTO && is_image(&body)).then_some(body)
}

fn fetch(keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let me = crate::chat::my_user();
    let peers = crate::tailscale::get_peers();
    let _ = std::fs::create_dir_all(cache_dir());
    for key in keys {
        // Whose devices hold the photo, and where on their agent.
        let (holder, local, photo_path) = match key.strip_prefix(GROUP_KEY) {
            Some(group) => {
                let Some(owner) = crate::chat::group_owner(group) else {
                    // A network group (no creator), or not ours to see: no photo.
                    forget(key);
                    continue;
                };
                (owner, own_group_path(group), format!("{GROUP_PREFIX}{group}"))
            }
            None => (key.clone(), own_path(), PATH.to_string()),
        };
        if me.as_deref() == Some(holder.as_str()) {
            take_local(key, &local);
        } else {
            take_remote(key, &holder, &photo_path, &peers);
        }
    }
}

/// No photo for `key`: drop any copy.
fn forget(key: &str) {
    if let Ok(mut k) = known().lock() {
        k.insert(key.to_string(), (None, Instant::now()));
    }
    if cached_any(key).is_some() {
        remove_cached(key);
        let _ = updates().0.try_send(key.to_string());
    }
}

/// Ours: straight from the file.
fn take_local(key: &str, file: &std::path::Path) {
    let bytes = std::fs::read(file).ok().filter(|b| is_image(b));
    let hash = bytes.as_deref().map(hash_of);
    let changed = match (&hash, &bytes) {
        (Some(h), Some(b)) if !cached_path(key, h).exists() => {
            remove_cached(key);
            let _ = std::fs::write(cached_path(key, h), b);
            true
        }
        (None, _) if cached_any(key).is_some() => {
            remove_cached(key);
            true
        }
        _ => false,
    };
    if let Ok(mut k) = known().lock() {
        k.insert(key.to_string(), (hash, Instant::now()));
    }
    if changed {
        let _ = updates().0.try_send(key.to_string());
    }
}

/// Theirs: from one of `holder`'s computers that's on.
fn take_remote(key: &str, holder: &str, photo_path: &str, peers: &[crate::tailscale::Peer]) {
    let hash_path = format!("{photo_path}/hash");
    let mut answer = Remote::Unreachable;
    let mut from_ip = String::new();
    for p in peers.iter().filter(|p| p.user == holder && p.online && !p.ipv4.is_empty()) {
        match ask_hash(&p.ipv4, &hash_path) {
            Remote::Unreachable => continue,
            Remote::NoPhoto if matches!(answer, Remote::Unreachable) => answer = Remote::NoPhoto,
            Remote::NoPhoto => {}
            photo @ Remote::Photo(_) => {
                answer = photo;
                from_ip = p.ipv4.clone();
                break;
            }
        }
    }
    let now = Instant::now();
    match answer {
        // Nobody to ask now: keep what we have, ask again later.
        Remote::Unreachable => {
            if let Ok(mut k) = known().lock() {
                k.insert(key.to_string(), (None, now - RECHECK + Duration::from_secs(60)));
            }
            if cached_any(key).is_some() {
                let _ = updates().0.try_send(key.to_string());
            }
        }
        Remote::NoPhoto => forget(key),
        Remote::Photo(h) => {
            if let Ok(mut k) = known().lock() {
                k.insert(key.to_string(), (Some(h.clone()), now));
            }
            if !cached_path(key, &h).exists() {
                if let Some(bytes) = download(&from_ip, photo_path) {
                    remove_cached(key);
                    // Named by what it really is, not what was claimed.
                    let _ = std::fs::write(cached_path(key, &hash_of(&bytes)), &bytes);
                    let _ = updates().0.try_send(key.to_string());
                }
            }
        }
    }
}

/// Another computer asks for our photo (agent thread).
pub fn serve(stream: std::net::TcpStream, path: &str) {
    send_photo(stream, &own_path(), path == HASH_PATH);
}

/// A member asks for the photo of a group we created (agent thread):
/// `<GROUP_PREFIX><group>` or `…/hash`. Nobody outside the group gets it.
pub fn serve_group(stream: std::net::TcpStream, path: &str, caller: &str) {
    let rest = path.strip_prefix(GROUP_PREFIX).unwrap_or_default();
    let (group, want_hash) = match rest.strip_suffix("/hash") {
        Some(g) => (g, true),
        None => (rest, false),
    };
    let member = |caller: &str| crate::chat::group_members(group).iter().any(|m| m == caller);
    // Someone who just joined isn't in what we learned yet: ask the panel
    // again (it answers from memory for 30 s, so this can't flood it).
    if crate::invite::group_name_ok(group) && !member(caller) {
        crate::chat::my_groups_refresh();
    }
    let me = crate::chat::my_user();
    let allowed = crate::invite::group_name_ok(group)
        && me.is_some()
        && crate::chat::group_owner(group) == me
        && member(caller);
    if !allowed {
        use std::io::Write;
        let mut stream = stream;
        let _ = write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    send_photo(stream, &own_group_path(group), want_hash);
}

/// The photo at `file` (or just its version), 404 when there is none.
fn send_photo(mut stream: std::net::TcpStream, file: &std::path::Path, want_hash: bool) {
    use std::io::Write;
    let photo = std::fs::read(file).ok().filter(|b| is_image(b) && b.len() as u64 <= MAX_PHOTO);
    let _ = match photo {
        None => write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        Some(b) if want_hash => {
            let h = hash_of(&b);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{h}", h.len())
        }
        Some(b) => {
            let kind = if b.starts_with(&[0xff, 0xd8]) { "image/jpeg" } else { "image/png" };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len())
                .and_then(|()| stream.write_all(&b))
        }
    };
    let _ = stream.flush();
}

fn remove_cached(user: &str) {
    if let Ok(entries) = std::fs::read_dir(cache_dir()) {
        for e in entries.flatten() {
            if is_cached_of(&e.file_name().to_string_lossy(), user) {
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
    // JPEG has no transparency (older glycin refuses to encode it at all):
    // lay the picture on white first.
    let small = if small.has_alpha() {
        let flat = Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, size, size).ok_or("could not resize the picture")?;
        flat.fill(0xffff_ffff);
        small.composite(&flat, 0, 0, size, size, 0.0, 0.0, 1.0, 1.0, InterpType::Nearest, 255);
        flat
    } else {
        small
    };
    for quality in ["88", "78", "65", "50"] {
        let bytes = small.save_to_bufferv("jpeg", &[("quality", quality)]).map_err(|e| e.to_string())?;
        if bytes.len() <= 44 * 1024 {
            return Ok(bytes);
        }
    }
    Err("the picture is too detailed to fit".into())
}

/// Set (or remove) our photo: kept on this computer, shown right away,
/// and handed to the others when they ask.
pub fn set_own(user: &str, bytes: Option<&[u8]>) -> Result<(), String> {
    set_photo(&own_path(), user, bytes)
}

/// Set (or remove) the photo of a group we created: kept on this computer,
/// shown right away, and handed to its members when they ask.
pub fn set_group(group: &str, bytes: Option<&[u8]>) -> Result<(), String> {
    if !crate::invite::group_name_ok(group) {
        return Err("invalid group name".into());
    }
    set_photo(&own_group_path(group), &group_key(group), bytes)
}

fn set_photo(path: &std::path::Path, key: &str, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(b) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, b).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| e.to_string())?;
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        },
    }
    if !key_ok(key) {
        return Ok(());
    }
    remove_cached(key);
    if let Some(b) = bytes {
        let _ = std::fs::create_dir_all(cache_dir());
        let _ = std::fs::write(cached_path(key, &hash_of(b)), b);
    }
    if let Ok(mut k) = known().lock() {
        k.remove(key);
    }
    Ok(())
}

/// Show our new photo everywhere (main thread, after [`set_own`]).
pub fn refresh_mine(user: &str) {
    if name_ok(user) {
        refresh_widgets(user);
    }
}

/// Show a group's new photo everywhere (main thread, after [`set_group`]).
pub fn refresh_group(group: &str) {
    refresh_widgets(&group_key(group));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_keys() {
        assert!(key_ok("bigrato-2"));
        assert!(key_ok("group.familia"));
        assert!(!key_ok("group.Familia"));
        assert!(!key_ok("group.../x"));
        assert!(!key_ok("a.b"));
    }

    /// One account's copy never matches another whose name starts the same.
    #[test]
    fn cached_files_by_exact_key() {
        assert!(is_cached_of("big-0123456789abcdef", "big"));
        assert!(!is_cached_of("big-plasma-0123456789abcdef", "big"));
        assert!(is_cached_of("group.familia-0123456789abcdef", "group.familia"));
        assert!(!is_cached_of("familia-0123456789abcdef", "group.familia"));
        assert!(!is_cached_of("big-0123", "big"));
    }

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
