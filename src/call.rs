//! One-to-one calls, voice or video, like Telegram's.
//!
//! - Ringing goes over the chat connection (`Op::Call`), straight to every
//!   device of the person that is online and understands calls; the tunnel
//!   and the device key say who sent each step, as for any message.
//! - The caller decides: the first device to accept gets the call, the
//!   others are told it was answered elsewhere. A decline from any device
//!   ends it.
//! - Once answered, both sides enter a room of two (`crate::room`): the same
//!   voice, cameras and screen sharing as a group's room, without the panel.
//! - Each side keeps how the call went in the conversation's history.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::chat::{self, CallOutcome, CallRecord};
use crate::invite::Participant;
use crate::tailscale;

/// Announced by versions that take calls.
pub const CAPABILITY: &str = "call1";
/// How long a call rings before it counts as unanswered.
const RING_FOR: Duration = Duration::from_secs(45);

/// A step of a call, as it travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Ring,
    Accept,
    Decline,
    /// In another call.
    Busy,
    /// The caller gave up before an answer.
    Cancel,
    /// Another device of the person took it.
    Answered,
    /// Hung up after talking.
    End,
}

/// What the window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallView {
    Idle,
    /// We're calling `user`.
    Outgoing { user: String, video: bool },
    /// `user` is calling us.
    Incoming { user: String, device: String, video: bool },
    /// Talking with `user` since `since` (ms since the epoch).
    Active { user: String, video: bool, since: u64 },
}

enum Phase {
    /// Devices still ringing (address and the node id).
    Outgoing { ringing: Vec<String> },
    Incoming { caller_ip: String, device: String },
    Active { peer_ip: String, since: Instant, since_ms: u64 },
}

struct Call {
    id: String,
    user: String,
    video: bool,
    /// We called (the history says who did).
    placed: bool,
    phase: Phase,
}

fn current() -> &'static Mutex<Option<Call>> {
    static CUR: OnceLock<Mutex<Option<Call>>> = OnceLock::new();
    CUR.get_or_init(Mutex::default)
}

fn lock() -> std::sync::MutexGuard<'static, Option<Call>> {
    current().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

static LISTENERS: crate::listeners::Listeners<CallView> = crate::listeners::Listeners::new();

/// Where the window (and the tray) hear about the call.
pub fn events() -> async_channel::Receiver<CallView> {
    LISTENERS.subscribe()
}

fn view(call: Option<&Call>) -> CallView {
    let Some(c) = call else { return CallView::Idle };
    let (user, video) = (c.user.clone(), c.video);
    match &c.phase {
        Phase::Outgoing { .. } => CallView::Outgoing { user, video },
        Phase::Incoming { device, .. } => CallView::Incoming { user, device: device.clone(), video },
        Phase::Active { since_ms, .. } => CallView::Active { user, video, since: *since_ms },
    }
}

fn publish(call: Option<&Call>) {
    LISTENERS.send(view(call));
}

/// The call right now.
pub fn state() -> CallView {
    view(lock().as_ref())
}

/// The room key both sides use for call `id`.
fn room_key(id: &str) -> String {
    format!("call-{id}")
}

/// Send a step of call `id` to `user`'s device at `ip` now: over the tailnet,
/// or over the pairing when `ip` is a paired device's address.
fn step(ip: &str, user: &str, id: &str, action: Action, video: bool) -> Result<(), String> {
    let op = chat::Op::Call { call: id.to_string(), action, video };
    #[cfg(target_os = "linux")]
    if let Some(key) = chat::lan_key(ip) {
        return chat::send_op_lan(key, op);
    }
    chat::send_op_now(ip, user, op)
}

/// Send a step in the background (nobody waits on it).
fn tell(ip: &str, user: &str, id: &str, action: Action, video: bool) {
    let (ip, user, id) = (ip.to_string(), user.to_string(), id.to_string());
    let _ = std::thread::Builder::new().name("biglace-call-tell".into()).spawn(move || {
        if let Err(e) = step(&ip, &user, &id, action, video) {
            tailscale::dbg(&format!("call: {action:?} to {ip}: {e}"));
        }
    });
}

/// `user`'s online devices that take calls; a paired device (its address) is its own.
fn devices_of(user: &str) -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Some(key) = chat::lan_key(user) {
        let takes = crate::lan::pairing::nearby_device(key).is_some_and(|d| d.calls);
        return if takes { vec![user.to_string()] } else { vec![] };
    }
    tailscale::get_peers()
        .into_iter()
        .filter(|p| p.online && p.user == user && !p.ipv4.is_empty())
        .filter(|p| crate::agent::client::supports(&p.ipv4, CAPABILITY))
        .map(|p| p.ipv4)
        .collect()
}

/// Call `user`, an account or a paired device's address. Blocking (finds and
/// rings their devices): off the UI thread.
pub fn start(user: &str, video: bool) -> Result<(), String> {
    if lock().is_some() {
        return Err(crate::tr!("You're already in a call."));
    }
    if crate::room::active_group().is_some() {
        return Err(crate::tr!("Leave the room you're in first."));
    }
    let id = chat::new_id();
    let mut ringing = Vec::new();
    for ip in devices_of(user) {
        match step(&ip, user, &id, Action::Ring, video) {
            Ok(()) => ringing.push(ip),
            Err(e) => tailscale::dbg(&format!("call: can't ring {ip}: {e}")),
        }
    }
    if ringing.is_empty() {
        return Err(crate::trf!("{user} can't take calls right now.", "user" => chat::label(user)));
    }
    {
        let mut cur = lock();
        if cur.is_some() {
            // Someone called us meanwhile: theirs wins.
            for ip in &ringing {
                tell(ip, user, &id, Action::Cancel, video);
            }
            return Err(crate::tr!("You're already in a call."));
        }
        *cur = Some(Call { id: id.clone(), user: user.to_string(), video, placed: true, phase: Phase::Outgoing { ringing } });
        publish(cur.as_ref());
    }
    give_up_after(&id);
    Ok(())
}

/// After [`RING_FOR`], a call still ringing stops.
fn give_up_after(id: &str) {
    let id = id.to_string();
    let _ = std::thread::Builder::new().name("biglace-call-ring".into()).spawn(move || {
        std::thread::sleep(RING_FOR);
        let cur = lock();
        let Some(c) = cur.as_ref().filter(|c| c.id == id) else { return };
        let outcome = match &c.phase {
            Phase::Outgoing { ringing } => {
                for ip in ringing {
                    tell(ip, &c.user, &c.id, Action::Cancel, c.video);
                }
                CallOutcome::Cancelled
            }
            Phase::Incoming { .. } => CallOutcome::Missed,
            Phase::Active { .. } => return,
        };
        drop(cur);
        finish(&id, outcome);
    });
}

/// Answer the call ringing here. Blocking: off the UI thread.
pub fn accept() -> Result<(), String> {
    let (id, user, video, caller_ip, device) = {
        let cur = lock();
        match cur.as_ref() {
            Some(Call { id, user, video, phase: Phase::Incoming { caller_ip, device }, .. }) => {
                (id.clone(), user.clone(), *video, caller_ip.clone(), device.clone())
            }
            _ => return Err(crate::tr!("The call has ended.")),
        }
    };
    if let Err(e) = step(&caller_ip, &user, &id, Action::Accept, video) {
        tailscale::dbg(&format!("call: accept to {caller_ip}: {e}"));
        finish(&id, CallOutcome::Missed);
        return Err(crate::tr!("The call has ended."));
    }
    if !activate(&id, &caller_ip) {
        return Err(crate::tr!("The call has ended."));
    }
    connect(&id, Participant { user, device, ip: caller_ip, muted: false }, video);
    Ok(())
}

/// Turn the call into a conversation with `peer_ip`. False if it ended meanwhile.
fn activate(id: &str, peer_ip: &str) -> bool {
    let mut cur = lock();
    let Some(c) = cur.as_mut().filter(|c| c.id == id) else { return false };
    c.phase = Phase::Active { peer_ip: peer_ip.to_string(), since: Instant::now(), since_ms: chat::now_ms() };
    publish(cur.as_ref());
    true
}

/// Enter the room of two, the camera on for a video call.
fn connect(id: &str, peer: Participant, video: bool) {
    let id = id.to_string();
    let _ = std::thread::Builder::new().name("biglace-call-room".into()).spawn(move || {
        if let Err(e) = crate::room::join_call(&room_key(&id), peer) {
            tailscale::dbg(&format!("call: room: {e}"));
            hang_up();
            return;
        }
        if video {
            if let Err(e) = crate::room::set_camera(true, None) {
                tailscale::dbg(&format!("call: camera: {e}"));
            }
        }
    });
}

/// Refuse the call ringing here.
pub fn decline() {
    let cur = lock();
    let Some(Call { id, user, video, phase: Phase::Incoming { caller_ip, .. }, .. }) = cur.as_ref() else { return };
    tell(caller_ip, user, id, Action::Decline, *video);
    let id = id.clone();
    drop(cur);
    finish(&id, CallOutcome::Declined);
}

/// Stop the call, whatever its phase (the window's red button, the room's
/// leave button during a call).
pub fn hang_up() {
    let cur = lock();
    let Some(c) = cur.as_ref() else { return };
    let id = c.id.clone();
    match &c.phase {
        Phase::Incoming { .. } => {
            drop(cur);
            decline();
        }
        Phase::Outgoing { ringing } => {
            for ip in ringing {
                tell(ip, &c.user, &c.id, Action::Cancel, c.video);
            }
            drop(cur);
            finish(&id, CallOutcome::Cancelled);
        }
        Phase::Active { peer_ip, .. } => {
            tell(peer_ip, &c.user, &c.id, Action::End, c.video);
            drop(cur);
            finish(&id, CallOutcome::Answered);
        }
    }
}

/// Leave the room we're in; in a call, that hangs up.
pub fn leave_room() {
    if matches!(state(), CallView::Active { .. }) {
        hang_up();
    } else {
        crate::room::leave();
    }
}

/// Call `id` is over: forget it, leave its room, keep it in the history.
fn finish(id: &str, outcome: CallOutcome) {
    let mut cur = lock();
    let Some(c) = cur.take_if(|c| c.id == id) else { return };
    publish(None);
    drop(cur);
    let seconds = match c.phase {
        Phase::Active { since, .. } => {
            let _ = std::thread::Builder::new().name("biglace-call-leave".into()).spawn(crate::room::leave);
            since.elapsed().as_secs()
        }
        _ => 0,
    };
    chat::record_call(&c.user, c.placed, CallRecord { video: c.video, outcome, seconds });
}

type Current = std::sync::MutexGuard<'static, Option<Call>>;

/// A call step from `sender`'s device `device` at `ip` (agent thread).
pub fn received(sender: &str, device: &str, ip: &str, id: &str, action: Action, video: bool) {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return;
    }
    let mut cur = lock();
    let ours = cur.as_ref().is_some_and(|c| c.id == id && c.user == sender);
    match action {
        Action::Ring if !ours => ring_here(cur, sender, device, ip, id, video),
        Action::Ring => {}
        Action::Accept => answered_there(cur, ours, sender, device, ip, id, video),
        Action::Decline | Action::Busy if ours => refused_there(cur, sender, ip, id, action),
        Action::Decline | Action::Busy => {}
        Action::Cancel if ours => {
            let answered = matches!(cur.as_ref().map(|c| &c.phase), Some(Phase::Active { .. }));
            drop(cur);
            finish(id, if answered { CallOutcome::Answered } else { CallOutcome::Missed });
        }
        Action::Cancel => {}
        Action::Answered => {
            if ours && matches!(cur.as_ref().map(|c| &c.phase), Some(Phase::Incoming { .. })) {
                *cur = None;
                publish(None);
            }
        }
        Action::End => {
            let from_peer = matches!(
                cur.as_ref().map(|c| &c.phase),
                Some(Phase::Active { peer_ip, .. }) if peer_ip == ip
            );
            if ours && from_peer {
                drop(cur);
                finish(id, CallOutcome::Answered);
            }
        }
    }
}

/// Someone calls: ring, or answer busy when we're in a call or a room.
fn ring_here(mut cur: Current, sender: &str, device: &str, ip: &str, id: &str, video: bool) {
    if cur.is_some() || crate::room::active_group().is_some() {
        drop(cur);
        tell(ip, sender, id, Action::Busy, video);
        return;
    }
    *cur = Some(Call {
        id: id.to_string(),
        user: sender.to_string(),
        video,
        placed: false,
        phase: Phase::Incoming { caller_ip: ip.to_string(), device: device.to_string() },
    });
    publish(cur.as_ref());
    drop(cur);
    give_up_after(id);
}

/// A device we rang took the call: the others stop, the talk starts.
fn answered_there(mut cur: Current, ours: bool, sender: &str, device: &str, ip: &str, id: &str, video: bool) {
    let rang_there = matches!(
        cur.as_ref().map(|c| &c.phase),
        Some(Phase::Outgoing { ringing }) if ringing.iter().any(|r| r == ip)
    );
    if !ours || !rang_there {
        // Too late: it ended, or another device took it.
        drop(cur);
        tell(ip, sender, id, Action::Cancel, video);
        return;
    }
    let Some(c) = cur.as_mut() else { return };
    if let Phase::Outgoing { ringing } = &c.phase {
        for other in ringing.iter().filter(|r| *r != ip) {
            tell(other, sender, id, Action::Answered, c.video);
        }
    }
    let video = c.video;
    drop(cur);
    if activate(id, ip) {
        connect(id, Participant { user: sender.into(), device: device.into(), ip: ip.into(), muted: false }, video);
    }
}

/// A device we rang declined, or is busy: a decline ends the call, busy
/// only once no other device still rings.
fn refused_there(mut cur: Current, sender: &str, ip: &str, id: &str, action: Action) {
    let Some(c) = cur.as_mut() else { return };
    let Phase::Outgoing { ringing } = &mut c.phase else { return };
    ringing.retain(|r| r != ip);
    if action == Action::Busy && !ringing.is_empty() {
        return;
    }
    for other in ringing.iter() {
        tell(other, sender, id, Action::Answered, c.video);
    }
    drop(cur);
    finish(id, if action == Action::Busy { CallOutcome::Busy } else { CallOutcome::Declined });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire names other versions read (Android's CallEngine too).
    #[test]
    fn actions_travel_by_name() {
        let names: Vec<String> = [
            Action::Ring,
            Action::Accept,
            Action::Decline,
            Action::Busy,
            Action::Cancel,
            Action::Answered,
            Action::End,
        ]
        .iter()
        .map(|a| serde_json::to_string(a).unwrap())
        .collect();
        assert_eq!(names, [r#""ring""#, r#""accept""#, r#""decline""#, r#""busy""#, r#""cancel""#, r#""answered""#, r#""end""#]);
    }
}
