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
    fn new(stream: &TcpStream) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            flag: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(stream.try_clone().ok())),
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

/// Check the viewer's signed request and that the BigLace at its address
/// holds the signing key. Returns a short reason on failure (for the log).
fn verify_viewer(headers: &HashMap<String, String>, peer: IpAddr, local: &str) -> Result<(), &'static str> {
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
    let message = device_key::request_message("GET", host, super::SCREEN_PATH, t, nonce, "");
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
    // The viewer's own BigLace must vouch for the key: something behind its
    // machine (a VM, a container) shares the address but not the key.
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
    if !ask(&identity) {
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
    run_session(stream, &stop);
    stop.stop();
    emit(HostEvent::Ended(stop));
    tailscale::dbg(&format!("remote: session ended for {}@{}", identity.login, identity.device));
}

fn run_session(stream: TcpStream, stop: &StopHandle) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nodelay(true);

    // Input: replayed as it arrives, on its own thread.
    #[cfg(target_os = "linux")]
    if let Ok(mut reader) = stream.try_clone() {
        let stop = stop.clone();
        let _ = std::thread::Builder::new()
            .name("biglace-remote-input".into())
            .spawn(move || {
                let mut injector = super::input_x11::Injector::connect();
                while !stop.stopped() {
                    match read_frame(&mut reader) {
                        Ok((Kind::Input, payload)) => {
                            if let (Some(inj), Ok(event)) = (injector.as_mut(), serde_json::from_slice::<Input>(&payload)) {
                                inj.apply(&event);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                stop.stop();
            });
    }

    if let Err(e) = stream_screen(stream, stop) {
        tailscale::dbg(&format!("remote: capture ended: {e}"));
    }
}

/// H.264 encoder to use: hardware (VA-API) first, then software.
pub fn encoder() -> Option<&'static str> {
    use gstreamer as gst;
    for (name, desc) in [
        ("vah264enc", "vah264enc rate-control=cbr bitrate=6000 key-int-max=60"),
        ("x264enc", "x264enc tune=zerolatency speed-preset=ultrafast bitrate=6000 key-int-max=60 threads=2"),
        ("openh264enc", "openh264enc bitrate=6000000 gop-size=60 complexity=low"),
    ] {
        if gst::ElementFactory::find(name).is_some() {
            return Some(desc);
        }
    }
    None
}

fn stream_screen(mut stream: TcpStream, stop: &StopHandle) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    gst::init()?;
    let enc = encoder().ok_or_else(|| anyhow::anyhow!("no H.264 encoder"))?;
    let desc = format!(
        "ximagesrc use-damage=false show-pointer=true \
         ! video/x-raw,framerate=30/1 ! videoconvert ! video/x-raw,format=I420 \
         ! {enc} ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=8"
    );
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    pipeline.set_state(gst::State::Playing)?;

    let result = (|| -> anyhow::Result<()> {
        let mut last_info: Option<VideoInfo> = None;
        while !stop.stopped() {
            let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
                if sink.is_eos() {
                    anyhow::bail!("capture stopped");
                }
                continue;
            };
            if let Some(s) = sample.caps().and_then(|c| c.structure(0)) {
                if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                    let info = VideoInfo { width: w as u32, height: h as u32 };
                    if last_info != Some(info) {
                        write_frame(&mut stream, Kind::Info, &serde_json::to_vec(&info)?)?;
                        last_info = Some(info);
                    }
                }
            }
            let Some(buffer) = sample.buffer() else { continue };
            let map = buffer.map_readable()?;
            write_frame(&mut stream, Kind::Video, map.as_slice())?;
        }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}
