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
mod cursor_x11;
#[cfg(target_os = "linux")]
pub(crate) mod damage_x11;
pub(crate) mod display;
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

/// Video tests (real encoders and decoders, timing-sensitive) take turns:
/// run in parallel they fight for the CPU and their latency numbers lie.
#[cfg(test)]
pub(crate) fn heavy_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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
    /// Host → viewer: one Opus packet of the host's sound output (protocol
    /// 3 viewers only).
    Audio = 8,
    /// Viewer → host: send a keyframe now (empty payload).
    KeyFrame = 9,
    /// Voice room, both ways: JSON `{ "muted": bool }`.
    RoomState = 10,
    /// Chat, sender → receiver: one JSON message (see `crate::chat`).
    Chat = 11,
    /// Chat, receiver → sender: JSON list of the message ids stored.
    ChatAck = 12,
    /// Room screen share, both ways (JSON): viewer → presenter
    /// `{"ask": true}` / `{"release": true}`; presenter → viewer
    /// `{"granted": bool}`.
    Control = 13,
    /// Remote session, both ways (JSON): viewer → host
    /// `{"action": "restart" | "shutdown"}`; host → viewer
    /// `{"action": …, "ok": bool, "error": "…"}`.
    Power = 14,
    /// Host → viewer (protocol 4): the pointer. Empty: it's drawn in the
    /// video (hide the viewer's own). Else width, height, hotspot x, y
    /// (u16 BE each) and RGBA rows: the shape to give the viewer's pointer.
    Cursor = 15,
    /// Viewer → host (JSON): fit the screen to the viewer's window,
    /// `{"width": …, "height": …}`, or `{"restore": true}`.
    Resize = 16,
    /// Voice room, both ways: one camera picture — capture time (µs, u64
    /// BE) and an H.264 access unit.
    CamVideo = 17,
    /// Phone ↔ its owner's computers (JSON): battery, calls, links,
    /// notifications and replies (see `crate::link`).
    Link = 18,
    /// Local network, no server (JSON): proving device keys and pairing
    /// (see `crate::lan`).
    Lan = 19,
}

/// Announced by hosts that can fit their resolution to the viewer's window.
pub const RESIZE_CAPABILITY: &str = "resize";

/// Screen protocol revision the viewer asks for.
/// 2: timestamped video, ping/pong. 3: plus sound.
pub const SCREEN_PROTOCOL: &str = "4";

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
            8 => Some(Self::Audio),
            9 => Some(Self::KeyFrame),
            10 => Some(Self::RoomState),
            11 => Some(Self::Chat),
            12 => Some(Self::ChatAck),
            13 => Some(Self::Control),
            14 => Some(Self::Power),
            15 => Some(Self::Cursor),
            16 => Some(Self::Resize),
            17 => Some(Self::CamVideo),
            18 => Some(Self::Link),
            19 => Some(Self::Lan),
            _ => None,
        }
    }
}

/// Kernel send buffer for a screen stream: big enough for a keyframe in
/// flight, small enough that a slow link backs up into the (leaky) queue
/// before the encoder, where frames are dropped instead of piling up seconds
/// of video in the kernel.
const SEND_BUFFER: usize = 512 * 1024;
#[cfg(target_os = "linux")]
const NOTSENT_LOWAT: u32 = 64 * 1024;

/// Caps how much of a screen stream the kernel may hold for `stream`: only a
/// little unsent data may sit there, so the next frame waits instead.
pub(crate) fn limit_send_queue(stream: &std::net::TcpStream) {
    let sock = socket2::SockRef::from(stream);
    let _ = sock.set_send_buffer_size(SEND_BUFFER);
    #[cfg(target_os = "linux")]
    let _ = sock.set_tcp_notsent_lowat(NOTSENT_LOWAT);
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Chroma of the H.264 stream: "420" or "444" (full color, software
    /// encoders). GPU decoders usually take only 4:2:0. Absent from older
    /// hosts (then: 4:2:0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroma: Option<String>,
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
    /// "Always allow" can be offered: the person is a BigScale account.
    pub rememberable: bool,
    /// (allowed, remember: always allow this person).
    pub reply: std::sync::mpsc::Sender<(bool, bool)>,
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
            // Presence only: test-encoding loads hardware drivers (CUDA for
            // NVENC), which waits for the first share.
            if gstreamer::init().is_err() || !host::Encoder::ALL.iter().any(|e| gstreamer::ElementFactory::find(e.element()).is_some()) {
                return false;
            }
            if wayland_session() {
                // The portal must really work here (some setups point it at
                // another desktop's backends, which offer nothing).
                gstreamer::ElementFactory::find("pipewiresrc").is_some() && portal::available()
            } else {
                std::env::var_os("DISPLAY").is_some() && gstreamer::ElementFactory::find("ximagesrc").is_some()
            }
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
        let mut bad = &[200u8, 0, 0, 0, 0][..];
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
        let _turn = heavy_test();
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
                std::thread::sleep(Duration::from_secs(7));
                stopper.stop();
            });
            // 60 fps asked; the source takes 40 ms per frame (25 fps).
            let head = "videotestsrc is-live=true pattern=ball ! video/x-raw,width=640,height=360,framerate=60/1 \
                        ! identity name=capture sleep-time=40000 ! videoconvert";
            let enc = host::encoder().unwrap();
            // First a pipeline that can't negotiate (DMA-BUF from a source
            // that has none, as on a compositor without it): the host must
            // fall back to the next one.
            let broken = host::pipeline_desc(
                "videotestsrc is-live=true ! video/x-raw(memory:DMABuf) ! identity name=capture",
                enc,
            );
            let pipelines = [broken, host::pipeline_desc(head, enc)];
            let send = |kind: Kind, payload: &[u8]| -> anyhow::Result<()> {
                let mut w = writer.lock().map_err(|_| anyhow::anyhow!("lock"))?;
                write_frame(&mut *w, kind, payload)?;
                Ok(())
            };
            let _ = host::stream_screen(&send, &pipelines, None, &stop, true, &std::sync::atomic::AtomicBool::new(false), &std::sync::atomic::AtomicBool::new(false));
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

/// Viewer decoding (GPU / CPU / fallback) and sound, end to end over TCP.
#[cfg(test)]
mod decode_and_sound {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn encode(format: &str, frames: u32) -> Vec<Vec<u8>> {
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers={frames} pattern=ball ! video/x-raw,width=640,height=360,framerate=30/1 \
             ! videoconvert ! video/x-raw,format={format} \
             ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=10 bframes=0 \
             ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = p.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Some(s) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            out.push(s.buffer().unwrap().map_readable().unwrap().as_slice().to_vec());
        }
        p.set_state(gst::State::Null).unwrap();
        out
    }

    /// Host side of a test session: optional Info, then timestamped units
    /// at 30 fps; answers KeyFrame requests by restarting from a keyframe
    /// (units are encoded with one every 10 frames).
    fn serve(units: Vec<Vec<u8>>, chroma: Option<&'static str>, audio: bool) -> (std::thread::JoinHandle<bool>, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut reader = s.try_clone().unwrap();
            let asked = Arc::new(AtomicBool::new(false));
            {
                let asked = asked.clone();
                std::thread::spawn(move || {
                    while let Ok((kind, _)) = read_frame(&mut reader) {
                        if kind == Kind::KeyFrame {
                            asked.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                });
            }
            let writer = Arc::new(Mutex::new(s));
            if let Some(c) = chroma {
                let info = VideoInfo { width: 640, height: 360, chroma: Some(c.into()) };
                write_frame(&mut *writer.lock().unwrap(), Kind::Info, &serde_json::to_vec(&info).unwrap()).unwrap();
            }
            let stop = host::StopHandle::new(&writer.lock().unwrap().try_clone().unwrap());
            if audio {
                let (w, st) = (writer.clone(), stop.clone());
                std::thread::spawn(move || {
                    let desc = "audiotestsrc is-live=true wave=sine ! audioconvert ! audioresample \
                                ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 frame-size=10 \
                                ! appsink name=asink sync=false";
                    let _ = host::stream_audio_from(&w, &st, desc);
                });
            }
            for u in units.iter().cycle().take(120) {
                let mut payload = now_us().to_be_bytes().to_vec();
                payload.extend_from_slice(u);
                if write_frame(&mut *writer.lock().unwrap(), Kind::VideoTs, &payload).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            stop.stop();
            asked.load(std::sync::atomic::Ordering::Acquire)
        });
        (h, addr)
    }

    /// Run a viewer against `addr`; returns (frames decoded, last stats).
    fn view(addr: std::net::SocketAddr) -> (u32, Vec<client::Stats>) {
        let stream = TcpStream::connect(addr).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let (key_tx, key_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for out in key_rx {
                if matches!(out, client::Outgoing::KeyFrame) {
                    let _ = write_frame(&mut writer, Kind::KeyFrame, &[]);
                }
            }
        });
        let (tx, rx) = async_channel::bounded(512);
        let muted = Arc::new(AtomicBool::new(false));
        let sink = "appsrc name=asrc is-live=true format=time do-timestamp=true \
                    caps=audio/x-opus,channel-mapping-family=0,channels=2,rate=48000 \
                    ! opusdec ! audioconvert ! volume name=vol ! fakesink sync=false";
        let t = std::thread::spawn(move || {
            let _ = client::decode_loop_with(std::io::BufReader::new(stream), &tx, &muted, sink, Some(&key_tx));
        });
        let (mut frames, mut stats) = (0u32, Vec::new());
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                client::Event::Frame(_) => frames += 1,
                client::Event::Stats(s) => stats.push(s),
                client::Event::Ended(_) => break,
                _ => {}
            }
        }
        let _ = t.join();
        (frames, stats)
    }

    fn ready() -> bool {
        gst::init().is_ok() && gst::ElementFactory::find("x264enc").is_some() && gst::ElementFactory::find("avdec_h264").is_some()
    }

    #[test]
    fn gpu_decodes_420_streams_where_available() {
        let _turn = heavy_test();
        if !ready() {
            return;
        }
        let (h, addr) = serve(encode("I420", 40), Some("420"), false);
        let (frames, stats) = view(addr);
        let _ = h.join();
        assert!(frames > 60, "only {frames} frames");
        let gpu_here = client::gpu_decoder_available();
        assert!(stats.iter().all(|s| s.gpu == gpu_here), "decoder choice: {stats:?}");
    }

    #[test]
    fn full_color_streams_decode_on_the_cpu() {
        let _turn = heavy_test();
        if !ready() {
            return;
        }
        let (h, addr) = serve(encode("Y444", 40), Some("444"), false);
        let (frames, stats) = view(addr);
        let _ = h.join();
        assert!(frames > 60, "only {frames} frames");
        assert!(stats.iter().all(|s| !s.gpu), "{stats:?}");
    }

    /// An older host doesn't say its stream is 4:4:4: the GPU tries, fails,
    /// and the viewer switches to the CPU and asks for a keyframe.
    #[test]
    fn gpu_failure_falls_back_to_cpu() {
        let _turn = heavy_test();
        if !ready() || !client::gpu_decoder_available() {
            return;
        }
        let (h, addr) = serve(encode("Y444", 40), Some("420"), false);
        let (frames, stats) = view(addr);
        let asked_key = h.join().unwrap();
        assert!(asked_key, "viewer didn't ask for a keyframe");
        assert!(frames > 40, "only {frames} frames after the fallback");
        assert!(stats.last().is_some_and(|s| !s.gpu), "{stats:?}");
    }

    #[test]
    fn host_sound_reaches_the_viewer() {
        let _turn = heavy_test();
        if !ready() || gst::ElementFactory::find("opusenc").is_none() {
            return;
        }
        let (h, addr) = serve(encode("I420", 40), Some("420"), true);
        let (_, stats) = view(addr);
        let _ = h.join();
        assert!(stats.iter().filter(|s| s.audio).count() >= 2, "no sound: {stats:?}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod send_queue {
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn screen_socket_holds_little_unsent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let sock = socket2::SockRef::from(&stream);
        // Start above the cap, so only the call can bring the buffer down.
        sock.set_send_buffer_size(8 * super::SEND_BUFFER).unwrap();
        super::limit_send_queue(&stream);
        let wmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/wmem_max").unwrap().trim().parse().unwrap();
        // Linux caps the request at wmem_max and doubles it for its own
        // bookkeeping; below SEND_BUFFER the call has nothing to lower.
        assert_eq!(sock.send_buffer_size().unwrap(), 2 * super::SEND_BUFFER.min(wmem_max));
        assert_eq!(sock.tcp_notsent_lowat().unwrap(), super::NOTSENT_LOWAT);
    }
}
