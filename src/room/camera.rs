//! Cameras in a voice room, Jitsi-style: ours is captured once, encoded as
//! H.264 at a modest size and sent to everyone in the room over the same
//! connections as the voice; theirs are decoded here. The window reads the
//! latest picture of each person (`frame`) as it draws.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// Size and rate sent: enough for a face, light on a mesh of connections.
const WIDTH: i32 = 640;
const HEIGHT: i32 = 360;
const FPS: i32 = 24;
const KBPS: u32 = 700;

/// One decoded picture, RGBA rows of `stride` bytes.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: gtk4::glib::Bytes,
    /// Bumped on every new picture (the window redraws only then).
    pub serial: u64,
}

/// Latest picture per person (by tailnet address; ours under our own).
fn frames() -> &'static Mutex<HashMap<String, Frame>> {
    static F: OnceLock<Mutex<HashMap<String, Frame>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

static SERIAL: AtomicU64 = AtomicU64::new(1);

/// The latest picture of the person at `ip`.
pub fn frame(ip: &str) -> Option<Frame> {
    frames().lock().ok()?.get(ip).cloned()
}

pub fn forget(ip: &str) {
    if let Ok(mut f) = frames().lock() {
        f.remove(ip);
    }
}

fn store(ip: &str, sample: &gst::Sample) {
    let Some(caps) = sample.caps() else { return };
    let Ok(info) = gstreamer_video::VideoInfo::from_caps(caps) else { return };
    let Some(buffer) = sample.buffer() else { return };
    let Ok(map) = buffer.map_readable() else { return };
    let frame = Frame {
        width: info.width(),
        height: info.height(),
        stride: info.stride()[0] as usize,
        data: gtk4::glib::Bytes::from(map.as_slice()),
        serial: SERIAL.fetch_add(1, Ordering::Relaxed),
    };
    if let Ok(mut f) = frames().lock() {
        f.insert(ip.to_string(), frame);
    }
}

/// A camera this computer has: (name for people, how to open it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    /// What identifies it across restarts: its V4L2 device, or
    /// `pipewire:<node name>` for cameras that only exist in PipeWire
    /// (e.g. BigCam's processed camera).
    pub path: String,
    /// Its PipeWire node name, when PipeWire manages it (how it's opened).
    pub node: Option<String>,
}

/// Cameras plugged in (V4L2 capture devices).
pub fn devices() -> Vec<Device> {
    if gst::init().is_err() {
        return vec![];
    }
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    if monitor.start().is_err() {
        return vec![];
    }
    let mut out: Vec<Device> = Vec::new();
    for d in monitor.devices() {
        let props = d.properties();
        let get = |k: &str| props.as_ref().and_then(|p| p.get::<String>(k).ok()).filter(|v| !v.is_empty());
        // PipeWire lists each camera too: that's the way to open it, shared
        // with other programs instead of taking the device for ourselves.
        let node = get("node.name");
        let path = match get("api.v4l2.path").or_else(|| get("device.path")) {
            Some(p) => p,
            None => match &node {
                Some(n) => format!("pipewire:{n}"),
                None => continue,
            },
        };
        if let Some(known) = out.iter_mut().find(|x| x.path == path) {
            if known.node.is_none() {
                known.node = node;
            }
            continue;
        }
        // Only devices that really capture (not metadata nodes).
        let caps_ok = d.caps().is_some_and(|c| c.iter().any(|s| s.name().starts_with("video/x-raw") || s.name() == "image/jpeg"));
        if caps_ok {
            out.push(Device { name: d.display_name().to_string(), path, node });
        }
    }
    monitor.stop();
    out
}

/// How the camera is reached.
enum Access {
    /// Through the desktop's camera portal: its PipeWire connection.
    Portal(std::os::fd::OwnedFd),
    /// The session's PipeWire (no camera portal here).
    PipeWire,
    /// No PipeWire plugin for GStreamer: the device itself.
    Direct,
}

/// Ask the desktop for the camera (the Camera portal: it may ask the person,
/// and shows that the camera is in use). `Err` = the person said no.
#[cfg(target_os = "linux")]
fn access() -> Result<Access, String> {
    if gst::ElementFactory::find("pipewiresrc").is_none() {
        return Ok(Access::Direct);
    }
    use ashpd::desktop::camera::Camera;
    let result: Result<Option<std::os::fd::OwnedFd>, Option<String>> = ashpd::zbus::block_on(async {
        // As a host app (no app id): the portal lets it in (GNOME refused
        // at once when we registered one — it then wants a stored permission).
        let cam = Camera::new().await.map_err(|_| None)?;
        let request = cam.request_access(Default::default()).await.map_err(|_| None)?;
        match request.response() {
            Ok(()) => {}
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                return Err(Some(crate::tr!("access to the camera was refused")));
            }
            Err(_) => return Err(None),
        }
        if !cam.is_present().await.unwrap_or(true) {
            return Ok(None);
        }
        cam.open_pipe_wire_remote(Default::default()).await.map(Some).map_err(|_| None)
    });
    match result {
        Ok(Some(fd)) => Ok(Access::Portal(fd)),
        Ok(None) | Err(None) => Ok(Access::PipeWire),
        Err(Some(refused)) => Err(refused),
    }
}

/// The source part of the capture pipeline.
fn source_desc(device: Option<&Device>, access: &Access) -> String {
    // Development and tests on machines without a camera.
    if std::env::var_os("BIGLACE_FAKE_CAMERA").is_some() {
        return "videotestsrc is-live=true pattern=ball".into();
    }
    match access {
        Access::Portal(_) | Access::PipeWire => {
            let fd = match access {
                Access::Portal(fd) => format!(" fd={}", std::os::fd::AsRawFd::as_raw_fd(fd)),
                _ => String::new(),
            };
            let target = device.and_then(|d| d.node.as_deref()).map(|n| format!(" target-object={n}")).unwrap_or_default();
            format!("pipewiresrc{fd}{target} always-copy=true ! decodebin")
        }
        Access::Direct => match device.filter(|d| d.path.starts_with("/dev/")) {
            Some(d) => format!("v4l2src device={} ! decodebin", d.path),
            None => "v4l2src ! decodebin".into(),
        },
    }
}

/// Our camera, running.
pub struct Capture {
    pipeline: gst::Pipeline,
    alive: Arc<AtomicBool>,
    /// The portal's PipeWire connection, open while we capture.
    _access: Access,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Where each encoded picture goes.
type Sender = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Start our camera (`path`, or the first one): `send` gets each encoded
/// picture (capture time µs + H.264 access unit); our own preview goes to
/// `frame(my_ip)`.
pub fn start(path: Option<&str>, my_ip: &str, send: impl Fn(&[u8]) + Send + Sync + 'static) -> Result<Capture, String> {
    gst::init().map_err(|e| e.to_string())?;
    let fake = std::env::var_os("BIGLACE_FAKE_CAMERA").is_some();
    let access = if fake { Access::Direct } else { access()? };
    // The chosen camera (by its device), with its PipeWire node.
    let device = if fake { None } else { path.and_then(|p| devices().into_iter().find(|d| d.path == p)) };
    let device = device.or_else(|| {
        path.map(|p| Device { name: String::new(), path: p.to_string(), node: p.strip_prefix("pipewire:").map(str::to_string) })
    });
    let send: Sender = Arc::new(send);
    let (pipeline, access) = match run(&source_desc(device.as_ref(), &access), my_ip, &send) {
        Ok(p) => (p, access),
        // A PipeWire that won't link the camera ("target not found") while
        // the device itself is free: open the device.
        Err(e) if !matches!(access, Access::Direct) => {
            let Some(d) = device.as_ref().filter(|d| d.path.starts_with("/dev/")) else { return Err(e) };
            crate::tailscale::dbg(&format!("room: camera through PipeWire failed ({e}), opening {}", d.path));
            (run(&format!("v4l2src device={} ! decodebin", d.path), my_ip, &send)?, Access::Direct)
        }
        Err(e) => return Err(e),
    };
    crate::tailscale::dbg(&format!(
        "room: camera through {}",
        match access {
            Access::Portal(_) => "the camera portal (PipeWire)",
            Access::PipeWire => "PipeWire",
            Access::Direct => "the device",
        }
    ));
    Ok(Capture { pipeline, alive: Arc::new(AtomicBool::new(true)), _access: access })
}

/// Build and start the capture pipeline from `src`; an error when the
/// camera can't open (busy, unplugged, not linked).
fn run(src: &str, my_ip: &str, send: &Sender) -> Result<gst::Pipeline, String> {
    let enc = encoder();
    let desc = format!(
        "{src} ! videoconvert ! videoscale ! videorate \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate={FPS}/1,pixel-aspect-ratio=1/1 \
         ! tee name=t \
         t. ! queue max-size-buffers=1 leaky=downstream ! videoflip method=horizontal-flip ! videoconvert \
            ! video/x-raw,format=RGBA ! appsink name=preview sync=false max-buffers=1 drop=true \
         t. ! queue max-size-buffers=2 leaky=downstream ! {enc} \
            ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
            ! appsink name=enc sync=false max-buffers=4 drop=true"
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let preview = pipeline.by_name("preview").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no preview")?;
    let me = my_ip.to_string();
    preview.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                if let Ok(sample) = s.pull_sample() {
                    store(&me, &sample);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let encoded = pipeline.by_name("enc").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no encoder")?;
    let send = send.clone();
    encoded.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                if let Ok(sample) = s.pull_sample() {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let mut payload = crate::remote::now_us().to_be_bytes().to_vec();
                            payload.extend_from_slice(map.as_slice());
                            send(&payload);
                        }
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(e.to_string());
    }
    if let Some(bus) = pipeline.bus() {
        if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(1500), &[gst::MessageType::Error]) {
            let _ = pipeline.set_state(gst::State::Null);
            if let gst::MessageView::Error(e) = msg.view() {
                return Err(e.error().to_string());
            }
        }
    }
    Ok(pipeline)
}

/// A light, low-delay H.264 encoder: the GPU's when there is one.
fn encoder() -> String {
    let kbps = KBPS;
    if gst::ElementFactory::find("vah264enc").is_some() {
        return format!("videoconvert ! video/x-raw,format=NV12 ! vah264enc bitrate={kbps} key-int-max=48 b-frames=0 rate-control=cbr");
    }
    format!(
        "videoconvert ! video/x-raw,format=I420 ! x264enc tune=zerolatency speed-preset=ultrafast bitrate={kbps} \
         key-int-max=48 bframes=0 threads=2"
    )
}

/// One person's camera, decoded here.
pub struct Decoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Decoder {
    /// Pictures from the person at `ip` land in `frame(ip)`.
    pub fn new(ip: &str) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
               caps=video/x-h264,stream-format=byte-stream,alignment=au \
             ! h264parse ! {} ! videoconvert ! video/x-raw,format=RGBA \
             ! appsink name=sink sync=false max-buffers=1 drop=true",
            crate::remote::client::decoder()
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        let src = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or("no appsrc")?;
        let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no appsink")?;
        let ip = ip.to_string();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    if let Ok(sample) = s.pull_sample() {
                        store(&ip, &sample);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        Ok(Self { pipeline, src })
    }

    /// One received picture (capture time + access unit).
    pub fn push(&self, payload: &[u8]) {
        if payload.len() > 8 {
            let _ = self.src.push_buffer(gst::Buffer::from_slice(payload[8..].to_vec()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real camera, through the Camera portal and PipeWire (run by hand
    /// on a machine with a camera: `cargo test real_camera -- --ignored`).
    #[test]
    #[ignore]
    fn real_camera_through_pipewire() {
        let _turn = crate::remote::heavy_test();
        let list = devices();
        eprintln!("cameras: {list:?}");
        let got = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let g = got.clone();
        let cap = start(list.first().map(|d| d.path.as_str()), "me", move |_| {
            g.fetch_add(1, Ordering::Relaxed);
        })
        .expect("camera starts");
        std::thread::sleep(std::time::Duration::from_secs(4));
        drop(cap);
        eprintln!("encoded pictures: {}", got.load(Ordering::Relaxed));
        assert!(got.load(Ordering::Relaxed) > 10);
    }

    /// A test picture goes through our capture chain and comes out of a
    /// decoder as a 640×360 frame.
    #[test]
    fn camera_pictures_round_trip() {
        let _turn = crate::remote::heavy_test();
        if gst::init().is_err() || gst::ElementFactory::find("avdec_h264").is_none() {
            return;
        }
        std::env::set_var("BIGLACE_FAKE_CAMERA", "1");
        let decoder = Arc::new(Decoder::new("peer").unwrap());
        let d2 = decoder.clone();
        let cap = start(None, "me", move |p| d2.push(p)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while frame("peer").is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        drop(cap);
        std::env::remove_var("BIGLACE_FAKE_CAMERA");
        let f = frame("peer").expect("no decoded picture");
        assert_eq!((f.width, f.height), (640, 360));
        assert!(frame("me").is_some(), "our own preview");
    }
}
