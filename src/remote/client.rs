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
}

/// Live numbers about the session, once a second.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// Capture on the host → decoded here (both clocks reconciled).
    pub latency_ms: f64,
    pub fps: f64,
    pub mbps: f64,
    /// Decoding on this machine's GPU.
    pub gpu: bool,
    /// Sound is coming in.
    pub audio: bool,
}

/// What the session thread tells the window.
pub enum Event {
    Info(VideoInfo),
    Frame(Frame),
    Stats(Stats),
    Ended(String),
}

pub(crate) enum Outgoing {
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
}

impl Session {
    pub fn send(&self, event: Input) {
        let _ = self.input.send(Outgoing::Input(event));
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
                Err(e) => e.to_string(),
            };
            let _ = events.send_blocking(Event::Ended(why));
        });

    Ok(Session { input: tx, stream, muted })
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
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=false \
           caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! h264parse ! {decoder} ! videoconvert n-threads=0 ! video/x-raw,format=RGBA \
         ! appsink name=sink sync=false max-buffers=1 drop=true"
    )
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
/// comes back to memory for the window.
fn gpu_decoder() -> Option<&'static str> {
    use gstreamer as gst;
    (gst::ElementFactory::find("vah264dec").is_some() && gst::ElementFactory::find("vapostproc").is_some())
        .then_some("vah264dec ! vapostproc")
}

#[cfg(test)]
pub(crate) fn gpu_decoder_available() -> bool {
    gpu_decoder().is_some()
}

/// Viewer pipeline for `decoder` (CPU: `decoder()`, GPU: `gpu_decoder()`).
fn decode_desc_with(decoder: &str, gpu: bool) -> String {
    let convert = if gpu { "" } else { "! videoconvert n-threads=0 " };
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=false \
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
                };
                // A closed window ends the session.
                if events.try_send(Event::Frame(frame)).is_err() && events.is_closed() {
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
pub(crate) const AUDIO_OUT_DESC: &str = "appsrc name=asrc is-live=true format=time do-timestamp=true \
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
            Kind::Input | Kind::Ping | Kind::KeyFrame | Kind::Audio | Kind::RoomState => {}
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

    /// Latency budget: frames encoded with the host's pipeline settings and
    /// decoded with the viewer's, pushed in real time (30 fps). Measures
    /// capture-to-decoded time per frame. Prints the numbers; fails above a
    /// generous bound so a regression (e.g. frame threading) is caught.
    #[test]
    fn encode_decode_latency_is_low() {
        let _turn = crate::remote::heavy_test();
        use std::time::Instant;
        gst::init().unwrap();
        let Some(enc) = crate::remote::host::encoder() else { return };
        if gst::ElementFactory::find("avdec_h264").is_none() {
            return;
        }
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
            let start = Instant::now();
            let captured_at = |n: u64| start + Duration::from_micros(n * 1_000_000 / fps);
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
            while let Some(s) = hsink.try_pull_sample(gst::ClockTime::from_seconds(3)) {
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
                .map(|(n, at)| at.saturating_duration_since(captured_at(*n)).as_secs_f64() * 1000.0)
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

    /// Encode a test pattern like a host does, send it over a real TCP
    /// connection in session frames, and decode it like the viewer does:
    /// RGBA frames of the right size must come out.
    #[test]
    fn host_frames_decode_on_the_viewer() {
        let _turn = crate::remote::heavy_test();
        gst::init().unwrap();
        if gst::ElementFactory::find("x264enc").is_none() || gst::ElementFactory::find("avdec_h264").is_none() {
            eprintln!("x264enc/avdec_h264 missing; skipping");
            return;
        }
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=75 ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert \
             ! video/x-raw,format=I420 ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=10 \
             ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false",
        )
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
        assert!(units.len() >= 10, "encoder produced {} units", units.len());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let info = serde_json::to_vec(&VideoInfo { width: 320, height: 240, chroma: None }).unwrap();
            write_frame(&mut s, Kind::Info, &info).unwrap();
            // Protocol 2: each unit carries its capture time; paced like a
            // live 30 fps source so the viewer reports stats.
            for u in units {
                let mut payload = crate::remote::now_us().to_be_bytes().to_vec();
                payload.extend_from_slice(&u);
                write_frame(&mut s, Kind::VideoTs, &payload).unwrap();
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
