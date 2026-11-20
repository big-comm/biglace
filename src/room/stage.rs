//! Showing your screen to the voice room: captured and encoded once, sent
//! to everyone who watches (no consent prompt for them — sharing *is* the
//! consent). Watching is view-only; the presenter may hand control to one
//! viewer at a time (asked for with "Request control", or given directly),
//! and takes it back with one click.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{emit, room, share_audio, RoomEvent, STAGE_PREFIX};
use crate::remote::host::{self, InputSink, StopHandle};
use crate::remote::{read_frame, write_frame, Input, Kind};
use crate::tailscale::{self, Identity};

/// Video frames waiting for one viewer before it counts as slow (it then
/// skips to the next keyframe instead of falling further behind).
const QUEUE: usize = 6;
/// Everything waiting for one viewer: [`QUEUE`] video frames plus about
/// 300 ms of sound (100 packets a second) and control messages.
const CHANNEL: usize = QUEUE + 32;
/// Forced keyframes are big: viewers falling behind or joining ask for at
/// most one this often.
const KEY_INTERVAL: Duration = Duration::from_secs(1);
/// A viewer that stops reading for this long is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(8);

type Out = (Kind, Arc<Vec<u8>>);

/// The last viewer left: the sound capture ends until the next one.
#[derive(Debug)]
struct NobodyWatching;

impl std::fmt::Display for NobodyWatching {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nobody watching")
    }
}

impl std::error::Error for NobodyWatching {}

struct Viewer {
    id: u64,
    ip: String,
    /// Account watching.
    who: String,
    tx: SyncSender<Out>,
    /// Video frames in `tx` not yet written (sound doesn't count).
    video_queued: Arc<AtomicUsize>,
    stream: TcpStream,
    /// Missed frames (or just joined): wait for a keyframe.
    skip_until_key: AtomicBool,
}

pub(super) struct Stage {
    stop: StopHandle,
    viewers: Mutex<Vec<Viewer>>,
    /// Signaled when someone starts watching or the share stops.
    audience_changed: Condvar,
    /// The last viewer left: capture and encoding pause until the next.
    no_viewers: AtomicBool,
    want_key: Arc<AtomicBool>,
    /// A keyframe was asked for and waits for [`KEY_INTERVAL`] to pass.
    key_pending: AtomicBool,
    last_forced_key: Mutex<Option<Instant>>,
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
        audience_changed: Condvar::new(),
        no_viewers: AtomicBool::new(false),
        want_key: Arc::new(AtomicBool::new(false)),
        key_pending: AtomicBool::new(false),
        last_forced_key: Mutex::new(None),
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
        host::only_420_here();
        let source = match host::open_source(host::Pointer::Embedded, host::RateMode::ConstantQuality) {
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
                    let _ = std::thread::Builder::new()
                        .name("biglace-stage-sound".into())
                        .spawn(move || st.stream_sound(&share_audio::capture_desc(&source)));
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
        #[cfg(target_os = "linux")]
        drop(source._damage);
        while stage.wait_for_viewers() {
            // X11: the damage thread polls, so it runs only while capturing;
            // without it every grab goes out.
            #[cfg(target_os = "linux")]
            let (dirty, _damage) = {
                let damage = dirty.as_ref().and_then(|d| crate::remote::damage_x11::watch(d.clone(), pointer));
                (dirty.clone().filter(|_| damage.is_some()), damage)
            };
            match host::stream_screen(&send, &pipelines, dirty.clone(), &stage.stop, true, &stage.want_key, &stage.no_viewers) {
                Ok(true) => tailscale::dbg("stage: nobody watching, capture paused"),
                Ok(false) => break,
                Err(e) => {
                    tailscale::dbg(&format!("stage: capture ended: {e}"));
                    if !stage.stop.stopped() {
                        emit(RoomEvent::Problem(crate::trf!("The screen share stopped: {error}", "error" => e)));
                    }
                    break;
                }
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
    stage.audience_changed.notify_all();
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
    s.release_keys();
    if let Some(who) = s.viewers.lock().ok().and_then(|v| v.iter().find(|v| v.id == id).map(|v| v.who.clone())) {
        crate::access::log(&who, "", "control", "allowed");
    }
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
    s.release_keys();
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
    /// One encoded frame (or video info) → every viewer. False when nobody
    /// watches.
    fn fanout(&self, kind: Kind, payload: &[u8]) -> bool {
        if kind == Kind::Info {
            if let Ok(mut i) = self.last_info.lock() {
                *i = Some(payload.to_vec());
            }
        }
        let Ok(mut viewers) = self.viewers.lock() else { return false };
        if viewers.is_empty() {
            return false;
        }
        let video = matches!(kind, Kind::VideoTs | Kind::Video);
        let key = video && is_keyframe(payload.get(8..).unwrap_or_default());
        if key {
            self.key_pending.store(false, Ordering::Relaxed);
        }
        let data = Arc::new(payload.to_vec());
        let mut gone = Vec::new();
        for v in viewers.iter() {
            if video && v.skip_until_key.load(Ordering::Relaxed) {
                if !key {
                    continue;
                }
                v.skip_until_key.store(false, Ordering::Relaxed);
            }
            if video && v.video_queued.load(Ordering::Acquire) >= QUEUE {
                // Behind: skip ahead to a fresh picture.
                v.skip_until_key.store(true, Ordering::Relaxed);
                self.request_key();
                continue;
            }
            match v.tx.try_send((kind, data.clone())) {
                Ok(()) if video => {
                    v.video_queued.fetch_add(1, Ordering::AcqRel);
                }
                Ok(()) => {}
                Err(TrySendError::Full(_)) if video => {
                    v.skip_until_key.store(true, Ordering::Relaxed);
                    self.request_key();
                }
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => gone.push(v.id),
            }
        }
        if video && self.key_pending.load(Ordering::Relaxed) {
            self.force_key_when_due();
        }
        if !gone.is_empty() {
            viewers.retain(|v| !gone.contains(&v.id));
            if viewers.is_empty() {
                self.no_viewers.store(true, Ordering::Release);
            }
        }
        true
    }

    /// Send the computer's sound (captured with `desc`) while anyone
    /// watches, until the share stops.
    fn stream_sound(&self, desc: &str) {
        let send = |payload: &[u8]| -> anyhow::Result<()> {
            if self.fanout(Kind::Audio, payload) {
                Ok(())
            } else {
                Err(NobodyWatching.into())
            }
        };
        while self.wait_for_viewers() {
            match host::stream_audio_to(&send, &self.stop, desc) {
                Err(e) if e.is::<NobodyWatching>() => tailscale::dbg("stage: nobody watching, sound paused"),
                Ok(()) => break,
                Err(e) => {
                    tailscale::dbg(&format!("stage: no sound: {e}"));
                    break;
                }
            }
        }
    }

    /// Block until someone watches; false once the share stopped.
    fn wait_for_viewers(&self) -> bool {
        let Ok(mut viewers) = self.viewers.lock() else { return false };
        while viewers.is_empty() && !self.stop.stopped() {
            viewers = match self.audience_changed.wait(viewers) {
                Ok(v) => v,
                Err(_) => return false,
            };
        }
        // Someone watches: a pause raised while capture was already paused
        // must not stop the capture that starts now.
        self.no_viewers.store(false, Ordering::Release);
        !self.stop.stopped()
    }

    /// Ask for a keyframe, at most once per [`KEY_INTERVAL`].
    fn request_key(&self) {
        self.key_pending.store(true, Ordering::Relaxed);
        self.force_key_when_due();
    }

    fn force_key_when_due(&self) {
        let Ok(mut last) = self.last_forced_key.lock() else { return };
        if last.is_some_and(|at| at.elapsed() < KEY_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
        self.key_pending.store(false, Ordering::Relaxed);
        self.want_key.store(true, Ordering::Release);
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
            if v.is_empty() {
                self.no_viewers.store(true, Ordering::Release);
            }
        }
        let was_controller = self.controller.lock().is_ok_and(|mut c| {
            let was = *c == Some(id);
            if was {
                *c = None;
            }
            was
        });
        if was_controller {
            self.release_keys();
        }
        if let Some(r) = room() {
            emit(r.view());
        }
    }

    /// Whatever the one in control held down, let go (control changes hands).
    fn release_keys(&self) {
        if let Ok(mut sink) = self.sink.lock() {
            if let Some(sink) = sink.as_mut() {
                sink.release_all();
            }
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
    crate::remote::limit_send_queue(&stream);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nodelay(true);

    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel::<Out>(CHANNEL);
    let video_queued = Arc::new(AtomicUsize::new(0));
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
            who: identity.login.clone(),
            tx: tx.clone(),
            video_queued: video_queued.clone(),
            stream: handle,
            skip_until_key: AtomicBool::new(true),
        });
    }
    stage.audience_changed.notify_all();
    stage.request_key();
    tailscale::dbg(&format!("stage: {}@{} watching", identity.login, identity.device));
    crate::access::log(&identity.login, &identity.device, "watch", "allowed");
    if let Some(r) = room() {
        emit(r.view());
    }

    // Writer: frames to this viewer, in order.
    let _ = std::thread::Builder::new().name("biglace-stage-out".into()).spawn(move || {
        for (kind, data) in rx {
            if write_frame(&mut writer, kind, &data).is_err() {
                break;
            }
            if matches!(kind, Kind::VideoTs | Kind::Video) {
                video_queued.fetch_sub(1, Ordering::AcqRel);
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
            Ok((Kind::KeyFrame, _)) => stage.request_key(),
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
            audience_changed: Condvar::new(),
            no_viewers: AtomicBool::new(false),
            want_key: Arc::new(AtomicBool::new(false)),
            key_pending: AtomicBool::new(false),
            last_forced_key: Mutex::new(None),
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
            who: String::new(),
            tx,
            video_queued: Arc::new(AtomicUsize::new(0)),
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

    /// Sound arriving while a viewer's writer stalls must not take the
    /// video's room: the viewer keeps its picture and gets all the sound.
    #[test]
    fn sound_leaves_the_video_budget_alone() {
        let stage = test_stage();
        let rx = viewer(&stage, 1, CHANNEL);
        stage.fanout(Kind::VideoTs, &frame(true));
        stage.want_key.store(false, Ordering::Release);
        for _ in 0..20 {
            stage.fanout(Kind::Audio, &[0; 16]);
        }
        stage.fanout(Kind::VideoTs, &frame(false));
        let got: Vec<Kind> = rx.try_iter().map(|(kind, _)| kind).collect();
        assert_eq!(got.iter().filter(|k| **k == Kind::Audio).count(), 20, "no sound dropped");
        assert_eq!(got.iter().filter(|k| **k == Kind::VideoTs).count(), 2, "no video skipped");
        assert!(!stage.want_key.load(Ordering::Acquire), "no keyframe forced");
    }

    /// A viewer that keeps falling behind forces at most one keyframe per
    /// interval; the request waits instead of being lost.
    #[test]
    fn forced_keyframes_are_rate_limited() {
        let stage = test_stage();
        let rx = viewer(&stage, 1, CHANNEL);
        let fall_behind = || {
            stage.fanout(Kind::VideoTs, &frame(true));
            for _ in 0..QUEUE {
                stage.fanout(Kind::VideoTs, &frame(false));
            }
            let _ = rx.try_iter().count();
            stage.viewers.lock().unwrap()[0].video_queued.store(0, Ordering::Release);
        };
        fall_behind();
        assert!(stage.want_key.swap(false, Ordering::AcqRel), "the first one is forced at once");
        fall_behind();
        stage.fanout(Kind::VideoTs, &frame(false));
        assert!(!stage.want_key.load(Ordering::Acquire), "a second one within the interval waits");
        *stage.last_forced_key.lock().unwrap() = Instant::now().checked_sub(KEY_INTERVAL);
        stage.fanout(Kind::VideoTs, &frame(false));
        assert!(stage.want_key.load(Ordering::Acquire), "the waiting one is forced once the interval passed");
    }

    /// Nobody watching: capture waits for the first viewer and pauses
    /// again when the last one leaves.
    #[test]
    fn capture_runs_only_while_someone_watches() {
        let stage = Arc::new(test_stage());
        let waiter = {
            let stage = stage.clone();
            std::thread::spawn(move || stage.wait_for_viewers())
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "capture started with nobody watching");
        let rx = viewer(&stage, 1, CHANNEL);
        stage.audience_changed.notify_all();
        assert!(waiter.join().unwrap(), "the first viewer starts the capture");
        stage.remove(1);
        drop(rx);
        assert!(stage.no_viewers.load(Ordering::Acquire), "the last one leaving pauses it");

        let waiter = {
            let stage = stage.clone();
            std::thread::spawn(move || stage.wait_for_viewers())
        };
        stage.stop.stop();
        // As finish() does: through the lock, so a waiter between its check
        // and its wait still hears it.
        drop(stage.viewers.lock().unwrap());
        stage.audience_changed.notify_all();
        assert!(!waiter.join().unwrap(), "stopping the share ends the wait");
    }

    /// The last viewer leaving again while capture is paused (its own
    /// thread ending after a write failed) does not stop the next start.
    #[test]
    fn repeated_pause_is_forgotten() {
        let stage = test_stage();
        stage.no_viewers.store(true, Ordering::Release);
        let _rx = viewer(&stage, 1, CHANNEL);
        assert!(stage.wait_for_viewers());
        assert!(!stage.no_viewers.load(Ordering::Acquire), "the new capture would stop at once");
    }

    /// The computer's sound is captured only while someone watches: a
    /// source with 300 ms of sound still has all of it for the first viewer
    /// after half a second with nobody, and starts again for the next.
    #[test]
    fn sound_runs_only_while_someone_watches() {
        let _turn = crate::remote::heavy_test();
        gstreamer::init().unwrap();
        let stage = Arc::new(test_stage());
        let sound = {
            let stage = stage.clone();
            std::thread::spawn(move || {
                stage.stream_sound("audiotestsrc is-live=true num-buffers=30 samplesperbuffer=480 ! appsink name=asink sync=false");
            })
        };
        std::thread::sleep(Duration::from_millis(500));
        let heard = |id| {
            let rx = viewer(&stage, id, CHANNEL);
            stage.audience_changed.notify_all();
            let first = rx.recv_timeout(Duration::from_secs(3)).map(|(kind, _)| kind);
            stage.remove(id);
            first
        };
        assert_eq!(heard(1).ok(), Some(Kind::Audio), "the first viewer hears nothing");
        // Longer than the source's sound: only a capture started anew has
        // any left for the next viewer.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(heard(2).ok(), Some(Kind::Audio), "the sound did not start again for the next viewer");
        stage.stop.stop();
        drop(stage.viewers.lock().unwrap());
        stage.audience_changed.notify_all();
        sound.join().unwrap();
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
