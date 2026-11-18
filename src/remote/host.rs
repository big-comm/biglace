//! Host side of a remote-screen session: check who's asking, ask the person
//! at this screen, then stream the screen and replay the viewer's input.

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{emit, read_frame, write_frame, ConsentRequest, HostEvent, HostSession, Input, Kind, VideoInfo};
use crate::device_key;
use crate::tailscale::{self, Identity};

/// Signed requests must be this fresh (either way, for clock skew).
const SKEW: Duration = Duration::from_secs(120);
/// How long the person at the screen has to answer.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(60);
/// Asking the viewer's agent to prove its key.
const PROOF_TIMEOUT: Duration = Duration::from_secs(4);
/// A viewer that stops reading for this long is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Ends a session from the UI ("End access") or when either side stops.
#[derive(Clone)]
pub struct StopHandle {
    id: u64,
    flag: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<TcpStream>>>,
}

impl StopHandle {
    pub(crate) fn new(stream: &TcpStream) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            flag: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(stream.try_clone().ok())),
        }
    }

    /// Not tied to one connection (a room share: many viewers).
    pub(crate) fn detached() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            flag: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(None)),
        }
    }

    pub fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        if let Ok(mut s) = self.stream.lock() {
            if let Some(s) = s.take() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }

    pub fn stopped(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    pub fn same(&self, other: &StopHandle) -> bool {
        self.id == other.id
    }
}

fn respond(mut stream: &TcpStream, status: u16, reason: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Nonces of accepted requests, so a captured request can't be replayed.
fn seen_nonces() -> &'static Mutex<HashMap<String, Instant>> {
    static SEEN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashMap::new()))
}

enum PanelAnswer {
    Key(String),
    Unknown,
}

/// The key the BigScale panel pinned for the device at `peer`, asked with a
/// request signed by this device. `Unknown` when there's no panel, it's
/// unreachable, or it has no key for that device.
fn panel_key_for(peer: IpAddr) -> PanelAnswer {
    let Some(base) = crate::panel::panel_tailnet_url() else {
        return PanelAnswer::Unknown;
    };
    let Ok(agent) = crate::panel::shared_agent() else {
        return PanelAnswer::Unknown;
    };
    #[derive(serde::Deserialize)]
    struct Answer {
        key: String,
    }
    match device_key::send(&agent, "GET", &format!("{base}/api/devices/key/{peer}"), None) {
        Ok(resp) => match crate::panel::read_json_limited::<Answer>(resp) {
            Ok(a) => PanelAnswer::Key(a.key),
            Err(_) => PanelAnswer::Unknown,
        },
        Err(_) => PanelAnswer::Unknown,
    }
}

/// Check the viewer's signed request and that the BigLace at its address
/// holds the signing key. Returns a short reason on failure (for the log).
pub(crate) fn verify_viewer(headers: &HashMap<String, String>, peer: IpAddr, local: &str) -> Result<(), &'static str> {
    verify_caller(headers, peer, local, super::SCREEN_PATH)
}

/// Check a peer's request to `path` on this agent: signed by the peer's
/// device key, fresh, not replayed, and the key really is that device's (the
/// panel's pin, else the peer's own agent proves it).
pub(crate) fn verify_caller(
    headers: &HashMap<String, String>,
    peer: IpAddr,
    local: &str,
    path: &str,
) -> Result<(), &'static str> {
    let get = |name: &str| headers.get(name).map(String::as_str).unwrap_or("");
    let (public, time, nonce, sig, host) = (
        get("x-biglace-key"),
        get("x-biglace-time"),
        get("x-biglace-nonce"),
        get("x-biglace-sig"),
        get("host"),
    );
    if public.is_empty() || sig.is_empty() || nonce.len() < 16 || nonce.len() > 64 {
        return Err("unsigned");
    }
    // Signed for this device, not replayed from a request to another one.
    if !host.eq_ignore_ascii_case(local) {
        return Err("wrong host");
    }
    let t: u64 = time.parse().map_err(|_| "bad time")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| "clock")?.as_secs();
    if now.abs_diff(t) > SKEW.as_secs() {
        return Err("stale");
    }
    let message = device_key::request_message("GET", host, path, t, nonce, "");
    if !device_key::verify(public, &message, sig) {
        return Err("bad signature");
    }
    {
        let mut seen = seen_nonces().lock().map_err(|_| "lock")?;
        let now = Instant::now();
        seen.retain(|_, at| now.duration_since(*at) < SKEW * 2);
        if seen.len() > 10_000 || seen.insert(format!("{public}|{nonce}"), now).is_some() {
            return Err("replayed");
        }
    }
    // Is that key really the viewer device's? Something behind its machine
    // (a VM, a container) shares the address but not the key.
    match panel_key_for(peer) {
        // The panel pinned this device's key when it enrolled: authoritative.
        PanelAnswer::Key(pinned) if pinned == public => return Ok(()),
        PanelAnswer::Key(_) => return Err("key differs from the one the panel knows"),
        // No panel (plain headscale) or no key there yet: ask the viewer's
        // own BigLace, over the tunnel, to prove it holds the key.
        PanelAnswer::Unknown => {}
    }
    let challenge = device_key::new_challenge().ok_or("no randomness")?;
    let host_part = match peer {
        IpAddr::V6(v6) => format!("[{v6}]"),
        IpAddr::V4(v4) => v4.to_string(),
    };
    let url = format!("http://{host_part}:{}/v1/proof/{challenge}", crate::agent::PORT);
    let agent = ureq::AgentBuilder::new()
        .timeout(PROOF_TIMEOUT)
        .redirects(0)
        .build();
    let resp = agent.get(&url).call().map_err(|_| "no proof")?;
    #[derive(serde::Deserialize)]
    struct Proof {
        key: String,
        sig: String,
    }
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(resp.into_reader(), 1024), &mut body).map_err(|_| "no proof")?;
    let proof: Proof = serde_json::from_slice(&body).map_err(|_| "bad proof")?;
    if proof.key != public || !device_key::verify(public, &device_key::proof_message(&challenge), &proof.sig) {
        return Err("key not proven");
    }
    Ok(())
}

/// Ask the person at this screen. False on deny, timeout or no UI.
fn ask(identity: &Identity) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let request = ConsentRequest {
        who: identity.login.clone(),
        device: identity.device.clone(),
        reply: tx,
    };
    if !emit(HostEvent::Consent(request)) {
        return false;
    }
    rx.recv_timeout(CONSENT_TIMEOUT).unwrap_or(false)
}

/// Handle a `GET /v1/screen` connection (called from the agent's thread).
pub fn serve(stream: TcpStream, peer: IpAddr, headers: &HashMap<String, String>, identity: Identity) {
    let local = stream
        .local_addr()
        .map(|a| match a.ip() {
            IpAddr::V6(v6) => format!("[{v6}]:{}", a.port()),
            IpAddr::V4(v4) => format!("{v4}:{}", a.port()),
        })
        .unwrap_or_default();
    if !super::can_host() {
        respond(&stream, 503, "Service Unavailable", "screen sharing not available on this device");
        return;
    }
    if let Err(why) = verify_viewer(headers, peer, &local) {
        tailscale::dbg(&format!("remote: refused {}@{}: {why}", identity.login, identity.device));
        respond(&stream, 401, "Unauthorized", why);
        return;
    }
    // The viewer that restarted this computer comes back without asking.
    let returning = crate::power::resume_allows(&identity.login, &identity.device);
    if !returning && !ask(&identity) {
        respond(&stream, 403, "Forbidden", "denied");
        return;
    }
    let _ = write!(
        &stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-screen\r\nConnection: keep-alive\r\n\r\n"
    );
    let stop = StopHandle::new(&stream);
    emit(HostEvent::Started(HostSession {
        who: identity.login.clone(),
        device: identity.device.clone(),
        stop: stop.clone(),
    }));
    tailscale::dbg(&format!("remote: session started for {}@{}", identity.login, identity.device));
    // What the viewer understands: 2 = timestamps + ping, 3 = also sound.
    let protocol: u32 = headers.get("x-biglace-screen").and_then(|v| v.parse().ok()).unwrap_or(1);
    run_session(stream, &stop, protocol, &identity);
    stop.stop();
    emit(HostEvent::Ended(stop));
    tailscale::dbg(&format!("remote: session ended for {}@{}", identity.login, identity.device));
}

/// Where the viewer's input goes on this host.
pub(crate) enum InputSink {
    #[cfg(target_os = "linux")]
    X11(Option<Box<super::input_x11::Injector>>),
    #[cfg(target_os = "linux")]
    Portal(async_channel::Sender<Input>),
}

impl InputSink {
    pub(crate) fn apply(&mut self, event: Input) {
        match self {
            #[cfg(target_os = "linux")]
            Self::X11(Some(inj)) => inj.apply(&event),
            #[cfg(target_os = "linux")]
            Self::X11(None) => {}
            #[cfg(target_os = "linux")]
            Self::Portal(tx) => {
                let _ = tx.send_blocking(event);
            }
        }
    }
}

/// The screen source, as the head of a GStreamer pipeline description, plus
/// what keeps it alive (the portal's PipeWire fd) and where input goes.
pub(crate) struct Source {
    /// Full pipeline descriptions to try, best first (see `stream_screen`).
    pub(crate) pipelines: Vec<String>,
    /// X11: raised by XDamage when the screen changed (None = always send).
    pub(crate) dirty: Option<Arc<AtomicBool>>,
    pub(crate) sink: InputSink,
    #[cfg(target_os = "linux")]
    pub(crate) _fd: Option<std::os::fd::OwnedFd>,
}

// What follows each source, up to raw video the encoder chain takes. Every
// head must end in an element (not caps): `pipeline_desc` appends caps.
// PipeWire only sends a frame when the screen changes: nothing to encode or
// send while it's still, which keeps the host's CPU free.
const WAYLAND_TAIL: &str = "videoconvert n-threads=2";
const X11_TAIL: &str = "video/x-raw,framerate=60/1 ! videoconvert n-threads=2";

/// From raw frames (already in the encoder's format) to the appsink:
/// a leaky one-frame queue (stale frames are dropped, never queued), the
/// encoder, and H.264 access units with SPS/PPS on every keyframe.
fn encode_tail(enc: &str) -> String {
    format!(
        "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
         ! {enc} ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=2"
    )
}

/// Full pipeline: `head` (source + conversion) → H.264 → appsink.
pub(crate) fn pipeline_desc(head: &str, (enc, format): (&str, &str)) -> String {
    format!("{head} ! video/x-raw,format={format} ! {}", encode_tail(enc))
}

/// Zero-copy Wayland pipeline: the compositor's frames stay in GPU memory
/// (DMA-BUF), the GPU converts them (vapostproc) and encodes them
/// (vah264enc) — no readback, no CPU conversion. What gnome-remote-desktop
/// does on GPUs; None when this machine has no VA-API encoder.
#[cfg(target_os = "linux")]
fn wayland_zero_copy(fd: i32, node: u32) -> Option<String> {
    use gstreamer as gst;
    let (enc, _) = encoder().filter(|(e, _)| e.starts_with("vah264enc"))?;
    gst::ElementFactory::find("vapostproc")?;
    Some(format!(
        "pipewiresrc name=capture fd={fd} path={node} do-timestamp=true keepalive-time=1000 \
         ! video/x-raw(memory:DMABuf) ! vapostproc \
         ! video/x-raw(memory:VAMemory),format=NV12 ! {}",
        encode_tail(enc)
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn open_source(stop: &StopHandle) -> anyhow::Result<Source> {
    use std::os::fd::AsRawFd;
    if super::wayland_session() {
        let portal = super::portal::start()?;
        let (fd, node) = (portal.fd.as_raw_fd(), portal.node);
        let enc = encoder().ok_or_else(|| anyhow::anyhow!("no H.264 encoder"))?;
        // Copy path, for compositors or GPUs without DMA-BUF/VA-API.
        let copy = pipeline_desc(
            &format!(
                "pipewiresrc name=capture fd={fd} path={node} do-timestamp=true keepalive-time=1000 always-copy=true \
                 ! {WAYLAND_TAIL}"
            ),
            enc,
        );
        Ok(Source {
            pipelines: wayland_zero_copy(fd, node).into_iter().chain([copy]).collect(),
            dirty: None,
            sink: InputSink::Portal(portal.input),
            _fd: Some(portal.fd),
        })
    } else {
        let dirty = Arc::new(AtomicBool::new(true));
        let watching = super::damage_x11::watch(dirty.clone(), stop.flag.clone());
        let enc = encoder().ok_or_else(|| anyhow::anyhow!("no H.264 encoder"))?;
        Ok(Source {
            dirty: watching.then_some(dirty),
            // The viewer draws the pointer itself (no video delay on it).
            pipelines: vec![pipeline_desc(
                &format!("ximagesrc name=capture use-damage=false show-pointer=false ! {X11_TAIL}"),
                enc,
            )],
            sink: InputSink::X11(super::input_x11::Injector::connect().map(Box::new)),
            _fd: None,
        })
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_source(_stop: &StopHandle) -> anyhow::Result<Source> {
    anyhow::bail!("screen sharing is not available on this system")
}

/// Kernel send buffer for the session: big enough for a keyframe in flight,
/// small enough that a slow link backs up into the (leaky) queue before the
/// encoder — frames are dropped there instead of piling up seconds of video.
const SEND_BUFFER: usize = 512 * 1024;
#[cfg(target_os = "linux")]
const NOTSENT_LOWAT: u32 = 64 * 1024;

fn run_session(mut stream: TcpStream, stop: &StopHandle, protocol: u32, identity: &Identity) {
    let timestamps = protocol >= 2;
    {
        let sock = socket2::SockRef::from(&stream);
        let _ = sock.set_send_buffer_size(SEND_BUFFER);
        // Only a little unsent data may sit in the kernel: the next frame
        // waits (and older ones get dropped before the encoder) instead.
        #[cfg(target_os = "linux")]
        let _ = sock.set_tcp_notsent_lowat(NOTSENT_LOWAT);
    }
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nodelay(true);

    let source = match open_source(stop) {
        Ok(s) => s,
        Err(e) => {
            tailscale::dbg(&format!("remote: no screen source: {e}"));
            let _ = write_frame(&mut stream, Kind::Error, e.to_string().as_bytes());
            return;
        }
    };
    let Source { pipelines, mut sink, dirty, .. } = source;
    #[cfg(target_os = "linux")]
    let _keep_fd = source._fd;
    // Video and pong replies write from two threads.
    let writer = match stream.try_clone() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(_) => return,
    };

    // Raised when the viewer asks for a keyframe (it switched decoders).
    let want_key = Arc::new(AtomicBool::new(false));

    // Input: replayed as it arrives, on its own thread.
    if let Ok(mut reader) = stream.try_clone() {
        let stop = stop.clone();
        let viewer = identity.clone();
        let writer = writer.clone();
        let want_key = want_key.clone();
        let _ = std::thread::Builder::new()
            .name("biglace-remote-input".into())
            .spawn(move || {
                while !stop.stopped() {
                    match read_frame(&mut reader) {
                        Ok((Kind::Input, payload)) => {
                            if let Ok(event) = serde_json::from_slice::<Input>(&payload) {
                                sink.apply(event);
                            }
                        }
                        Ok((Kind::KeyFrame, _)) => want_key.store(true, Ordering::Release),
                        Ok((Kind::Power, payload)) => {
                            let action = serde_json::from_slice::<serde_json::Value>(&payload)
                                .ok()
                                .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_string))
                                .unwrap_or_default();
                            power_action(&action, &viewer, &writer);
                        }
                        Ok((Kind::Ping, payload)) if payload.len() == 8 => {
                            let mut pong = payload;
                            pong.extend_from_slice(&super::now_us().to_be_bytes());
                            if let Ok(mut w) = writer.lock() {
                                let _ = write_frame(&mut *w, Kind::Pong, &pong);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                // Dropping the sink ends the portal session too.
                drop(sink);
                stop.stop();
            });
    }

    drop(stream);
    if protocol >= 3 {
        let (writer, stop) = (writer.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("biglace-remote-audio".into())
            .spawn(move || {
                if let Err(e) = stream_audio(&writer, &stop) {
                    tailscale::dbg(&format!("remote: no sound: {e}"));
                }
            });
    }
    let send = |kind: Kind, payload: &[u8]| -> anyhow::Result<()> {
        let mut w = writer.lock().map_err(|_| anyhow::anyhow!("lock"))?;
        write_frame(&mut *w, kind, payload)?;
        Ok(())
    };
    if let Err(e) = stream_screen(&send, &pipelines, dirty, stop, timestamps, &want_key) {
        tailscale::dbg(&format!("remote: capture ended: {e}"));
    }
}

/// The viewer asks to restart (and come back) or shut down this computer.
/// Answered before it happens: the connection drops right after.
fn power_action(action: &str, viewer: &Identity, writer: &Arc<Mutex<TcpStream>>) {
    let reply = |ok: bool, error: &str| {
        let json = serde_json::json!({ "action": action, "ok": ok, "error": error }).to_string();
        if let Ok(mut w) = writer.lock() {
            let _ = write_frame(&mut *w, Kind::Power, json.as_bytes());
        }
    };
    tailscale::dbg(&format!("remote: {}@{} asks to {action}", viewer.login, viewer.device));
    let result = match action {
        "restart" => {
            // Tell the viewer first; a moment later the machine goes down.
            if !crate::power::available() {
                Err(crate::tr!("this computer can't restart remotely (BigLace's helper isn't installed)"))
            } else {
                reply(true, "");
                std::thread::sleep(Duration::from_millis(400));
                crate::power::restart_and_return(&viewer.login, &viewer.device).map(|()| false)
            }
        }
        "shutdown" => {
            reply(true, "");
            std::thread::sleep(Duration::from_millis(400));
            crate::power::shutdown().map(|()| false)
        }
        _ => Err("unknown action".to_string()),
    };
    if let Err(e) = result {
        tailscale::dbg(&format!("remote: {action} failed: {e}"));
        reply(false, &e);
    }
}

/// H.264 encoder to use — hardware (VA-API) first, then software — and the
/// raw format it takes. VA-API elements are only registered when a usable
/// device exists, so finding the factory is enough.
pub fn encoder() -> Option<(&'static str, &'static str)> {
    use gstreamer as gst;
    // Software encoding takes at most half the cores: the person at this
    // computer keeps using it while it's being shared.
    static X264: OnceLock<String> = OnceLock::new();
    let x264: &'static str = X264.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
        // Constant quality (CRF 20: crisp text) capped at 30 Mbit/s with a
        // short VBV, full chroma (4:4:4 — colored text edges stay sharp,
        // what RDP's AVC444 is for).
        format!(
            "x264enc tune=zerolatency speed-preset=ultrafast sliced-threads=true threads={threads} \
             pass=qual quantizer=20 bitrate=30000 vbv-buf-capacity=100 key-int-max=120 bframes=0 b-adapt=false"
        )
    });
    for (name, desc, format) in [
        // Low-latency settings (GStreamer docs): no B-frames, one reference,
        // fastest preset, a CPB of ~2 frames, slices so the viewer's decoder
        // can work on one frame in parallel.
        // Hardware: constant quality too (4:2:0 only on VA-API).
        (
            "vah264enc",
            "vah264enc rate-control=cqp qpi=20 qpp=22 key-int-max=120 b-frames=0 ref-frames=1 target-usage=4 num-slices=4",
            "NV12",
        ),
        ("x264enc", x264, "Y444"),
        ("openh264enc", "openh264enc bitrate=6000000 gop-size=60 complexity=low", "I420"),
    ] {
        if gst::ElementFactory::find(name).is_some() {
            return Some((desc, format));
        }
    }
    None
}

/// What plays on this computer (the default output's monitor), as Opus:
/// 10 ms packets, 128 kbit/s stereo — music-grade, and short frames keep
/// the delay low.
pub(crate) const AUDIO_DESC: &str = "pulsesrc name=asrc device=@DEFAULT_MONITOR@ buffer-time=40000 latency-time=10000 \
     ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
     ! opusenc bitrate=128000 frame-size=10 audio-type=generic \
     ! appsink name=asink sync=false max-buffers=50";

/// Send the host's sound until the session stops. Each frame: capture time
/// (µs, host clock) + one Opus packet.
fn stream_audio(writer: &Mutex<TcpStream>, stop: &StopHandle) -> anyhow::Result<()> {
    stream_audio_from(writer, stop, AUDIO_DESC)
}

pub(crate) fn stream_audio_from(writer: &Mutex<TcpStream>, stop: &StopHandle, desc: &str) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    gst::init()?;
    let pipeline = gst::parse::launch(desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("asink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let result = (|| -> anyhow::Result<()> {
        while !stop.stopped() {
            let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) else {
                if sink.is_eos() {
                    anyhow::bail!("sound capture stopped");
                }
                continue;
            };
            let Some(buffer) = sample.buffer() else { continue };
            let map = buffer.map_readable()?;
            let mut payload = Vec::with_capacity(8 + map.len());
            payload.extend_from_slice(&super::now_us().to_be_bytes());
            payload.extend_from_slice(map.as_slice());
            let mut w = writer.lock().map_err(|_| anyhow::anyhow!("lock"))?;
            write_frame(&mut *w, Kind::Audio, &payload)?;
        }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Build and start `desc`; Ok once it produced its first encoded frame
/// (within a few seconds), with that frame. Errors leave nothing running.
fn start_pipeline(
    desc: &str,
    dirty: Option<Arc<AtomicBool>>,
) -> anyhow::Result<(gstreamer::Pipeline, gstreamer_app::AppSink, gstreamer::Sample)> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    let pipeline = gst::parse::launch(desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    stamp_and_gate(&pipeline, dirty);
    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        anyhow::bail!("could not start");
    }
    let deadline = Instant::now() + FIRST_FRAME_WITHIN;
    while Instant::now() < deadline {
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
            return Ok((pipeline, sink, sample));
        }
        if let Some(bus) = pipeline.bus() {
            if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
                let _ = pipeline.set_state(gst::State::Null);
                if let gst::MessageView::Error(e) = msg.view() {
                    anyhow::bail!("{}", e.error());
                }
                anyhow::bail!("pipeline error");
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    anyhow::bail!("no frame within {:?}", FIRST_FRAME_WITHIN)
}

/// A pipeline that hasn't produced a frame by then is given up on.
const FIRST_FRAME_WITHIN: Duration = Duration::from_secs(4);

/// Right after capture: stamp each frame with the moment it was really
/// captured (a source that can't keep the requested rate — ximagesrc
/// asked for 60 fps on a machine doing 25 — timestamps frames on a
/// schedule that drifts further behind every second), and on X11 drop
/// frames of an unchanged screen before they cost a conversion and an
/// encode.
fn stamp_and_gate(pipeline: &gstreamer::Pipeline, dirty: Option<Arc<AtomicBool>>) {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let Some(capture) = pipeline.by_name("capture") else { return };
    let Some(pad) = capture.static_pad("src") else { return };
    let last = Mutex::new(Instant::now() - IDLE_FRAME_EVERY);
    let element = capture.downgrade();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(dirty) = &dirty {
            let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
            if dirty.swap(false, Ordering::AcqRel) || last.elapsed() >= IDLE_FRAME_EVERY {
                *last = Instant::now();
            } else {
                return gst::PadProbeReturn::Drop;
            }
        }
        let now = element.upgrade().and_then(|e| e.current_running_time());
        if let (Some(now), Some(gst::PadProbeData::Buffer(buffer))) = (now, info.data.as_mut()) {
            buffer.make_mut().set_pts(now);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Still screen: one frame a second keeps the session (and a late keyframe)
/// alive; everything else waits for a change.
const IDLE_FRAME_EVERY: Duration = Duration::from_secs(1);

pub(crate) fn stream_screen(
    send: &dyn Fn(Kind, &[u8]) -> anyhow::Result<()>,
    pipelines: &[String],
    dirty: Option<Arc<AtomicBool>>,
    stop: &StopHandle,
    timestamps: bool,
    want_key: &AtomicBool,
) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    gst::init()?;
    // Best pipeline first; one that can't start (no DMA-BUF from this
    // compositor, a driver refusing a format…) gives way to the next.
    let mut started = None;
    let mut last_err = anyhow::anyhow!("no capture pipeline");
    for (i, desc) in pipelines.iter().enumerate() {
        match start_pipeline(desc, dirty.clone()) {
            Ok(p) => {
                tailscale::dbg(&format!("remote: capture pipeline #{i} running"));
                started = Some(p);
                break;
            }
            Err(e) => {
                tailscale::dbg(&format!("remote: capture pipeline #{i} failed: {e}"));
                last_err = e;
            }
        }
    }
    let Some((pipeline, sink, first)) = started else {
        return Err(last_err);
    };
    let mut pending = Some(first);

    let result = (|| -> anyhow::Result<()> {
        let mut last_info: Option<VideoInfo> = None;
        let (mut frames, mut bytes, mut since) = (0u32, 0usize, std::time::Instant::now());
        while !stop.stopped() {
            if want_key.swap(false, Ordering::AcqRel) {
                let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
                sink.send_event(event);
            }
            let Some(sample) = pending.take().or_else(|| sink.try_pull_sample(gst::ClockTime::from_mseconds(500))) else {
                if sink.is_eos() {
                    anyhow::bail!("capture stopped");
                }
                continue;
            };
            if let Some(s) = sample.caps().and_then(|c| c.structure(0)) {
                if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                    let chroma = match s.get::<&str>("chroma-format") {
                        Ok("4:4:4") => "444",
                        _ => "420",
                    };
                    let info = VideoInfo { width: w as u32, height: h as u32, chroma: Some(chroma.into()) };
                    if last_info.as_ref() != Some(&info) {
                        send(Kind::Info, &serde_json::to_vec(&info)?)?;
                        last_info = Some(info);
                    }
                }
            }
            let Some(buffer) = sample.buffer() else { continue };
            let map = buffer.map_readable()?;
            if timestamps {
                // When the frame was captured, on this host's wall clock:
                // now, minus how long it spent in the pipeline.
                let in_pipeline = match (pipeline.current_running_time(), buffer.pts()) {
                    (Some(now), Some(pts)) if now > pts => (now - pts).useconds(),
                    _ => 0,
                };
                let captured = super::now_us().saturating_sub(in_pipeline);
                let mut payload = Vec::with_capacity(8 + map.len());
                payload.extend_from_slice(&captured.to_be_bytes());
                payload.extend_from_slice(map.as_slice());
                send(Kind::VideoTs, &payload)?;
            } else {
                send(Kind::Video, map.as_slice())?;
            }
            frames += 1;
            bytes += map.len();
            if since.elapsed() >= Duration::from_secs(5) {
                let secs = since.elapsed().as_secs_f64();
                tailscale::dbg(&format!(
                    "remote: sent {:.1} fps, {:.0} kbit/s",
                    f64::from(frames) / secs,
                    bytes as f64 * 8.0 / 1000.0 / secs
                ));
                (frames, bytes, since) = (0, 0, std::time::Instant::now());
            }
        }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPU half of the zero-copy chain (convert + encode on VA-API) must
    /// produce H.264 wherever a VA encoder exists.
    #[test]
    fn va_zero_copy_chain_encodes() {
        let _turn = crate::remote::heavy_test();
        use gstreamer as gst;
        use gstreamer::prelude::*;
        gst::init().unwrap();
        let Some((enc, _)) = encoder().filter(|(e, _)| e.starts_with("vah264enc")) else {
            eprintln!("no VA-API encoder here; skipping");
            return;
        };
        if gst::ElementFactory::find("vapostproc").is_none() {
            return;
        }
        let desc = format!(
            "videotestsrc num-buffers=20 ! video/x-raw,width=1280,height=720 ! vapostproc \
             ! video/x-raw(memory:VAMemory),format=NV12 ! {}",
            encode_tail(enc)
        );
        let pipeline = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
        pipeline.set_state(gst::State::Null).unwrap();
        let caps = sample.expect("VA chain produced nothing").caps().unwrap().structure(0).unwrap().to_owned();
        assert_eq!(caps.name(), "video/x-h264");
    }
    use gstreamer as gst;
    use gstreamer::prelude::*;

    /// Both host pipelines, with a test source in place of the screen, must
    /// parse and actually produce H.264 access units at the appsink.
    #[test]
    fn host_pipelines_parse_and_encode() {
        let _turn = crate::remote::heavy_test();
        gst::init().unwrap();
        if encoder().is_none() {
            eprintln!("no H.264 encoder here; skipping");
            return;
        }
        // Every encoder present on this machine, not just the preferred one.
        let encoders = [
            ("vah264enc", ("vah264enc rate-control=cqp qpi=20 qpp=22 key-int-max=120 b-frames=0 ref-frames=1 target-usage=4 num-slices=4", "NV12")),
            ("x264enc", ("x264enc tune=zerolatency speed-preset=ultrafast sliced-threads=true threads=2 pass=qual quantizer=20 bitrate=30000 vbv-buf-capacity=100 key-int-max=120 bframes=0 b-adapt=false", "Y444")),
            ("openh264enc", ("openh264enc bitrate=6000000 gop-size=60 complexity=low", "I420")),
        ];
        for (tail, (name, enc)) in [WAYLAND_TAIL, X11_TAIL]
            .iter()
            .flat_map(|t| encoders.iter().map(move |e| (*t, *e)))
            .filter(|(_, (name, _))| gst::ElementFactory::find(name).is_some())
        {
            let _ = name;
            let head = format!("videotestsrc num-buffers=20 ! video/x-raw,width=320,height=240 ! videoconvert ! {tail}");
            let desc = pipeline_desc(&head, enc);
            let pipeline = gst::parse::launch(&desc)
                .unwrap_or_else(|e| panic!("{desc}: {e}"))
                .downcast::<gst::Pipeline>()
                .unwrap();
            let sink = pipeline
                .by_name("sink")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
            pipeline.set_state(gst::State::Null).unwrap();
            let sample = sample.unwrap_or_else(|| panic!("no frame from: {desc}"));
            let caps = sample.caps().unwrap().structure(0).unwrap().to_owned();
            assert_eq!(caps.name(), "video/x-h264");
            assert_eq!(caps.get::<i32>("width").unwrap(), 320);
        }
    }
}
