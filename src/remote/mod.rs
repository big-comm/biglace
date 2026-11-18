//! Remote desktop between BigLace devices: one device (the host) shows its
//! screen to another (the viewer) and takes its mouse and keyboard.
//!
//! Transport: the viewer opens one TCP connection to the host's agent port
//! over the tailnet (WireGuard-encrypted) and asks for [`SCREEN_PATH`] with a
//! request signed by its device key. The host checks the signature, asks the
//! viewer's own BigLace (over the tunnel) to prove it holds that key — so a
//! VM or container behind the viewer's machine can't pass as it — and then
//! asks the person at the host: "Allow / Deny". If allowed, the same
//! connection carries length-prefixed frames both ways (see [`Frame`]).
//!
//! Video is H.264 (low-latency settings) produced and decoded by GStreamer;
//! input is replayed on the host (XTest on X11).

pub mod client;
pub mod host;
#[cfg(target_os = "linux")]
mod input_x11;
#[cfg(target_os = "linux")]
mod portal;

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::sync::OnceLock;

/// Agent path of a remote-screen session.
pub const SCREEN_PATH: &str = "/v1/screen";
/// Capability announced in the agent's hello when this device can be a host.
pub const CAPABILITY: &str = "screen";

/// Largest frame accepted (a keyframe of a 4K screen fits comfortably).
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Frame kinds on a session connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Host → viewer: JSON [`VideoInfo`].
    Info = 1,
    /// Host → viewer: one H.264 access unit (Annex B byte stream).
    Video = 2,
    /// Viewer → host: JSON [`Input`].
    Input = 3,
    /// Host → viewer: why the session can't go on (UTF-8 text).
    Error = 4,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Info),
            2 => Some(Self::Video),
            3 => Some(Self::Input),
            4 => Some(Self::Error),
            _ => None,
        }
    }
}

/// Write one frame: kind (1 byte), length (u32 big-endian), payload.
pub fn write_frame(w: &mut impl Write, kind: Kind, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too large"))?;
    let mut head = [0u8; 5];
    head[0] = kind as u8;
    head[1..].copy_from_slice(&len.to_be_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame. Unknown kinds and oversized frames are errors (the
/// session ends): both sides are BigLace, so they never send either.
pub fn read_frame(r: &mut impl Read) -> io::Result<(Kind, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let kind = Kind::from_u8(head[0]).ok_or_else(|| io::Error::other("unknown frame kind"))?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((kind, payload))
}

/// Size of the shared screen, sent before the first video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
}

/// One input event from the viewer. Pointer positions are normalized to
/// 0..1 of the shared screen, so window size and scaling don't matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Input {
    #[serde(rename = "move")]
    Move { x: f64, y: f64 },
    /// X11 button numbers: 1 left, 2 middle, 3 right.
    #[serde(rename = "button")]
    Button { button: u8, pressed: bool },
    /// Scroll steps (positive = down / right).
    #[serde(rename = "scroll")]
    Scroll { dx: i32, dy: i32 },
    /// X keysym (GDK keyvals are X keysyms).
    #[serde(rename = "key")]
    Key { keysym: u32, pressed: bool },
}

/// Someone asks to see and control this screen. The UI answers on `reply`.
pub struct ConsentRequest {
    /// Account name (e.g. `tales`).
    pub who: String,
    /// Their device (e.g. `tales-notebook`).
    pub device: String,
    pub reply: std::sync::mpsc::Sender<bool>,
}

/// A running session on this host, for the "end access" indicator.
#[derive(Clone)]
pub struct HostSession {
    pub who: String,
    pub device: String,
    pub stop: host::StopHandle,
}

/// Events from the agent threads to the window.
pub enum HostEvent {
    Consent(ConsentRequest),
    Started(HostSession),
    Ended(host::StopHandle),
}

type Channel = (async_channel::Sender<HostEvent>, async_channel::Receiver<HostEvent>);

fn channel() -> &'static Channel {
    static CH: OnceLock<Channel> = OnceLock::new();
    CH.get_or_init(async_channel::unbounded)
}

/// Where the window listens for host-side events (consent, sessions).
pub fn host_events() -> async_channel::Receiver<HostEvent> {
    channel().1.clone()
}

pub(crate) fn emit(event: HostEvent) -> bool {
    channel().0.send_blocking(event).is_ok()
}

/// A Wayland session (GNOME, Plasma…): capture and input go through the
/// remote-desktop portal instead of X11.
pub fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Whether this device can be a remote-screen host: Linux with the GStreamer
/// pieces the host pipeline needs — X11 (XTest) or Wayland (portal +
/// PipeWire).
pub fn can_host() -> bool {
    #[cfg(target_os = "linux")]
    {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| {
            if gstreamer::init().is_err() || host::encoder().is_none() {
                return false;
            }
            let source = if wayland_session() { "pipewiresrc" } else { "ximagesrc" };
            let display = wayland_session() || std::env::var_os("DISPLAY").is_some();
            display && gstreamer::ElementFactory::find(source).is_some()
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_reject_junk() {
        let mut buf = Vec::new();
        write_frame(&mut buf, Kind::Video, b"abc").unwrap();
        write_frame(&mut buf, Kind::Input, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), (Kind::Video, b"abc".to_vec()));
        assert_eq!(read_frame(&mut r).unwrap(), (Kind::Input, vec![]));
        assert!(read_frame(&mut r).is_err());
        let mut bad = &[9u8, 0, 0, 0, 0][..];
        assert!(read_frame(&mut bad).is_err());
        let huge = [2u8, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut &huge[..]).is_err());
    }

    #[test]
    fn input_json_is_compact_and_tagged() {
        let e = Input::Key { keysym: 0x61, pressed: true };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"t":"key","keysym":97,"pressed":true}"#);
        assert_eq!(serde_json::from_str::<Input>(&json).unwrap(), e);
        assert!(serde_json::from_str::<Input>(r#"{"t":"nope"}"#).is_err());
    }
}
