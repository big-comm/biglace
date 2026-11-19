//! Showing your screen to the voice room: captured and encoded once, sent
//! to everyone who watches (no consent prompt for them — sharing *is* the
//! consent). Watching is view-only; the presenter may hand control to one
//! viewer at a time (asked for with "Request control", or given directly),
//! and takes it back with one click.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::{emit, room, share_audio, RoomEvent, STAGE_PREFIX};
use crate::remote::host::{self, InputSink, StopHandle};
use crate::remote::{read_frame, write_frame, Input, Kind};
use crate::tailscale::{self, Identity};

/// Frames waiting for one viewer before it counts as slow (it then skips
/// to the next keyframe instead of falling further behind).
const QUEUE: usize = 6;
/// A viewer that stops reading for this long is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(8);

type Out = (Kind, Arc<Vec<u8>>);

struct Viewer {
    id: u64,
    ip: String,
    tx: SyncSender<Out>,
    stream: TcpStream,
    /// Missed frames (or just joined): wait for a keyframe.
    skip_until_key: AtomicBool,
}

pub(super) struct Stage {
    stop: StopHandle,
    viewers: Mutex<Vec<Viewer>>,
    want_key: Arc<AtomicBool>,
    last_info: Mutex<Option<Vec<u8>>>,
    /// The viewer allowed to control the screen.
    controller: Mutex<Option<u64>>,
    sink: Mutex<Option<InputSink>>,
    group: String,
    /// The presenter's pointer is in the frames (viewers hide their own).
    pointer_in_video: AtomicBool,
}

fn slot() -> &'static Mutex<Option<Arc<Stage>>> {
    static S: OnceLock<Mutex<Option<Arc<Stage>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn stage() -> Option<Arc<Stage>> {
    slot().lock().ok()?.clone()
}

/// We're showing our screen to the room.
pub fn presenting() -> bool {
    stage().is_some()
}

/// Who watches (address → (viewer id, controls it)).
pub(super) fn audience() -> HashMap<String, (u64, bool)> {
    let Some(s) = stage() else { return HashMap::new() };
    let controller = s.controller.lock().ok().and_then(|c| *c);
    s.viewers
        .lock()
        .map(|v| v.iter().map(|v| (v.ip.clone(), (v.id, controller == Some(v.id)))).collect())
        .unwrap_or_default()
}

/// Start showing our screen to the room we're in. The capture starts in the
/// background (on Wayland the desktop asks which screen first).
pub fn start(with_sound: bool) -> Result<(), String> {
    let current = room().ok_or_else(|| crate::tr!("you are not in a room"))?;
    if !crate::remote::can_host() {
        return Err(crate::tr!("screen sharing isn't available on this computer"));
    }
    if presenting() {
        return Ok(());
    }
    let stage = Arc::new(Stage {
        stop: StopHandle::detached(),
        viewers: Mutex::new(Vec::new()),
        want_key: Arc::new(AtomicBool::new(false)),
        last_info: Mutex::new(None),
        controller: Mutex::new(None),
        sink: Mutex::new(None),
        group: current.group.clone(),
        pointer_in_video: AtomicBool::new(false),
    });
    if let Ok(mut s) = slot().lock() {
        *s = Some(stage.clone());
    }
    let _ = std::thread::Builder::new().name("biglace-stage".into()).spawn(move || {
        let source = match host::open_source(&stage.stop, host::Pointer::Embedded) {
            Ok(s) => s,
            Err(e) => {
                tailscale::dbg(&format!("stage: no screen source: {e}"));
                emit(RoomEvent::Problem(crate::trf!("Could not share the screen: {error}", "error" => e)));
                finish(&stage);
                return;
            }
        };
        let host::Source { pipelines, dirty, sink, pointer, .. } = source;
        stage.pointer_in_video.store(pointer == host::Pointer::Embedded, Ordering::Release);
        #[cfg(target_os = "linux")]
        let _keep_fd = source._fd;
        if let Ok(mut s) = stage.sink.lock() {
            *s = Some(sink);
        }
        // The computer's sound too (not the room's voices), if asked.
        if with_sound {
            match share_audio::start() {
                Ok(source) => {
                    let st = stage.clone();
                    let _ = std::thread::Builder::new().name("biglace-stage-sound".into()).spawn(move || {
                        let send = |payload: &[u8]| -> anyhow::Result<()> {
                            st.fanout(Kind::Audio, payload);
                            Ok(())
                        };
                        if let Err(e) = host::stream_audio_to(&send, &st.stop, &share_audio::capture_desc(&source)) {
                            tailscale::dbg(&format!("stage: no sound: {e}"));
                        }
                    });
                }
                Err(e) => emit(RoomEvent::Problem(crate::trf!("The computer's sound can't be shared: {error}", "error" => e))),
            }
        }
        // Tell the room once the screen is really being captured.
        if let Some(r) = room() {
            r.send_state();
            emit(r.view());
        }
        let send = |kind: Kind, payload: &[u8]| -> anyhow::Result<()> {
            stage.fanout(kind, payload);
            Ok(())
        };
        if let Err(e) = host::stream_screen(&send, &pipelines, dirty, &stage.stop, true, &stage.want_key, &std::sync::atomic::AtomicBool::new(false)) {
            tailscale::dbg(&format!("stage: capture ended: {e}"));
            if !stage.stop.stopped() {
                emit(RoomEvent::Problem(crate::trf!("The screen share stopped: {error}", "error" => e)));
            }
        }
        finish(&stage);
    });
    if let Some(r) = room() {
        emit(r.view());
    }
    Ok(())
}

/// Stop showing our screen.
pub fn stop() {
    if let Some(s) = stage() {
        s.stop.stop();
        finish(&s);
    }
}

/// The share is over: forget it, drop the viewers, tell the room.
fn finish(stage: &Arc<Stage>) {
    stage.stop.stop();
    share_audio::stop();
    if let Ok(mut s) = slot().lock() {
        if s.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, stage)) {
            *s = None;
        }
    }
    if let Ok(mut v) = stage.viewers.lock() {
        for viewer in v.drain(..) {
            let _ = viewer.stream.shutdown(std::net::Shutdown::Both);
        }
    }
    if let Ok(mut sink) = stage.sink.lock() {
        // Dropping the sink ends the portal session too.
        sink.take();
    }
    if let Some(r) = room() {
        r.send_state();
        emit(r.view());
    }
}

/// Let viewer `id` control the screen (taking it from whoever had it).
pub fn grant(id: u64) {
    let Some(s) = stage() else { return };
    let previous = s.controller.lock().ok().and_then(|mut c| c.replace(id));
    if let Some(p) = previous.filter(|p| *p != id) {
        s.tell(p, false);
    }
    s.tell(id, true);
    if let Some(r) = room() {
        emit(r.view());
    }
}

/// Say no to viewer `id`'s request for control.
pub fn deny(id: u64) {
    if let Some(s) = stage() {
        if s.controller.lock().ok().and_then(|c| *c) != Some(id) {
            s.tell(id, false);
        }
    }
}

/// Take control back.
pub fn revoke() {
    let Some(s) = stage() else { return };
    if let Some(p) = s.controller.lock().ok().and_then(|mut c| c.take()) {
        s.tell(p, false);
    }
    if let Some(r) = room() {
        emit(r.view());
    }
}

/// An H.264 access unit that starts a picture on its own (IDR).
fn is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1f == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

impl Stage {
    /// One encoded frame (or video info) → every viewer.
    fn fanout(&self, kind: Kind, payload: &[u8]) {
        if kind == Kind::Info {
            if let Ok(mut i) = self.last_info.lock() {
                *i = Some(payload.to_vec());
            }
        }
        let Ok(mut viewers) = self.viewers.lock() else { return };
        if viewers.is_empty() {
            return;
        }
        let video = matches!(kind, Kind::VideoTs | Kind::Video);
        let key = video && is_keyframe(payload.get(8..).unwrap_or_default());
        let data = Arc::new(payload.to_vec());
        let mut gone = Vec::new();
        for v in viewers.iter() {
            if video && v.skip_until_key.load(Ordering::Relaxed) {
                if !key {
                    continue;
                }
                v.skip_until_key.store(false, Ordering::Relaxed);
            }
            match v.tx.try_send((kind, data.clone())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) if video => {
                    // Behind: skip ahead to a fresh picture.
                    v.skip_until_key.store(true, Ordering::Relaxed);
                    self.want_key.store(true, Ordering::Release);
                }
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => gone.push(v.id),
            }
        }
        if !gone.is_empty() {
            viewers.retain(|v| !gone.contains(&v.id));
        }
    }

    /// Tell viewer `id` whether it controls the screen now.
    fn tell(&self, id: u64, granted: bool) {
        let msg = Arc::new(serde_json::json!({ "granted": granted }).to_string().into_bytes());
        if let Ok(v) = self.viewers.lock() {
            if let Some(v) = v.iter().find(|v| v.id == id) {
                let _ = v.tx.try_send((Kind::Control, msg));
            }
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut v) = self.viewers.lock() {
            v.retain(|x| x.id != id);
        }
        if let Ok(mut c) = self.controller.lock() {
            if *c == Some(id) {
                *c = None;
            }
        }
        if let Some(r) = room() {
            emit(r.view());
        }
    }

    fn input(&self, id: u64, event: Input) {
        if self.controller.lock().ok().and_then(|c| *c) != Some(id) {
            return;
        }
        if let Ok(mut sink) = self.sink.lock() {
            if let Some(sink) = sink.as_mut() {
                sink.apply(event);
            }
        }
    }
}

fn respond(mut stream: &TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// Someone in the room starts watching (agent thread): `/v1/stage/<group>`.
pub fn accept(stream: TcpStream, peer: IpAddr, headers: &HashMap<String, String>, identity: &Identity, path: &str) {
    let local = stream.local_addr().map(|a| format!("{}:{}", a.ip(), a.port())).unwrap_or_default();
    if let Err(why) = host::verify_caller(headers, peer, &local, path) {
        tailscale::dbg(&format!("stage: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let group = path.strip_prefix(STAGE_PREFIX).unwrap_or_default();
    let Some(stage) = stage().filter(|s| s.group == group) else {
        return respond(&stream, 404, "Not Found");
    };
    // Only the people in this room may watch.
    let ip = peer.to_string();
    let in_room = room().is_some_and(|r| r.roster.lock().map(|l| l.iter().any(|p| p.ip == ip)).unwrap_or(false));
    if !in_room {
        return respond(&stream, 403, "Forbidden");
    }
    let _ = write!(&stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-screen\r\n\r\n");
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nodelay(true);

    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel::<Out>(QUEUE);
    let (Ok(mut writer), Ok(reader), Ok(handle)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else {
        return;
    };
    // What the screen looks like, then a fresh keyframe.
    if let Some(info) = stage.last_info.lock().ok().and_then(|i| i.clone()) {
        let _ = tx.try_send((Kind::Info, Arc::new(info)));
    }
    if stage.pointer_in_video.load(Ordering::Acquire) {
        let _ = tx.try_send((Kind::Cursor, Arc::new(Vec::new())));
    }
    if let Ok(mut v) = stage.viewers.lock() {
        v.push(Viewer {
            id,
            ip: ip.clone(),
            tx: tx.clone(),
            stream: handle,
            skip_until_key: AtomicBool::new(true),
        });
    }
    stage.want_key.store(true, Ordering::Release);
    tailscale::dbg(&format!("stage: {}@{} watching", identity.login, identity.device));
    if let Some(r) = room() {
        emit(r.view());
    }

    // Writer: frames to this viewer, in order.
    let _ = std::thread::Builder::new().name("biglace-stage-out".into()).spawn(move || {
        for (kind, data) in rx {
            if write_frame(&mut writer, kind, &data).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });

    // Reader: pings, keyframe requests, control requests and (when given
    // control) input.
    let user = identity.login.clone();
    let mut reader = BufReader::new(reader);
    while !stage.stop.stopped() {
        match read_frame(&mut reader) {
            Ok((Kind::Input, payload)) => {
                if let Ok(event) = serde_json::from_slice::<Input>(&payload) {
                    stage.input(id, event);
                }
            }
            Ok((Kind::Ping, payload)) if payload.len() == 8 => {
                let mut pong = payload;
                pong.extend_from_slice(&crate::remote::now_us().to_be_bytes());
                let _ = tx.try_send((Kind::Pong, Arc::new(pong)));
            }
            Ok((Kind::KeyFrame, _)) => stage.want_key.store(true, Ordering::Release),
            Ok((Kind::Control, payload)) => {
                let v: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
                if v.get("ask").and_then(serde_json::Value::as_bool) == Some(true) {
                    emit(RoomEvent::ControlRequest { viewer: id, user: user.clone() });
                } else if v.get("release").and_then(serde_json::Value::as_bool) == Some(true) {
                    let had = stage.controller.lock().ok().is_some_and(|c| *c == Some(id));
                    if had {
                        revoke();
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(tx);
    stage.remove(id);
    tailscale::dbg(&format!("stage: {user} stopped watching"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stage() -> Stage {
        Stage {
            stop: StopHandle::detached(),
            viewers: Mutex::new(Vec::new()),
            want_key: Arc::new(AtomicBool::new(false)),
            last_info: Mutex::new(None),
            controller: Mutex::new(None),
            sink: Mutex::new(None),
            group: "familia".into(),
            pointer_in_video: AtomicBool::new(false),
        }
    }

    fn viewer(stage: &Stage, id: u64, queue: usize) -> mpsc::Receiver<Out> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (tx, rx) = mpsc::sync_channel(queue);
        stage.viewers.lock().unwrap().push(Viewer {
            id,
            ip: format!("100.64.0.{id}"),
            tx,
            stream,
            skip_until_key: AtomicBool::new(true),
        });
        rx
    }

    fn frame(key: bool) -> Vec<u8> {
        let mut f = 42u64.to_be_bytes().to_vec();
        f.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }, 7, 7]);
        f
    }

    /// Everyone watching gets the frames; someone who just arrived (or fell
    /// behind) starts at a keyframe; a slow one doesn't hold up the rest.
    #[test]
    fn frames_fan_out_to_every_viewer() {
        let stage = test_stage();
        stage.fanout(Kind::Info, b"{\"width\":1}");
        assert!(stage.last_info.lock().unwrap().is_some(), "info kept for late viewers");
        let fast = viewer(&stage, 1, 16);
        let slow = viewer(&stage, 2, 1);
        // Not a keyframe yet: nobody starts on it.
        stage.fanout(Kind::VideoTs, &frame(false));
        assert!(fast.try_recv().is_err() && slow.try_recv().is_err());
        stage.fanout(Kind::VideoTs, &frame(true));
        stage.fanout(Kind::VideoTs, &frame(false));
        stage.fanout(Kind::VideoTs, &frame(false));
        assert_eq!(fast.try_iter().count(), 3, "the fast viewer got all three");
        // The slow one took the keyframe, then its queue was full: it skips
        // to the next keyframe, and one is asked for.
        assert_eq!(slow.try_iter().count(), 1);
        assert!(stage.want_key.load(Ordering::Acquire));
        stage.fanout(Kind::VideoTs, &frame(false));
        assert!(slow.try_recv().is_err(), "still waiting for a keyframe");
        stage.fanout(Kind::VideoTs, &frame(true));
        assert_eq!(slow.try_iter().count(), 1);
        // A viewer that went away is dropped.
        drop(fast);
        stage.fanout(Kind::VideoTs, &frame(true));
        assert_eq!(stage.viewers.lock().unwrap().len(), 1);
    }

    /// Input counts only from the viewer holding control.
    #[test]
    fn control_goes_to_one_viewer() {
        let stage = test_stage();
        let a = viewer(&stage, 1, 8);
        let b = viewer(&stage, 2, 8);
        *stage.controller.lock().unwrap() = Some(1);
        stage.tell(1, true);
        let msg = a.try_recv().unwrap();
        assert_eq!(msg.0, Kind::Control);
        assert!(b.try_recv().is_err());
        stage.remove(1);
        assert_eq!(*stage.controller.lock().unwrap(), None, "control ends with the viewer");
    }

    #[test]
    fn finds_keyframes() {
        // SPS, PPS, IDR.
        assert!(is_keyframe(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9]));
        // A P-frame only.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 1, 2, 3]));
        assert!(!is_keyframe(&[]));
    }
}
