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
mod damage_x11;
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
    /// Host → viewer: capture time (host clock, µs since the epoch, u64 BE)
    /// followed by one H.264 access unit. Sent to viewers that ask for
    /// protocol 2 (header `X-BigLace-Screen: 2`); older ones get `Video`.
    VideoTs = 5,
    /// Viewer → host: viewer clock (µs, u64 BE). The host answers `Pong`.
    Ping = 6,
    /// Host → viewer: the ping's viewer clock, then the host clock (µs,
    /// u64 BE each): lets the viewer measure latency across both clocks.
    Pong = 7,
}

/// Screen protocol revision the viewer asks for.
pub const SCREEN_PROTOCOL: &str = "2";

/// Wall clock in µs since the epoch (both ends use it for latency).
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Info),
            2 => Some(Self::Video),
            3 => Some(Self::Input),
            4 => Some(Self::Error),
            5 => Some(Self::VideoTs),
            6 => Some(Self::Ping),
            7 => Some(Self::Pong),
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

/// Drop pointer moves that a later move in the same batch supersedes, keeping
/// every other event (clicks, keys, scroll) in order. A host that replays
/// input slower than it arrives (each portal call is a D-Bus round trip)
/// would otherwise fall further and further behind the viewer's hand.
pub fn coalesce(batch: Vec<Input>) -> Vec<Input> {
    let mut out: Vec<Input> = Vec::with_capacity(batch.len());
    for event in batch {
        if matches!(event, Input::Move { .. }) && matches!(out.last(), Some(Input::Move { .. })) {
            out.pop();
        }
        out.push(event);
    }
    out
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
    fn moves_coalesce_but_clicks_and_keys_stay_in_order() {
        let mv = |x: f64| Input::Move { x, y: 0.5 };
        let click = Input::Button { button: 1, pressed: true };
        let out = coalesce(vec![mv(0.1), mv(0.2), mv(0.3), click.clone(), mv(0.4), mv(0.5)]);
        assert_eq!(out, vec![mv(0.3), click, mv(0.5)]);
        assert!(coalesce(vec![]).is_empty());
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

/// The real host code feeding the real viewer code over a real TCP
/// connection, with a capture source slower than the rate it was asked for
/// (as ximagesrc in a VM: 60 fps asked, 25 delivered). Latency must stay
/// flat and low — it used to grow by the second.
#[cfg(test)]
mod end_to_end {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    #[test]
    fn slow_capture_keeps_latency_flat() {
        if gstreamer::init().is_err() || host::encoder().is_none() {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let stop = host::StopHandle::new(&s);
            let writer = Mutex::new(s);
            let stopper = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(5));
                stopper.stop();
            });
            // 60 fps asked; the source takes 40 ms per frame (25 fps).
            let head = "videotestsrc is-live=true pattern=ball ! video/x-raw,width=640,height=360,framerate=60/1 \
                        ! identity name=capture sleep-time=40000 ! videoconvert";
            let _ = host::stream_screen(&writer, head, None, &stop, true);
        });
        let stream = TcpStream::connect(addr).unwrap();
        let (tx, rx) = async_channel::bounded(256);
        let viewer = std::thread::spawn(move || client::decode_loop(std::io::BufReader::new(stream), &tx));
        let start = Instant::now();
        let mut stats = Vec::new();
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                client::Event::Stats(s) => stats.push(s),
                client::Event::Ended(_) => break,
                _ => {}
            }
            if start.elapsed() > Duration::from_secs(8) {
                break;
            }
        }
        drop(rx);
        let _ = host.join();
        let _ = viewer.join();
        eprintln!("stats: {stats:#?}");
        assert!(stats.len() >= 3, "too few stats: {stats:?}");
        let tail = &stats[1..];
        let worst = tail.iter().map(|s| s.latency_ms).fold(0.0, f64::max);
        let fps = tail.iter().map(|s| s.fps).sum::<f64>() / tail.len() as f64;
        assert!(worst < 200.0, "latency grew to {worst:.0} ms");
        assert!(fps > 15.0, "only {fps:.1} fps");
        let (first, last) = (tail[0].latency_ms, tail[tail.len() - 1].latency_ms);
        assert!(last < first + 100.0, "latency drifting: {first:.0} → {last:.0} ms");
    }
}
