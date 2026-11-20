//! A phone and its owner's computers (`/v1/link` on the agent): the phone
//! keeps one connection to each of them and sends what happens on it
//! (battery and storage, a ringing call, a link to open, its notifications);
//! the computer answers a notification through the same connection. Only
//! devices of our own account get in, each request signed by its device key.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::remote::{read_frame, write_frame, Kind};
use crate::tailscale::{self, Identity};

pub const PATH: &str = "/v1/link";
/// What `/v1/hello` lists when this computer takes phone links.
pub const CAPABILITY: &str = "link";

/// Longest text taken from a phone (a notification body, a link).
const MAX_TEXT: usize = 2000;
/// The phone says something at least this often (it pings).
const SILENCE: Duration = Duration::from_secs(90);

/// What a phone sends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum FromPhone {
    Ping,
    Status { battery: u8, charging: bool, storage_free: u64, storage_total: u64 },
    Call { ringing: bool },
    Open { url: String },
    Notify { key: String, app: String, title: String, text: String, can_reply: bool },
    Removed { key: String },
}

/// What we send a phone.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ToPhone<'a> {
    Reply { key: &'a str, text: &'a str },
}

/// A phone's battery and storage, as it last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhoneStatus {
    pub battery: u8,
    pub charging: bool,
    pub storage_free: u64,
    pub storage_total: u64,
}

/// What the window learns.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    /// A phone's status changed, or it left.
    Status,
    /// The phone shared a link to open here.
    Open { device: String, url: String },
    /// One of the phone's notifications.
    Notify { ip: String, device: String, key: String, app: String, title: String, text: String, can_reply: bool },
    /// That notification is gone from the phone.
    Removed { ip: String, key: String },
}

type Events = (async_channel::Sender<LinkEvent>, async_channel::Receiver<LinkEvent>);

fn events_channel() -> &'static Events {
    static E: OnceLock<Events> = OnceLock::new();
    E.get_or_init(async_channel::unbounded)
}

/// Link events, for the window (one listener).
pub fn events() -> async_channel::Receiver<LinkEvent> {
    events_channel().1.clone()
}

fn emit(e: LinkEvent) {
    let _ = events_channel().0.try_send(e);
}

/// Connected phones: address → (what it said, the way back to it).
type Phones = Mutex<HashMap<String, (Option<PhoneStatus>, Arc<Mutex<TcpStream>>)>>;

fn phones() -> &'static Phones {
    static P: OnceLock<Phones> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// The battery and storage of the phone at `ip`, while it's connected.
pub fn status(ip: &str) -> Option<PhoneStatus> {
    phones().lock().ok()?.get(ip).and_then(|(s, _)| *s)
}

/// Answer notification `key` on the phone at `ip` with `text`.
pub fn reply(ip: &str, key: &str, text: &str) -> Result<(), String> {
    let writer = phones().lock().ok().and_then(|p| p.get(ip).map(|(_, w)| w.clone())).ok_or("the phone is no longer connected")?;
    let payload = serde_json::to_vec(&ToPhone::Reply { key, text }).map_err(|e| e.to_string())?;
    let mut w = writer.lock().map_err(|_| "the phone is no longer connected")?;
    write_frame(&mut *w, Kind::Link, &payload).map_err(|e| e.to_string())
}

fn respond(mut stream: &TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// A phone of ours connects (agent thread); returns when it goes.
pub fn accept(stream: TcpStream, peer: IpAddr, headers: &HashMap<String, String>, identity: &Identity, path: &str) {
    let local = stream.local_addr().map(|a| format!("{}:{}", a.ip(), a.port())).unwrap_or_default();
    if let Err(why) = crate::remote::host::verify_caller(headers, peer, &local, path) {
        tailscale::dbg(&format!("link: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let me = crate::chat::my_user().or_else(crate::chat::my_groups_refresh);
    if me.as_deref() != Some(identity.login.as_str()) {
        tailscale::dbg(&format!("link: refused {}@{}: not our account", identity.login, identity.device));
        return respond(&stream, 403, "Forbidden");
    }
    let _ = write!(&stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-link\r\n\r\n");
    let _ = stream.set_read_timeout(Some(SILENCE));
    let (Ok(read_half), Ok(write_half)) = (stream.try_clone(), stream.try_clone()) else { return };
    let ip = peer.to_string();
    let writer = Arc::new(Mutex::new(write_half));
    if let Ok(mut p) = phones().lock() {
        p.insert(ip.clone(), (None, writer.clone()));
    }
    tailscale::dbg(&format!("link: {}@{} connected", identity.login, identity.device));
    let mut reader = BufReader::new(read_half);
    let mut ringing_paused = Vec::new();
    while let Ok((Kind::Link, payload)) = read_frame(&mut reader) {
        if let Ok(msg) = serde_json::from_slice::<FromPhone>(&payload) {
            handle(msg, &ip, &identity.device, &mut ringing_paused);
        }
    }
    mpris::resume(&ringing_paused);
    // A phone that came back has a newer connection under the same address.
    if let Ok(mut p) = phones().lock() {
        if p.get(&ip).is_some_and(|(_, w)| Arc::ptr_eq(w, &writer)) {
            p.remove(&ip);
        }
    }
    emit(LinkEvent::Status);
    tailscale::dbg(&format!("link: {}@{} left", identity.login, identity.device));
}

/// What a phone said, applied here.
fn handle(msg: FromPhone, ip: &str, device: &str, ringing_paused: &mut Vec<String>) {
    match msg {
        FromPhone::Ping => {}
        FromPhone::Status { battery, charging, storage_free, storage_total } => {
            let s = PhoneStatus { battery: battery.min(100), charging, storage_free, storage_total };
            if let Ok(mut p) = phones().lock() {
                if let Some(entry) = p.get_mut(ip) {
                    entry.0 = Some(s);
                }
            }
            emit(LinkEvent::Status);
        }
        FromPhone::Call { ringing: true } => *ringing_paused = mpris::pause_playing(),
        FromPhone::Call { ringing: false } => mpris::resume(&std::mem::take(ringing_paused)),
        FromPhone::Open { url } => {
            if url_ok(&url) {
                emit(LinkEvent::Open { device: device.to_string(), url });
            }
        }
        FromPhone::Notify { key, app, title, text, can_reply } => {
            if key.len() <= 256 {
                emit(LinkEvent::Notify {
                    ip: ip.to_string(),
                    device: device.to_string(),
                    key,
                    app: clip(&app),
                    title: clip(&title),
                    text: clip(&text),
                    can_reply,
                });
            }
        }
        FromPhone::Removed { key } => emit(LinkEvent::Removed { ip: ip.to_string(), key }),
    }
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Only web pages open from a phone (no file:, no custom schemes).
fn url_ok(url: &str) -> bool {
    url.len() <= MAX_TEXT
        && (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The session's media players (MPRIS) on D-Bus.
mod mpris {
    use gtk4::gio::{self, prelude::*};
    use gtk4::glib::Variant;

    const PREFIX: &str = "org.mpris.MediaPlayer2.";
    const PATH: &str = "/org/mpris/MediaPlayer2";
    const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

    fn call(bus: &gio::DBusConnection, dest: &str, path: &str, iface: &str, method: &str, args: Option<&Variant>) -> Option<Variant> {
        bus.call_sync(Some(dest), path, iface, method, args, None, gio::DBusCallFlags::NONE, 2000, gio::Cancellable::NONE).ok()
    }

    /// Pause whatever plays now; returns the players paused (to resume them).
    pub fn pause_playing() -> Vec<String> {
        let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return vec![] };
        let names = call(&bus, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", None)
            .and_then(|v| v.child_value(0).get::<Vec<String>>())
            .unwrap_or_default();
        let mut paused = Vec::new();
        for name in names.into_iter().filter(|n| n.starts_with(PREFIX)) {
            let status = call(&bus, &name, PATH, "org.freedesktop.DBus.Properties", "Get", Some(&(PLAYER, "PlaybackStatus").to_variant()))
                .and_then(|v| v.child_value(0).as_variant())
                .and_then(|v| v.get::<String>());
            if status.as_deref() == Some("Playing") && call(&bus, &name, PATH, PLAYER, "Pause", None).is_some() {
                paused.push(name);
            }
        }
        if !paused.is_empty() {
            crate::tailscale::dbg(&format!("link: call ringing, paused {paused:?}"));
        }
        paused
    }

    /// Play again what [`pause_playing`] paused.
    pub fn resume(players: &[String]) {
        if players.is_empty() {
            return;
        }
        let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
        for name in players {
            let _ = call(&bus, name, PATH, PLAYER, "Play", None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_messages_read() {
        let m: FromPhone = serde_json::from_str(r#"{"t":"status","battery":81,"charging":true,"storage_free":10,"storage_total":64}"#).unwrap();
        assert_eq!(m, FromPhone::Status { battery: 81, charging: true, storage_free: 10, storage_total: 64 });
        let m: FromPhone = serde_json::from_str(r#"{"t":"call","ringing":true}"#).unwrap();
        assert_eq!(m, FromPhone::Call { ringing: true });
        assert!(serde_json::from_str::<FromPhone>(r#"{"t":"run","cmd":"rm"}"#).is_err());
        let reply = serde_json::to_string(&ToPhone::Reply { key: "k", text: "oi" }).unwrap();
        assert_eq!(reply, r#"{"t":"reply","key":"k","text":"oi"}"#);
    }

    #[test]
    fn only_web_links_open() {
        assert!(url_ok("https://biglinux.com.br/x?y=1"));
        assert!(url_ok("http://example.org"));
        assert!(!url_ok("file:///etc/passwd"));
        assert!(!url_ok("javascript:alert(1)"));
        assert!(!url_ok("https://a b"));
        assert!(!url_ok(&format!("https://{}", "a".repeat(MAX_TEXT))));
    }
}
