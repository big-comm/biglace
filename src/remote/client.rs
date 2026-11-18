//! Viewer side of a remote-screen session: ask the host, then decode its
//! screen and send it our input.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::mpsc;
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

/// One decoded frame, RGBA, tightly packed rows of `stride` bytes.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// What the session thread tells the window.
pub enum Event {
    Info(VideoInfo),
    Frame(Frame),
    Ended(String),
}

/// A live session: send input with `input`; drop or `close` to end it.
pub struct Session {
    input: mpsc::Sender<Input>,
    stream: TcpStream,
}

impl Session {
    pub fn send(&self, event: Input) {
        let _ = self.input.send(event);
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
pub fn open(ip: &str, events: async_channel::Sender<Event>) -> Result<Session, OpenError> {
    let addr: IpAddr = ip.parse().map_err(|_| OpenError::Other("bad address".into()))?;
    let sock = SocketAddr::new(addr, crate::agent::PORT);
    let stream = TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT).map_err(|e| OpenError::Other(e.to_string()))?;
    let host = match addr {
        IpAddr::V6(v6) => format!("[{v6}]:{}", crate::agent::PORT),
        IpAddr::V4(v4) => format!("{v4}:{}", crate::agent::PORT),
    };
    let url = format!("http://{host}{}", super::SCREEN_PATH);
    let signed = device_key::sign_headers("GET", &url, "").ok_or(OpenError::Unverified)?;
    let mut req = format!("GET {} HTTP/1.1\r\nHost: {host}\r\n", super::SCREEN_PATH);
    for (name, value) in signed {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    (&stream).write_all(req.as_bytes()).map_err(|e| OpenError::Other(e.to_string()))?;

    // Status line + headers.
    let _ = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
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

    // Input: written by its own thread so the UI never blocks on the network.
    let (tx, rx) = mpsc::channel::<Input>();
    let mut writer = stream.try_clone().map_err(|e| OpenError::Other(e.to_string()))?;
    let _ = std::thread::Builder::new()
        .name("biglace-viewer-input".into())
        .spawn(move || {
            for event in rx {
                let Ok(json) = serde_json::to_vec(&event) else { continue };
                if write_frame(&mut writer, Kind::Input, &json).is_err() {
                    break;
                }
            }
        });

    // Video: read frames, decode, hand RGBA frames to the window.
    let _ = std::thread::Builder::new()
        .name("biglace-viewer-video".into())
        .spawn(move || {
            let why = match decode_loop(reader, &events) {
                Ok(()) => String::new(),
                Err(e) => e.to_string(),
            };
            let _ = events.send_blocking(Event::Ended(why));
        });

    Ok(Session { input: tx, stream })
}

fn decode_loop(mut reader: BufReader<TcpStream>, events: &async_channel::Sender<Event>) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    gst::init()?;
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=true format=time do-timestamp=true \
           caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! h264parse ! avdec_h264 ! videoconvert ! video/x-raw,format=RGBA \
         ! appsink name=sink sync=false max-buffers=2 drop=true",
    )?
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

    // Decoded frames go straight to the window (newest wins: drop=true).
    {
        let events = events.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let info = gstreamer_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let frame = Frame {
                        width: info.width(),
                        height: info.height(),
                        stride: info.stride()[0] as usize,
                        data: map.as_slice().to_vec(),
                    };
                    // A closed window ends the session.
                    if events.try_send(Event::Frame(frame)).is_err() && events.is_closed() {
                        return Err(gst::FlowError::Eos);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }
    pipeline.set_state(gst::State::Playing)?;

    let result = (|| -> anyhow::Result<()> {
        loop {
            let (kind, payload) = read_frame(&mut reader)?;
            match kind {
                Kind::Info => {
                    if let Ok(info) = serde_json::from_slice::<VideoInfo>(&payload) {
                        if events.send_blocking(Event::Info(info)).is_err() {
                            return Ok(());
                        }
                    }
                }
                Kind::Video => {
                    let buffer = gst::Buffer::from_mut_slice(payload);
                    if src.push_buffer(buffer).is_err() {
                        anyhow::bail!("decoder stopped");
                    }
                }
                Kind::Input => {}
                Kind::Error => anyhow::bail!("{}", String::from_utf8_lossy(&payload)),
            }
        }
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use std::net::TcpListener;

    /// Encode a test pattern like a host does, send it over a real TCP
    /// connection in session frames, and decode it like the viewer does:
    /// RGBA frames of the right size must come out.
    #[test]
    fn host_frames_decode_on_the_viewer() {
        gst::init().unwrap();
        if gst::ElementFactory::find("x264enc").is_none() || gst::ElementFactory::find("avdec_h264").is_none() {
            eprintln!("x264enc/avdec_h264 missing; skipping");
            return;
        }
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=30 ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert \
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
            let info = serde_json::to_vec(&VideoInfo { width: 320, height: 240 }).unwrap();
            write_frame(&mut s, Kind::Info, &info).unwrap();
            for u in units {
                write_frame(&mut s, Kind::Video, &u).unwrap();
            }
            // Give the decoder time to drain before the connection closes.
            std::thread::sleep(Duration::from_millis(800));
        });
        let stream = TcpStream::connect(addr).unwrap();
        let (tx, rx) = async_channel::bounded(64);
        let viewer = std::thread::spawn(move || decode_loop(BufReader::new(stream), &tx));
        let mut got_info = false;
        let mut frames = 0;
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                Event::Info(i) => got_info = i.width == 320 && i.height == 240,
                Event::Frame(f) => {
                    assert_eq!((f.width, f.height), (320, 240));
                    assert!(f.stride >= 320 * 4 && f.data.len() >= f.stride * 240);
                    frames += 1;
                }
                Event::Ended(_) => break,
            }
            if frames >= 5 {
                break;
            }
        }
        drop(rx);
        host.join().unwrap();
        let _ = viewer.join();
        assert!(got_info, "no video info");
        assert!(frames >= 5, "only {frames} decoded frames");
    }
}
