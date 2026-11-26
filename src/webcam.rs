//! One of our phones as this computer's webcam and/or microphone: the phone
//! sends camera pictures (H.264) and sound (Opus) over its agent connection,
//! or the pairing on the local network, and here they become a PipeWire
//! camera and microphone that browsers, OBS or a call app can pick.

use crate::remote::{read_frame, Kind};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::io::BufReader;
use std::net::TcpStream;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Announced by phones that can be a webcam.
pub const CAPABILITY: &str = "webcam";
const PATH: &str = "/v1/webcam";
/// The webcam's picture, whichever way the phone is held: programs keep one size from start to end,
/// so an upright phone shows with bars at its sides.
const FRAME_WIDTH: u32 = 1280;
const FRAME_HEIGHT: u32 = 720;
/// `card_label` of the loopback camera, set in the package's modprobe.d/biglace.conf.
const LOOPBACK_LABEL: &str = "BigLace";

/// What changed, for the window: the webcam started (`true`) or ended.
#[derive(Clone, Debug)]
pub struct WebcamEvent {
    pub on: bool,
    /// Why it ended, when it didn't end on our side.
    pub error: Option<String>,
}

static LISTENERS: crate::listeners::Listeners<WebcamEvent> = crate::listeners::Listeners::new();

pub fn events() -> async_channel::Receiver<WebcamEvent> {
    LISTENERS.subscribe()
}

/// The phone asked for its camera: its address, the connection to end it, whether it sends video,
/// and whether its pictures or sound flow yet (the person there started it).
struct Active {
    target: String,
    stream: TcpStream,
    video: bool,
    flowing: bool,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// A paired phone on the local network asks it this way (see `crate::lan::pairing`).
#[derive(serde::Serialize)]
struct WebcamAsk {
    webcam: Want,
}

#[derive(serde::Serialize)]
struct Want {
    video: bool,
    audio: bool,
    camera: &'static str,
}

/// The `camera` asked for: the phone's back one, or its front one (also when an older phone ignores it).
fn camera(back: bool) -> &'static str {
    if back {
        "back"
    } else {
        "front"
    }
}

/// The phone at `target` (tailnet address, or a paired device's
/// `crate::chat::lan_address`) whose camera we use now.
pub fn active() -> Option<String> {
    ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|a| a.target.clone())
}

/// The phone at this address sends its camera or microphone now, and whether it sends video.
pub fn flowing() -> Option<(String, bool)> {
    ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|a| a.flowing).map(|a| (a.target.clone(), a.video))
}

/// Ask the phone to turn to its other camera, front or back.
pub fn switch_camera() -> Result<(), String> {
    let stream = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|a| a.stream.try_clone());
    let mut stream = stream.ok_or("no webcam")?.map_err(|e| e.to_string())?;
    crate::remote::write_frame(&mut stream, Kind::Control, br#"{"switch":true}"#).map_err(|e| e.to_string())
}

/// The first picture or sound arrived: the person there started it.
fn started() {
    if let Some(a) = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        a.flowing = true;
    }
    LISTENERS.send(WebcamEvent { on: true, error: None });
}

/// Ask `target` (shown as `name`) for its camera (`video`, the back one when `back`) and/or
/// microphone (`audio`); the person there starts it. Blocking until the phone answers.
pub fn start(target: &str, name: &str, video: bool, audio: bool, back: bool) -> Result<(), String> {
    stop();
    let (stream, reader) = connect(target, name, video, audio, back)?;
    let handle = stream.try_clone().map_err(|e| e.to_string())?;
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Active { target: target.to_string(), stream: handle, video, flowing: false });
    let (target, name) = (target.to_string(), name.to_string());
    std::thread::Builder::new()
        .name("biglace-webcam".into())
        .spawn(move || {
            let error = match receive(reader, &name, video, audio) {
                Ok(true) => None,
                Ok(false) => Some(crate::trf!("{name} didn't start it.", "name" => name)),
                Err(e) => Some(e),
            };
            let mine = {
                let mut a = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
                let mine = a.as_ref().is_some_and(|a| a.target == target);
                if mine {
                    *a = None;
                }
                mine
            };
            if mine {
                LISTENERS.send(WebcamEvent { on: false, error });
            }
        })
        .map(drop)
        .map_err(|e| e.to_string())
}

/// End the webcam we use, if any.
pub fn stop() {
    let Some(a) = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
    let _ = a.stream.shutdown(std::net::Shutdown::Both);
    LISTENERS.send(WebcamEvent { on: false, error: None });
}

fn connect(target: &str, name: &str, video: bool, audio: bool, back: bool) -> Result<(TcpStream, BufReader<TcpStream>), String> {
    #[cfg(target_os = "linux")]
    if let Some(key) = crate::chat::lan_key(target) {
        let ask = WebcamAsk { webcam: Want { video, audio, camera: camera(back) } };
        let stream = crate::lan::pairing::take_over(key, &ask).map_err(|_| crate::tr!("The phone isn't reachable on the local network."))?;
        let reader = stream.try_clone().map_err(|e| e.to_string())?;
        return Ok((stream, BufReader::new(reader)));
    }
    let path = format!("{PATH}?video={}&audio={}&camera={}", u8::from(video), u8::from(audio), camera(back));
    let (stream, reader) = crate::remote::client::dial(target, &path, &[], Duration::from_secs(8)).map_err(|e| match e {
        crate::remote::client::OpenError::Denied => crate::trf!("{name} didn't allow the access.", "name" => name),
        crate::remote::client::OpenError::Unverified => {
            crate::tr!("The phone couldn't confirm this computer. Make sure both run an up-to-date BigLace.")
        }
        crate::remote::client::OpenError::Unavailable => crate::trf!("Could not connect: {error}", "error" => "HTTP 503"),
        crate::remote::client::OpenError::Other(d) => crate::trf!("Could not connect: {error}", "error" => d),
    })?;
    // The person there has a minute to start it; then pictures flow without pause.
    let _ = stream.set_read_timeout(None);
    Ok((stream, reader))
}

/// A PipeWire node of `class` ("Video/Source", "Audio/Source") fed by `desc`'s appsrc.
fn source(desc: &str, class: &str, name: &str) -> Result<(gst::Pipeline, gst_app::AppSrc), String> {
    let pipeline = gst::parse::launch(desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let src = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or("no appsrc")?;
    let sink = pipeline.by_name("sink").ok_or("no sink")?;
    let video = class.starts_with("Video");
    let mut props = gst::Structure::builder("props")
        .field("media.class", class)
        .field("node.description", name)
        .field("node.name", if video { "biglace-webcam" } else { "biglace-phone-microphone" })
        .build();
    // The camera portal (browsers, Flatpak apps) only offers nodes with this role.
    if video {
        props.set("media.role", "Camera");
    }
    sink.set_property("stream-properties", props);
    pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
    Ok((pipeline, src))
}

/// The camera device the package's v4l2loopback configuration creates (`card_label=BigLace`), when loaded.
fn loopback_device() -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir("/sys/class/video4linux").ok()?;
    entries.flatten().find_map(|e| {
        let label = std::fs::read_to_string(e.path().join("name")).ok()?;
        (label.trim() == LOOPBACK_LABEL).then(|| std::path::Path::new("/dev").join(e.file_name()))
    })
}

/// H.264 from the phone to the PipeWire node and, when loaded, the loopback camera.
fn video_pipeline() -> String {
    // Browsers and most programs only list /dev/video cameras: feed the package's loopback one too.
    let loopback = loopback_device()
        .map(|dev| format!(" t. ! queue leaky=downstream ! v4l2sink device={} sync=false", dev.display()))
        .unwrap_or_default();
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=true \
           caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! h264parse ! {} ! videoconvert ! videoscale add-borders=true \
         ! video/x-raw,format=YUY2,width={FRAME_WIDTH},height={FRAME_HEIGHT},pixel-aspect-ratio=1/1 ! tee name=t \
         t. ! queue leaky=downstream ! pipewiresink name=sink mode=provide{loopback}",
        crate::remote::client::decoder()
    )
}

/// Feed the phone's frames into the PipeWire nodes until the connection ends. True when any arrived.
fn receive(mut reader: BufReader<TcpStream>, name: &str, video: bool, audio: bool) -> Result<bool, String> {
    gst::init().map_err(|e| e.to_string())?;
    let camera = if video {
        Some(source(&video_pipeline(), "Video/Source", &crate::trf!("{name} (camera)", "name" => name))?)
    } else {
        None
    };
    let microphone = if audio {
        let desc = "appsrc name=src is-live=true format=time do-timestamp=true \
               caps=audio/x-opus,channel-mapping-family=0,channels=1,rate=48000 \
             ! opusdec plc=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 \
             ! pipewiresink name=sink mode=provide";
        Some(source(desc, "Audio/Source", &crate::trf!("{name} (microphone)", "name" => name))?)
    } else {
        None
    };
    let push = |src: &gst_app::AppSrc, payload: &[u8]| {
        // Each frame starts with its capture time (µs, u64 BE).
        if let Some(data) = payload.get(8..).filter(|d| !d.is_empty()) {
            let _ = src.push_buffer(gst::Buffer::from_slice(data.to_vec()));
        }
    };
    let mut flowed = false;
    let ended = loop {
        let frame = read_frame(&mut reader);
        if !flowed && matches!(frame, Ok((Kind::CamVideo | Kind::Audio, _))) {
            flowed = true;
            started();
        }
        match frame {
            Ok((Kind::CamVideo, payload)) => {
                if let Some((_, src)) = &camera {
                    push(src, &payload);
                }
            }
            Ok((Kind::Audio, payload)) => {
                if let Some((_, src)) = &microphone {
                    push(src, &payload);
                }
            }
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    for (pipeline, _) in camera.iter().chain(microphone.iter()) {
        let _ = pipeline.set_state(gst::State::Null);
    }
    // The phone stopped, or we did (the connection shut here).
    match ended.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::NotConnected => Ok(flowed),
        _ if active().is_none() => Ok(flowed),
        _ => Err(ended.to_string()),
    }
}
