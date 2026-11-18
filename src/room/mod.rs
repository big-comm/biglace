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
const LEVEL_STALE_US: u64 = 250_000;

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
}

/// What the window learns about the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// In `group` with these people (or `None`: not in a room).
    State {
        group: Option<String>,
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

/// A live connection to one peer.
struct Peer {
    writer: Arc<Mutex<TcpStream>>,
    stream: TcpStream,
    muted: Arc<AtomicBool>,
    sharing: Arc<AtomicBool>,
    last_voice_us: Arc<AtomicU64>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

struct Room {
    group: String,
    my_ip: String,
    muted: AtomicBool,
    stop: AtomicBool,
    roster: Mutex<Vec<Participant>>,
    peers: Mutex<HashMap<String, Peer>>,
    dialing: Mutex<HashSet<String>>,
    my_last_voice_us: AtomicU64,
    /// Loudness (0–1) and when it was measured, per address (ours too).
    levels: Mutex<HashMap<String, (f32, u64)>>,
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
        RoomEvent::State { group: Some(self.group.clone()), muted: me_muted, presenting, members }
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
                let path = format!("{PATH_PREFIX}{}", room.group);
                match crate::remote::client::dial(&ip, &path, &[], Duration::from_secs(8)) {
                    Ok((stream, reader)) => room.add_peer(&ip, stream, reader),
                    Err(e) => tailscale::dbg(&format!("room: can't reach {ip}: {e:?}")),
                }
                if let Ok(mut d) = room.dialing.lock() {
                    d.remove(&ip);
                }
            });
        }
    }

    /// A connection to `ip` is up (dialed or accepted): read its sound.
    fn add_peer(self: &Arc<Self>, ip: &str, stream: TcpStream, mut reader: BufReader<TcpStream>) {
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Ok(writer) = stream.try_clone() else { return };
        let writer = Arc::new(Mutex::new(writer));
        let muted = Arc::new(AtomicBool::new(false));
        let sharing = Arc::new(AtomicBool::new(false));
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
                    last_voice_us: last_voice.clone(),
                },
            );
        }
        emit(self.view());
        let room = self.clone();
        let ip = ip.to_string();
        let _ = std::thread::Builder::new().name("biglace-room-peer".into()).spawn(move || {
            let out = start_playback();
            let playing = Arc::new(AtomicBool::new(true));
            match &out {
                Ok((p, _)) => watch_levels(p, Arc::downgrade(&room), ip.clone(), playing.clone()),
                Err(e) => tailscale::dbg(&format!("room: can't play {ip}'s sound: {e}")),
            }
            loop {
                if room.stop.load(Ordering::Acquire) {
                    break;
                }
                match read_frame(&mut reader) {
                    Ok((Kind::Audio, payload)) if payload.len() > 8 => {
                        let packet = &payload[8..];
                        if packet.len() > VOICE_PACKET {
                            last_voice.store(now_us(), Ordering::Relaxed);
                        }
                        if let Ok((_, src)) = &out {
                            let _ = src.push_buffer(gstreamer::Buffer::from_slice(packet.to_vec()));
                        }
                    }
                    Ok((Kind::RoomState, payload)) => {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                            muted.store(v.get("muted").and_then(|m| m.as_bool()).unwrap_or(false), Ordering::Relaxed);
                            sharing.store(v.get("sharing").and_then(|m| m.as_bool()).unwrap_or(false), Ordering::Relaxed);
                            emit(room.view());
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            playing.store(false, Ordering::Release);
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

    /// Send one encoded microphone packet to everyone.
    fn broadcast(&self, packet: &[u8]) {
        if self.muted.load(Ordering::Relaxed) {
            return;
        }
        if packet.len() > VOICE_PACKET {
            self.my_last_voice_us.store(now_us(), Ordering::Relaxed);
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
        serde_json::json!({ "muted": self.muted.load(Ordering::Relaxed), "sharing": stage::presenting() }).to_string()
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

/// Keep `room.levels[key]` current from `p`'s "level" element, while
/// `alive` and the room last.
fn watch_levels(p: &gstreamer::Pipeline, room: std::sync::Weak<Room>, key: String, alive: Arc<AtomicBool>) {
    use gstreamer::prelude::*;
    let Some(bus) = p.bus() else { return };
    let _ = std::thread::Builder::new().name("biglace-room-level".into()).spawn(move || {
        while alive.load(Ordering::Acquire) {
            let Some(msg) = bus.timed_pop_filtered(
                gstreamer::ClockTime::from_mseconds(200),
                &[gstreamer::MessageType::Element],
            ) else {
                if room.upgrade().is_none_or(|r| r.stop.load(Ordering::Acquire)) {
                    break;
                }
                continue;
            };
            let Some(level) = msg.structure().filter(|s| s.name() == "level").and_then(loudness) else { continue };
            let Some(room) = room.upgrade() else { break };
            if room.stop.load(Ordering::Acquire) {
                break;
            }
            if let Ok(mut l) = room.levels.lock() {
                l.insert(key.clone(), (level, now_us()));
            };
        }
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
    let p = gst::parse::launch(PEER_OUT_DESC)?
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
    let room = Arc::new(Room {
        group: group.to_string(),
        my_ip,
        muted: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        roster: Mutex::new(roster),
        peers: Mutex::new(HashMap::new()),
        dialing: Mutex::new(HashSet::new()),
        my_last_voice_us: AtomicU64::new(0),
        levels: Mutex::new(HashMap::new()),
    });
    let mic = start_mic(&room, mic_desc);
    match &mic {
        Ok(p) => watch_levels(p, Arc::downgrade(&room), room.my_ip.clone(), Arc::new(AtomicBool::new(true))),
        Err(e) => emit(RoomEvent::Problem(format!("microphone: {e}"))),
    }
    if let Ok(mut cur) = current().lock() {
        *cur = Some(room.clone());
    }
    room.reconcile();
    emit(room.view());

    // Presence + reconnects, and a light ticker for "who's talking".
    let _ = std::thread::Builder::new().name("biglace-room".into()).spawn(move || {
        let mic = mic.ok();
        let mut last_beat = std::time::Instant::now();
        let mut last_view = room.view();
        while !room.stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(200));
            if last_beat.elapsed() >= HEARTBEAT {
                last_beat = std::time::Instant::now();
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
            let view = room.view();
            if view != last_view {
                emit(view.clone());
                last_view = view;
            }
        }
        if let Some(p) = mic {
            use gstreamer::prelude::*;
            let _ = p.set_state(gstreamer::State::Null);
        }
    });
    Ok(())
}

/// Leave the room we're in, if any. Blocking (tells the panel).
pub fn leave() {
    stage::stop();
    let Some(room) = current().lock().ok().and_then(|mut c| c.take()) else {
        return;
    };
    room.stop.store(true, Ordering::Release);
    if let Ok(mut peers) = room.peers.lock() {
        peers.clear();
    }
    let _ = invite::room_leave(&room.group);
    emit(RoomEvent::State { group: None, muted: false, presenting: false, members: vec![] });
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
    let listed = |r: &Room| r.roster.lock().map(|l| l.iter().any(|p| p.ip == ip)).unwrap_or(false);
    if !listed(&room) {
        if let Ok(r) = invite::room_join(&room.group, room.muted.load(Ordering::Relaxed)) {
            if let Ok(mut roster) = room.roster.lock() {
                *roster = r;
            }
        }
        if !listed(&room) {
            return respond(&stream, 403, "Forbidden");
        }
    }
    let _ = write!(&stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-room\r\n\r\n");
    let _ = stream.set_read_timeout(None);
    let Ok(reader) = stream.try_clone() else { return };
    room.add_peer(&ip, stream, BufReader::new(reader));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_side_dials() {
        assert!(dials("100.64.0.3", "100.64.0.17") ^ dials("100.64.0.17", "100.64.0.3"));
        assert!(!dials("100.64.0.3", "100.64.0.3"));
    }

    /// The microphone chain reports how loud it is: a tone reads loud,
    /// silence reads zero (what the talking animation follows).
    #[test]
    fn microphone_level_follows_the_sound() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let _turn = crate::remote::heavy_test();
        if gst::init().is_err() || gst::ElementFactory::find("level").is_none() {
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
