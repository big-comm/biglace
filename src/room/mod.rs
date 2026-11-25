//! Voice rooms: talk with the people of a group, like a Telegram or Jitsi
//! call, with no server in the middle of the sound.
//!
//! - Presence lives on the BigScale panel (who is in which group's room),
//!   refreshed every few seconds with signed requests.
//! - The sound goes straight between participants over the tailnet: everyone
//!   in a group reaches everyone else in it. Each pair keeps one TCP
//!   connection on the BigLace agent port (the lower address dials), signed
//!   with the device key and checked like a remote-screen request.
//! - The microphone is captured once and encoded as voice Opus (20 ms, DTX:
//!   nothing is sent while you're silent, which also tells the others when
//!   you're talking), then sent to every peer. Each peer's sound plays on its
//!   own stream; PipeWire mixes them. With bigsound installed, the default
//!   microphone already has noise reduction and echo cancellation.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::invite::{self, Participant};
use crate::remote::{now_us, read_frame, write_frame, Kind};
use crate::tailscale::{self, Identity};

pub mod camera;
pub mod share_audio;
pub mod shortcut;
pub mod stage;

/// Agent path to watch a screen shared with a room: `/v1/stage/<group>`.
pub const STAGE_PREFIX: &str = "/v1/stage/";

/// Agent path of a room connection: `/v1/room/<group>`.
pub const PATH_PREFIX: &str = "/v1/room/";
/// Presence refresh (the panel forgets a participant after 30 s).
const HEARTBEAT: Duration = Duration::from_secs(5);
/// "Talking" lasts this long after the last voice packet.
const TALKING_HOLD_US: u64 = 400_000;
/// DTX packets during silence are a few bytes; voice is more.
const VOICE_PACKET: usize = 12;
/// A peer that stops reading for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Microphone → voice Opus (mono 48 kHz, 20 ms frames, DTX, in-band FEC).
pub const MIC_DESC: &str = "pulsesrc name=mic buffer-time=40000 latency-time=10000 \
     ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 \
     ! level name=level interval=50000000 post-messages=true \
     ! opusenc bitrate=32000 frame-size=20 audio-type=voice dtx=true inband-fec=true packet-loss-percentage=5 \
     ! appsink name=asink sync=false max-buffers=50 drop=true";
/// One peer's voice → the speakers (played as it arrives).
pub const PEER_OUT_DESC: &str = "appsrc name=asrc is-live=true format=time do-timestamp=true \
       caps=audio/x-opus,channel-mapping-family=0,channels=1,rate=48000 \
     ! opusdec plc=true ! level name=level interval=50000000 post-messages=true \
     ! audioconvert ! audioresample ! autoaudiosink sync=false";
/// A sound level older than this is silence (DTX: nothing arrives then).
pub const LEVEL_STALE_US: u64 = 250_000;
/// A level from here up shows the person as speaking.
const SPEAKING: f32 = 0.06;

/// Whether `level` (0–1, as [`level`] gives it) shows the person as speaking.
pub fn speaking(level: f32) -> bool {
    level >= SPEAKING
}

/// One participant, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub user: String,
    pub device: String,
    /// Their tailnet address — what [`level`] takes.
    pub ip: String,
    pub me: bool,
    pub muted: bool,
    /// Sound path to them is up (always true for yourself).
    pub connected: bool,
    pub talking: bool,
    /// Showing their screen to the room.
    pub sharing: bool,
    /// Watching *our* screen (we're presenting), and the viewer's id.
    pub watching: Option<u64>,
    /// Controls our screen right now.
    pub controlling: bool,
    /// Their camera is on.
    pub camera: bool,
}

/// What the window learns about the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// In `group` with these people (or `None`: not in a room).
    State {
        group: Option<String>,
        /// A call with this person rather than a group's room.
        call_with: Option<String>,
        muted: bool,
        /// We're showing our screen.
        presenting: bool,
        members: Vec<MemberView>,
    },
    /// Someone watching our screen asks to control it.
    ControlRequest { viewer: u64, user: String },
    /// Something went wrong (shown once, the room may continue).
    Problem(String),
}

type Channel = (async_channel::Sender<RoomEvent>, async_channel::Receiver<RoomEvent>);

fn channel() -> &'static Channel {
    static CH: OnceLock<Channel> = OnceLock::new();
    CH.get_or_init(async_channel::unbounded)
}

/// Where the window listens for room changes.
pub fn events() -> async_channel::Receiver<RoomEvent> {
    channel().1.clone()
}

fn emit(e: RoomEvent) {
    let _ = channel().0.try_send(e);
}

fn room_view_listeners() -> &'static Mutex<Vec<async_channel::Sender<()>>> {
    static LISTENERS: OnceLock<Mutex<Vec<async_channel::Sender<()>>>> = OnceLock::new();
    LISTENERS.get_or_init(Mutex::default)
}

/// Wakes its listener when a camera picture arrives or a voice level is
/// worth showing, for the room's stage and member list; many changes before
/// it looks are one wakeup. Every listener hears each change.
pub fn room_view_wakeups() -> async_channel::Receiver<()> {
    let (tx, rx) = async_channel::bounded(1);
    room_view_listeners().lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(tx);
    rx
}

pub fn room_view_changed() {
    room_view_listeners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|tx| !matches!(tx.try_send(()), Err(async_channel::TrySendError::Closed(()))));
}

/// A live connection to one peer.
struct Peer {
    writer: Arc<Mutex<TcpStream>>,
    stream: TcpStream,
    muted: Arc<AtomicBool>,
    sharing: Arc<AtomicBool>,
    camera: Arc<AtomicBool>,
    last_voice_us: Arc<AtomicU64>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

struct Room {
    /// The group, or the call's key (`call-<id>`).
    group: String,
    /// A call: its other person. The roster is the two of us, fixed; the
    /// panel isn't involved.
    call_with: Option<String>,
    my_ip: String,
    muted: AtomicBool,
    stop: AtomicBool,
    roster: Mutex<Vec<Participant>>,
    peers: Mutex<HashMap<String, Peer>>,
    dialing: Mutex<HashSet<String>>,
    my_last_voice_us: AtomicU64,
    /// Loudness (0–1) and when it was measured, per address (ours too).
    levels: Mutex<HashMap<String, (f32, u64)>>,
    /// Our camera, while on.
    camera: Mutex<Option<camera::Capture>>,
    /// Wakes the room thread (`keep_current`): someone started talking, or
    /// the room was left.
    wake: std::sync::mpsc::SyncSender<()>,
}

fn current() -> &'static Mutex<Option<Arc<Room>>> {
    static CUR: OnceLock<Mutex<Option<Arc<Room>>>> = OnceLock::new();
    CUR.get_or_init(|| Mutex::new(None))
}

fn room() -> Option<Arc<Room>> {
    current().lock().ok()?.clone()
}

/// The group whose room we're in.
pub fn active_group() -> Option<String> {
    room().map(|r| r.group.clone())
}

impl Room {
    fn view(&self) -> RoomEvent {
        let now = now_us();
        let talking = |last: u64| last != 0 && now.saturating_sub(last) < TALKING_HOLD_US;
        let peers = self.peers.lock().map(|p| {
            p.iter()
                .map(|(ip, peer)| {
                    (
                        ip.clone(),
                        (
                            peer.muted.load(Ordering::Relaxed),
                            peer.last_voice_us.load(Ordering::Relaxed),
                            peer.sharing.load(Ordering::Relaxed),
                            peer.camera.load(Ordering::Relaxed),
                        ),
                    )
                })
                .collect::<HashMap<_, _>>()
        });
        let peers = peers.unwrap_or_default();
        let me_muted = self.muted.load(Ordering::Relaxed);
        let presenting = stage::presenting();
        let audience = stage::audience();
        let members = self
            .roster
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let me = p.ip == self.my_ip;
                let live = peers.get(&p.ip);
                let watching = audience.get(&p.ip);
                MemberView {
                    sharing: if me { presenting } else { live.is_some_and(|l| l.2) },
                    watching: watching.map(|w| w.0),
                    controlling: watching.is_some_and(|w| w.1),
                    camera: if me { self.camera_on() } else { live.is_some_and(|l| l.3) },
                    me,
                    muted: if me { me_muted } else { live.map_or(p.muted, |l| l.0) },
                    connected: me || live.is_some(),
                    talking: if me {
                        !me_muted && talking(self.my_last_voice_us.load(Ordering::Relaxed))
                    } else {
                        live.is_some_and(|l| talking(l.1))
                    },
                    user: p.user,
                    device: p.device,
                    ip: p.ip,
                }
            })
            .collect();
        RoomEvent::State {
            group: Some(self.group.clone()),
            call_with: self.call_with.clone(),
            muted: me_muted,
            presenting,
            members,
        }
    }

    /// Connect to the peers we're meant to dial; drop the ones who left.
    fn reconcile(self: &Arc<Self>) {
        let roster = self.roster.lock().map(|r| r.clone()).unwrap_or_default();
        let wanted: HashSet<String> = roster.iter().filter(|p| p.ip != self.my_ip).map(|p| p.ip.clone()).collect();
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|ip, _| wanted.contains(ip));
        }
        for ip in wanted {
            // One connection per pair: the lower address dials.
            let connected = self.peers.lock().map(|p| p.contains_key(&ip)).unwrap_or(true);
            if connected || !dials(&self.my_ip, &ip) {
                continue;
            }
            if !self.dialing.lock().map(|mut d| d.insert(ip.clone())).unwrap_or(false) {
                continue;
            }
            let room = self.clone();
            let _ = std::thread::Builder::new().name("biglace-room-dial".into()).spawn(move || {
                match room.dial(&ip) {
                    Ok((stream, reader)) => room.add_peer(&ip, stream, reader),
                    Err(e) => tailscale::dbg(&format!("room: can't reach {ip}: {e}")),
                }
                if let Ok(mut d) = room.dialing.lock() {
                    d.remove(&ip);
                }
            });
        }
    }

    /// Connect to the peer at `ip`: over the tailnet, or over the pairing for
    /// a paired device on the local network.
    fn dial(&self, ip: &str) -> Result<(TcpStream, BufReader<TcpStream>), String> {
        #[cfg(target_os = "linux")]
        if let Some(key) = crate::chat::lan_key(ip) {
            let ask = crate::lan::pairing::RoomAsk { room: self.group.clone() };
            let stream = crate::lan::pairing::take_over(key, &ask).map_err(|_| "not reachable on the local network".to_string())?;
            let reader = stream.try_clone().map_err(|e| e.to_string())?;
            return Ok((stream, BufReader::new(reader)));
        }
        let path = format!("{PATH_PREFIX}{}", self.group);
        crate::remote::client::dial(ip, &path, &[], Duration::from_secs(8)).map_err(|e| format!("{e:?}"))
    }

    /// A connection to `ip` is up (dialed or accepted): read its sound.
    fn add_peer(self: &Arc<Self>, ip: &str, stream: TcpStream, mut reader: BufReader<TcpStream>) {
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Ok(writer) = stream.try_clone() else { return };
        let writer = Arc::new(Mutex::new(writer));
        let muted = Arc::new(AtomicBool::new(false));
        let sharing = Arc::new(AtomicBool::new(false));
        let cam = Arc::new(AtomicBool::new(false));
        let last_voice = Arc::new(AtomicU64::new(0));
        // Tell them our state right away.
        if let Ok(mut w) = writer.lock() {
            let _ = write_frame(&mut *w, Kind::RoomState, self.state_json().as_bytes());
        }
        let Ok(stream_for_peer) = stream.try_clone() else { return };
        if let Ok(mut peers) = self.peers.lock() {
            peers.insert(
                ip.to_string(),
                Peer {
                    writer,
                    stream: stream_for_peer,
                    muted: muted.clone(),
                    sharing: sharing.clone(),
                    camera: cam.clone(),
                    last_voice_us: last_voice.clone(),
                },
            );
        }
        emit(self.view());
        let room = self.clone();
        let ip = ip.to_string();
        let _ = std::thread::Builder::new().name("biglace-room-peer".into()).spawn(move || {
            let out = start_playback();
            match &out {
                Ok((p, _)) => watch_levels(p, Arc::downgrade(&room), ip.clone()),
                Err(e) => tailscale::dbg(&format!("room: can't play {ip}'s sound: {e}")),
            }
            // Their camera, decoded while it's on.
            let mut decoder: Option<camera::Decoder> = None;
            loop {
                if room.stop.load(Ordering::Acquire) {
                    break;
                }
                match read_frame(&mut reader) {
                    Ok((Kind::CamVideo, payload)) => {
                        if decoder.is_none() {
                            tailscale::dbg(&format!("room: {ip}'s camera: first picture ({} bytes)", payload.len()));
                            decoder = camera::Decoder::new(&ip)
                                .map_err(|e| tailscale::dbg(&format!("room: can't show {ip}'s camera: {e}")))
                                .ok();
                        }
                        if let Some(d) = &decoder {
                            d.push(&payload);
                        }
                    }
                    Ok((Kind::Audio, payload)) if payload.len() > 8 => {
                        let packet = &payload[8..];
                        if packet.len() > VOICE_PACKET {
                            room.heard_voice(&last_voice);
                        }
                        if let Ok((_, src)) = &out {
                            let _ = src.push_buffer(gstreamer::Buffer::from_slice(packet.to_vec()));
                        }
                    }
                    Ok((Kind::RoomState, payload)) => {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                            muted.store(v.get("muted").and_then(|m| m.as_bool()).unwrap_or(false), Ordering::Relaxed);
                            sharing.store(v.get("sharing").and_then(|m| m.as_bool()).unwrap_or(false), Ordering::Relaxed);
                            let on = v.get("camera").and_then(|m| m.as_bool()).unwrap_or(false);
                            if !cam.swap(on, Ordering::Relaxed) == on && !on {
                                decoder = None;
                                camera::forget(&ip);
                            }
                            cam.store(on, Ordering::Relaxed);
                            emit(room.view());
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if let Ok((p, _)) = out {
                use gstreamer::prelude::*;
                let _ = p.set_state(gstreamer::State::Null);
            }
            // Gone: forget it (the next heartbeat redials if still listed).
            if let Ok(mut peers) = room.peers.lock() {
                if peers.get(&ip).is_some_and(|p| Arc::ptr_eq(&p.last_voice_us, &last_voice)) {
                    peers.remove(&ip);
                }
            }
            emit(room.view());
        });
    }

    /// Records a voice packet in `last` (ours or a peer's), waking the room
    /// thread when it starts someone talking.
    fn heard_voice(&self, last: &AtomicU64) {
        let now = now_us();
        if now.saturating_sub(last.swap(now, Ordering::Relaxed)) >= TALKING_HOLD_US {
            let _ = self.wake.try_send(());
        }
    }

    /// How long until the first person talking now stops showing as talking.
    fn talking_ends_in(&self) -> Option<Duration> {
        let now = now_us();
        let mut lasts = vec![self.my_last_voice_us.load(Ordering::Relaxed)];
        if let Ok(peers) = self.peers.lock() {
            lasts.extend(peers.values().map(|p| p.last_voice_us.load(Ordering::Relaxed)));
        }
        lasts
            .into_iter()
            .filter_map(|last| (last + TALKING_HOLD_US).checked_sub(now).filter(|&left| left > 0))
            .min()
            .map(Duration::from_micros)
    }

    /// Whether `ip` may connect: on the roster, or (a group room only) on
    /// the panel's fresh one. A call's roster is fixed.
    fn admits(&self, ip: &str) -> bool {
        let listed = || self.roster.lock().map(|l| l.iter().any(|p| p.ip == ip)).unwrap_or(false);
        if listed() {
            return true;
        }
        if self.call_with.is_some() {
            return false;
        }
        if let Ok(r) = invite::room_join(&self.group, self.muted.load(Ordering::Relaxed)) {
            if let Ok(mut roster) = self.roster.lock() {
                *roster = r;
            }
        }
        listed()
    }

    fn camera_on(&self) -> bool {
        self.camera.lock().is_ok_and(|c| c.is_some())
    }

    /// Send one camera picture to everyone.
    fn broadcast_camera(&self, payload: &[u8]) {
        let writers: Vec<Arc<Mutex<TcpStream>>> = self
            .peers
            .lock()
            .map(|p| p.values().map(|peer| peer.writer.clone()).collect())
            .unwrap_or_default();
        for w in writers {
            // A slow connection skips pictures rather than delaying voices.
            if let Ok(mut w) = w.try_lock() {
                let _ = write_frame(&mut *w, Kind::CamVideo, payload);
            }
        }
    }

    /// Send one encoded microphone packet to everyone.
    fn broadcast(&self, packet: &[u8]) {
        if self.muted.load(Ordering::Relaxed) {
            return;
        }
        if packet.len() > VOICE_PACKET {
            self.heard_voice(&self.my_last_voice_us);
        }
        let mut frame = now_us().to_be_bytes().to_vec();
        frame.extend_from_slice(packet);
        let writers: Vec<Arc<Mutex<TcpStream>>> = self
            .peers
            .lock()
            .map(|p| p.values().map(|peer| peer.writer.clone()).collect())
            .unwrap_or_default();
        for w in writers {
            if let Ok(mut w) = w.lock() {
                let _ = write_frame(&mut *w, Kind::Audio, &frame);
            }
        }
    }

    /// What the others need to know about us.
    fn state_json(&self) -> String {
        serde_json::json!({
            "muted": self.muted.load(Ordering::Relaxed),
            "sharing": stage::presenting(),
            "camera": self.camera_on(),
        })
        .to_string()
    }

    fn send_state(&self) {
        let state = self.state_json();
        let writers: Vec<Arc<Mutex<TcpStream>>> = self
            .peers
            .lock()
            .map(|p| p.values().map(|peer| peer.writer.clone()).collect())
            .unwrap_or_default();
        for w in writers {
            if let Ok(mut w) = w.lock() {
                let _ = write_frame(&mut *w, Kind::RoomState, state.as_bytes());
            }
        }
    }
}

/// "level" element message → loudness 0–1 (the loudest channel's RMS,
/// −50 dB and below is silence, −10 dB and above is full).
pub(crate) fn loudness(s: &gstreamer::StructureRef) -> Option<f32> {
    let rms = s.get::<gstreamer::glib::ValueArray>("rms").ok()?;
    let db = rms.iter().filter_map(|v| v.get::<f64>().ok()).fold(f64::NEG_INFINITY, f64::max);
    if !db.is_finite() {
        return Some(0.0);
    }
    Some((((db + 50.0) / 40.0).clamp(0.0, 1.0)) as f32)
}

/// Keep `room.levels[key]` current from `p`'s "level" element while the
/// room lasts. Runs on the streaming thread that posts each level, and drops
/// every message of `p`'s bus, which nothing else reads.
fn watch_levels(p: &gstreamer::Pipeline, room: std::sync::Weak<Room>, key: String) {
    use gstreamer::prelude::*;
    let Some(bus) = p.bus() else { return };
    bus.set_sync_handler(move |_, msg| {
        let level = msg.structure().filter(|s| s.name() == "level").and_then(loudness);
        if let (Some(level), Some(room)) = (level, room.upgrade().filter(|r| !r.stop.load(Ordering::Acquire))) {
            let before = room.levels.lock().ok().and_then(|mut l| l.insert(key.clone(), (level, now_us())));
            // Silence going on stays quiet; speaking, or its end, is shown.
            if speaking(level) || before.is_some_and(|(v, _)| speaking(v)) {
                room_view_changed();
            }
        }
        gstreamer::BusSyncReply::Drop
    });
}

/// How loud `ip` (a participant, us included) sounds right now, 0–1. Cheap:
/// the window asks every frame while it animates. 0 when muted or silent.
pub fn level(ip: &str) -> f32 {
    #[cfg(debug_assertions)]
    if preview() {
        return preview_level(ip);
    }
    let Some(room) = room() else { return 0.0 };
    if ip == room.my_ip && room.muted.load(Ordering::Relaxed) {
        return 0.0;
    }
    let now = now_us();
    room.levels
        .lock()
        .ok()
        .and_then(|l| l.get(ip).copied())
        .filter(|(_, at)| now.saturating_sub(*at) < LEVEL_STALE_US)
        .map_or(0.0, |(v, _)| v)
}

/// Development builds only: BIGLACE_PREVIEW=room shows a sample room whose
/// people "talk" in turns, to look at the animation without a real call.
#[cfg(debug_assertions)]
pub fn preview() -> bool {
    std::env::var("BIGLACE_PREVIEW").as_deref() == Ok("room")
}

#[cfg(debug_assertions)]
fn preview_level(ip: &str) -> f32 {
    let t = now_us() as f64 / 1_000_000.0;
    let (period, offset) = match ip {
        "100.64.0.101" => (4.0, 0.0),
        "100.64.0.102" => (4.0, 2.0),
        "100.64.0.103" => (6.0, 1.0),
        _ => return 0.0,
    };
    let on = ((t + offset) % period) < period / 2.2;
    if !on {
        return 0.0;
    }
    // Syllable-like bursts.
    let s = (t * 7.3 + offset).sin().abs() * 0.6 + (t * 13.1).sin().abs() * 0.4;
    (s * 0.9) as f32
}

/// The lower address dials (string order of the dotted IPv4 is fine: both
/// sides compute the same answer).
fn dials(me: &str, them: &str) -> bool {
    me < them
}

fn start_playback() -> anyhow::Result<(gstreamer::Pipeline, gstreamer_app::AppSrc)> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    gst::init()?;
    // While the computer's sound is shared, the room's voices go straight
    // to the speakers (not into what's shared).
    let desc = match share_audio::real_sink() {
        Some(sink) => PEER_OUT_DESC.replace("autoaudiosink sync=false", &format!("pulsesink device={sink} sync=false")),
        None => PEER_OUT_DESC.to_string(),
    };
    let p = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let src = p
        .by_name("asrc")
        .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsrc"))?;
    p.set_state(gst::State::Playing)?;
    Ok((p, src))
}

/// Capture and encode the microphone, feeding `room` until it stops.
fn start_mic(room: &Arc<Room>, desc: &str) -> anyhow::Result<gstreamer::Pipeline> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    gst::init()?;
    let p = gst::parse::launch(desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = p
        .by_name("asink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    let weak = Arc::downgrade(room);
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let Some(room) = weak.upgrade() else { return Err(gst::FlowError::Eos) };
                if let Some(b) = sample.buffer() {
                    if let Ok(map) = b.map_readable() {
                        room.broadcast(map.as_slice());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    p.set_state(gst::State::Playing)?;
    Ok(p)
}

/// Enter `group`'s room (leaving any other). Blocking: call off the UI
/// thread. Uses the default microphone (`mic_desc` for tests).
pub fn join(group: &str) -> Result<(), String> {
    join_with(group, MIC_DESC)
}

pub fn join_with(group: &str, mic_desc: &str) -> Result<(), String> {
    leave();
    let my_ip = tailscale::get_status().ip.ok_or("not connected")?;
    let roster = invite::room_join(group, false).map_err(|e| format!("{e:?}"))?;
    enter(group, None, my_ip, roster, mic_desc);
    Ok(())
}

/// Start the sound of call `key` with `peer` (who answered, or whom we
/// answered). Blocking: call off the UI thread.
pub fn join_call(key: &str, mut peer: Participant) -> Result<(), String> {
    leave();
    let status = tailscale::get_status();
    let device = status.hostname.unwrap_or_else(|| gtk4::glib::host_name().to_string());
    // A paired device on the local network: both sides go by their pairing address.
    let my_ip = match crate::chat::lan_key(&peer.ip) {
        Some(_) => crate::chat::lan_address(&crate::device_key::public_key().ok_or("no device key")?),
        None => status.ip.ok_or("not connected")?,
    };
    // The call goes by who it's with: a paired device's address, or the account.
    let with = if crate::chat::lan_key(&peer.ip).is_some() {
        peer.user = crate::chat::label(&peer.ip);
        peer.ip.clone()
    } else {
        peer.user.clone()
    };
    let me = Participant {
        user: crate::chat::my_user().unwrap_or_else(|| device.clone()),
        device,
        ip: my_ip.clone(),
        muted: false,
    };
    enter(key, Some(with), my_ip, vec![me, peer], MIC_DESC);
    Ok(())
}

fn enter(group: &str, call_with: Option<String>, my_ip: String, roster: Vec<Participant>, mic_desc: &str) {
    let (wake, woken) = std::sync::mpsc::sync_channel(1);
    let room = Arc::new(Room {
        group: group.to_string(),
        call_with,
        my_ip,
        muted: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        roster: Mutex::new(roster),
        peers: Mutex::new(HashMap::new()),
        dialing: Mutex::new(HashSet::new()),
        my_last_voice_us: AtomicU64::new(0),
        levels: Mutex::new(HashMap::new()),
        camera: Mutex::new(None),
        wake,
    });
    let mic = start_mic(&room, mic_desc);
    match &mic {
        Ok(p) => watch_levels(p, Arc::downgrade(&room), room.my_ip.clone()),
        Err(e) => emit(RoomEvent::Problem(format!("microphone: {e}"))),
    }
    if let Ok(mut cur) = current().lock() {
        *cur = Some(room.clone());
    }
    room.reconcile();
    emit(room.view());

    let _ = std::thread::Builder::new().name("biglace-room".into()).spawn(move || {
        let mic = mic.ok();
        keep_current(&room, &woken);
        if let Some(p) = mic {
            use gstreamer::prelude::*;
            let _ = p.set_state(gstreamer::State::Null);
        }
    });
}

/// Until the room is left: presence and reconnects every heartbeat, and the
/// panel told when someone starts or stops talking. Sleeps in between.
fn keep_current(room: &Arc<Room>, woken: &std::sync::mpsc::Receiver<()>) {
    let mut next_beat = std::time::Instant::now() + HEARTBEAT;
    let mut last_view = room.view();
    while !room.stop.load(Ordering::Acquire) {
        let to_beat = next_beat.saturating_duration_since(std::time::Instant::now());
        let _ = woken.recv_timeout(room.talking_ends_in().map_or(to_beat, |ends| ends.min(to_beat)));
        if room.stop.load(Ordering::Acquire) {
            break;
        }
        if std::time::Instant::now() >= next_beat {
            next_beat = std::time::Instant::now() + HEARTBEAT;
            if room.call_with.is_some() {
                room.reconcile();
            } else {
                match invite::room_join(&room.group, room.muted.load(Ordering::Relaxed)) {
                    Ok(r) => {
                        if let Ok(mut roster) = room.roster.lock() {
                            *roster = r;
                        }
                        room.reconcile();
                    }
                    Err(e) => tailscale::dbg(&format!("room: presence refresh failed: {e:?}")),
                }
            }
        }
        let view = room.view();
        if view != last_view {
            emit(view.clone());
            last_view = view;
        }
    }
}

/// Leave the room we're in, if any. Blocking (tells the panel).
pub fn leave() {
    stage::stop();
    let Some(room) = current().lock().ok().and_then(|mut c| c.take()) else {
        return;
    };
    if let Ok(mut c) = room.camera.lock() {
        c.take();
    }
    camera::forget(&room.my_ip);
    room.stop.store(true, Ordering::Release);
    let _ = room.wake.try_send(());
    if let Ok(mut peers) = room.peers.lock() {
        peers.clear();
    }
    if room.call_with.is_none() {
        let _ = invite::room_leave(&room.group);
    }
    emit(RoomEvent::State { group: None, call_with: None, muted: false, presenting: false, members: vec![] });
}

/// Turn our camera on (`path`: which one; None: the first) or off.
pub fn set_camera(on: bool, path: Option<String>) -> Result<(), String> {
    let room = room().ok_or_else(|| crate::tr!("you are not in a room"))?;
    if on {
        let weak = Arc::downgrade(&room);
        let capture = camera::start(path.as_deref(), &room.my_ip, move |payload| {
            if let Some(r) = weak.upgrade() {
                r.broadcast_camera(payload);
            }
        })?;
        if let Ok(mut c) = room.camera.lock() {
            *c = Some(capture);
        }
    } else {
        if let Ok(mut c) = room.camera.lock() {
            c.take();
        }
        camera::forget(&room.my_ip);
    }
    room.send_state();
    emit(room.view());
    Ok(())
}

/// Our tailnet address in the room (whose picture is our camera's).
pub fn my_ip() -> Option<String> {
    room().map(|r| r.my_ip.clone())
}

/// Mute if speaking, unmute if muted (tray, global shortcut).
pub fn toggle_mute() {
    if let Some(r) = room() {
        set_muted(!r.muted.load(Ordering::Relaxed));
    }
}

/// Mute or unmute our microphone for everyone.
pub fn set_muted(muted: bool) {
    if let Some(room) = room() {
        room.muted.store(muted, Ordering::Relaxed);
        room.send_state();
        emit(room.view());
    }
}

fn respond(mut stream: &TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// A peer dials us for `/v1/room/<group>` (agent thread).
pub fn accept(stream: TcpStream, peer: IpAddr, headers: &HashMap<String, String>, identity: &Identity, path: &str) {
    let Some(group) = path.strip_prefix(PATH_PREFIX) else {
        return respond(&stream, 404, "Not Found");
    };
    let Some(room) = room().filter(|r| r.group == group) else {
        return respond(&stream, 409, "Conflict");
    };
    let local = stream
        .local_addr()
        .map(|a| format!("{}:{}", a.ip(), a.port()))
        .unwrap_or_default();
    if let Err(why) = crate::remote::host::verify_caller(headers, peer, &local, path) {
        tailscale::dbg(&format!("room: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let ip = peer.to_string();
    // Must be in the same room per the panel (refresh once if it's new).
    if !room.admits(&ip) {
        return respond(&stream, 403, "Forbidden");
    }
    let _ = write!(&stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-room\r\n\r\n");
    let _ = stream.set_read_timeout(None);
    let Ok(reader) = stream.try_clone() else { return };
    room.add_peer(&ip, stream, BufReader::new(reader));
}

/// A paired device on the local network in the call `group` with us connects
/// (see `crate::lan`): the call's roster names its pairing address.
#[cfg(target_os = "linux")]
pub fn accept_lan(tls: openssl::ssl::SslStream<TcpStream>, key: &str, group: &str) {
    use crate::lan::pairing::answer;
    let ip = crate::chat::lan_address(key);
    let Some(room) = room().filter(|r| r.group == group && r.admits(&ip)) else {
        answer(tls, Some("unavailable"));
        return;
    };
    if let Some(stream) = answer(tls, None) {
        let Ok(reader) = stream.try_clone() else { return };
        room.add_peer(&ip, stream, BufReader::new(reader));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call takes only the two devices that set it up, without asking the panel.
    #[test]
    fn call_admits_only_its_two_devices() {
        let (wake, _woken) = std::sync::mpsc::sync_channel(1);
        let person = |user: &str, ip: &str| Participant { user: user.into(), device: user.into(), ip: ip.into(), muted: false };
        let room = Room {
            call_with: Some("bruna".into()),
            group: "call-ab12".into(),
            my_ip: "100.64.0.1".into(),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            roster: Mutex::new(vec![person("tales", "100.64.0.1"), person("bruna", "100.64.0.2")]),
            peers: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            my_last_voice_us: AtomicU64::new(0),
            levels: Mutex::new(HashMap::new()),
            camera: Mutex::new(None),
            wake,
        };
        assert!(room.admits("100.64.0.2"));
        assert!(!room.admits("100.64.0.3"), "another device of the network");
        assert!(!room.admits(""));
    }

    #[test]
    fn exactly_one_side_dials() {
        assert!(dials("100.64.0.3", "100.64.0.17") ^ dials("100.64.0.17", "100.64.0.3"));
        assert!(!dials("100.64.0.3", "100.64.0.3"));
    }

    /// The stage and the member list both redraw on the same changes.
    #[test]
    fn every_listener_hears_a_change() {
        let (stage, members) = (room_view_wakeups(), room_view_wakeups());
        room_view_changed();
        assert!(stage.try_recv().is_ok(), "the stage missed it");
        assert!(members.try_recv().is_ok(), "the member list missed it");
    }

    /// The microphone chain reports how loud it is: a tone reads loud,
    /// silence reads zero (what the talking animation follows).
    #[test]
    fn microphone_level_follows_the_sound() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let _turn = crate::remote::heavy_test();
        if gst::init().is_err() || gst::ElementFactory::find("level").is_none() {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("no level element here; skipping");
            return;
        }
        let measure = |wave: &str| -> f32 {
            let desc = MIC_DESC.replace(
                "pulsesrc name=mic buffer-time=40000 latency-time=10000",
                &format!("audiotestsrc num-buffers=25 samplesperbuffer=960 volume=0.3 wave={wave}"),
            );
            let p = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
            p.set_state(gst::State::Playing).unwrap();
            let bus = p.bus().unwrap();
            let mut loudest: f32 = 0.0;
            let mut seen = 0;
            while let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Element, gst::MessageType::Eos],
            ) {
                if msg.type_() == gst::MessageType::Eos {
                    break;
                }
                if let Some(v) = msg.structure().filter(|s| s.name() == "level").and_then(loudness) {
                    loudest = loudest.max(v);
                    seen += 1;
                }
            }
            p.set_state(gst::State::Null).unwrap();
            assert!(seen > 5, "only {seen} level messages");
            loudest
        };
        let tone = measure("sine");
        let quiet = measure("silence");
        assert!(tone > 0.6, "tone reads {tone}");
        assert!(quiet < 0.05, "silence reads {quiet}");
    }

    /// A voice's level reaches the room and wakes the stage straight from
    /// the pipeline: no thread waits on the bus for it.
    #[test]
    fn levels_arrive_without_a_polling_thread() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let _turn = crate::remote::heavy_test();
        if gst::init().is_err() || gst::ElementFactory::find("level").is_none() {
            // allow-noisy-log: a test run must say this test was skipped, not passed.
            eprintln!("no level element here; skipping");
            return;
        }
        let room = Arc::new(Room {
            call_with: None,
            group: "familia".into(),
            my_ip: "100.64.0.1".into(),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            roster: Mutex::default(),
            peers: Mutex::default(),
            dialing: Mutex::default(),
            my_last_voice_us: AtomicU64::new(0),
            levels: Mutex::default(),
            camera: Mutex::default(),
            wake: std::sync::mpsc::sync_channel(1).0,
        });
        let changes = room_view_wakeups();
        let p = gst::parse::launch(
            "audiotestsrc is-live=true samplesperbuffer=960 volume=0.3 \
             ! level name=level interval=50000000 post-messages=true ! fakesink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        watch_levels(&p, Arc::downgrade(&room), "100.64.0.9".into());
        p.set_state(gst::State::Playing).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let level = loop {
            let level = room.levels.lock().unwrap().get("100.64.0.9").map(|(v, _)| *v);
            if level.is_some() || std::time::Instant::now() > deadline {
                break level;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let threads: Vec<String> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
            .collect();
        p.set_state(gst::State::Null).unwrap();
        assert!(level.is_some_and(speaking), "level {level:?}");
        assert!(changes.try_recv().is_ok(), "the stage was not told");
        assert!(!threads.iter().any(|t| t.starts_with("biglace-room-le")), "a thread waits on the bus");
    }

    /// The room thread sleeps while nothing changes, and tells the panel
    /// when we start talking and again when we stop.
    #[cfg(target_os = "linux")]
    #[test]
    fn room_thread_sleeps_until_someone_talks() {
        let switches = |tid: &str| -> u64 {
            std::fs::read_to_string(format!("/proc/self/task/{tid}/status"))
                .unwrap()
                .lines()
                .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|n| n.trim().parse().ok())
                .unwrap()
        };
        let (wake, woken) = std::sync::mpsc::sync_channel(1);
        let room = Arc::new(Room {
            call_with: None,
            group: "talk-test".into(),
            my_ip: "100.64.0.1".into(),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            roster: Mutex::new(vec![Participant { user: "ana".into(), device: "pc".into(), ip: "100.64.0.1".into(), muted: false }]),
            peers: Mutex::default(),
            dialing: Mutex::default(),
            my_last_voice_us: AtomicU64::new(0),
            levels: Mutex::default(),
            camera: Mutex::default(),
            wake,
        });
        let events = events();
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let worker = {
            let room = room.clone();
            std::thread::spawn(move || {
                let _ = tid_tx.send(std::fs::read_link("/proc/thread-self").unwrap().file_name().unwrap().to_owned());
                keep_current(&room, &woken);
            })
        };
        let tid = tid_rx.recv().unwrap().to_string_lossy().into_owned();
        std::thread::sleep(Duration::from_millis(100));
        let start = switches(&tid);
        std::thread::sleep(Duration::from_millis(500));
        let idle = switches(&tid) - start;

        room.heard_voice(&room.my_last_voice_us);
        std::thread::sleep(Duration::from_micros(TALKING_HOLD_US) + Duration::from_millis(200));
        room.stop.store(true, Ordering::Release);
        let _ = room.wake.try_send(());
        worker.join().unwrap();

        let talking: Vec<bool> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|e| match e {
                RoomEvent::State { group: Some(group), members, .. } if group == "talk-test" => Some(members[0].talking),
                _ => None,
            })
            .collect();
        assert!(idle <= 1, "wakeups in 500 ms idle: {idle}");
        assert_eq!(talking, [true, false]);
    }

    /// The room's sound chain works: microphone pipeline (a test tone here)
    /// produces voice Opus, mostly-silent input gets DTX'd down to tiny
    /// packets, and the playback chain decodes them.
    #[test]
    fn voice_chain_encodes_and_decodes() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let _turn = crate::remote::heavy_test();
        if gst::init().is_err() || gst::ElementFactory::find("opusenc").is_none() {
            return;
        }
        let encode = |wave: &str| -> Vec<Vec<u8>> {
            let desc = MIC_DESC.replace(
                "pulsesrc name=mic buffer-time=40000 latency-time=10000",
                &format!("audiotestsrc num-buffers=50 samplesperbuffer=960 wave={wave}"),
            );
            let p = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
            let sink = p.by_name("asink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
            p.set_state(gst::State::Playing).unwrap();
            let mut out = Vec::new();
            while let Some(s) = sink.try_pull_sample(gst::ClockTime::from_seconds(3)) {
                out.push(s.buffer().unwrap().map_readable().unwrap().as_slice().to_vec());
            }
            p.set_state(gst::State::Null).unwrap();
            out
        };
        let voice = encode("sine");
        let silence = encode("silence");
        assert!(voice.len() >= 20, "{} packets", voice.len());
        assert!(voice.iter().filter(|p| p.len() > VOICE_PACKET).count() > voice.len() / 2);
        assert!(
            silence.iter().filter(|p| p.len() > VOICE_PACKET).count() < silence.len() / 4,
            "silence isn't DTX'd: {:?}",
            silence.iter().map(Vec::len).collect::<Vec<_>>()
        );
        // Decode what was encoded.
        let desc = PEER_OUT_DESC.replace("autoaudiosink sync=false", "appsink name=out sync=false");
        let p = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
        let src = p.by_name("asrc").unwrap().downcast::<gstreamer_app::AppSrc>().unwrap();
        let out = p.by_name("out").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        p.set_state(gst::State::Playing).unwrap();
        for pkt in &voice {
            src.push_buffer(gst::Buffer::from_slice(pkt.clone())).unwrap();
        }
        let decoded = out.try_pull_sample(gst::ClockTime::from_seconds(3));
        p.set_state(gst::State::Null).unwrap();
        assert!(decoded.is_some(), "playback chain decoded nothing");
    }
}
