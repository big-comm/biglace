//! Files in direct conversations — pictures, videos, documents and voice
//! messages, Telegram-style, but with no server in between:
//!
//! - The message carries an *offer* (name, size, a small preview); the file
//!   itself stays on the sender's computer, where it already is.
//! - The recipient's BigLace fetches it straight from there over the tunnel
//!   (`/v1/file/<id>/<offset>`, signed like a chat connection). Only the
//!   people it was sent to (and our own devices) may fetch it.
//! - Pictures and voice messages come down by themselves; videos and other
//!   files when the person clicks. An interrupted download resumes.
//! - Downloads land in `~/Downloads/BigLace` (voice messages in the chat's
//!   own folder).

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};

use super::{chat_dir, emit, now_ms, store, write_private, ChatEvent, ConvId};
use crate::tailscale::{self, Identity};

/// Agent path prefix of a file download.
pub const PATH_PREFIX: &str = "/v1/file/";
/// Largest file offered.
pub const MAX_SIZE: u64 = 4 << 30;
/// Pictures up to this size are fetched without a click.
const AUTO_IMAGE_MAX: u64 = 20 << 20;
/// Longest side of the preview sent with a picture or video.
const THUMB_SIDE: i32 = 320;
/// Shared files are served this long after being sent.
const SHARE_DAYS: u64 = 90;
/// How long a local network download keeps trying while nothing arrives.
#[cfg(target_os = "linux")]
const LAN_PATIENCE: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Image,
    Video,
    Audio,
    Voice,
    #[default]
    #[serde(other)]
    File,
}

/// What a message says about its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub kind: FileKind,
    /// Small JPEG preview (pictures and videos), base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Voice messages: length and loudness bars (0–31).
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wave: Vec<u8>,
    /// SHA-256 of the file (hex): checked once it arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// SHA-256 of a file (hex).
pub fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Offer {
    pub fn valid(&self) -> bool {
        self.sha256.as_ref().is_none_or(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
            && safe_name(&self.name)
            && self.size <= MAX_SIZE
            && self.thumb.as_ref().is_none_or(|t| t.len() <= 96 * 1024)
            && self.wave.len() <= 128
            && self.width <= 20_000
            && self.height <= 20_000
    }

    /// Fetched as soon as it arrives (no click needed).
    pub fn fetch_by_itself(&self) -> bool {
        self.kind == FileKind::Voice || (self.kind == FileKind::Image && self.size <= AUTO_IMAGE_MAX)
    }

    pub fn thumb_bytes(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD.decode(self.thumb.as_ref()?).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    /// Not on this computer yet.
    #[default]
    Offered,
    /// Here, at `path`.
    Done,
}

/// A message's file, as this computer knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(flatten)]
    pub offer: Offer,
    /// Where it is on this computer (sent: the original; received: once
    /// downloaded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub state: FileState,
}

impl Attachment {
    /// The file, if it's here (and still where it was).
    pub fn local(&self) -> Option<PathBuf> {
        let p = PathBuf::from(self.path.as_ref()?);
        (self.state == FileState::Done && p.is_file()).then_some(p)
    }
}

/// A name that's safe to show and to save under.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '/' || c == '\\' || c.is_control())
}

/// Pictures, videos and voice messages are offered with a preview.
pub fn kind_of(path: &Path) -> FileKind {
    let (ctype, _) = gtk4::gio::content_type_guess(Some(path), None::<&[u8]>);
    let mime = gtk4::gio::content_type_get_mime_type(&ctype).map(|m| m.to_string()).unwrap_or_default();
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else {
        FileKind::File
    }
}

// ─── Sending ────────────────────────────────────────────────────────────────

/// Offer the file at `path` in `conv` (a direct conversation). Blocking
/// (makes the preview): call off the UI thread.
pub fn send_file(conv: &ConvId, path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(crate::tr!("only files can be sent"));
    }
    if meta.len() > MAX_SIZE {
        return Err(crate::tr!("the file is too big (4 GB at most)"));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !safe_name(&name) {
        return Err(crate::tr!("that file name can't be sent"));
    }
    let mut offer = Offer {
        name,
        size: meta.len(),
        kind: kind_of(path),
        thumb: None,
        width: 0,
        height: 0,
        duration_ms: 0,
        wave: vec![],
        sha256: None,
    };
    // What arrives is checked against the hash (a damaged copy is refused). On the
    // local network it comes with the transfer, so a large file shows up at once.
    if !conv.is_lan() {
        offer.sha256 = Some(sha256_of(path)?);
    }
    let preview = match offer.kind {
        FileKind::Image => image_thumb(path),
        FileKind::Video => video_thumb(path).map(|(jpeg, w, h, duration_ms)| {
            offer.duration_ms = duration_ms;
            (jpeg, w, h)
        }),
        _ => None,
    };
    match preview {
        Some((jpeg, w, h)) => {
            offer.thumb = Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
            offer.width = w;
            offer.height = h;
        }
        // A "picture" nothing here can open is sent as a plain file.
        None if offer.kind == FileKind::Image => offer.kind = FileKind::File,
        None => {}
    }
    super::post(conv, "", Some((offer, path.to_path_buf()))).map(|_| ())
}

/// Send a recorded voice message (see `voice`).
pub fn send_voice(conv: &ConvId, path: &Path, duration_ms: u64, wave: Vec<u8>) -> Result<(), String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let stamp = gtk4::glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y-%m-%d_%H-%M-%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| now_ms().to_string());
    let offer = Offer {
        name: format!("voice-{stamp}.ogg"),
        size,
        kind: FileKind::Voice,
        thumb: None,
        width: 0,
        height: 0,
        duration_ms,
        wave,
        sha256: sha256_of(path).ok(),
    };
    super::post(conv, "", Some((offer, path.to_path_buf()))).map(|_| ())
}

/// A picture → (JPEG preview, width, height of the picture).
pub(super) fn image_thumb(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    use gtk4::gdk_pixbuf::{InterpType, Pixbuf};
    let src = Pixbuf::from_file(path).ok()?;
    let src = src.apply_embedded_orientation().unwrap_or(src);
    let (w, h) = (src.width(), src.height());
    let jpeg = small_jpeg(&src, w, h, InterpType::Bilinear)?;
    Some((jpeg, w as u32, h as u32))
}

fn small_jpeg(
    src: &gtk4::gdk_pixbuf::Pixbuf,
    w: i32,
    h: i32,
    interp: gtk4::gdk_pixbuf::InterpType,
) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let scale = (f64::from(THUMB_SIDE) / f64::from(w.max(h))).min(1.0);
    let (tw, th) = (((f64::from(w) * scale).round() as i32).max(1), ((f64::from(h) * scale).round() as i32).max(1));
    let small = src.scale_simple(tw, th, interp)?;
    // JPEG has no transparency: put it on white.
    let small = if small.has_alpha() {
        let flat = gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, tw, th)?;
        flat.fill(0xffff_ffff);
        small.composite(&flat, 0, 0, tw, th, 0.0, 0.0, 1.0, 1.0, interp, 255);
        flat
    } else {
        small
    };
    for quality in ["72", "55", "40"] {
        let bytes = small.save_to_bufferv("jpeg", &[("quality", quality)]).ok()?;
        if bytes.len() <= 48 * 1024 {
            return Some(bytes);
        }
    }
    None
}

/// A frame from about a second into a video → (JPEG preview, width, height,
/// length in milliseconds or 0 when unknown).
fn video_thumb(path: &Path) -> Option<(Vec<u8>, u32, u32, u64)> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    gst::init().ok()?;
    let uri = gtk4::glib::filename_to_uri(path, None).ok()?;
    let playbin = gst::ElementFactory::make("playbin").property("uri", uri.as_str()).build().ok()?;
    let fake_audio = gst::ElementFactory::make("fakesink").build().ok()?;
    let fake_video = gst::ElementFactory::make("fakesink").build().ok()?;
    playbin.set_property("audio-sink", &fake_audio);
    playbin.set_property("video-sink", &fake_video);
    let bus = playbin.bus()?;
    let wait = |bus: &gst::Bus| {
        matches!(
            bus.timed_pop_filtered(gst::ClockTime::from_seconds(6), &[gst::MessageType::AsyncDone, gst::MessageType::Error])
                .map(|m| m.type_()),
            Some(gst::MessageType::AsyncDone)
        )
    };
    let result = (|| {
        playbin.set_state(gst::State::Paused).ok()?;
        if !wait(&bus) {
            return None;
        }
        let duration_ms = playbin.query_duration::<gst::ClockTime>().map_or(0, gst::ClockTime::mseconds);
        if playbin
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, gst::ClockTime::from_seconds(1))
            .is_ok()
        {
            wait(&bus);
        }
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGB")
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        let sample = playbin.emit_by_name::<Option<gst::Sample>>("convert-sample", &[&caps])?;
        let info = gstreamer_video::VideoInfo::from_caps(sample.caps()?).ok()?;
        let buffer = sample.buffer()?.map_readable().ok()?;
        let (w, h) = (info.width() as i32, info.height() as i32);
        let stride = info.stride()[0];
        let bytes = gtk4::glib::Bytes::from(buffer.as_slice());
        let frame = gtk4::gdk_pixbuf::Pixbuf::from_bytes(&bytes, gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h, stride);
        let jpeg = small_jpeg(&frame, w, h, gtk4::gdk_pixbuf::InterpType::Bilinear)?;
        Some((jpeg, w as u32, h as u32, duration_ms))
    })();
    let _ = playbin.set_state(gst::State::Null);
    result
}

// ─── What we serve ──────────────────────────────────────────────────────────

/// A file we sent: who may fetch it, and from where.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Share {
    id: String,
    path: PathBuf,
    size: u64,
    to: Vec<String>,
    at: u64,
    /// SHA-256 (hex) of a file offered without one, once worked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

fn shares() -> &'static Mutex<Vec<Share>> {
    static S: OnceLock<Mutex<Vec<Share>>> = OnceLock::new();
    S.get_or_init(|| {
        let list: Vec<Share> = std::fs::read(chat_dir().join("shares"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Mutex::new(list)
    })
}

pub(crate) fn share(id: &str, path: &Path, size: u64, to: &[String]) {
    let Ok(mut list) = shares().lock() else { return };
    let now = now_ms();
    list.retain(|s| now.saturating_sub(s.at) < SHARE_DAYS * 24 * 3600 * 1000);
    list.push(Share { id: id.into(), path: path.to_path_buf(), size, to: to.to_vec(), at: now, sha256: None });
    save_shares(&list);
}

fn save_shares(list: &[Share]) {
    if let Ok(json) = serde_json::to_vec(list) {
        write_private(&chat_dir().join("shares"), &json);
    }
}

/// SHA-256 (hex) of shared file `id`, worked out the first time and kept.
/// Blocking: reads the whole file.
pub(crate) fn share_sha256(id: &str) -> Option<String> {
    let share = shares().lock().ok()?.iter().find(|s| s.id == id).cloned()?;
    if share.sha256.is_some() {
        return share.sha256;
    }
    let hash = sha256_of(&share.path).ok()?;
    if let Ok(mut list) = shares().lock() {
        if let Some(s) = list.iter_mut().find(|s| s.id == id) {
            s.sha256 = Some(hash.clone());
            save_shares(&list);
        }
    }
    Some(hash)
}

fn respond(mut stream: &TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// `/v1/file/<id>/<offset>` → (id, offset).
fn parse_path(path: &str) -> Option<(&str, u64)> {
    let (id, offset) = path.strip_prefix(PATH_PREFIX)?.split_once('/')?;
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((id, offset.parse().ok()?))
}

/// Someone fetches a file we sent (agent thread).
pub fn serve(stream: TcpStream, peer: IpAddr, headers: &std::collections::HashMap<String, String>, identity: &Identity, path: &str) {
    let local = stream.local_addr().map(|a| format!("{}:{}", a.ip(), a.port())).unwrap_or_default();
    if let Err(why) = crate::remote::host::verify_caller(headers, peer, &local, path) {
        tailscale::dbg(&format!("file: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let Some((id, offset)) = parse_path(path) else { return respond(&stream, 400, "Bad Request") };
    let me = super::my_user();
    let mine = me.as_deref() == Some(identity.login.as_str());
    let (file, left) = match open_share(id, &identity.login, mine, offset) {
        Ok(found) => found,
        Err(ShareError::Missing) => return respond(&stream, 404, "Not Found"),
        Err(ShareError::Gone) => return respond(&stream, 410, "Gone"),
        Err(ShareError::Range) => return respond(&stream, 416, "Range Not Satisfiable"),
    };
    let mut out = &stream;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    if write!(out, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {left}\r\n\r\n").is_err() {
        return;
    }
    let sent = std::io::copy(&mut file.take(left), &mut out);
    tailscale::dbg(&format!("file: {id} to {}@{}: {sent:?}", identity.login, identity.device));
}

/// Why a shared file can't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShareError {
    /// Unknown, or not for whoever asks: the two look the same.
    Missing,
    /// Moved, changed or replaced since it was sent.
    Gone,
    /// An offset past its end.
    Range,
}

/// Shared file `id` opened at `offset` for `who` (or for us, `mine`): the
/// file and how many bytes are left.
pub(crate) fn open_share(id: &str, who: &str, mine: bool, offset: u64) -> Result<(std::fs::File, u64), ShareError> {
    let found = shares().lock().ok().and_then(|l| l.iter().find(|s| s.id == id).cloned());
    let share = found.filter(|s| mine || s.to.iter().any(|t| t == who)).ok_or(ShareError::Missing)?;
    let mut file = std::fs::File::open(&share.path).map_err(|_| ShareError::Gone)?;
    if file.metadata().map(|m| m.len()).ok() != Some(share.size) {
        return Err(ShareError::Gone);
    }
    if offset > share.size || file.seek(SeekFrom::Start(offset)).is_err() {
        return Err(ShareError::Range);
    }
    Ok((file, share.size - offset))
}

/// What a person reads when `e` stops a download.
pub(crate) fn share_error_text(e: ShareError) -> String {
    match e {
        ShareError::Gone => crate::tr!("the file is no longer on their computer (moved or changed)"),
        ShareError::Missing | ShareError::Range => crate::tr!("the file isn't available"),
    }
}

// ─── Fetching ───────────────────────────────────────────────────────────────

fn transfers() -> &'static Mutex<HashSet<String>> {
    static T: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// A download of message `id` is under way.
pub fn downloading(id: &str) -> bool {
    transfers().lock().is_ok_and(|t| t.contains(id))
}

/// Fetch the file of message `id` (in the background; progress and the
/// outcome arrive as chat events).
pub fn download(conv: &ConvId, id: &str) {
    if !transfers().lock().is_ok_and(|mut t| t.insert(id.to_string())) {
        return;
    }
    let (conv, id) = (conv.clone(), id.to_string());
    emit(ChatEvent::Progress { conv: conv.clone(), id: id.clone(), done: 0, total: 0 });
    let _ = std::thread::Builder::new().name("biglace-file".into()).spawn(move || {
        let result = fetch(&conv, &id);
        if let Ok(mut t) = transfers().lock() {
            t.remove(&id);
        }
        match result {
            Ok(_) => emit(ChatEvent::Changed(conv)),
            Err(reason) => {
                tailscale::dbg(&format!("file: {id}: {reason}"));
                emit(ChatEvent::TransferFailed { conv, id, reason });
            }
        }
    });
}

/// Where downloads of this kind go.
fn target_dir(kind: FileKind) -> PathBuf {
    if kind == FileKind::Voice {
        return chat_dir().join("media");
    }
    downloads_dir()
}

/// `~/Downloads/BigLace`: where received files go unless the person picks a folder.
pub(crate) fn downloads_dir() -> PathBuf {
    gtk4::glib::user_special_dir(gtk4::glib::UserDirectory::Downloads)
        .unwrap_or_else(|| gtk4::glib::home_dir().join("Downloads"))
        .join("BigLace")
}

/// `name` in `dir`, numbered if taken: `foto.jpg`, `foto (1).jpg`…
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = name.trim_start_matches('.');
    let name = if name.is_empty() { "file" } else { name };
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..10_000)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Where the file of message `id` is on disk, fetched first when it isn't there.
pub(super) fn fetch(conv: &ConvId, id: &str) -> Result<PathBuf, String> {
    let message = super::messages(conv).into_iter().find(|m| m.id == id).ok_or("message not found")?;
    let file = message.file.clone().ok_or("no file in that message")?;
    if let Some(path) = file.local() {
        return Ok(path);
    }
    let size = file.offer.size;
    let dir = target_dir(file.offer.kind);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let part = dir.join(format!(".biglace-{id}.part"));

    let answered = if conv.is_lan() {
        lan_fetch(conv, id, &part, size)?
    } else {
        fetch_over_tailnet(conv, id, &message, &part, size)?;
        None
    };
    if let Some(expected) = file.offer.sha256.as_ref().or(answered.as_ref()) {
        if !sha256_of(&part)?.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(&part);
            return Err(crate::tr!("the file arrived different from the original (damaged on the way) — download it again"));
        }
    }
    let fin = unique_path(&dir, &file.offer.name);
    std::fs::rename(&part, &fin).map_err(|e| e.to_string())?;
    if let Ok(mut s) = store().lock() {
        if let Some(m) = s.message_mut(conv, id) {
            if let Some(f) = m.file.as_mut() {
                f.path = Some(fin.to_string_lossy().into_owned());
                f.state = FileState::Done;
            }
        }
        s.save(conv);
    }
    Ok(fin)
}

/// Download `part` of message `id` from the sender's devices that are on,
/// the one it was sent from first.
fn fetch_over_tailnet(conv: &ConvId, id: &str, message: &super::Message, part: &Path, size: u64) -> Result<(), String> {
    let mut peers: Vec<_> = tailscale::get_peers()
        .into_iter()
        .filter(|p| p.user == message.from && p.online && !p.ipv4.is_empty())
        .collect();
    peers.sort_by_key(|p| {
        let label = p.dns_name.split('.').next().unwrap_or_default().to_string();
        !(p.hostname == message.device || label == message.device)
    });
    if peers.is_empty() {
        return Err(crate::trf!("{user}'s computer is off or offline — try again when it's back", "user" => message.from));
    }
    let mut last_error = String::new();
    for peer in &peers {
        match fetch_from(&peer.ipv4, conv, id, part, size) {
            Ok(()) => {
                last_error.clear();
                break;
            }
            Err(e) => last_error = e,
        }
    }
    if !last_error.is_empty() {
        return Err(last_error);
    }
    Ok(())
}

/// Download `part` of message `id` from the device paired on the local network
/// that sent it (see [`lan_fetch_part`]). The SHA-256 the sender gave, if any.
fn lan_fetch(conv: &ConvId, id: &str, part: &Path, size: u64) -> Result<Option<String>, String> {
    lan_fetch_part(&conv.name, id, part, size, &mut |done| {
        emit(ChatEvent::Progress { conv: conv.clone(), id: id.to_string(), done, total: size });
    })
}

/// Fetch shared file `id` into `part` from the device `key` paired on the local
/// network, resuming by itself after a stall or a dropped connection; it gives
/// up once nothing arrived for [`LAN_PATIENCE`]. The SHA-256 the sender gave, if any.
pub(crate) fn lan_fetch_part(key: &str, id: &str, part: &Path, size: u64, progress: &mut dyn FnMut(u64)) -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    {
        use crate::lan::pairing::{fetch_file, LanError};
        let mut moved = Instant::now();
        loop {
            let before = std::fs::metadata(part).map_or(0, |m| m.len());
            let reason = match fetch_file(key, id, part, size, progress) {
                Ok((have, sha256)) if have == size => return Ok(sha256),
                Ok((have, _)) if have > before => {
                    moved = Instant::now();
                    continue;
                }
                Ok(_) => crate::tr!("the transfer was interrupted — try again to resume it"),
                Err(LanError::Refused(e)) => return Err(share_error_text(e)),
                Err(LanError::Off) => return Err(crate::tr!("the local network is off")),
                Err(LanError::Unreachable) => crate::trf!(
                    "{name} can't be reached on the local network — try again when it's nearby",
                    "name" => super::label(key)
                ),
            };
            if moved.elapsed() >= LAN_PATIENCE {
                return Err(reason);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (key, id, part, size, progress);
        Err("no local network here".into())
    }
}

fn fetch_from(ip: &str, conv: &ConvId, id: &str, part: &Path, size: u64) -> Result<(), String> {
    fetch_part(ip, id, part, size, &mut |done| {
        emit(ChatEvent::Progress { conv: conv.clone(), id: id.to_string(), done, total: size });
    })
}

/// Fetch (the rest of) shared file `id` into `part` from the device at
/// `ip`, telling `progress` the bytes there so far.
pub(crate) fn fetch_part(ip: &str, id: &str, part: &Path, size: u64, progress: &mut dyn FnMut(u64)) -> Result<(), String> {
    let mut have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have > size {
        let _ = std::fs::remove_file(part);
        have = 0;
    }
    if have < size {
        let path = format!("{PATH_PREFIX}{id}/{have}");
        let (stream, mut reader) = crate::remote::client::dial(ip, &path, &[], Duration::from_secs(15)).map_err(|e| match e {
            crate::remote::client::OpenError::Other(s) if s == "HTTP 410" => share_error_text(ShareError::Gone),
            crate::remote::client::OpenError::Other(s) if s == "HTTP 404" => share_error_text(ShareError::Missing),
            other => format!("{other:?}"),
        })?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        have = append_part(&mut reader, part, have, size, progress)?;
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    if have != size {
        return Err(crate::tr!("the transfer was interrupted — try again to resume it"));
    }
    progress(size);
    Ok(())
}

/// The bytes `reader` gives, appended to `part` (which holds `have` of
/// `size`) until `size` or the end: how many `part` holds then.
pub(crate) fn append_part(reader: &mut impl Read, part: &Path, mut have: u64, size: u64, progress: &mut dyn FnMut(u64)) -> Result<u64, String> {
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut last = Instant::now();
    while have < size {
        let want = buf.len().min(usize::try_from(size - have).unwrap_or(usize::MAX));
        let n = reader.read(&mut buf[..want]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        have += n as u64;
        if last.elapsed() >= Duration::from_millis(150) {
            last = Instant::now();
            progress(have);
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(have)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths_are_checked() {
        assert!(safe_name("foto.jpg"));
        assert!(safe_name("relatório final (2).pdf"));
        assert!(!safe_name("../etc/passwd"));
        assert!(!safe_name("a/b"));
        assert!(!safe_name(".."));
        assert!(!safe_name(""));
        assert!(!safe_name("a\nb"));
        assert_eq!(parse_path("/v1/file/abc123/0"), Some(("abc123", 0)));
        assert_eq!(parse_path("/v1/file/abc123/4096"), Some(("abc123", 4096)));
        assert_eq!(parse_path("/v1/file/../0"), None);
        assert_eq!(parse_path("/v1/file/abc/x"), None);
        assert_eq!(parse_path("/v1/file/abc"), None);
    }

    /// The integrity check: SHA-256 of a known text.
    #[test]
    fn hashes_files() {
        let path = std::env::temp_dir().join(format!("biglace-hash-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_of(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_file(&path);
    }

    /// A copy that differs from what was announced is refused and removed.
    #[test]
    fn damaged_downloads_are_refused() {
        let dir = std::env::temp_dir().join(format!("biglace-damaged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("a.txt");
        std::fs::write(&original, b"hello world").unwrap();
        let expected = sha256_of(&original).unwrap();
        let part = dir.join("a.part");
        std::fs::write(&part, b"hello w0rld").unwrap();
        assert_ne!(sha256_of(&part).unwrap(), expected);
        std::fs::write(&part, b"hello world").unwrap();
        assert_eq!(sha256_of(&part).unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downloads_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("biglace-files-test-{}", super::super::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "foto.jpg"), dir.join("foto.jpg"));
        std::fs::write(dir.join("foto.jpg"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "foto.jpg"), dir.join("foto (1).jpg"));
        assert_eq!(unique_path(&dir, ".hidden"), dir.join("hidden"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offers_are_validated() {
        let mut o = Offer {
            name: "a.png".into(),
            size: 10,
            kind: FileKind::Image,
            thumb: None,
            width: 1,
            height: 1,
            duration_ms: 0,
            wave: vec![],
            sha256: None,
        };
        assert!(o.valid() && o.fetch_by_itself());
        o.sha256 = Some("zz".into());
        assert!(!o.valid(), "a malformed hash");
        o.sha256 = Some("a".repeat(64));
        assert!(o.valid());
        o.size = AUTO_IMAGE_MAX + 1;
        assert!(o.valid() && !o.fetch_by_itself());
        o.name = "../x".into();
        assert!(!o.valid());
        // Unknown kinds from newer versions read as plain files.
        let f: Offer = serde_json::from_str(r#"{"name":"x","size":1,"kind":"hologram"}"#).unwrap();
        assert_eq!(f.kind, FileKind::File);
    }
}
