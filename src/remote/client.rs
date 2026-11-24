//! Viewer side of a remote-screen session: ask the host, then decode its
//! screen and send it our input.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::{read_frame, write_frame, Input, Kind, VideoInfo};
use crate::device_key;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// No word from the host for this long ends the session.
const SILENCE: Duration = Duration::from_secs(15);
/// The host waits up to a minute for the person there to answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(75);

/// Why a session couldn't start.
#[derive(Debug)]
pub enum OpenError {
    /// The person at the host said no (or didn't answer).
    Denied,
    /// The host couldn't verify this device (old BigLace, agent not running).
    Unverified,
    /// The host can't share its screen (no X11 session, no encoder…).
    Unavailable,
    Other(String),
}

/// One decoded frame, RGBA, rows of `stride` bytes. `data` wraps the
/// decoder's own buffer (no copy on the way to the screen).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: gtk4::glib::Bytes,
    /// When decoding finished: the window adds its own time to draw it.
    pub decoded_at: std::time::Instant,
}

/// Live numbers about the session, once a second.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// Capture on the host → decoded here (both clocks reconciled). Drawing
    /// it on screen comes on top.
    pub latency_ms: f64,
    pub fps: f64,
    pub mbps: f64,
    /// Decoding on this machine's GPU.
    pub gpu: bool,
    /// Sound is coming in.
    pub audio: bool,
}

/// The host's pointer shape, RGBA rows of `width * 4` bytes.
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub data: gtk4::glib::Bytes,
}

/// What the session thread tells the window.
pub enum Event {
    Info(VideoInfo),
    Frame(Frame),
    Stats(Stats),
    /// Room share: whether we may control the screen now.
    Control(bool),
    /// Room share: this screen can't be controlled at all (a phone's).
    ViewOnly,
    /// The host's pointer: its shape (`Some`), or drawn in the video
    /// (`None`: hide the viewer's own).
    Cursor(Option<CursorImage>),
    /// The host's answer to a power action.
    Power { action: String, ok: bool, error: String },
    Ended(String),
}

pub(crate) enum Outgoing {
    /// Room share: JSON for a `Control` frame.
    Control(String),
    /// JSON for a `Power` frame.
    Power(String),
    /// JSON for a `Resize` frame.
    Resize(String),
    Input(Input),
    Ping,
    /// Ask the host for a keyframe (after switching decoders).
    KeyFrame,
}

/// A live session: send input with `input`; drop or `close` to end it.
pub struct Session {
    input: mpsc::Sender<Outgoing>,
    stream: TcpStream,
    muted: Arc<std::sync::atomic::AtomicBool>,
    /// Our input goes to the host (always for remote access; in a room
    /// share only once the presenter gave us control).
    input_on: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    pub fn send(&self, event: Input) {
        if self.input_on.load(Ordering::Relaxed) {
            let _ = self.input.send(Outgoing::Input(event));
        }
    }

    /// Ask the host to fit its screen to a `width`×`height` window (pixels).
    pub fn fit(&self, width: u32, height: u32) {
        let _ = self.input.send(Outgoing::Resize(serde_json::json!({ "width": width, "height": height }).to_string()));
    }

    /// Ask the host to go back to its own resolution.
    pub fn restore_size(&self) {
        let _ = self.input.send(Outgoing::Resize(r#"{"restore":true}"#.into()));
    }

    /// Ask the host to `restart` (and come back) or `shutdown`.
    pub fn power(&self, action: &str) {
        let _ = self.input.send(Outgoing::Power(serde_json::json!({ "action": action }).to_string()));
    }

    /// Room share: ask the presenter for control.
    pub fn ask_control(&self) {
        let _ = self.input.send(Outgoing::Control(r#"{"ask":true}"#.into()));
    }

    /// Room share: hand control back.
    pub fn release_control(&self) {
        self.input_on.store(false, Ordering::Relaxed);
        let _ = self.input.send(Outgoing::Control(r#"{"release":true}"#.into()));
    }

    pub(crate) fn set_input(&self, on: bool) {
        self.input_on.store(on, Ordering::Relaxed);
    }

    /// Mute or unmute the host's sound here.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn close(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// Ask `ip` for its screen. Blocks until the person there answers (up to
/// [`ANSWER_TIMEOUT`]); call it on a worker thread. Events (frames, end)
/// are delivered on `events`.
/// Open a signed request to `path` on the agent at `ip` and wait for the
/// answer (up to `answer_within`: the screen waits for the person there to
/// click). On 200, the connection and a reader positioned after the headers.
pub(crate) fn dial(
    ip: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    answer_within: Duration,
) -> Result<(TcpStream, BufReader<TcpStream>), OpenError> {
    let addr: IpAddr = ip.parse().map_err(|_| OpenError::Other("bad address".into()))?;
    let sock = SocketAddr::new(addr, crate::agent::PORT);
    let stream = TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT).map_err(|e| OpenError::Other(e.to_string()))?;
    let host = match addr {
        IpAddr::V6(v6) => format!("[{v6}]:{}", crate::agent::PORT),
        IpAddr::V4(v4) => format!("{v4}:{}", crate::agent::PORT),
    };
    let url = format!("http://{host}{path}");
    let signed = device_key::sign_headers("GET", &url, "").ok_or(OpenError::Unverified)?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (name, value) in extra_headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    for (name, value) in signed {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    (&stream).write_all(req.as_bytes()).map_err(|e| OpenError::Other(e.to_string()))?;

    // Status line + headers.
    let _ = stream.set_read_timeout(Some(answer_within));
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| OpenError::Other(e.to_string()))?);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|_| OpenError::Denied)?;
    let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| OpenError::Other(e.to_string()))? == 0 || line == "\r\n" {
            break;
        }
    }
    match code {
        200 => {}
        403 => return Err(OpenError::Denied),
        401 => return Err(OpenError::Unverified),
        503 => return Err(OpenError::Unavailable),
        other => return Err(OpenError::Other(format!("HTTP {other}"))),
    }
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nodelay(true);
    Ok((stream, reader))
}

pub fn open(ip: &str, events: async_channel::Sender<Event>) -> Result<Session, OpenError> {
    let (stream, reader) = dial(
        ip,
        super::SCREEN_PATH,
        &[("X-BigLace-Screen", super::SCREEN_PROTOCOL)],
        ANSWER_TIMEOUT,
    )?;
    start_session(stream, reader, events, true)
}

/// Ask the paired device `key` on the local network, called `name`, for its
/// screen (see [`open`]).
#[cfg(target_os = "linux")]
pub fn open_lan(key: &str, name: &str, events: async_channel::Sender<Event>) -> Result<Session, OpenError> {
    use crate::lan::pairing::{LanError, ScreenError};
    let ask = super::host::LanAsk { screen: super::SCREEN_PROTOCOL.parse().unwrap_or(4), chroma: None };
    let stream = crate::lan::pairing::open_screen(key, &ask).map_err(|e| match e {
        ScreenError::Refused(why) if why == "denied" => OpenError::Denied,
        ScreenError::Refused(why) if why == "unavailable" => OpenError::Unavailable,
        ScreenError::Refused(why) => OpenError::Other(why),
        ScreenError::Lan(LanError::Off) => OpenError::Other(crate::tr!("the local network is off")),
        ScreenError::Lan(_) => OpenError::Other(crate::trf!(
            "{name} can't be reached on the local network — try again when it's nearby",
            "name" => name
        )),
    })?;
    let _ = stream.set_nodelay(true);
    let reader = BufReader::new(stream.try_clone().map_err(|e| OpenError::Other(e.to_string()))?);
    start_session(stream, reader, events, true)
}

/// Watch what `ip` shares with the voice room of `group` (view only until
/// they give us control).
pub fn open_stage(ip: &str, group: &str, events: async_channel::Sender<Event>) -> Result<Session, OpenError> {
    let path = format!("{}{group}", crate::room::STAGE_PREFIX);
    let (stream, reader) = dial(ip, &path, &[("X-BigLace-Screen", super::SCREEN_PROTOCOL)], Duration::from_secs(10))?;
    start_session(stream, reader, events, false)
}

fn start_session(
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    events: async_channel::Sender<Event>,
    input: bool,
) -> Result<Session, OpenError> {
    // The host sends something every second (frames, pongs): this long in
    // silence means it's gone (a restart takes the tunnel down without a
    // goodbye).
    let _ = stream.set_read_timeout(Some(SILENCE));
    // Input (and clock pings): written by their own thread so the UI never
    // blocks on the network.
    let (tx, rx) = mpsc::channel::<Outgoing>();
    let mut writer = stream.try_clone().map_err(|e| OpenError::Other(e.to_string()))?;
    let _ = std::thread::Builder::new()
        .name("biglace-viewer-input".into())
        .spawn(move || {
            for out in rx {
                let r = match out {
                    Outgoing::Input(event) => match serde_json::to_vec(&event) {
                        Ok(json) => write_frame(&mut writer, Kind::Input, &json),
                        Err(_) => continue,
                    },
                    Outgoing::Ping => write_frame(&mut writer, Kind::Ping, &super::now_us().to_be_bytes()),
                    Outgoing::Control(json) => write_frame(&mut writer, Kind::Control, json.as_bytes()),
                    Outgoing::Power(json) => write_frame(&mut writer, Kind::Power, json.as_bytes()),
                    Outgoing::Resize(json) => write_frame(&mut writer, Kind::Resize, json.as_bytes()),
                    Outgoing::KeyFrame => write_frame(&mut writer, Kind::KeyFrame, &[]),
                };
                if r.is_err() {
                    break;
                }
            }
        });
    {
        let tx = tx.clone();
        let _ = std::thread::Builder::new()
            .name("biglace-viewer-ping".into())
            .spawn(move || {
                while tx.send(Outgoing::Ping).is_ok() {
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
    }

    // Video: read frames, decode, hand RGBA frames to the window.
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (muted_r, key_tx) = (muted.clone(), tx.clone());
    let _ = std::thread::Builder::new()
        .name("biglace-viewer-video".into())
        .spawn(move || {
            let why = match decode_loop_with(reader, &events, &muted_r, AUDIO_OUT_DESC, Some(&key_tx)) {
                Ok(()) => String::new(),
                // The host closed the connection ("End access", or it quit):
                // a normal end, not an error to show.
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                        matches!(
                            io.kind(),
                            std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        )
                    }) =>
                {
                    String::new()
                }
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                        matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
                    }) =>
                {
                    crate::tr!("the connection was lost")
                }
                Err(e) => e.to_string(),
            };
            let _ = events.send_blocking(Event::Ended(why));
        });

    Ok(Session { input: tx, stream, muted, input_on: Arc::new(std::sync::atomic::AtomicBool::new(input)) })
}

/// H.264 decoder for the viewer. Software decoding must not use frame
/// threading: each extra thread holds a frame back (14 cores = ~14 frames,
/// half a second). Slice threads decode one frame in parallel instead.
pub(crate) fn decoder() -> &'static str {
    "avdec_h264 thread-type=slice"
}

/// Viewer pipeline: H.264 access units in, RGBA frames out, nothing queued
/// for later (stale frames are dropped, not shown late).
pub(crate) fn decode_desc(decoder: &str) -> String {
    decode_desc_with(decoder, false)
}

/// Host clock − viewer clock, from the ping with the shortest round trip
/// (the least skewed by queuing), and the first frame's capture time.
#[derive(Default)]
struct Clock {
    offset_us: AtomicI64,
    best_rtt_us: AtomicU64,
    first_capture_us: AtomicU64,
}

impl Clock {
    fn pong(&self, payload: &[u8]) {
        let (Ok(a), Ok(b)) = (<[u8; 8]>::try_from(&payload[..8.min(payload.len())]), <[u8; 8]>::try_from(payload.get(8..16).unwrap_or(&[]))) else {
            return;
        };
        let (t0, th, t1) = (u64::from_be_bytes(a), u64::from_be_bytes(b), super::now_us());
        let rtt = t1.saturating_sub(t0);
        let best = self.best_rtt_us.load(Ordering::Relaxed);
        if best == 0 || rtt <= best + 2_000 {
            self.best_rtt_us.store(if best == 0 { rtt } else { best.min(rtt) }, Ordering::Relaxed);
            let mid = (t0 + t1) / 2;
            self.offset_us.store(th as i64 - mid as i64, Ordering::Relaxed);
        }
    }

    /// ms from capture on the host to now, here.
    fn latency_ms(&self, capture_us: u64) -> f64 {
        let now_on_host = super::now_us() as i64 + self.offset_us.load(Ordering::Relaxed);
        (now_on_host - capture_us as i64) as f64 / 1000.0
    }
}

/// GPU decoder chain for 4:2:0 streams, when this machine has VA-API: the
/// GPU decodes and converts to RGBA (vapostproc); only the finished frame
/// comes back to memory for the window. Where the driver gets that
/// conversion's colors wrong, the CPU converts instead.
fn gpu_decoder() -> Option<&'static str> {
    use gstreamer as gst;
    static CHAIN: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *CHAIN.get_or_init(|| {
        if gst::ElementFactory::find("vah264dec").is_none() || gst::ElementFactory::find("vapostproc").is_none() {
            return None;
        }
        if va_colors_right() {
            Some("vah264dec ! vapostproc")
        } else {
            crate::tailscale::dbg("viewer: this GPU's color conversion is wrong; converting on the CPU");
            Some("vah264dec ! videoconvert n-threads=0")
        }
    })
}

/// A 128×128 picture: red, green, blue and gray quarters.
const COLOR_CHECK: &[u8] = include_bytes!("color-check.h264");

/// Decode [`COLOR_CHECK`] with VA-API once and look at the colors: some
/// drivers' vapostproc swaps channels (the whole picture turns pink).
fn va_colors_right() -> bool {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    let check = || -> anyhow::Result<bool> {
        let pipeline = gst::parse::launch(
            "appsrc name=src caps=video/x-h264,stream-format=byte-stream,alignment=au \
             ! h264parse ! vah264dec ! vapostproc ! video/x-raw,format=RGBA \
             ! appsink name=sink sync=false",
        )?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
        let src = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or_else(|| anyhow::anyhow!("no appsrc"))?;
        let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or_else(|| anyhow::anyhow!("no appsink"))?;
        pipeline.set_state(gst::State::Playing)?;
        let pulled = src
            .push_buffer(gst::Buffer::from_slice(COLOR_CHECK))
            .ok()
            .and_then(|_| src.end_of_stream().ok())
            .and_then(|_| sink.try_pull_sample(gst::ClockTime::from_seconds(3)));
        let _ = pipeline.set_state(gst::State::Null);
        let sample = pulled.ok_or_else(|| anyhow::anyhow!("no picture"))?;
        let caps = sample.caps().ok_or_else(|| anyhow::anyhow!("no caps"))?;
        let info = gstreamer_video::VideoInfo::from_caps(caps)?;
        let buffer = sample.buffer().ok_or_else(|| anyhow::anyhow!("no buffer"))?;
        let map = buffer.map_readable()?;
        let stride = usize::try_from(info.stride()[0])?;
        let pixel = |x: usize, y: usize| -> anyhow::Result<[i32; 3]> {
            let at = y * stride + x * 4;
            let p = map.get(at..at + 3).ok_or_else(|| anyhow::anyhow!("short picture"))?;
            Ok([i32::from(p[0]), i32::from(p[1]), i32::from(p[2])])
        };
        Ok(colors_right(pixel(32, 32)?, pixel(96, 32)?, pixel(32, 96)?, pixel(96, 96)?))
    };
    check().unwrap_or_else(|e| {
        crate::tailscale::dbg(&format!("viewer: GPU color check failed: {e}"));
        false
    })
}

/// The red, green and blue quarters lead with their own channel and the
/// gray one stays gray (each driver's matrix shifts the values a little).
fn colors_right(red: [i32; 3], green: [i32; 3], blue: [i32; 3], gray: [i32; 3]) -> bool {
    let leads = |p: [i32; 3], c: usize| (0..3).filter(|&o| o != c).all(|o| p[c] > p[o] + 80);
    leads(red, 0)
        && leads(green, 1)
        && leads(blue, 2)
        && (gray[0] - gray[1]).abs() < 24
        && (gray[1] - gray[2]).abs() < 24
        && (100..=156).contains(&gray[1])
}

#[cfg(test)]
pub(crate) fn gpu_decoder_available() -> bool {
    gpu_decoder().is_some()
}

/// Viewer pipeline for `decoder` (CPU: `decoder()`, GPU: `gpu_decoder()`).
/// A decoder that falls behind blocks the reader after two access units, so
/// TCP backs up and the host drops frames instead of latency growing here.
fn decode_desc_with(decoder: &str, gpu: bool) -> String {
    let convert = if gpu { "" } else { "! videoconvert n-threads=0 " };
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=false block=true max-buffers=2 max-bytes=0 \
           caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! h264parse ! {decoder} {convert}! video/x-raw,format=RGBA \
         ! appsink name=sink sync=false max-buffers=1 drop=true"
    )
}

/// A running decode pipeline.
struct Decoder {
    pipeline: gstreamer::Pipeline,
    src: gstreamer_app::AppSrc,
    gpu: bool,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        use gstreamer::prelude::*;
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

/// Shared between the reader loop and the decoder's callback.
struct Shared {
    clock: Clock,
    bytes_in: AtomicU64,
    audio_bytes: AtomicU64,
    gpu: std::sync::atomic::AtomicBool,
}

fn start_decoder(gpu: bool, events: &async_channel::Sender<Event>, shared: &Arc<Shared>) -> anyhow::Result<Decoder> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    let desc = match gpu.then(gpu_decoder).flatten() {
        Some(d) => decode_desc_with(d, true),
        None => decode_desc(decoder()),
    };
    let gpu = gpu && gpu_decoder().is_some();
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsrc"))?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    shared.gpu.store(gpu, Ordering::Relaxed);

    // A stream that stopped leaves the appsrc full: ending it wakes a reader
    // blocked in push_buffer, from any thread and without a state change.
    let release_reader = {
        let src = src.clone();
        move || {
            let _ = src.end_of_stream();
        }
    };
    if let Some(bus) = pipeline.bus() {
        let release_reader = release_reader.clone();
        bus.set_sync_handler(move |_, msg| {
            if let gst::MessageView::Error(_) = msg.view() {
                release_reader();
            }
            gst::BusSyncReply::Pass
        });
    }

    // Decoded frames go straight to the window (newest wins: drop=true).
    let events = events.clone();
    let shared_cb = shared.clone();
    let mut window = (std::time::Instant::now(), 0u32, 0f64);
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let shared = &shared_cb;
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info = gstreamer_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                let first = shared.clock.first_capture_us.load(Ordering::Relaxed);
                if let (Some(pts), true) = (buffer.pts(), first != 0) {
                    window.1 += 1;
                    window.2 += shared.clock.latency_ms(first + pts.useconds());
                }
                if window.0.elapsed() >= Duration::from_secs(1) {
                    let secs = window.0.elapsed().as_secs_f64();
                    let stats = Stats {
                        latency_ms: if window.1 > 0 { window.2 / f64::from(window.1) } else { 0.0 },
                        fps: f64::from(window.1) / secs,
                        mbps: shared.bytes_in.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1e6 / secs,
                        gpu: shared.gpu.load(Ordering::Relaxed),
                        audio: shared.audio_bytes.swap(0, Ordering::Relaxed) > 0,
                    };
                    let _ = events.try_send(Event::Stats(stats));
                    window = (std::time::Instant::now(), 0, 0.0);
                }
                let mapped = buffer.into_mapped_buffer_readable().map_err(|_| gst::FlowError::Error)?;
                let frame = Frame {
                    width: info.width(),
                    height: info.height(),
                    stride: info.stride()[0] as usize,
                    data: gtk4::glib::Bytes::from_owned(mapped),
                    decoded_at: std::time::Instant::now(),
                };
                // A closed window ends the session.
                if events.try_send(Event::Frame(frame)).is_err() && events.is_closed() {
                    release_reader();
                    return Err(gst::FlowError::Eos);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.set_state(gst::State::Playing)?;
    Ok(Decoder { pipeline, src, gpu })
}

/// Did the decoder fail (driver refusing the stream, …)?
fn decoder_failed(dec: &Decoder) -> Option<String> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let msg = dec.pipeline.bus()?.pop_filtered(&[gst::MessageType::Error])?;
    match msg.view() {
        gst::MessageView::Error(e) => Some(e.error().to_string()),
        _ => Some("decoder error".into()),
    }
}

/// The host's sound, played here.
struct AudioOut {
    pipeline: gstreamer::Pipeline,
    src: gstreamer_app::AppSrc,
    volume: gstreamer::Element,
    muted: bool,
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        use gstreamer::prelude::*;
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

/// Opus in, the default output out. Played as it arrives (a small buffer
/// absorbs network jitter): sound follows the picture instead of a clock.
/// After a stall, all but the newest 60 ms (six 10 ms packets) waiting are
/// dropped, so the backlog never plays late for the rest of the session.
pub(crate) const AUDIO_OUT_DESC: &str = "appsrc name=asrc is-live=true format=time do-timestamp=true \
       max-buffers=6 max-bytes=0 leaky-type=downstream \
       caps=audio/x-opus,channel-mapping-family=0,channels=2,rate=48000 \
     ! opusdec plc=true ! audioconvert ! audioresample ! volume name=vol \
     ! autoaudiosink name=out sync=false";

fn start_audio(desc: &str) -> anyhow::Result<AudioOut> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    let pipeline = gst::parse::launch(desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let src = pipeline
        .by_name("asrc")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsrc"))?;
    let volume = pipeline.by_name("vol").ok_or_else(|| anyhow::anyhow!("no volume"))?;
    pipeline.set_state(gst::State::Playing)?;
    Ok(AudioOut { pipeline, src, volume, muted: false })
}

#[cfg(test)]
pub(crate) fn decode_loop(
    reader: BufReader<TcpStream>,
    events: &async_channel::Sender<Event>,
) -> anyhow::Result<()> {
    decode_loop_with(reader, events, &Arc::new(std::sync::atomic::AtomicBool::new(false)), AUDIO_OUT_DESC, None)
}

/// Read the session: video to the decoder, sound to the speakers (unless
/// `muted`), clock pongs to the latency meter. `key_request` asks the host
/// for a keyframe (after switching decoders).
pub(crate) fn decode_loop_with(
    mut reader: BufReader<TcpStream>,
    events: &async_channel::Sender<Event>,
    muted: &Arc<std::sync::atomic::AtomicBool>,
    audio_desc: &str,
    key_request: Option<&mpsc::Sender<Outgoing>>,
) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    gst::init()?;
    let shared = Arc::new(Shared {
        clock: Clock::default(),
        bytes_in: AtomicU64::new(0),
        audio_bytes: AtomicU64::new(0),
        gpu: std::sync::atomic::AtomicBool::new(false),
    });
    let mut decoder: Option<Decoder> = None;
    let mut audio: Option<AudioOut> = None;
    // GPU unless the stream is 4:4:4 (or a GPU decode already failed).
    let mut want_gpu = true;

    loop {
        let (kind, payload) = read_frame(&mut reader)?;
        match kind {
            Kind::Info => {
                if let Ok(info) = serde_json::from_slice::<VideoInfo>(&payload) {
                    let gpu = want_gpu && info.chroma.as_deref() != Some("444");
                    if decoder.as_ref().is_none_or(|d| d.gpu != gpu) {
                        decoder = Some(start_decoder(gpu, events, &shared)?);
                    }
                    if events.send_blocking(Event::Info(info)).is_err() {
                        return Ok(());
                    }
                }
            }
            Kind::Video | Kind::VideoTs => {
                if payload.len() <= 8 && kind == Kind::VideoTs {
                    continue;
                }
                shared.bytes_in.fetch_add(payload.len() as u64, Ordering::Relaxed);
                if decoder.is_none() {
                    // Older host: no Info before video.
                    decoder = Some(start_decoder(false, events, &shared)?);
                }
                let buffer = if kind == Kind::VideoTs {
                    let captured = u64::from_be_bytes(payload[..8].try_into().unwrap_or_default());
                    let first = match shared.clock.first_capture_us.load(Ordering::Relaxed) {
                        0 => {
                            shared.clock.first_capture_us.store(captured, Ordering::Relaxed);
                            captured
                        }
                        f => f,
                    };
                    // PTS = capture time (relative): it survives decoding,
                    // so each decoded frame knows when it was captured. Set
                    // on the sliced buffer itself: copy_region only copies
                    // timestamps for regions starting at offset 0.
                    let mut sub = gst::Buffer::from_slice(payload)
                        .copy_region(gst::BufferCopyFlags::MEMORY, 8..)
                        .map_err(|_| anyhow::anyhow!("bad frame"))?;
                    if let Some(b) = sub.get_mut() {
                        b.set_pts(gst::ClockTime::from_useconds(captured.saturating_sub(first)));
                    }
                    sub
                } else {
                    gst::Buffer::from_slice(payload)
                };
                let dec = decoder.as_ref().expect("decoder started above");
                let pushed = dec.src.push_buffer(buffer).is_ok();
                if !pushed && events.is_closed() {
                    return Ok(());
                }
                let failed = decoder_failed(dec).or_else(|| (!pushed).then(|| "decoder stopped".into()));
                if let Some(why) = failed {
                    if !dec.gpu {
                        anyhow::bail!("{why}");
                    }
                    // The GPU won't decode this stream: software from now on,
                    // starting at a fresh keyframe.
                    crate::tailscale::dbg(&format!("remote: GPU decoding failed ({why}); using the CPU"));
                    want_gpu = false;
                    decoder = Some(start_decoder(false, events, &shared)?);
                    if let Some(tx) = key_request {
                        let _ = tx.send(Outgoing::KeyFrame);
                    }
                }
            }
            Kind::Audio if payload.len() > 8 => {
                shared.audio_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                if audio.is_none() {
                    match start_audio(audio_desc) {
                        Ok(a) => audio = Some(a),
                        Err(e) => crate::tailscale::dbg(&format!("remote: can't play sound: {e}")),
                    }
                }
                if let Some(a) = audio.as_mut() {
                    let m = muted.load(Ordering::Relaxed);
                    if m != a.muted {
                        a.volume.set_property("mute", m);
                        a.muted = m;
                    }
                    let packet = gst::Buffer::from_slice(payload)
                        .copy_region(gst::BufferCopyFlags::MEMORY, 8..)
                        .map_err(|_| anyhow::anyhow!("bad packet"))?;
                    let _ = a.src.push_buffer(packet);
                }
            }
            Kind::Pong => shared.clock.pong(&payload),
            Kind::Control => {
                let v = serde_json::from_slice::<serde_json::Value>(&payload).unwrap_or_default();
                if v.get("controllable").and_then(serde_json::Value::as_bool) == Some(false) {
                    let _ = events.send_blocking(Event::ViewOnly);
                    continue;
                }
                let granted = v.get("granted").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let _ = events.send_blocking(Event::Control(granted));
            }
            Kind::Cursor => {
                let cursor = if payload.is_empty() {
                    None
                } else {
                    let field = |i: usize| u32::from(u16::from_be_bytes([payload[i], payload[i + 1]]));
                    if payload.len() < 8 {
                        continue;
                    }
                    let (w, h) = (field(0), field(2));
                    if w == 0 || h == 0 || payload.len() != 8 + (w * h * 4) as usize {
                        continue;
                    }
                    Some(CursorImage {
                        width: w,
                        height: h,
                        hot_x: field(4),
                        hot_y: field(6),
                        data: gtk4::glib::Bytes::from(&payload[8..]),
                    })
                };
                let _ = events.send_blocking(Event::Cursor(cursor));
            }
            Kind::Power => {
                let v: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
                let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let _ = events.send_blocking(Event::Power {
                    action: text("action"),
                    ok: v.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false),
                    error: text("error"),
                });
            }
            Kind::Input
            | Kind::Ping
            | Kind::KeyFrame
            | Kind::Audio
            | Kind::RoomState
            | Kind::Chat
            | Kind::ChatAck
            | Kind::Resize
            | Kind::CamVideo
            | Kind::Link
            | Kind::Lan => {}
            Kind::Error => anyhow::bail!("{}", String::from_utf8_lossy(&payload)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use std::net::TcpListener;

    /// A phone's shared screen says it can't be controlled; a refusal
    /// alone is just "not now".
    #[test]
    fn view_only_screen_told_apart() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            write_frame(&mut s, Kind::Control, br#"{"granted":false}"#).unwrap();
            write_frame(&mut s, Kind::Control, br#"{"granted":false,"controllable":false}"#).unwrap();
        });
        let (tx, rx) = async_channel::bounded(8);
        let stream = TcpStream::connect(addr).unwrap();
        let viewer = std::thread::spawn(move || decode_loop(BufReader::new(stream), &tx));
        host.join().unwrap();
        assert!(matches!(rx.recv_blocking(), Ok(Event::Control(false))));
        assert!(matches!(rx.recv_blocking(), Ok(Event::ViewOnly)));
        let _ = viewer.join();
    }

    /// Values read back from [`COLOR_CHECK`]: Intel VA-API (its own matrix)
    /// and the CPU decoder pass; red and blue swapped, or a pink cast, fail.
    #[test]
    fn gpu_colors_judged() {
        assert!(colors_right([240, 51, 28], [20, 169, 60], [26, 70, 229], [128, 128, 128]));
        assert!(colors_right([222, 29, 31], [30, 187, 62], [29, 60, 219], [126, 126, 126]));
        assert!(!colors_right([28, 51, 240], [20, 169, 60], [229, 70, 26], [128, 128, 128]));
        assert!(!colors_right([240, 51, 28], [20, 169, 60], [26, 70, 229], [160, 110, 140]));
    }

    /// This machine's VA-API, when it has one, decodes the check picture.
    #[test]
    fn color_check_decodes() {
        if gstreamer::init().is_err() || gst::ElementFactory::find("vah264dec").is_none() {
            return;
        }
        assert!(va_colors_right(), "this GPU's colors come out wrong");
    }

    /// Capture-to-decoded time per frame with the host's encoder and the
    /// viewer's decoder, the first frames (encoder start-up) left out; a
    /// generous bound still catches a frame held back (frame threading).
    #[test]
    fn encode_decode_latency_is_low() {
        let _turn = crate::remote::heavy_test();
        use std::time::Instant;
        gst::init().unwrap();
        let (Some(enc), Some(_)) = (crate::remote::host::encoder(), gst::ElementFactory::find("avdec_h264")) else {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("no H.264 encoder or avdec_h264 here; skipping");
            return;
        };
        let measure = |dec: &str, enc: (&str, &str), fps: u64| -> f64 {
            let host = gst::parse::launch(&crate::remote::host::pipeline_desc(
                &format!("videotestsrc is-live=true num-buffers={} pattern=ball ! video/x-raw,width=1280,height=720,framerate={fps}/1 ! videoconvert", fps * 4),
                enc,
            ))
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
            let hsink = host.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
            let viewer = gst::parse::launch(&decode_desc(dec)).unwrap().downcast::<gst::Pipeline>().unwrap();
            let src = viewer.by_name("src").unwrap().downcast::<gstreamer_app::AppSrc>().unwrap();
            // Our own timestamps (frame index) so each decoded frame can be
            // matched with the moment it was captured.
            src.set_property("do-timestamp", false);
            let vsink = viewer.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
            viewer.set_state(gst::State::Playing).unwrap();
            let collector = std::thread::spawn(move || {
                let mut lat = Vec::new();
                while let Some(s) = vsink.try_pull_sample(gst::ClockTime::from_seconds(2)) {
                    let now = Instant::now();
                    let n = s.buffer().and_then(|b| b.pts()).map(|p| p.nseconds() * fps / 1_000_000_000).unwrap_or(0);
                    if n > 2 {
                        lat.push((n, now));
                    }
                }
                lat
            });
            host.set_state(gst::State::Playing).unwrap();
            let mut frame = 0u64;
            let mut captured_at = Vec::new();
            while let Some(s) = hsink.try_pull_sample(gst::ClockTime::from_seconds(3)) {
                // Captured now minus its time in the host pipeline. The PTS is
                // the capture's running time once the encoder's segment (which
                // shifts timestamps by 1000 h) is taken out.
                let taken = s
                    .segment()
                    .and_then(|seg| seg.downcast_ref::<gst::ClockTime>())
                    .zip(s.buffer().and_then(|b| b.pts()))
                    .and_then(|(seg, pts)| seg.to_running_time(pts))
                    .expect("the encoder keeps capture timestamps");
                let running = host.current_running_time().expect("the host pipeline runs");
                captured_at.push(Instant::now() - Duration::from_nanos(running.nseconds().saturating_sub(taken.nseconds())));
                let mut buf = gst::Buffer::from_mut_slice(s.buffer().unwrap().map_readable().unwrap().as_slice().to_vec());
                buf.get_mut().unwrap().set_pts(gst::ClockTime::from_nseconds(frame * 1_000_000_000 / fps + 1_000_000));
                src.push_buffer(buf).unwrap();
                frame += 1;
            }
            let _ = src.end_of_stream();
            let lat = collector.join().unwrap();
            host.set_state(gst::State::Null).unwrap();
            viewer.set_state(gst::State::Null).unwrap();
            let ms: Vec<f64> = lat
                .iter()
                .map(|(n, at)| at.saturating_duration_since(captured_at[*n as usize]).as_secs_f64() * 1000.0)
                .collect();
            ms.iter().sum::<f64>() / ms.len().max(1) as f64
        };
        let mut encoders = vec![enc];
        let x264 = ("x264enc tune=zerolatency speed-preset=ultrafast sliced-threads=true threads=2 pass=qual quantizer=20 bitrate=30000 vbv-buf-capacity=100 key-int-max=120 bframes=0 b-adapt=false", "Y444");
        if enc.0 != x264.0 && gst::ElementFactory::find("x264enc").is_some() {
            encoders.push(x264);
        }
        for e in encoders {
            for fps in [30, 2] {
                let ms = measure(decoder(), e, fps);
                eprintln!("{fps:>2} fps  {ms:7.1} ms  {}", e.0.split_whitespace().next().unwrap_or(""));
                // A frame held back costs a whole frame interval: at 2 fps
                // that's 500 ms. Bound both cases.
                assert!(ms < 120.0, "{} at {fps} fps: {ms:.1} ms", e.0);
            }
        }
    }

    /// `count` H.264 access units of a 320x240 test pattern, a keyframe every 10.
    fn h264_units(count: u32) -> Vec<Vec<u8>> {
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers={count} ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert \
             ! video/x-raw,format=I420 ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=10 \
             ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut units = Vec::new();
        while let Some(s) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            units.push(s.buffer().unwrap().map_readable().unwrap().as_slice().to_vec());
        }
        pipeline.set_state(gst::State::Null).unwrap();
        units
    }

    fn h264_available() -> bool {
        gst::init().unwrap();
        let available = gst::ElementFactory::find("x264enc").is_some() && gst::ElementFactory::find("avdec_h264").is_some();
        if !available {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("x264enc/avdec_h264 missing; skipping");
        }
        available
    }

    /// A decoder that stops taking frames must stall the reader (so TCP backs
    /// up to the host) instead of queuing every frame that arrives.
    #[test]
    fn a_stalled_decoder_blocks_the_reader() {
        let _turn = crate::remote::heavy_test();
        if !h264_available() {
            return;
        }
        let units = h264_units(40);
        let pipeline = gst::parse::launch(&decode_desc(decoder())).unwrap().downcast::<gst::Pipeline>().unwrap();
        let src = pipeline.by_name("src").unwrap().downcast::<gstreamer_app::AppSrc>().unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        let (open_gate, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let _ = sink.pull_sample();
                    let _ = gate.lock().unwrap().recv();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        let pushed = Arc::new(AtomicU64::new(0));
        let pusher = {
            let (src, pushed, total) = (src.clone(), pushed.clone(), units.len());
            std::thread::spawn(move || {
                for (n, u) in units.into_iter().enumerate() {
                    let mut buf = gst::Buffer::from_slice(u);
                    buf.get_mut().unwrap().set_pts(gst::ClockTime::from_mseconds(n as u64 * 33));
                    if src.push_buffer(buf).is_err() {
                        break;
                    }
                    pushed.fetch_add(1, Ordering::Relaxed);
                }
                total
            })
        };
        std::thread::sleep(Duration::from_secs(1));
        let queued = pushed.load(Ordering::Relaxed);
        drop(open_gate);
        let total = pusher.join().unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(queued < 10, "{queued} of {total} frames pushed into a stalled decoder");
    }

    /// A burst of sound after a stall keeps only the newest 60 ms waiting.
    #[test]
    fn late_sound_keeps_only_60_ms() {
        gst::init().unwrap();
        if gst::ElementFactory::find("opusenc").is_none() {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("opusenc missing; skipping");
            return;
        }
        let enc = gst::parse::launch(
            "audiotestsrc num-buffers=50 samplesperbuffer=480 ! audio/x-raw,rate=48000,channels=2 \
             ! opusenc frame-size=10 ! appsink name=sink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = enc.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        enc.set_state(gst::State::Playing).unwrap();
        let mut packets = Vec::new();
        while let Some(s) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            packets.push(s.buffer().unwrap().map_readable().unwrap().as_slice().to_vec());
        }
        enc.set_state(gst::State::Null).unwrap();
        assert!(packets.len() >= 40, "encoder produced {} packets", packets.len());

        let desc = AUDIO_OUT_DESC.replace("autoaudiosink name=out", "appsink name=out");
        assert_ne!(desc, AUDIO_OUT_DESC);
        let out = start_audio(&desc).unwrap();
        let stalled = out.pipeline.by_name("out").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        let (open_gate, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        stalled.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let _ = sink.pull_sample();
                    let _ = gate.lock().unwrap().recv();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        for p in &packets {
            out.src.push_buffer(gst::Buffer::from_slice(p.clone())).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        let waiting = out.src.property::<u64>("current-level-buffers");
        drop(open_gate);
        drop(out);
        assert!(waiting <= 6, "{waiting} of {} packets waiting to play", packets.len());
    }

    /// Closing the window ends the reader even while the host keeps sending
    /// and the reader is blocked on a full decoder.
    #[test]
    fn closed_window_ends_blocked_reader() {
        let _turn = crate::remote::heavy_test();
        if !h264_available() {
            return;
        }
        let units = h264_units(30);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let info = serde_json::to_vec(&VideoInfo { width: 320, height: 240, chroma: None }).unwrap();
            write_frame(&mut s, Kind::Info, &info).unwrap();
            for u in units.iter().cycle() {
                if write_frame(&mut s, Kind::Video, u).is_err() {
                    break;
                }
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        let (tx, rx) = async_channel::bounded(4);
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(decode_loop(BufReader::new(stream), &tx).is_ok());
        });
        while let Ok(ev) = rx.recv_blocking() {
            if let Event::Frame(_) = ev {
                break;
            }
        }
        drop(rx);
        assert_eq!(done.recv_timeout(Duration::from_secs(5)), Ok(true), "the reader did not end");
    }

    /// Encode a test pattern like a host does, send it over a real TCP
    /// connection in session frames, and decode it like the viewer does:
    /// RGBA frames of the right size must come out.
    #[test]
    fn host_frames_decode_on_the_viewer() {
        let _turn = crate::remote::heavy_test();
        if !h264_available() {
            return;
        }
        let units = h264_units(75);
        assert!(units.len() >= 10, "encoder produced {} units", units.len());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let info = serde_json::to_vec(&VideoInfo { width: 320, height: 240, chroma: None }).unwrap();
            write_frame(&mut s, Kind::Info, &info).unwrap();
            // Protocol 2: each unit carries its capture time; paced like a
            // live 30 fps source so the viewer reports stats. The viewer
            // hangs up once the test has seen enough.
            for u in units {
                let mut payload = crate::remote::now_us().to_be_bytes().to_vec();
                payload.extend_from_slice(&u);
                if write_frame(&mut s, Kind::VideoTs, &payload).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            // Give the decoder time to drain before the connection closes.
            std::thread::sleep(Duration::from_millis(800));
        });
        let stream = TcpStream::connect(addr).unwrap();
        let (tx, rx) = async_channel::bounded(64);
        let viewer = std::thread::spawn(move || decode_loop(BufReader::new(stream), &tx));
        let mut got_info = false;
        let mut frames = 0;
        let mut stats: Option<Stats> = None;
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                Event::Info(i) => got_info = i.width == 320 && i.height == 240,
                Event::Frame(f) => {
                    assert_eq!((f.width, f.height), (320, 240));
                    assert!(f.stride >= 320 * 4 && f.data.len() >= f.stride * 240);
                    frames += 1;
                }
                Event::Stats(st) => stats = Some(st),
                Event::Control(_) | Event::ViewOnly | Event::Power { .. } | Event::Cursor(_) => {}
                Event::Ended(_) => break,
            }
            if frames >= 5 && stats.is_some() {
                break;
            }
        }
        drop(rx);
        host.join().unwrap();
        let _ = viewer.join();
        assert!(got_info, "no video info");
        assert!(frames >= 5, "only {frames} decoded frames");
        let st = stats.expect("no stats from the viewer");
        eprintln!("viewer stats: {st:?}");
        assert!(st.fps > 5.0 && (0.0..300.0).contains(&st.latency_ms), "{st:?}");
    }
}
