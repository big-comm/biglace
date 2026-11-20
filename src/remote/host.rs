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
/// (allowed, remember the answer).
fn ask(identity: &Identity) -> (bool, bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    let request = ConsentRequest {
        who: identity.login.clone(),
        device: identity.device.clone(),
        reply: tx,
    };
    if !emit(HostEvent::Consent(request)) {
        return (false, false);
    }
    rx.recv_timeout(CONSENT_TIMEOUT).unwrap_or((false, false))
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
    // The viewer that restarted this computer comes back without asking;
    // otherwise this person's rule decides (ask, always, never).
    let (who, device) = (identity.login.as_str(), identity.device.as_str());
    let returning = crate::power::resume_allows(who, device);
    let outcome = if returning {
        "returning"
    } else {
        match crate::access::rule(who) {
            crate::access::Rule::Never => "blocked",
            crate::access::Rule::Always => "remembered",
            crate::access::Rule::Ask => match ask(&identity) {
                (true, true) => {
                    crate::access::set_rule(who, crate::access::Rule::Always);
                    "allowed"
                }
                (true, false) => "allowed",
                (false, _) => "denied",
            },
        }
    };
    crate::access::log(who, device, "screen", outcome);
    if matches!(outcome, "blocked" | "denied") {
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
    crate::power::session_started(&identity.login, &identity.device);
    // What the viewer understands: 2 = timestamps + ping, 3 = also sound.
    let protocol: u32 = headers.get("x-biglace-screen").and_then(|v| v.parse().ok()).unwrap_or(1);
    ONLY_420.with(|c| c.set(headers.get("x-biglace-chroma").is_some_and(|v| v == "420")));
    run_session(stream, &stop, protocol, &identity);
    stop.stop();
    emit(HostEvent::Ended(stop));
    crate::access::log(&identity.login, &identity.device, "screen", "ended");
    crate::power::session_ended(&identity.login, &identity.device);
    tailscale::dbg(&format!("remote: session ended for {}@{}", identity.login, identity.device));
}

/// Where the viewer's input goes on this host.
enum SinkOut {
    #[cfg(target_os = "linux")]
    X11(Option<Box<super::input_x11::Injector>>),
    #[cfg(target_os = "linux")]
    Portal {
        tx: async_channel::Sender<Input>,
        handled: Arc<std::sync::atomic::AtomicU64>,
        sent: u64,
    },
}

/// The viewer's input, applied here — keeping note of what is held down so
/// it can all be let go (control changes hands or ends, the viewer leaves):
/// a key left pressed would stay pressed on this computer.
pub(crate) struct InputSink {
    out: SinkOut,
    keys: std::collections::BTreeSet<u32>,
    buttons: std::collections::BTreeSet<u8>,
}

impl InputSink {
    #[cfg(target_os = "linux")]
    pub(crate) fn x11(inj: Option<Box<super::input_x11::Injector>>) -> Self {
        Self { out: SinkOut::X11(inj), keys: Default::default(), buttons: Default::default() }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn portal(tx: async_channel::Sender<Input>, handled: Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self { out: SinkOut::Portal { tx, handled, sent: 0 }, keys: Default::default(), buttons: Default::default() }
    }

    /// Wait (a little) until the compositor got everything sent so far.
    fn flush(&self) {
        #[cfg(target_os = "linux")]
        if let SinkOut::Portal { handled, sent, .. } = &self.out {
            let deadline = Instant::now() + Duration::from_millis(1500);
            while handled.load(Ordering::Acquire) < *sent && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn send(&mut self, event: Input) {
        match &mut self.out {
            #[cfg(target_os = "linux")]
            SinkOut::X11(Some(inj)) => inj.apply(&event),
            #[cfg(target_os = "linux")]
            SinkOut::X11(None) => {}
            #[cfg(target_os = "linux")]
            SinkOut::Portal { tx, sent, .. } => {
                if tx.send_blocking(event).is_ok() {
                    *sent += 1;
                }
            }
        }
    }

    pub(crate) fn apply(&mut self, event: Input) {
        let event = match event {
            // Older viewers send the shifted letter ("A"); typing it through
            // the portal makes the compositor press a Shift of its own that
            // nothing lets go. The letter itself is enough: the viewer's real
            // Shift arrives as its own key.
            Input::Key { keysym, pressed } if (0x41..=0x5a).contains(&keysym) => Input::Key { keysym: keysym + 0x20, pressed },
            other => other,
        };
        match event {
            Input::Key { keysym, pressed: true } => {
                self.keys.insert(keysym);
            }
            Input::Key { keysym, pressed: false } => {
                if !self.keys.remove(&keysym) {
                    // Never pressed through us: nothing to let go.
                    return;
                }
            }
            Input::Button { button, pressed: true } => {
                self.buttons.insert(button);
            }
            Input::Button { button, pressed: false } => {
                self.buttons.remove(&button);
            }
            _ => {}
        }
        self.send(event);
    }

    /// Let go of every key and button still held through us, and wait
    /// until the compositor has them: a key a session leaves pressed stays
    /// pressed in GNOME (Mutter) even after the session is gone, until the
    /// person logs out.
    pub(crate) fn release_all(&mut self) {
        let keys = std::mem::take(&mut self.keys);
        let buttons = std::mem::take(&mut self.buttons);
        if keys.is_empty() && buttons.is_empty() {
            return;
        }
        tailscale::dbg(&format!("remote: letting go of {} key(s), {} button(s)", keys.len(), buttons.len()));
        for keysym in keys.into_iter().rev() {
            self.send(Input::Key { keysym, pressed: false });
        }
        for button in buttons {
            self.send(Input::Button { button, pressed: false });
        }
        self.flush();
    }
}

/// Every viewer input in use (to let go of everything when we quit).
fn live_sinks() -> &'static Mutex<Vec<std::sync::Weak<Mutex<InputSink>>>> {
    static L: OnceLock<Mutex<Vec<std::sync::Weak<Mutex<InputSink>>>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// Quitting (or a signal): whatever remote viewers hold down, let go.
pub fn release_all_inputs() {
    let sinks: Vec<_> = live_sinks().lock().map(|l| l.iter().filter_map(std::sync::Weak::upgrade).collect()).unwrap_or_default();
    for s in sinks {
        if let Ok(mut s) = s.lock() {
            s.release_all();
        }
    }
}

impl Drop for InputSink {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// The screen source, as the head of a GStreamer pipeline description, plus
/// what keeps it alive (the portal's PipeWire fd) and where input goes.
/// Where the viewer's pointer comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pointer {
    /// The viewer draws a plain arrow (older viewers).
    Local,
    /// Drawn into the frames (Wayland; room shares).
    Embedded,
    /// The viewer's own, shaped like the host's (X11).
    Shapes,
}

pub(crate) struct Source {
    /// How the pointer is shown (what was asked for, as far as possible).
    pub(crate) pointer: Pointer,
    /// Full pipeline descriptions to try, best first (see `stream_screen`).
    pub(crate) pipelines: Vec<String>,
    /// X11: raised by XDamage when the screen changed (None = always send).
    pub(crate) dirty: Option<Arc<Damage>>,
    pub(crate) sink: InputSink,
    #[cfg(target_os = "linux")]
    pub(crate) _fd: Option<std::os::fd::OwnedFd>,
    /// X11: the XDamage thread raising `dirty`, stopped with this source.
    #[cfg(target_os = "linux")]
    pub(crate) _damage: Option<super::damage_x11::Watch>,
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
    // A viewer that decodes 4:2:0 only (phones' hardware decoders).
    let format = if format == "Y444" && ONLY_420.with(std::cell::Cell::get) { "I420" } else { format };
    format!("{head} ! video/x-raw,format={format} ! {}", encode_tail(enc))
}

/// Pictures made on this thread are 4:2:0 (a room's shared screen: phones
/// may watch it, and their decoders take 4:2:0 only).
pub(crate) fn only_420_here() {
    ONLY_420.with(|c| c.set(true));
}

thread_local! {
    /// This session's viewer asked for 4:2:0 (`X-BigLace-Chroma: 420`).
    static ONLY_420: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Zero-copy Wayland pipeline: the compositor's frames stay in GPU memory
/// (DMA-BUF), the GPU converts them (vapostproc) and encodes them
/// (vah264enc) — no readback, no CPU conversion. What gnome-remote-desktop
/// does on GPUs; None when this machine has no VA-API encoder.
#[cfg(target_os = "linux")]
fn wayland_zero_copy(capture: &str) -> Option<String> {
    use gstreamer as gst;
    let (enc, _) = encoder().filter(|(e, _)| e.starts_with("vah264enc"))?;
    gst::ElementFactory::find("vapostproc")?;
    Some(format!(
        "{capture} ! video/x-raw(memory:DMABuf) ! vapostproc \
         ! video/x-raw(memory:VAMemory),format=NV12 ! {}",
        encode_tail(enc)
    ))
}

/// `pipewiresrc` on the stream the portal shared, by its serial where the
/// portal gives one: a node id can be handed to another producer.
#[cfg(target_os = "linux")]
fn pipewire_capture(fd: i32, node: u32, serial: Option<u64>) -> String {
    let target = match serial {
        Some(serial) => format!("target-object={serial}"),
        None => format!("path={node}"),
    };
    format!("pipewiresrc name=capture fd={fd} {target} do-timestamp=true keepalive-time=1000")
}

#[cfg(target_os = "linux")]
pub(crate) fn open_source(_stop: &StopHandle, want: Pointer) -> anyhow::Result<Source> {
    use std::os::fd::AsRawFd;
    if super::wayland_session() {
        // Only a room's shared screen keeps the pointer in the video (the
        // people watching follow it). One person seeing and controlling
        // uses their own pointer — it follows their hand at once, where one
        // drawn in the video comes back late.
        let portal = super::portal::start(want == Pointer::Embedded)?;
        let pointer = if portal.cursor_embedded { Pointer::Embedded } else { Pointer::Local };
        let capture = pipewire_capture(portal.fd.as_raw_fd(), portal.node, portal.serial);
        let enc = encoder().ok_or_else(|| anyhow::anyhow!("no H.264 encoder"))?;
        // Copy path, for compositors or GPUs without DMA-BUF/VA-API.
        let copy = pipeline_desc(&format!("{capture} use-bufferpool=false ! {WAYLAND_TAIL}"), enc);
        Ok(Source {
            pointer,
            pipelines: wayland_zero_copy(&capture).into_iter().chain([copy]).collect(),
            dirty: None,
            sink: InputSink::portal(portal.input, portal.handled),
            _fd: Some(portal.fd),
            _damage: None,
        })
    } else {
        let embedded = want == Pointer::Embedded;
        let dirty = Arc::new(Damage::default());
        dirty.raise();
        let watch = super::damage_x11::watch(dirty.clone(), embedded);
        let enc = encoder().ok_or_else(|| anyhow::anyhow!("no H.264 encoder"))?;
        Ok(Source {
            pointer: want,
            dirty: watch.is_some().then_some(dirty),
            // Usually the viewer draws the pointer itself (no video delay
            // on it); a room share shows the presenter's in the frames.
            pipelines: vec![pipeline_desc(
                &format!("ximagesrc name=capture use-damage=false show-pointer={embedded} ! {X11_TAIL}"),
                enc,
            )],
            sink: InputSink::x11(super::input_x11::Injector::connect().map(Box::new)),
            _fd: None,
            _damage: watch,
        })
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_source(_stop: &StopHandle, _want: Pointer) -> anyhow::Result<Source> {
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

    // Protocol 4 viewers learn the pointer's shape (X11) or get it in the
    // frames (Wayland).
    let want = if protocol >= 4 { Pointer::Shapes } else { Pointer::Local };
    let source = match open_source(stop, want) {
        Ok(s) => s,
        Err(e) => {
            tailscale::dbg(&format!("remote: no screen source: {e}"));
            let why = if crate::power::screen_locked_blocks_remote() {
                crate::tr!("the screen there is locked, and GNOME doesn't allow remote access to a locked screen — ask the person to unlock it")
            } else {
                e.to_string()
            };
            let _ = write_frame(&mut stream, Kind::Error, why.as_bytes());
            return;
        }
    };
    let Source { mut pipelines, sink, mut dirty, pointer, .. } = source;
    #[cfg(target_os = "linux")]
    let (mut keep_fd, mut keep_damage) = (source._fd, source._damage);
    // Shared: a new source (after a resolution change) brings a new one.
    let sink = Arc::new(Mutex::new(sink));
    if let Ok(mut l) = live_sinks().lock() {
        l.retain(|w| w.strong_count() > 0);
        l.push(Arc::downgrade(&sink));
    }
    // Video and pong replies write from two threads.
    let writer = match stream.try_clone() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(_) => return,
    };

    // The pointer: in the frames (the viewer hides its own), or its shape.
    match pointer {
        Pointer::Embedded if protocol >= 4 => {
            if let Ok(mut w) = writer.lock() {
                let _ = write_frame(&mut *w, Kind::Cursor, &[]);
            }
        }
        #[cfg(target_os = "linux")]
        Pointer::Shapes => {
            let w = writer.clone();
            super::cursor_x11::watch(
                move |shape| {
                    if let Ok(mut w) = w.lock() {
                        let _ = write_frame(&mut *w, Kind::Cursor, &shape);
                    }
                },
                stop.flag.clone(),
            );
        }
        _ => {}
    }

    // Raised when the viewer asks for a keyframe (it switched decoders).
    let want_key = Arc::new(AtomicBool::new(false));
    // Raised when the resolution changed: the capture starts over.
    let restart = Arc::new(AtomicBool::new(false));

    // Input: replayed as it arrives, on its own thread.
    if let Ok(mut reader) = stream.try_clone() {
        let stop = stop.clone();
        let viewer = identity.clone();
        let writer = writer.clone();
        let want_key = want_key.clone();
        let restart = restart.clone();
        let sink = sink.clone();
        let _ = std::thread::Builder::new()
            .name("biglace-remote-input".into())
            .spawn(move || {
                // The viewer's resolution changes, undone when this ends.
                let mut fitter = super::display::Fitter::default();
                while !stop.stopped() {
                    match read_frame(&mut reader) {
                        Ok((Kind::Resize, payload)) => {
                            let v: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
                            let size = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).map(|n| n.clamp(320, 16384) as u32);
                            let changed = if v.get("restore").and_then(serde_json::Value::as_bool) == Some(true) {
                                fitter.restore()
                            } else if let (Some(w), Some(h)) = (size("width"), size("height")) {
                                fitter.fit(w, h).unwrap_or_else(|e| {
                                    tailscale::dbg(&format!("remote: can't fit the resolution: {e}"));
                                    false
                                })
                            } else {
                                false
                            };
                            if changed {
                                // Let the screen settle, then capture it anew.
                                std::thread::sleep(Duration::from_millis(700));
                                restart.store(true, Ordering::Release);
                            }
                        }
                        Ok((Kind::Input, payload)) => {
                            if let Ok(event) = serde_json::from_slice::<Input>(&payload) {
                                if let Ok(mut sink) = sink.lock() {
                                    sink.apply(event);
                                }
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
                // The viewer is gone (or the session ends): nothing stays held.
                if let Ok(mut sink) = sink.lock() {
                    sink.release_all();
                }
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
    loop {
        match stream_screen(&send, &pipelines, dirty.clone(), stop, timestamps, &want_key, &restart) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                tailscale::dbg(&format!("remote: capture ended: {e}"));
                break;
            }
        }
        // The resolution changed: open the screen again (the portal
        // remembers the choice, nobody is asked).
        tailscale::dbg("remote: screen size changed — opening the capture again");
        #[cfg(target_os = "linux")]
        {
            if let Ok(mut s) = sink.lock() {
                *s = InputSink::x11(None);
            }
            keep_fd = None;
            keep_damage = None;
        }
        match open_source(stop, want) {
            Ok(fresh) => {
                pipelines = fresh.pipelines;
                dirty = fresh.dirty;
                if let Ok(mut s) = sink.lock() {
                    *s = fresh.sink;
                }
                #[cfg(target_os = "linux")]
                {
                    keep_fd = fresh._fd;
                    keep_damage = fresh._damage;
                }
            }
            Err(e) => {
                tailscale::dbg(&format!("remote: can't capture the screen again: {e}"));
                let _ = send(Kind::Error, e.to_string().as_bytes());
                break;
            }
        }
    }
    // Dropping the sink ends the portal session too.
    #[cfg(target_os = "linux")]
    drop((keep_fd, keep_damage));
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
    if matches!(action, "restart" | "relogin" | "shutdown") {
        crate::access::log(&viewer.login, &viewer.device, action, "allowed");
    }
    let result = match action {
        "restart" => {
            // Ready first (automatic sign-in armed), then tell the viewer;
            // a moment later the machine goes down.
            match crate::power::prepare_restart(&viewer.login, &viewer.device) {
                Err(e) => Err(e),
                Ok(()) => {
                    reply(true, "");
                    std::thread::sleep(Duration::from_millis(400));
                    crate::power::reboot_now().map(|()| false)
                }
            }
        }
        "relogin" => {
            // The login screen restarts a second after the helper returns:
            // enough to tell the viewer.
            crate::power::restart_session(&viewer.login, &viewer.device).map(|()| {
                reply(true, "");
                false
            })
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
    let send = |payload: &[u8]| -> anyhow::Result<()> {
        let mut w = writer.lock().map_err(|_| anyhow::anyhow!("lock"))?;
        write_frame(&mut *w, Kind::Audio, payload)?;
        Ok(())
    };
    stream_audio_to(&send, stop, desc)
}

/// Capture `desc` (Opus at its appsink) and `send` each packet with its
/// capture time, until `stop`.
pub(crate) fn stream_audio_to(send: &dyn Fn(&[u8]) -> anyhow::Result<()>, stop: &StopHandle, desc: &str) -> anyhow::Result<()> {
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
            send(&payload)?;
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
    dirty: Option<Arc<Damage>>,
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

/// Raised by XDamage when the X11 screen changed; the capture waits on it.
#[derive(Default)]
pub(crate) struct Damage {
    changed: Mutex<bool>,
    wake: std::sync::Condvar,
}

impl Damage {
    pub(crate) fn raise(&self) {
        *self.changed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    fn take(&self) -> bool {
        std::mem::take(&mut *self.changed.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Waits up to `timeout` for a change and leaves it raised.
    fn wait(&self, timeout: Duration) -> bool {
        let changed = self.changed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (changed, _) = self
            .wake
            .wait_timeout_while(changed, timeout, |changed| !*changed)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *changed
    }
}

/// Which X11 grabs go on to the encoder. It runs on the capture's streaming
/// thread, so holding a frame back also holds back the next grab.
struct Gate {
    damage: Arc<Damage>,
    last_sent: Instant,
    /// The grab right after a change: a change raised during a grab may be
    /// missing from it.
    trailing: bool,
}

impl Gate {
    fn admit(&mut self) -> bool {
        if self.damage.take() {
            self.trailing = true;
        } else if self.trailing {
            self.trailing = false;
        } else if self.damage.wait(IDLE_FRAME_EVERY.saturating_sub(self.last_sent.elapsed())) {
            // Taken before the change: the next grab has it.
            return false;
        }
        self.last_sent = Instant::now();
        true
    }
}

/// Right after capture: stamp each frame with the moment it was really
/// captured (a source that can't keep the requested rate — ximagesrc
/// asked for 60 fps on a machine doing 25 — timestamps frames on a
/// schedule that drifts further behind every second), and on X11 wait for
/// a change before the next grab.
fn stamp_and_gate(pipeline: &gstreamer::Pipeline, dirty: Option<Arc<Damage>>) {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let Some(capture) = pipeline.by_name("capture") else { return };
    let Some(pad) = capture.static_pad("src") else { return };
    let gate = dirty.map(|damage| Mutex::new(Gate { damage, last_sent: Instant::now(), trailing: false }));
    let element = capture.downgrade();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gate) = &gate {
            if !gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner).admit() {
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

/// Capture, encode and `send` the screen until `stop` — or until `restart`
/// is raised (the screen's resolution changed): then true, for the caller
/// to open the source again (GNOME ends its screencast on a mode change).
pub(crate) fn stream_screen(
    send: &dyn Fn(Kind, &[u8]) -> anyhow::Result<()>,
    pipelines: &[String],
    dirty: Option<Arc<Damage>>,
    stop: &StopHandle,
    timestamps: bool,
    want_key: &AtomicBool,
    restart: &AtomicBool,
) -> anyhow::Result<bool> {
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

    let result = (|| -> anyhow::Result<bool> {
        let mut last_info: Option<VideoInfo> = None;
        let (mut frames, mut bytes, mut since) = (0u32, 0usize, std::time::Instant::now());
        while !stop.stopped() {
            if restart.swap(false, Ordering::AcqRel) {
                return Ok(true);
            }
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
        Ok(false)
    })();
    // Frees a capture thread waiting in the gate, so stopping takes no second.
    if let Some(damage) = &dirty {
        damage.raise();
    }
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What's held is let go when control ends; "A" counts as "a" (older
    /// viewers), and a release of something never pressed goes nowhere.
    #[test]
    fn held_keys_are_let_go() {
        let (tx, rx) = async_channel::unbounded();
        let handled = Arc::new(AtomicU64::new(u64::MAX));
        let mut sink = InputSink::portal(tx, handled);
        sink.apply(Input::Key { keysym: 0xffe1, pressed: true }); // Shift_L
        sink.apply(Input::Key { keysym: 0x41, pressed: true }); // "A"
        sink.apply(Input::Key { keysym: 0x61, pressed: false }); // "a" up
        sink.apply(Input::Key { keysym: 0x62, pressed: false }); // never down
        sink.apply(Input::Button { button: 1, pressed: true });
        let sent: Vec<Input> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(sent.len(), 4);
        assert!(matches!(sent[1], Input::Key { keysym: 0x61, pressed: true }));
        drop(sink);
        let released: Vec<Input> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(released.iter().any(|e| matches!(e, Input::Key { keysym: 0xffe1, pressed: false })));
        assert!(released.iter().any(|e| matches!(e, Input::Button { button: 1, pressed: false })));
        assert_eq!(released.len(), 2);
    }

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

    /// A still X11 screen holds back the next grab until it changes, or
    /// until the keepalive frame is due.
    #[test]
    fn x11_gate_waits_for_damage() {
        let damage = Arc::new(Damage::default());
        let mut gate = Gate { damage: damage.clone(), last_sent: Instant::now(), trailing: false };
        damage.raise();
        assert!(gate.admit(), "a change goes out");
        assert!(gate.admit(), "and the grab after it");
        let raiser = {
            let damage = damage.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                damage.raise();
            })
        };
        let start = Instant::now();
        assert!(!gate.admit(), "the grab from before the change is dropped");
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(90) && waited < IDLE_FRAME_EVERY / 2, "{waited:?}");
        raiser.join().unwrap();
        assert!(gate.admit(), "the next grab has the change");
        assert!(gate.admit());
        let start = Instant::now();
        assert!(gate.admit(), "a still screen still sends its keepalive");
        assert!(start.elapsed() >= IDLE_FRAME_EVERY - Duration::from_millis(50), "{:?}", start.elapsed());
    }

    /// Deprecated `pipewiresrc` properties (`path`, `always-copy`) go away
    /// in a later GStreamer; the serial must survive above `u32::MAX`.
    #[test]
    fn pipewire_capture_names_no_deprecated_property() {
        gst::init().unwrap();
        if gst::ElementFactory::find("pipewiresrc").is_none() {
            eprintln!("no pipewiresrc here; skipping");
            return;
        }
        let serial = u64::from(u32::MAX) + 7;
        let desc = format!("{} use-bufferpool=false", pipewire_capture(3, 42, Some(serial)));
        assert!(desc.contains(&format!("target-object={serial}")), "{desc}");
        let element = gst::parse::launch(&desc).unwrap();
        for setting in desc.split_whitespace().filter_map(|w| w.split_once('=')) {
            let spec = element.find_property(setting.0).unwrap_or_else(|| panic!("{}: no such property", setting.0));
            assert!(!spec.flags().contains(gst::glib::ParamFlags::DEPRECATED), "{} is deprecated", setting.0);
        }
    }

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
