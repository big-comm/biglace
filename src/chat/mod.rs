//! Chat: messages between people of the network, one to one or in a group,
//! sent straight between their computers — the server never sees them.
//!
//! - Delivery: the sender's BigLace opens a signed connection to each
//!   recipient device's agent (`/v1/chat`, checked like a voice-room
//!   connection) and sends the messages; the receiver answers with the ids it
//!   stored. Whatever isn't acknowledged waits in a local outbox and is
//!   retried while that device is reachable, so messages to someone offline
//!   arrive when they're back. A copy goes to our own other devices too.
//! - Trust: the sender is who the tunnel and the device key say, never what
//!   the message claims; a group message is kept only if both of us are in
//!   that group.
//! - History lives on each computer (one JSON file per conversation, 0600).

pub mod files;
mod history;
pub mod markdown;
pub mod voice;

pub use files::{Attachment, FileKind, FileState, Offer};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::remote::{read_frame, write_frame, Kind};
use crate::tailscale::{self, Identity};

/// Agent path of a chat delivery connection.
pub const PATH: &str = "/v1/chat";
/// Longest message accepted (bytes of text).
pub const MAX_TEXT: usize = 16 * 1024;
/// Incoming messages accepted per sender and minute.
const RATE_PER_MINUTE: usize = 120;
/// How often the outbox is retried (sooner when kicked).
const RETRY: Duration = Duration::from_secs(15);

/// Which conversation: `dm` with a person, or a `group`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConvId {
    pub kind: String,
    pub name: String,
}

impl ConvId {
    pub fn direct(user: &str) -> Self {
        Self { kind: "dm".into(), name: user.into() }
    }
    pub fn group(name: &str) -> Self {
        Self { kind: "group".into(), name: name.into() }
    }
    pub fn is_group(&self) -> bool {
        self.kind == "group"
    }
    /// With a device paired on the local network, named by its device key (see `crate::lan`).
    pub fn lan(key: &str) -> Self {
        Self { kind: "lan".into(), name: key.into() }
    }
    pub fn is_lan(&self) -> bool {
        self.kind == "lan"
    }
    /// File-safe key (`dm-bruna`, `group-familia`).
    pub fn key(&self) -> String {
        format!("{}-{}", self.kind, self.name)
    }
    /// [`ConvId::key`] read back; `None` for anything else.
    pub fn from_key(key: &str) -> Option<Self> {
        let (kind, name) = key.split_once('-')?;
        let conv = Self { kind: kind.into(), name: name.into() };
        conv.valid().then_some(conv)
    }
    fn valid(&self) -> bool {
        matches!(self.kind.as_str(), "dm" | "group" | "lan") && name_ok(&self.name)
    }
}

/// Account and group names as the panel spells them.
pub fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// One message, as kept in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// Account that wrote it.
    pub from: String,
    /// Their device.
    #[serde(default)]
    pub device: String,
    pub text: String,
    /// When it was written (ms since the epoch, sender's clock).
    pub at: u64,
    /// Written by us (on this or another of our devices).
    pub mine: bool,
    /// Recipients (accounts) whose device stored it.
    #[serde(default)]
    pub delivered: Vec<String>,
    /// A file, picture, video or voice message (direct conversations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Attachment>,
    /// The message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyRef>,
    /// Forwarded: who wrote it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from: Option<String>,
    /// Emoji → who reacted with it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reactions: BTreeMap<String, Vec<String>>,
    /// When it was last edited (ms), 0 if never.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub edited_at: u64,
    /// Deleted by its author: only "deleted" remains.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// A call with the person (kept on this device only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<CallRecord>,
}

/// How a call went, as the history shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub video: bool,
    pub outcome: CallOutcome,
    /// Talking time, for an answered call.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallOutcome {
    Answered,
    /// Rang here and nobody answered.
    Missed,
    Declined,
    Busy,
    /// Called and hung up (or gave up) before an answer.
    Cancelled,
    #[serde(other)]
    Unknown,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a reply shows of the message it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyRef {
    pub id: String,
    pub from: String,
    /// A line of it (plain text).
    pub preview: String,
}

impl ReplyRef {
    fn valid(&self) -> bool {
        !self.id.is_empty() && self.id.len() <= 64 && name_ok(&self.from) && self.preview.len() <= 400
    }
}

/// Something done to a message (or a conversation) instead of writing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Op {
    /// Add (or take back) a reaction.
    React { target: String, emoji: String, on: bool },
    /// The author changed the text.
    Edit { target: String, text: String },
    /// The author deleted it for everyone.
    Delete { target: String },
    /// Pin a message to the top of the conversation (`None`: unpin).
    Pin { target: Option<String> },
    /// Typing right now (not kept).
    Typing,
    /// A step of a call (not kept): see `crate::call`.
    Call {
        call: String,
        action: crate::call::Action,
        #[serde(default)]
        video: bool,
    },
    /// A step of sending files to a device (not kept): see `crate::transfer`.
    Send {
        send: String,
        action: crate::transfer::Action,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<crate::transfer::Item>,
    },
    /// Read on another of our own devices, up to the message written at
    /// `at` (milliseconds); only between our devices, not kept.
    Read { at: u64 },
}

/// One emoji (or a short sequence of them) as a reaction.
fn emoji_ok(e: &str) -> bool {
    !e.is_empty() && e.len() <= 32 && !e.chars().any(|c| c.is_whitespace() || c.is_control() || c.is_ascii_alphanumeric())
}

/// What travels between devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Wire {
    id: String,
    /// The sender's view: `dm` names the recipient.
    conv: ConvId,
    text: String,
    at: u64,
    /// A file offered for download from the sender's computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<Offer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_to: Option<ReplyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forwarded_from: Option<String>,
    /// Not a message but something done to one; `text` then is what older
    /// versions show instead ("reacted with ❤️"…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    op: Option<Op>,
    /// Passed on by one of the recipient's own devices: who wrote it, from
    /// which device. Taken only from a device of our own account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relayed: Option<Relayed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Relayed {
    from: String,
    device: String,
}

/// A message waiting to reach one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    wire: Wire,
    /// Account behind the device (ours for the copies to our devices).
    to_user: String,
    /// The device: tailnet node id (stable even if its address changes). Empty while
    /// none of `to_user`'s devices is known yet: delivery places it once one shows up.
    node: String,
}

/// A conversation, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: ConvId,
    pub last: Option<Message>,
    pub unread: u32,
}

/// What the window learns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// Messages of this conversation changed (new, delivered…).
    Changed(ConvId),
    /// Someone else's message arrived (for notifications).
    Incoming { conv: ConvId, message: Box<Message> },
    /// A download moved on: `done` of `total` bytes.
    Progress { conv: ConvId, id: String, done: u64, total: u64 },
    /// A download stopped short (the sender's computer is off…).
    TransferFailed { conv: ConvId, id: String, reason: String },
    /// Someone is typing in this conversation.
    Typing { conv: ConvId, user: String },
    /// Read on another of our devices: its notification goes.
    ReadElsewhere(ConvId),
    /// The local network went on or off: other conversations show (see [`shown`]).
    ShownChanged,
    /// Removed from our list (see [`remove_conversation`]).
    Removed(ConvId),
}

/// The history and the outbox, on disk under `dir`.
struct Store {
    dir: PathBuf,
    history: history::History,
    outbox: Vec<Pending>,
}

impl Store {
    fn open(dir: &Path) -> Self {
        let history = history::History::open(dir).unwrap_or_else(|e| {
            // allow-noisy-log: without its history file the chat still works this session, and says why.
            eprintln!("[biglace] chat history: can't open {}: {e}; this session's messages aren't kept", dir.display());
            history::History::in_memory()
        });
        let outbox = std::fs::read(dir.join("outbox"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { dir: dir.to_path_buf(), history, outbox }
    }

    /// Messages that wait for a person none of whose devices was known go to each device of theirs in `peers`.
    fn place_unplaced(&mut self, peers: &[tailscale::Peer]) {
        let mut placed = Vec::new();
        self.outbox.retain(|p| {
            if !p.node.is_empty() {
                return true;
            }
            let theirs: Vec<_> = peers.iter().filter(|d| d.user == p.to_user && !d.id.is_empty()).collect();
            placed.extend(theirs.iter().map(|d| Pending { node: d.id.clone(), ..p.clone() }));
            theirs.is_empty()
        });
        if !placed.is_empty() {
            self.outbox.extend(placed);
            self.save_outbox();
        }
    }

    /// `items` were for a device missing from `peers`: the ones for a person with no other device
    /// in sight wait for them again, the rest are dropped (their other devices have their own).
    fn unplace(&mut self, items: &[Pending], peers: &[tailscale::Peer]) {
        for p in items {
            self.outbox.retain(|x| !(x.node == p.node && x.wire.id == p.wire.id));
            let waiting = self.outbox.iter().any(|x| x.node.is_empty() && x.wire.id == p.wire.id && x.to_user == p.to_user);
            if !waiting && !peers.iter().any(|d| d.user == p.to_user) {
                self.outbox.push(Pending { node: String::new(), ..p.clone() });
            }
        }
        self.save_outbox();
    }

    /// Conversation `old` is called `new` now: its history merges into any
    /// `new` already holds, its outbox follows. False when there was no `old`.
    fn rename_conv(&mut self, old: &ConvId, new: &ConvId) -> bool {
        if !self.history.rename(old, new) {
            return false;
        }
        let mut outbox_changed = false;
        for p in self.outbox.iter_mut().filter(|p| p.wire.conv == *old) {
            p.wire.conv = new.clone();
            outbox_changed = true;
        }
        if outbox_changed {
            self.save_outbox();
        }
        true
    }

    fn save_outbox(&self) {
        if let Ok(json) = serde_json::to_vec(&self.outbox) {
            write_private(&self.dir.join("outbox"), &json);
        }
    }

    /// Add `m` to `id` unless it's already there. True if added.
    fn add(&mut self, id: &ConvId, m: Message, count_unread: bool) -> bool {
        self.history.add(id, &m, count_unread)
    }

    fn summaries(&self) -> Vec<Summary> {
        let mut out: Vec<Summary> = self
            .history
            .conversations()
            .into_iter()
            .map(|(id, unread)| Summary { last: self.history.last(&id), id, unread })
            .collect();
        out.sort_by(|a, b| b.last.as_ref().map(|m| m.at).cmp(&a.last.as_ref().map(|m| m.at)).then(a.id.cmp(&b.id)));
        out
    }

    /// Read up to `at` on another of our devices: what came later stays
    /// unread. True when that changed the count.
    fn read_until(&mut self, conv: &ConvId, at: u64) -> bool {
        let before = self.history.unread(conv);
        let unread = self.history.received_after(conv, at).min(before);
        if unread == before {
            return false;
        }
        self.history.set_unread(conv, unread);
        true
    }

    /// `sender` did `op` in `conv`. True when something changed.
    fn apply_op(&mut self, conv: &ConvId, sender: &str, op: &Op) -> bool {
        match op {
            Op::React { target, emoji, on } => {
                if !emoji_ok(emoji) {
                    return false;
                }
                self.update_message(conv, target, |m| {
                    if m.deleted {
                        return false;
                    }
                    let who = m.reactions.entry(emoji.clone()).or_default();
                    let had = who.iter().any(|u| u == sender);
                    if *on && !had {
                        who.push(sender.to_string());
                    } else if !*on && had {
                        who.retain(|u| u != sender);
                    }
                    m.reactions.retain(|_, users| !users.is_empty());
                    *on != had
                })
            }
            Op::Edit { target, text } => self.update_message(conv, target, |m| {
                if m.from != sender || m.deleted || text.trim().is_empty() || text.len() > MAX_TEXT || m.text == *text {
                    return false;
                }
                m.text = text.clone();
                m.edited_at = now_ms();
                true
            }),
            Op::Delete { target } => {
                let deleted = self.update_message(conv, target, |m| {
                    if m.from != sender || m.deleted {
                        return false;
                    }
                    m.deleted = true;
                    m.text.clear();
                    m.file = None;
                    m.reply_to = None;
                    m.reactions.clear();
                    true
                });
                if deleted && self.history.pinned(conv).as_deref() == Some(target.as_str()) {
                    self.history.set_pinned(conv, None);
                }
                deleted
            }
            Op::Pin { target } => {
                if target.as_ref().is_some_and(|t| self.history.message(conv, t).is_none()) || self.history.pinned(conv) == *target {
                    return false;
                }
                self.history.set_pinned(conv, target.as_deref());
                true
            }
            Op::Typing | Op::Call { .. } | Op::Send { .. } | Op::Read { .. } => false,
        }
    }

    /// Change message `id` of `conv` with `change`, which says whether it did; kept when it did.
    fn update_message(&mut self, conv: &ConvId, id: &str, change: impl FnOnce(&mut Message) -> bool) -> bool {
        let Some(mut m) = self.history.message(conv, id) else { return false };
        if !change(&mut m) {
            return false;
        }
        self.history.put(conv, &m);
        true
    }

    /// `user`'s device stored message `id` of `conv` (key of the sender's view).
    fn delivered(&mut self, conv: &ConvId, id: &str, user: &str) -> bool {
        self.update_message(conv, id, |m| {
            if m.delivered.iter().any(|u| u == user) {
                return false;
            }
            m.delivered.push(user.to_string());
            true
        })
    }
}

fn write_private(path: &Path, data: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data));
    if ok.is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ─── Who we are and our groups (from the panel, refreshed now and then) ─────

#[derive(Default, Clone)]
struct Me {
    user: String,
    /// Group → its members.
    groups: HashMap<String, Vec<String>>,
    /// Group → who created it (BigLace groups only).
    owners: HashMap<String, String>,
    fetched: Option<Instant>,
}

fn me_slot() -> &'static Mutex<Me> {
    static ME: OnceLock<Mutex<Me>> = OnceLock::new();
    ME.get_or_init(|| Mutex::new(Me::default()))
}

/// Our account and groups, asking the panel when unknown or older than
/// `max_age`. Blocking.
fn me(max_age: Duration) -> Option<Me> {
    let cur = me_slot().lock().ok()?.clone();
    if !cur.user.is_empty() && cur.fetched.is_some_and(|t| t.elapsed() < max_age) {
        return Some(cur);
    }
    match crate::invite::my_groups() {
        Ok(mine) => {
            follow_renames(&mine.groups);
            follow_people(&mine.renamed);
            let owners = mine
                .groups
                .iter()
                .filter(|g| g.is_member_group())
                .filter_map(|g| g.owner.clone().map(|o| (g.name.clone(), o)))
                .collect();
            let fresh = Me {
                user: mine.user,
                groups: mine.groups.into_iter().map(|g| (g.name, g.members)).collect(),
                owners,
                fetched: Some(Instant::now()),
            };
            if let Ok(mut slot) = me_slot().lock() {
                *slot = fresh.clone();
            }
            Some(fresh)
        }
        Err(_) => (!cur.user.is_empty()).then_some(cur),
    }
}

/// Groups renamed by their owner: their conversation and photo follow the new name.
fn follow_renames(groups: &[crate::invite::GroupInfo]) {
    for g in groups {
        for old in &g.former {
            group_renamed(old, &g.name);
        }
    }
}

/// Group `from` is called `to` now: its conversation and photo move over.
pub fn group_renamed(from: &str, to: &str) {
    let (old, new) = (ConvId::group(from), ConvId::group(to));
    let moved = store().lock().is_ok_and(|mut s| s.rename_conv(&old, &new));
    crate::avatars::group_renamed(from, to);
    if moved {
        emit(ChatEvent::Changed(new));
    }
}

/// People who took another account name: their conversation, photo and
/// access rule follow.
fn follow_people(renamed: &[crate::invite::Renamed]) {
    for (from, to) in crate::invite::current_names(renamed) {
        let (old, new) = (ConvId::direct(&from), ConvId::direct(&to));
        if store().lock().is_ok_and(|mut s| s.rename_conv(&old, &new)) {
            emit(ChatEvent::Changed(new));
        }
        crate::access::person_renamed(&from, &to);
    }
}

/// Ask the panel again (if our groups are older than 30 s). Blocking.
/// Returns our account name.
pub fn my_groups_refresh() -> Option<String> {
    me(Duration::from_secs(30)).map(|m| m.user)
}

/// Ask the panel for our account and groups now (after we renamed
/// ourselves). Blocking.
pub fn my_groups_reload() -> Option<String> {
    me(Duration::ZERO).map(|m| m.user)
}

/// Our account name, if known.
pub fn my_user() -> Option<String> {
    me_slot().lock().ok().map(|m| m.user.clone()).filter(|u| !u.is_empty())
}

/// Who created group `name` (as last learned): only they set its photo.
pub fn group_owner(name: &str) -> Option<String> {
    me_slot().lock().ok().and_then(|m| m.owners.get(name).cloned())
}

/// The members of group `name` (as last learned), if we're in it.
pub fn group_members(name: &str) -> Vec<String> {
    me_slot().lock().ok().and_then(|m| m.groups.get(name).cloned()).unwrap_or_default()
}

/// Groups we're in (names), if known.
pub fn my_groups() -> Vec<String> {
    let mut g: Vec<String> = me_slot().lock().map(|m| m.groups.keys().cloned().collect()).unwrap_or_default();
    g.sort();
    g
}

// ─── The shared store and the window's channel ──────────────────────────────

fn chat_dir() -> PathBuf {
    // Development builds: a scratch history (BIGLACE_CHAT_DIR), so previews
    // never touch the real one.
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("BIGLACE_CHAT_DIR") {
        return PathBuf::from(dir);
    }
    gtk4::glib::user_data_dir().join("biglace").join("chat")
}

/// Where pasted pictures and recorded voice messages are kept (they're
/// served from here when sent).
pub fn media_dir() -> PathBuf {
    let dir = chat_dir().join("media");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Development builds only (BIGLACE_PREVIEW=chat with BIGLACE_CHAT_DIR): a
/// sample history to look at the chat without a second person.
#[cfg(debug_assertions)]
pub fn seed_preview() -> Option<ConvId> {
    if std::env::var("BIGLACE_PREVIEW").as_deref() != Ok("chat") || std::env::var("BIGLACE_CHAT_DIR").is_err() {
        return None;
    }
    let now = now_ms();
    let day = 24 * 60 * 60 * 1000;
    let m = |id: &str, from: &str, mine: bool, text: &str, at: u64, delivered: &[&str]| Message {
        id: id.into(),
        from: from.into(),
        device: "notebook".into(),
        text: text.into(),
        at,
        mine,
        delivered: delivered.iter().map(|s| s.to_string()).collect(),
        file: None,
        reply_to: None,
        forwarded_from: None,
        reactions: BTreeMap::new(),
        edited_at: 0,
        deleted: false,
        call: None,
    };
    let dm = ConvId::direct("bruna");
    let grp = ConvId::group("familia");
    let mut s = store().lock().ok()?;
    for msg in [
        m("p1", "bruna", false, "Oi! Conseguiu entrar na rede? 😊", now - day - 3_600_000, &[]),
        m("p2", "tales", true, "Consegui sim! Agora dá pra **ver a tela** do seu notebook quando precisar.", now - day - 3_500_000, &["bruna"]),
        m("p3", "bruna", false, "Que ótimo 🎉", now - 600_000, &[]),
        m("p4", "bruna", false, "Me manda o link daquele tutorial?\n\n- instalar o *BigLace*\n- abrir o convite\n- digitar o nome", now - 590_000, &[]),
        m("p5", "tales", true, "Claro: [biglinux.com.br](https://www.biglinux.com.br) — e o comando é `sudo pacman -Syu`", now - 300_000, &["bruna"]),
        m("p6", "tales", true, "👍", now - 120_000, &[]),
        m("p7", "tales", true, "Vou te ligar na **sala** daqui a pouco", now - 30_000, &[]),
    ] {
        s.add(&dm, msg, false);
    }
    // Files: a picture and a voice message here, a document and a video
    // still on Bruna's computer.
    {
        use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
        let dir = chat_dir().join("media");
        let _ = std::fs::create_dir_all(&dir);
        let pic = dir.join("praia.png");
        if let Some(pb) = Pixbuf::new(Colorspace::Rgb, false, 8, 640, 420) {
            for y in 0..420 {
                for x in 0..640 {
                    let sky = y < 260;
                    let (r, g, b) = if sky { (40 + y / 3, 120 + y / 4, 220) } else { (230 - (y - 260) / 3, 200 - (y - 260) / 4, 140) };
                    pb.put_pixel(x as u32, y as u32, r as u8, g as u8, b as u8, 255);
                }
            }
            let _ = pb.savev(&pic, "png", &[]);
        }
        let voice_path = dir.join("voice-preview.ogg");
        if !voice_path.exists() {
            use gstreamer::prelude::*;
            let _ = gstreamer::init();
            if let Ok(p) = gstreamer::parse::launch(&format!(
                "audiotestsrc num-buffers=300 freq=330 ! audioconvert ! opusenc ! oggmux ! filesink location={}",
                voice_path.display()
            )) {
                let _ = p.set_state(gstreamer::State::Playing);
                if let Some(bus) = p.bus() {
                    let _ = bus.timed_pop_filtered(gstreamer::ClockTime::from_seconds(5), &[gstreamer::MessageType::Eos]);
                }
                let _ = p.set_state(gstreamer::State::Null);
            }
        }
        let thumb = files::image_thumb(&pic);
        let offer = |name: &str, size: u64, kind: FileKind| Offer {
            name: name.into(),
            size,
            kind,
            thumb: None,
            width: 0,
            height: 0,
            duration_ms: 0,
            wave: vec![],
            sha256: None,
        };
        let mut photo = offer("praia.png", std::fs::metadata(&pic).map(|m| m.len()).unwrap_or(0), FileKind::Image);
        if let Some((jpeg, w, h)) = &thumb {
            use base64::Engine;
            photo.thumb = Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
            photo.width = *w;
            photo.height = *h;
        }
        let mut video = photo.clone();
        video.name = "aniversario.mp4".into();
        video.kind = FileKind::Video;
        video.size = 48_300_000;
        let mut voice_offer = offer("voice.ogg", std::fs::metadata(&voice_path).map(|m| m.len()).unwrap_or(0), FileKind::Voice);
        voice_offer.duration_ms = 6_000;
        voice_offer.wave = (0..48).map(|i: u32| ((i * 37 + 11) % 29 + 2) as u8).collect();
        let with = |mut msg: Message, o: Offer, path: Option<&Path>| {
            msg.file = Some(Attachment {
                offer: o,
                path: path.map(|p| p.to_string_lossy().into_owned()),
                state: if path.is_some() { FileState::Done } else { FileState::Offered },
            });
            msg
        };
        for msg in [
            with(m("f1", "bruna", false, "", now - 25_000, &[]), photo, Some(&pic)),
            with(m("f2", "tales", true, "", now - 20_000, &["bruna"]), voice_offer, Some(&voice_path)),
            with(m("f3", "bruna", false, "", now - 15_000, &[]), offer("relatorio-2026.pdf", 2_412_000, FileKind::File), None),
            with(m("f4", "bruna", false, "", now - 10_000, &[]), video, None),
        ] {
            s.add(&dm, msg, false);
        }
    }
    // Telegram-like extras: a reply, reactions, an edit, a deletion, a
    // forward, and a pinned message.
    {
        let mut reply = m("x1", "bruna", false, "Achei! Vou te mandar agora", now - 8_000, &[]);
        reply.reply_to = Some(ReplyRef { id: "p4".into(), from: "bruna".into(), preview: "Me manda o link daquele tutorial?".into() });
        let mut edited = m("x2", "tales", true, "Combinado, **às 15h** então", now - 7_000, &["bruna"]);
        edited.edited_at = now - 6_000;
        edited.reactions.insert("👍".into(), vec!["bruna".into()]);
        edited.reactions.insert("❤️".into(), vec!["bruna".into(), "tales".into()]);
        let mut gone = m("x3", "bruna", false, "", now - 5_000, &[]);
        gone.deleted = true;
        let mut fwd = m("x4", "tales", true, "Atualização do BigLinux sai amanhã 🎉", now - 4_000, &["bruna"]);
        fwd.forwarded_from = Some("bruno".into());
        for msg in [reply, edited, gone, fwd] {
            s.add(&dm, msg, false);
        }
        s.history.set_pinned(&dm, Some("x2"));
    }
    for msg in [
        m("g1", "bruno", false, "Bom dia, família! ☀️", now - 7_200_000, &[]),
        m("g2", "maria", false, "Bom dia!! Alguém vai no almoço de domingo?", now - 7_100_000, &[]),
        m("g3", "tales", true, "Eu vou! ~~Levo a sobremesa~~ levo o refri 😅", now - 7_000_000, &["bruno", "maria"]),
        m("g4", "bruno", false, "```\nalmoço: domingo 12h\nlocal: casa da vó\n```", now - 60_000, &[]),
        m("g5", "maria", false, "@tales traz o violão também 🎸", now - 30_000, &[]),
    ] {
        s.add(&grp, msg, true);
    }
    drop(s);
    // Who's in the group (the panel would say), for "@" in the preview.
    if let Ok(mut me) = me_slot().lock() {
        me.user = "tales".into();
        me.groups.insert("familia".into(), vec!["tales".into(), "bruna".into(), "bruno".into(), "maria".into()]);
        me.fetched = Some(Instant::now());
    }
    Some(dm)
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::open(&chat_dir())))
}

/// Everyone listening (each chat view: the Chat page, a room's chat).
static LISTENERS: crate::listeners::Listeners<ChatEvent> = crate::listeners::Listeners::new();

/// A stream of what happens, of its own: every listener gets every event.
pub fn events() -> async_channel::Receiver<ChatEvent> {
    LISTENERS.subscribe()
}

fn emit(e: ChatEvent) {
    LISTENERS.send(e);
}

pub fn conversations() -> Vec<Summary> {
    let mut all = store().lock().map(|s| s.summaries()).unwrap_or_default();
    all.retain(|c| shown(&c.id));
    all
}

/// Only the local network is on (BigScale left for it).
static LOCAL_ONLY: AtomicBool = AtomicBool::new(false);

/// Show the local network's conversations, and only them, while it is on.
pub fn set_local_only(on: bool) {
    if LOCAL_ONLY.swap(on, Ordering::Relaxed) != on {
        emit(ChatEvent::ShownChanged);
    }
}

/// Only the local network's conversations show.
pub fn local_only() -> bool {
    LOCAL_ONLY.load(Ordering::Relaxed)
}

/// Whether `conv` shows: one network at a time, as the devices page.
pub fn shown(conv: &ConvId) -> bool {
    conv.is_lan() == local_only()
}

/// The newest `n` messages of `id`, oldest first.
pub fn recent(id: &ConvId, n: usize) -> Vec<Message> {
    store().lock().map(|s| s.history.recent(id, n)).unwrap_or_default()
}

/// The `n` messages of `id` just before message `anchor`, oldest first.
pub fn before(id: &ConvId, anchor: &str, n: usize) -> Vec<Message> {
    store().lock().map(|s| s.history.before(id, anchor, n)).unwrap_or_default()
}

/// Message `anchor` of `id` and every one after it, oldest first (none when
/// `anchor` isn't there).
pub fn since(id: &ConvId, anchor: &str) -> Vec<Message> {
    store().lock().map(|s| s.history.since(id, anchor)).unwrap_or_default()
}

/// Message `msg` of `id`.
pub fn message(id: &ConvId, msg: &str) -> Option<Message> {
    store().lock().ok()?.history.message(id, msg)
}

/// The ids of `id`'s messages that `keep` takes, oldest first.
pub fn find(id: &ConvId, mut keep: impl FnMut(&Message) -> bool) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(s) = store().lock() {
        s.history.each(id, |m| {
            if keep(m) {
                found.push(m.id.clone());
            }
        });
    }
    found
}

/// `ids` of `id`'s messages in the conversation's order.
pub fn in_order(id: &ConvId, ids: &[String]) -> Vec<String> {
    store().lock().map(|s| s.history.in_order(id, ids)).unwrap_or_default()
}

/// The conversation is on screen: nothing unread in it anymore, here and
/// (once they hear it) on our other devices.
/// `id` is read: its unread count goes, and our other devices hear it.
/// True when something was unread.
pub fn mark_read(id: &ConvId) -> bool {
    let mut read_up_to = None;
    let mut was_unread = false;
    if let Ok(s) = store().lock() {
        if s.history.unread(id) > 0 {
            was_unread = true;
            s.history.set_unread(id, 0);
            read_up_to = s.history.last_received_at(id);
        }
    }
    if let Some(at) = read_up_to.filter(|_| !id.is_lan()) {
        // Off the caller's thread: finding who we are may ask tailscale.
        let id = id.clone();
        let _ = std::thread::Builder::new().name("biglace-read".into()).spawn(move || post_own(&id, Op::Read { at }));
    }
    was_unread
}

/// Queue `op` in `conv` for our own other devices alone.
fn post_own(conv: &ConvId, op: Op) {
    let Some(me) = me(Duration::from_secs(300)) else { return };
    let wire = Wire {
        id: new_id(),
        conv: conv.clone(),
        text: String::new(),
        at: now_ms(),
        file: None,
        reply_to: None,
        forwarded_from: None,
        op: Some(op),
        relayed: None,
    };
    queue(&wire, &[], &me.user, true);
}

/// Who `conv` goes to besides us; `None` for a group we're not in.
fn recipients(conv: &ConvId, me: &Me) -> Option<Vec<String>> {
    if conv.is_group() {
        Some(me.groups.get(&conv.name)?.iter().filter(|u| **u != me.user).cloned().collect())
    } else {
        Some(vec![conv.name.clone()])
    }
}

/// `wire` for every device of `users`, and for our own other devices when `ours`.
/// A person with no device in sight yet gets one unplaced entry, so the message waits for them.
fn pending_for(wire: &Wire, users: &[String], me: &str, ours: bool) -> Vec<Pending> {
    let peers = tailscale::get_peers();
    let mut pending: Vec<Pending> = peers
        .iter()
        .filter(|p| !p.user.is_empty() && !p.id.is_empty() && (users.contains(&p.user) || (ours && p.user == me)))
        .map(|p| Pending { wire: wire.clone(), to_user: p.user.clone(), node: p.id.clone() })
        .collect();
    for user in users.iter().filter(|u| u.as_str() != me && !peers.iter().any(|p| &p.user == *u)) {
        pending.push(Pending { wire: wire.clone(), to_user: user.clone(), node: String::new() });
    }
    pending
}

/// Put `wire` in the outbox for `users` (and our own devices when `ours`).
fn queue(wire: &Wire, users: &[String], me: &str, ours: bool) {
    let pending = pending_for(wire, users, me, ours);
    if pending.is_empty() {
        return;
    }
    if let Ok(mut s) = store().lock() {
        s.outbox.extend(pending);
        s.save_outbox();
    }
    kick();
}

/// Pass on what reached us, in case its writer's device has gone quiet: our
/// own words to the conversation's people, someone else's to our devices.
fn pass_on(wire: &Wire, conv: &ConvId, author: &str, device: &str, me: &Me) {
    if wire.relayed.is_some() || matches!(wire.op, Some(Op::Typing | Op::Call { .. } | Op::Send { .. } | Op::Read { .. })) {
        return;
    }
    if author == me.user {
        // Our own conversation went from its writer to each of our devices already.
        if conv.name == me.user && !conv.is_group() {
            return;
        }
        if let Some(users) = recipients(conv, me) {
            queue(wire, &users, &me.user, false);
        }
    } else {
        let mut on = wire.clone();
        on.relayed = Some(Relayed { from: author.to_string(), device: device.to_string() });
        queue(&on, &[], &me.user, true);
    }
}

/// Who wrote `wire`: the caller, or, when one of our own devices passes on
/// someone else's message, the author it names.
fn author_of(wire: &Wire, caller: &str, caller_device: &str, me: &str) -> (String, String) {
    match &wire.relayed {
        Some(r) if caller == me && r.from != me && name_ok(&r.from) && r.device.len() <= 64 => (r.from.clone(), r.device.clone()),
        _ => (caller.to_string(), caller_device.to_string()),
    }
}

pub fn unread_total() -> u32 {
    store().lock().map(|s| s.history.conversations().into_iter().filter(|(id, _)| shown(id)).map(|(_, unread)| unread).sum()).unwrap_or(0)
}

/// Make sure a conversation exists (an empty one shows in the list).
pub fn open(id: &ConvId) {
    if let Ok(s) = store().lock() {
        s.history.ensure(id);
    }
}

pub(crate) fn new_id() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn now_ms() -> u64 {
    crate::remote::now_us() / 1000
}

/// Write `text` in `conv`, answering message `reply_to` (if any).
/// Blocking (looks up who should get it): call off the UI thread.
pub fn send_reply(conv: &ConvId, text: &str, reply_to: Option<&str>) -> Result<(), String> {
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Ok(());
    }
    let reply = reply_to.and_then(|id| reply_ref(conv, id));
    post_with(conv, text, None, Extra { reply_to: reply, ..Extra::default() }).map(|_| ())
}

/// What a reply to message `id` of `conv` shows.
fn reply_ref(conv: &ConvId, id: &str) -> Option<ReplyRef> {
    let m = message(conv, id).filter(|m| !m.deleted)?;
    Some(ReplyRef { id: m.id.clone(), from: m.from.clone(), preview: preview_line(&m) })
}

/// One line about a message (plain): its text, what its file is, or how
/// its call went.
pub fn preview_line(m: &Message) -> String {
    if let Some(c) = &m.call {
        return call_line(c, m.mine);
    }
    let text = markdown::plain_text(&m.text);
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = if line.is_empty() {
        m.file.as_ref().map(|f| f.offer.name.clone()).unwrap_or_default()
    } else {
        line
    };
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(120).collect::<String>())
    } else {
        line
    }
}

/// "Outgoing video call · 3:12", "Missed voice call"…
pub fn call_line(c: &CallRecord, outgoing: bool) -> String {
    use crate::tr;
    let kind = match (c.outcome, c.video, outgoing) {
        (CallOutcome::Missed, false, _) => tr!("Missed voice call"),
        (CallOutcome::Missed, true, _) => tr!("Missed video call"),
        (CallOutcome::Declined, false, false) => tr!("Declined voice call"),
        (CallOutcome::Declined, true, false) => tr!("Declined video call"),
        (_, false, true) => tr!("Outgoing voice call"),
        (_, true, true) => tr!("Outgoing video call"),
        (_, false, false) => tr!("Incoming voice call"),
        (_, true, false) => tr!("Incoming video call"),
    };
    let detail = match c.outcome {
        CallOutcome::Answered => format!("{}:{:02}", c.seconds / 60, c.seconds % 60),
        CallOutcome::Declined if outgoing => tr!("declined"),
        CallOutcome::Busy => tr!("busy"),
        CallOutcome::Cancelled => tr!("no answer"),
        _ => return kind,
    };
    format!("{kind} · {detail}")
}

/// Keep how a call with `user` (an account, or a paired device's
/// [`lan_address`]) went in its conversation (this device only). A missed
/// one counts as unread and is announced like a message.
pub fn record_call(user: &str, outgoing: bool, record: CallRecord) {
    let conv = conversation_with(user);
    let me = if conv.is_lan() { crate::device_key::public_key() } else { my_user() };
    let Some(me) = me else { return };
    let user = conv.name.as_str();
    let missed = record.outcome == CallOutcome::Missed;
    let message = Message {
        id: new_id(),
        from: if outgoing { me } else { user.to_string() },
        device: String::new(),
        text: String::new(),
        at: now_ms(),
        mine: outgoing,
        delivered: vec![],
        file: None,
        reply_to: None,
        forwarded_from: None,
        reactions: BTreeMap::new(),
        edited_at: 0,
        deleted: false,
        call: Some(record),
    };
    let added = store().lock().map(|mut s| s.add(&conv, message.clone(), missed)).unwrap_or(false);
    if added {
        emit(ChatEvent::Changed(conv.clone()));
        if missed {
            emit(ChatEvent::Incoming { conv, message: Box::new(message) });
        }
    }
}

/// Send `op` to `user`'s device at `ip` now (not queued, not kept): a call
/// or transfer step. Ok once it took it.
pub(crate) fn send_op_now(ip: &str, user: &str, op: Op) -> Result<(), String> {
    let wire = Wire {
        id: new_id(),
        conv: ConvId::direct(user),
        text: String::new(),
        at: now_ms(),
        file: None,
        reply_to: None,
        forwarded_from: None,
        op: Some(op),
        relayed: None,
    };
    let id = wire.id.clone();
    let acked = deliver_to(ip, &[Pending { wire, to_user: user.to_string(), node: String::new() }])?;
    if acked.contains(&id) {
        Ok(())
    } else {
        Err("not acknowledged".into())
    }
}

/// [`send_op_now`] to the device `key` paired on the local network.
#[cfg(target_os = "linux")]
pub(crate) fn send_op_lan(key: &str, op: Op) -> Result<(), String> {
    let wire = Wire {
        id: new_id(),
        conv: ConvId::lan(key),
        text: String::new(),
        at: now_ms(),
        file: None,
        reply_to: None,
        forwarded_from: None,
        op: Some(op),
        relayed: None,
    };
    let payload = serde_json::to_vec(&wire).map_err(|e| e.to_string())?;
    let acked = crate::lan::pairing::deliver(key, &[payload]).map_err(|_| crate::trf!(
        "{name} can't be reached on the local network — try again when it's nearby",
        "name" => label(key)
    ))?;
    if acked.contains(&wire.id) {
        Ok(())
    } else {
        Err("not acknowledged".into())
    }
}

/// What comes with a message besides its text.
#[derive(Default)]
struct Extra {
    reply_to: Option<ReplyRef>,
    forwarded_from: Option<String>,
    op: Option<Op>,
}

/// Put a message (with or without a file) in `conv` and queue it for every
/// recipient device. Returns its id.
fn post(conv: &ConvId, text: &str, file: Option<(Offer, PathBuf)>) -> Result<String, String> {
    post_with(conv, text, file, Extra::default())
}

fn post_with(conv: &ConvId, text: &str, file: Option<(Offer, PathBuf)>, extra: Extra) -> Result<String, String> {
    if !conv.valid() {
        return Err("invalid conversation".into());
    }
    if text.len() > MAX_TEXT {
        return Err(crate::tr!("message too long"));
    }
    if conv.is_lan() {
        return post_lan(conv, text, file, extra);
    }
    let me = me(Duration::from_secs(300)).ok_or_else(|| crate::tr!("not signed in to a BigScale network"))?;
    let recipients = recipients(conv, &me).ok_or_else(|| crate::tr!("you are not in that group"))?;
    let wire = Wire {
        id: new_id(),
        conv: conv.clone(),
        text: text.to_string(),
        at: now_ms(),
        file: file.as_ref().map(|(o, _)| o.clone()),
        reply_to: extra.reply_to.clone(),
        forwarded_from: extra.forwarded_from.clone(),
        op: extra.op.clone(),
        relayed: None,
    };
    let device = tailscale::get_status().hostname.unwrap_or_default();
    let message = Message {
        id: wire.id.clone(),
        from: me.user.clone(),
        device,
        text: wire.text.clone(),
        at: wire.at,
        mine: true,
        delivered: vec![],
        file: file.as_ref().map(|(o, path)| Attachment {
            offer: o.clone(),
            path: Some(path.to_string_lossy().into_owned()),
            state: FileState::Done,
        }),
        reply_to: extra.reply_to,
        forwarded_from: extra.forwarded_from,
        reactions: BTreeMap::new(),
        edited_at: 0,
        deleted: false,
        call: None,
    };
    if let Some((o, path)) = &file {
        // Served to the recipient (and our own devices) when they ask.
        files::share(&wire.id, path, o.size, &recipients);
    }
    // Every device of every recipient, and our own other devices.
    let pending = pending_for(&wire, &recipients, &me.user, true);
    if let Ok(mut s) = store().lock() {
        // An op changes what's here; a message is added.
        match &extra.op {
            Some(op) => {
                s.apply_op(conv, &me.user, op);
            }
            None => {
                s.add(conv, message, false);
            }
        }
        s.outbox.extend(pending);
        s.save_outbox();
    }
    emit(ChatEvent::Changed(conv.clone()));
    kick();
    Ok(wire.id)
}

/// How a device paired on the local network is addressed where a tailnet
/// address or node goes (outbox, calls, rooms): this, then its key.
const LAN_ADDRESS: &str = "lan:";

/// The address of the device `key` paired on the local network.
pub fn lan_address(key: &str) -> String {
    format!("{LAN_ADDRESS}{key}")
}

/// The device key in a paired device's [`lan_address`].
pub fn lan_key(address: &str) -> Option<&str> {
    address.strip_prefix(LAN_ADDRESS)
}

/// The conversation with `user` of a call: a paired device's (its
/// [`lan_address`]) on the local network, or the account's.
pub fn conversation_with(user: &str) -> ConvId {
    match lan_key(user) {
        Some(key) => ConvId::lan(key),
        None => ConvId::direct(user),
    }
}

/// [`post_with`] to a device paired on the local network: no account, no
/// tailnet; we are our device key.
fn post_lan(conv: &ConvId, text: &str, file: Option<(Offer, PathBuf)>, extra: Extra) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    if !crate::lan::peers::is_paired(&conv.name) {
        return Err(crate::trf!("Not paired with {name}", "name" => label(&conv.name)));
    }
    let me = crate::device_key::public_key().ok_or("no device key")?;
    let wire = Wire {
        id: new_id(),
        conv: conv.clone(),
        text: text.to_string(),
        at: now_ms(),
        file: file.as_ref().map(|(o, _)| o.clone()),
        reply_to: extra.reply_to.clone(),
        forwarded_from: extra.forwarded_from.clone(),
        op: extra.op.clone(),
        relayed: None,
    };
    let message = own_lan_message(&wire, &me, file.as_ref());
    let id = wire.id.clone();
    if let Some((o, path)) = &file {
        // Served to the paired device when it asks (see lan::pairing::fetch_file).
        files::share(&id, path, o.size, std::slice::from_ref(&conv.name));
        if o.sha256.is_none() {
            // Ready by the time the paired device asks for the file.
            let id = id.clone();
            let _ = std::thread::Builder::new().name("biglace-hash".into()).spawn(move || files::share_sha256(&id));
        }
    }
    if let Ok(mut s) = store().lock() {
        match &wire.op {
            Some(op) => {
                s.apply_op(conv, &me, op);
            }
            None => {
                s.add(conv, message, false);
            }
        }
        s.outbox.push(Pending { node: lan_address(&conv.name), to_user: conv.name.clone(), wire });
        s.save_outbox();
    }
    emit(ChatEvent::Changed(conv.clone()));
    kick();
    Ok(id)
}

/// What we wrote in `wire`, as our history keeps it.
fn own_lan_message(wire: &Wire, me: &str, file: Option<&(Offer, PathBuf)>) -> Message {
    Message {
        id: wire.id.clone(),
        from: me.to_string(),
        device: String::new(),
        text: wire.text.clone(),
        at: wire.at,
        mine: true,
        delivered: vec![],
        file: file.map(|(o, path)| Attachment { offer: o.clone(), path: Some(path.to_string_lossy().into_owned()), state: FileState::Done }),
        reply_to: wire.reply_to.clone(),
        forwarded_from: wire.forwarded_from.clone(),
        reactions: BTreeMap::new(),
        edited_at: 0,
        deleted: false,
        call: None,
    }
}

/// A message (or a step) from `sender`, a device paired on the local
/// network; the id to acknowledge, `None` to have it sent again later.
#[cfg(target_os = "linux")]
pub(crate) fn accept_lan(sender: &str, payload: &[u8]) -> Option<String> {
    let wire: Wire = serde_json::from_slice(payload).ok()?;
    let own = crate::device_key::public_key()?;
    if !lan_wire_ok(&wire, &own) {
        // Never good: acknowledged so it isn't sent again.
        return Some(wire.id);
    }
    if !rate_ok(sender) {
        return None;
    }
    let conv = ConvId::lan(sender);
    match &wire.op {
        Some(Op::Typing) => emit(ChatEvent::Typing { conv, user: sender.to_string() }),
        Some(op @ (Op::React { .. } | Op::Edit { .. } | Op::Delete { .. } | Op::Pin { .. })) => {
            if store().lock().map(|mut s| s.apply_op(&conv, sender, op)).unwrap_or(false) {
                emit(ChatEvent::Changed(conv));
            }
        }
        Some(Op::Send { send, action, items }) => crate::transfer::received_lan(sender, send, *action, items),
        Some(Op::Call { call, action, video }) => {
            let address = lan_address(sender);
            crate::call::received(&address, "", &address, call, *action, *video);
        }
        Some(_) => {}
        None => {
            let message = Message {
                id: wire.id.clone(),
                from: sender.to_string(),
                device: String::new(),
                text: wire.text.clone(),
                at: wire.at,
                mine: false,
                delivered: vec![],
                file: wire.file.clone().map(|offer| Attachment { offer, path: None, state: FileState::Offered }),
                reply_to: wire.reply_to.clone(),
                forwarded_from: wire.forwarded_from.clone(),
                reactions: BTreeMap::new(),
                edited_at: 0,
                deleted: false,
                call: None,
            };
            if store().lock().map(|mut s| s.add(&conv, message.clone(), true)).unwrap_or(false) {
                emit(ChatEvent::Changed(conv.clone()));
                // Pictures and voice messages come down by themselves, as on the tailnet.
                if message.file.as_ref().is_some_and(|f| f.offer.fetch_by_itself()) {
                    files::download(&conv, &message.id);
                }
                emit(ChatEvent::Incoming { conv, message: Box::new(message) });
            }
        }
    }
    Some(wire.id)
}

/// What a paired device may send us: written to us, nothing passed on, and
/// the same limits as the tailnet's.
fn lan_wire_ok(wire: &Wire, own: &str) -> bool {
    wire.conv.is_lan()
        && wire.conv.name == own
        && wire.file.as_ref().is_none_or(Offer::valid)
        && wire.relayed.is_none()
        && wire.text.len() <= MAX_TEXT
        && !wire.id.is_empty()
        && wire.id.len() <= 64
        && wire.id.bytes().all(|c| c.is_ascii_alphanumeric())
        && wire.reply_to.as_ref().is_none_or(ReplyRef::valid)
        && wire.forwarded_from.as_deref().is_none_or(name_ok)
}

/// Hand what waits for a device paired on the local network to it, if it's nearby.
fn deliver_lan(key: &str, items: &[Pending]) {
    #[cfg(target_os = "linux")]
    {
        let payloads: Vec<Vec<u8>> = items.iter().filter_map(|p| serde_json::to_vec(&p.wire).ok()).collect();
        let Ok(acked) = crate::lan::pairing::deliver(key, &payloads) else { return };
        let mut changed = HashSet::new();
        if let Ok(mut s) = store().lock() {
            for p in items.iter().filter(|p| acked.contains(&p.wire.id)) {
                s.outbox.retain(|x| !(x.node == p.node && x.wire.id == p.wire.id));
                if p.wire.op.is_none() && s.delivered(&p.wire.conv, &p.wire.id, key) {
                    changed.insert(p.wire.conv.clone());
                }
            }
            s.save_outbox();
        }
        for conv in changed {
            emit(ChatEvent::Changed(conv));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (key, items);
}

/// A device paired on the local network was forgotten: what waited for it never goes.
pub fn forget_lan(key: &str) {
    let node = lan_address(key);
    if let Ok(mut s) = store().lock() {
        s.outbox.retain(|p| p.node != node);
        s.save_outbox();
    }
}

/// How a name shows: a device paired on the local network (its key, or its
/// [`lan_address`]) by the name it gave, us as "You", everyone else as is.
pub fn label(name: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        let name = lan_key(name).unwrap_or(name);
        if crate::device_key::public_key().is_some_and(|own| own == name) {
            return crate::tr!("You");
        }
        if let Some(paired) = crate::lan::peers::list().into_iter().find(|p| p.key == name) {
            return paired.name;
        }
    }
    name.to_string()
}

/// Our own conversation: what we write there reaches our other devices.
pub fn is_own(conv: &ConvId) -> bool {
    !conv.is_group() && !conv.is_lan() && my_user().is_some_and(|me| me == conv.name)
}

/// How a conversation shows: ours as "My devices", the rest by [`label`].
pub fn title(conv: &ConvId) -> String {
    if is_own(conv) {
        return crate::tr!("My devices");
    }
    label(&conv.name)
}

/// Do `op` in `conv`, for everyone there; `fallback` is what older
/// versions show instead.
fn send_op(conv: &ConvId, op: Op, fallback: String) -> Result<(), String> {
    post_with(conv, &fallback, None, Extra { op: Some(op), ..Extra::default() }).map(|_| ())
}

/// Add our reaction `emoji` to message `target`, or take it back if it's
/// there already.
pub fn react(conv: &ConvId, target: &str, emoji: &str) -> Result<(), String> {
    if !emoji_ok(emoji) {
        return Err("not an emoji".into());
    }
    let me = my_user().unwrap_or_default();
    let m = message(conv, target).ok_or("message not found")?;
    let on = !m.reactions.get(emoji).is_some_and(|who| who.contains(&me));
    let fallback = crate::trf!("{emoji} (reaction to: {text})", "emoji" => emoji, "text" => preview_line(&m));
    send_op(conv, Op::React { target: target.into(), emoji: emoji.into(), on }, fallback)
}

/// Change the text of our message `target`.
pub fn edit(conv: &ConvId, target: &str, text: &str) -> Result<(), String> {
    let text = text.trim_end();
    if text.trim().is_empty() || text.len() > MAX_TEXT {
        return Err(crate::tr!("message too long"));
    }
    let fallback = crate::trf!("✏️ (edited) {text}", "text" => text);
    send_op(conv, Op::Edit { target: target.into(), text: text.into() }, fallback)
}

/// Delete our messages `targets` for everyone.
pub fn delete_for_all(conv: &ConvId, targets: &[String]) -> Result<(), String> {
    for t in targets {
        send_op(conv, Op::Delete { target: t.clone() }, crate::tr!("🗑 (deleted a message)"))?;
    }
    Ok(())
}

/// Remove messages from this computer only.
pub fn delete_for_me(conv: &ConvId, targets: &[String]) {
    if let Ok(s) = store().lock() {
        s.history.remove(conv, targets);
    }
    emit(ChatEvent::Changed(conv.clone()));
}

/// Take `conv` out of our list, with its history on this device; a new
/// message brings it back. Others keep theirs.
pub fn remove_conversation(conv: &ConvId) {
    if let Ok(mut s) = store().lock() {
        s.history.drop_conversation(conv);
    }
    emit(ChatEvent::Removed(conv.clone()));
}

/// Pin message `target` to the top of `conv` (None: unpin).
pub fn pin(conv: &ConvId, target: Option<&str>) -> Result<(), String> {
    let fallback = match target.and_then(|t| message(conv, t)) {
        Some(m) => crate::trf!("📌 (pinned: {text})", "text" => preview_line(&m)),
        None => crate::tr!("📌 (unpinned the message)"),
    };
    send_op(conv, Op::Pin { target: target.map(str::to_string) }, fallback)
}

/// The pinned message of `conv`.
pub fn pinned(conv: &ConvId) -> Option<Message> {
    let s = store().lock().ok()?;
    let id = s.history.pinned(conv)?;
    s.history.message(conv, &id)
}

/// Send messages `ids` of `from` to the conversation `to`.
pub fn forward(to: &ConvId, from: &ConvId, ids: &[String]) -> Result<(), String> {
    for id in ids {
        let Some(m) = message(from, id).filter(|m| !m.deleted) else { continue };
        let extra = Extra { forwarded_from: Some(m.forwarded_from.clone().unwrap_or_else(|| m.from.clone())), ..Extra::default() };
        let Some(file) = &m.file else {
            if !m.text.trim().is_empty() {
                post_with(to, &m.text, None, extra)?;
            }
            continue;
        };
        // A file that hasn't come down yet is fetched first: it goes on from here.
        let path = files::fetch(from, id)?;
        post_with(to, &m.text, Some((file.offer.clone(), path)), extra)?;
    }
    Ok(())
}

/// We're typing in `conv`: tell the others (at most every few seconds; not
/// kept, only to devices that understand it).
pub fn typing(conv: &ConvId) {
    static LAST: OnceLock<Mutex<HashMap<ConvId, Instant>>> = OnceLock::new();
    {
        let Ok(mut last) = LAST.get_or_init(Default::default).lock() else { return };
        if last.get(conv).is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return;
        }
        last.insert(conv.clone(), Instant::now());
    }
    let wire = Wire {
        id: new_id(),
        conv: conv.clone(),
        text: String::new(),
        at: now_ms(),
        file: None,
        reply_to: None,
        forwarded_from: None,
        op: Some(Op::Typing),
        relayed: None,
    };
    let conv = conv.clone();
    let _ = std::thread::Builder::new().name("biglace-typing".into()).spawn(move || {
        if conv.is_lan() {
            // Straight to the paired device, if it's within reach: typing isn't queued.
            #[cfg(target_os = "linux")]
            if let Ok(payload) = serde_json::to_vec(&wire) {
                let _ = crate::lan::pairing::deliver(&conv.name, &[payload]);
            }
            return;
        }
        let Some(me) = me(Duration::from_secs(300)) else { return };
        let recipients = recipients(&conv, &me).unwrap_or_default();
        for p in tailscale::get_peers() {
            if !p.online || p.ipv4.is_empty() || !recipients.contains(&p.user) || !understands_ops(&p.ipv4) {
                continue;
            }
            let item = Pending { wire: wire.clone(), to_user: p.user.clone(), node: p.id.clone() };
            let _ = deliver_to(&p.ipv4, &[item]);
        }
    });
}

/// Announced by versions that understand ops (reactions, edits…).
pub const OPS_CAPABILITY: &str = "chat2";
/// Announced by versions that take [`Op::Read`] from our other devices.
pub const READ_CAPABILITY: &str = "read-sync";
/// Announced by versions that take messages our other devices pass on (see `pass_on`).
pub const RELAY_CAPABILITY: &str = "relay";

fn understands_ops(ip: &str) -> bool {
    crate::agent::client::supports(ip, OPS_CAPABILITY)
}

// ─── Delivery ───────────────────────────────────────────────────────────────

/// Wakes the delivery worker; the receiver is taken once by `start`.
type Kick = (std::sync::mpsc::SyncSender<()>, Mutex<Option<std::sync::mpsc::Receiver<()>>>);

fn kick_slot() -> &'static Kick {
    static K: OnceLock<Kick> = OnceLock::new();
    K.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        (tx, Mutex::new(Some(rx)))
    })
}

/// Try the outbox now (a message was written, someone came online…).
pub fn kick() {
    let _ = kick_slot().0.try_send(());
}

/// Start the delivery worker (once).
pub fn start() {
    let Some(rx) = kick_slot().1.lock().ok().and_then(|mut r| r.take()) else { return };
    let _ = std::thread::Builder::new().name("biglace-chat".into()).spawn(move || loop {
        let _ = rx.recv_timeout(RETRY);
        deliver_pending();
    });
}

fn deliver_pending() {
    let peers = tailscale::get_peers();
    let outbox = store()
        .lock()
        .map(|mut s| {
            s.place_unplaced(&peers);
            s.outbox.clone()
        })
        .unwrap_or_default();
    let mut by_node: HashMap<String, Vec<Pending>> = HashMap::new();
    for p in outbox.into_iter().filter(|p| !p.node.is_empty()) {
        by_node.entry(p.node.clone()).or_default().push(p);
    }
    // Each device on its own thread: one out of reach doesn't hold up the others.
    std::thread::scope(|scope| {
        for (node, items) in by_node {
            let peers = &peers;
            scope.spawn(move || deliver_node(&node, items, peers));
        }
    });
}

/// Send what waits for the device `node` to it, if it's reachable.
fn deliver_node(node: &str, items: Vec<Pending>, peers: &[tailscale::Peer]) {
    if let Some(key) = lan_key(node) {
        deliver_lan(key, &items);
        return;
    }
    let Some(peer) = peers.iter().find(|p| p.id == node) else {
        // That device is out of sight: gone for good, or its access was taken away for now.
        if !peers.is_empty() {
            if let Ok(mut s) = store().lock() {
                s.unplace(&items, peers);
            }
        }
        return;
    };
    if !peer.online || peer.ipv4.is_empty() {
        return;
    }
    let items = heard_by(&peer.ipv4, items);
    if items.is_empty() {
        return;
    }
    match deliver_to(&peer.ipv4, &items) {
        Ok(acked) => {
            let mut changed = HashSet::new();
            if let Ok(mut s) = store().lock() {
                for p in items.iter().filter(|p| acked.contains(&p.wire.id)) {
                    s.outbox.retain(|x| !(x.node == p.node && x.wire.id == p.wire.id));
                    let me = my_user().unwrap_or_default();
                    if p.to_user != me && s.delivered(&p.wire.conv, &p.wire.id, &p.to_user) {
                        changed.insert(p.wire.conv.clone());
                    }
                }
                s.save_outbox();
            }
            for c in changed {
                emit(ChatEvent::Changed(c));
            }
        }
        Err(e) => tailscale::dbg(&format!("chat: {} unreachable: {e}", peer.hostname)),
    }
}

/// What of `items` the device at `ip` hears; the rest leaves the outbox.
fn heard_by(ip: &str, items: Vec<Pending>) -> Vec<Pending> {
    // A device that doesn't take reads would refuse them for ever: it
    // doesn't hear them.
    // Nor what was passed on, which it would show as ours.
    let (unheard, items): (Vec<Pending>, Vec<Pending>) = items.into_iter().partition(|p| {
        (matches!(p.wire.op, Some(Op::Read { .. })) && !crate::agent::client::supports(ip, READ_CAPABILITY))
            || (p.wire.relayed.is_some() && !crate::agent::client::supports(ip, RELAY_CAPABILITY))
    });
    drop_pending(&unheard);
    // Older versions: ops become their plain-text stand-in; those with
    // nothing to say (a reaction taken back, typing) are skipped.
    if items.iter().any(|p| p.wire.op.is_some()) && !understands_ops(ip) {
        let (silent, rest): (Vec<Pending>, Vec<Pending>) = items.into_iter().partition(|p| {
            matches!(p.wire.op, Some(Op::React { on: false, .. } | Op::Typing | Op::Call { .. } | Op::Send { .. }))
        });
        drop_pending(&silent);
        rest.into_iter()
            .map(|mut p| {
                p.wire.op = None;
                p
            })
            .collect()
    } else {
        items
    }
}

fn drop_pending(items: &[Pending]) {
    if let Ok(mut s) = store().lock() {
        s.outbox.retain(|x| !items.iter().any(|p| p.node == x.node && p.wire.id == x.wire.id));
        s.save_outbox();
    }
}

/// Send `items` to the agent at `ip`; the ids it stored.
fn deliver_to(ip: &str, items: &[Pending]) -> Result<HashSet<String>, String> {
    let (stream, mut reader) =
        crate::remote::client::dial(ip, PATH, &[], Duration::from_secs(8)).map_err(|e| format!("{e:?}"))?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    for p in items {
        let json = serde_json::to_vec(&p.wire).map_err(|e| e.to_string())?;
        write_frame(&mut w, Kind::Chat, &json).map_err(|e| e.to_string())?;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(8)));
    let mut acked = HashSet::new();
    while acked.len() < items.len() {
        match read_frame(&mut reader) {
            Ok((Kind::ChatAck, payload)) => {
                if let Ok(ids) = serde_json::from_slice::<Vec<String>>(&payload) {
                    acked.extend(ids);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(acked)
}

// ─── Receiving ──────────────────────────────────────────────────────────────

fn respond(mut stream: &TcpStream, status: u16, reason: &str) {
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

fn rate_ok(sender: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() else { return false };
    let now = Instant::now();
    let list = seen.entry(sender.to_string()).or_default();
    list.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if list.len() >= RATE_PER_MINUTE {
        return false;
    }
    list.push(now);
    true
}

/// Where a message from `sender` belongs on this computer, or `None` to
/// drop it. `me`: our account and groups.
fn place(wire: &Wire, sender: &str, me: &Me) -> Option<(ConvId, bool)> {
    if !wire.conv.valid() || wire.text.len() > MAX_TEXT || wire.id.is_empty() || wire.id.len() > 64 {
        return None;
    }
    if !wire.id.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if wire.file.as_ref().is_some_and(|f| !f.valid()) {
        return None;
    }
    if wire.reply_to.as_ref().is_some_and(|r| !r.valid()) || wire.forwarded_from.as_ref().is_some_and(|f| !name_ok(f)) {
        return None;
    }
    let mine = sender == me.user;
    if wire.conv.is_group() {
        let members = me.groups.get(&wire.conv.name)?;
        if !members.iter().any(|m| m == sender) {
            return None;
        }
        Some((wire.conv.clone(), mine))
    } else if mine && wire.conv.name == me.user {
        // Written to our own devices from another of them: news here.
        Some((wire.conv.clone(), false))
    } else if mine {
        // A copy of what we wrote to someone, from another of our devices.
        Some((wire.conv.clone(), true))
    } else if wire.conv.name == me.user {
        Some((ConvId::direct(sender), false))
    } else {
        // Addressed to someone else.
        None
    }
}

/// A device delivers messages to us (agent thread).
pub fn accept(stream: TcpStream, peer: IpAddr, headers: &HashMap<String, String>, identity: &Identity, path: &str) {
    let local = stream
        .local_addr()
        .map(|a| format!("{}:{}", a.ip(), a.port()))
        .unwrap_or_default();
    if let Err(why) = crate::remote::host::verify_caller(headers, peer, &local, path) {
        tailscale::dbg(&format!("chat: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let Some(mut me) = me(Duration::from_secs(600)) else {
        return respond(&stream, 503, "Unavailable");
    };
    let _ = write!(&stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-biglace-chat\r\n\r\n");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let caller = identity.login.clone();
    let mut refreshed = false;
    while let Ok((Kind::Chat, payload)) = read_frame(&mut reader) {
        let Ok(wire) = serde_json::from_slice::<Wire>(&payload) else { continue };
        if !rate_ok(&caller) {
            break;
        }
        let (sender, device) = author_of(&wire, &caller, &identity.device, &me.user);
        let mut placed = place(&wire, &sender, &me);
        // A group we don't know of yet (just created, or just joined)?
        if placed.is_none() && wire.conv.is_group() && !refreshed {
            refreshed = true;
            if let Some(fresh) = self::me(Duration::ZERO) {
                me = fresh;
                placed = place(&wire, &sender, &me);
            }
        }
        if let (Some((conv, _)), Some(op)) = (&placed, &wire.op) {
            if *op == Op::Typing {
                emit(ChatEvent::Typing { conv: conv.clone(), user: sender.clone() });
            } else if let Op::Call { call, action, video } = op {
                if !conv.is_group() && conv.name == sender {
                    crate::call::received(&sender, &identity.device, &peer.to_string(), call, *action, *video);
                }
            } else if let Op::Send { send, action, items } = op {
                if !conv.is_group() && conv.name == sender {
                    crate::transfer::received(&sender, &identity.device, &peer.to_string(), send, *action, items);
                }
            } else if let Op::Read { at } = op {
                if sender == me.user && store().lock().map(|mut s| s.read_until(conv, *at)).unwrap_or(false) {
                    emit(ChatEvent::ReadElsewhere(conv.clone()));
                }
            } else if store().lock().map(|mut s| s.apply_op(conv, &sender, op)).unwrap_or(false) {
                emit(ChatEvent::Changed(conv.clone()));
                pass_on(&wire, conv, &sender, &device, &me);
            }
            placed = None;
        }
        if let Some((conv, mine)) = placed {
            let message = Message {
                id: wire.id.clone(),
                from: sender.clone(),
                device: device.clone(),
                text: wire.text.clone(),
                at: wire.at,
                mine,
                delivered: vec![],
                file: wire.file.clone().map(|offer| Attachment { offer, path: None, state: FileState::Offered }),
                reply_to: wire.reply_to.clone(),
                forwarded_from: wire.forwarded_from.clone(),
                reactions: BTreeMap::new(),
                edited_at: 0,
                deleted: false,
                call: None,
            };
            let added = store().lock().map(|mut s| s.add(&conv, message.clone(), !mine)).unwrap_or(false);
            if added {
                emit(ChatEvent::Changed(conv.clone()));
                pass_on(&wire, &conv, &sender, &device, &me);
                // Pictures and voice messages come down by themselves.
                if message.file.as_ref().is_some_and(|f| f.offer.fetch_by_itself()) {
                    files::download(&conv, &message.id);
                }
                if !mine {
                    emit(ChatEvent::Incoming { conv, message: Box::new(message) });
                }
            }
        }
        // Acknowledged either way: kept, a duplicate, or not for us — the
        // sender stops retrying.
        let ack = serde_json::to_vec(&[&wire.id]).unwrap_or_default();
        if write_frame(&mut writer, Kind::ChatAck, &ack).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_keys_read_back() {
        assert_eq!(ConvId::from_key("dm-bruna"), Some(ConvId::direct("bruna")));
        assert_eq!(ConvId::from_key("group-big-linux"), Some(ConvId::group("big-linux")));
        assert_eq!(ConvId::from_key("x-bruna"), None);
        assert_eq!(ConvId::from_key("dm-../x"), None);
    }

    fn me() -> Me {
        Me {
            user: "tales".into(),
            groups: HashMap::from([("familia".into(), vec!["tales".into(), "bruna".into()])]),
            owners: HashMap::new(),
            fetched: None,
        }
    }

    fn wire(conv: ConvId) -> Wire {
        Wire { id: "a1".into(), conv, text: "oi".into(), at: 1, file: None, reply_to: None, forwarded_from: None, op: None, relayed: None }
    }

    #[test]
    fn messages_land_where_they_belong() {
        let me = me();
        // Bruna writes to me.
        assert_eq!(place(&wire(ConvId::direct("tales")), "bruna", &me), Some((ConvId::direct("bruna"), false)));
        // Bruna writes to someone else: not for this computer.
        assert_eq!(place(&wire(ConvId::direct("bruno")), "bruna", &me), None);
        // My other device copies what I wrote to Bruno.
        assert_eq!(place(&wire(ConvId::direct("bruno")), "tales", &me), Some((ConvId::direct("bruno"), true)));
        // My other device writes to my devices: news here, not a copy of mine.
        assert_eq!(place(&wire(ConvId::direct("tales")), "tales", &me), Some((ConvId::direct("tales"), false)));
        // Group: only from its members, only for groups I'm in.
        assert_eq!(place(&wire(ConvId::group("familia")), "bruna", &me), Some((ConvId::group("familia"), false)));
        assert_eq!(place(&wire(ConvId::group("familia")), "bruno", &me), None);
        assert_eq!(place(&wire(ConvId::group("trabalho")), "bruna", &me), None);
        // Junk.
        assert_eq!(place(&wire(ConvId { kind: "dm".into(), name: "../x".into() }), "bruna", &me), None);
        assert_eq!(place(&wire(ConvId { kind: "x".into(), name: "tales".into() }), "bruna", &me), None);
        let mut big = wire(ConvId::direct("tales"));
        big.text = "a".repeat(MAX_TEXT + 1);
        assert_eq!(place(&big, "bruna", &me), None);
    }

    /// Every message of `conv`, by id, in order.
    fn ids_of(s: &Store, conv: &ConvId) -> Vec<String> {
        s.history.recent(conv, usize::MAX).into_iter().map(|m| m.id).collect()
    }

    fn plain(id: &str, at: u64) -> Message {
        Message {
            id: id.into(),
            from: "bruna".into(),
            device: String::new(),
            text: id.into(),
            at,
            mine: false,
            delivered: vec![],
            file: None,
            reply_to: None,
            forwarded_from: None,
            reactions: BTreeMap::new(),
            edited_at: 0,
            deleted: false,
            call: None,
        }
    }

    /// A conversation of the JSON files the history was kept in comes over whole,
    /// once: the file goes aside (kept), and opening again adds nothing.
    #[test]
    fn json_history_moves_into_the_database() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-migrate-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conv = ConvId::direct("bruna");
        let old = serde_json::json!({
            "messages": [plain("a", 10), plain("b", 10), plain("c", 5)],
            "unread": 2,
            "pinned": "b",
        });
        std::fs::write(dir.join("dm-bruna.json"), old.to_string()).unwrap();
        std::fs::write(dir.join("group-bad name.json"), "{}").unwrap();
        std::fs::write(dir.join("dm-maria.json"), "{ not json").unwrap();
        let s = Store::open(&dir);
        // The file's order stays for equal times; a later time sorts by itself.
        assert_eq!(ids_of(&s, &conv), ["c", "a", "b"]);
        assert_eq!(s.history.unread(&conv), 2);
        assert_eq!(s.history.pinned(&conv).as_deref(), Some("b"));
        assert!(!dir.join("dm-bruna.json").exists());
        assert!(dir.join("json-before-history/dm-bruna.json").exists());
        assert!(dir.join("group-bad name.json").exists(), "not a conversation: left alone");
        assert!(dir.join("dm-maria.json").exists(), "unreadable: left where it is");
        drop(s);
        std::fs::copy(dir.join("json-before-history/dm-bruna.json"), dir.join("dm-bruna.json")).unwrap();
        let again = Store::open(&dir);
        assert_eq!(ids_of(&again, &conv).len(), 3, "copied again: nothing doubles");
        assert_eq!(again.history.unread(&conv), 2);
        assert!(dir.join("json-before-history/dm-bruna.json").exists(), "the first copy kept");
        assert!(dir.join("json-before-history/dm-bruna.json.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pages follow the conversation's order, across messages written at the same time.
    #[test]
    fn history_reads_in_pages() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-pages-{}", new_id()));
        let conv = ConvId::direct("bruna");
        let mut s = Store::open(&dir);
        for (i, at) in [1, 2, 2, 2, 3, 4].into_iter().enumerate() {
            assert!(s.add(&conv, plain(&format!("m{i}"), at), false));
        }
        let ids = |page: Vec<Message>| page.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(s.history.recent(&conv, 2)), ["m4", "m5"]);
        assert_eq!(ids(s.history.before(&conv, "m4", 2)), ["m2", "m3"]);
        assert_eq!(ids(s.history.before(&conv, "m2", 5)), ["m0", "m1"]);
        assert!(s.history.before(&conv, "m0", 5).is_empty());
        assert_eq!(ids(s.history.since(&conv, "m3")), ["m3", "m4", "m5"]);
        assert!(s.history.since(&conv, "gone").is_empty());
        assert_eq!(s.history.in_order(&conv, &["m5".into(), "x".into(), "m1".into()]), ["m1", "m5"]);
        s.history.remove(&conv, &["m4".into()]);
        assert_eq!(ids(s.history.recent(&conv, 2)), ["m3", "m5"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A removed conversation leaves the list with its messages; a new one starts it again.
    #[test]
    fn removed_conversation_comes_back_empty() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-remove-{}", new_id()));
        let (gone, kept) = (ConvId::direct("thiago"), ConvId::direct("bruna"));
        let mut s = Store::open(&dir);
        assert!(s.add(&gone, plain("old", 1), true));
        assert!(s.add(&kept, plain("stays", 1), false));
        s.history.drop_conversation(&gone);
        let listed = |s: &Store| s.history.conversations().into_iter().map(|(c, _)| c).collect::<Vec<_>>();
        assert_eq!(listed(&s), std::slice::from_ref(&kept));
        assert!(s.add(&gone, plain("new", 2), true));
        assert_eq!(s.history.recent(&gone, 5).into_iter().map(|m| m.id).collect::<Vec<_>>(), ["new"]);
        assert_eq!(s.history.unread(&gone), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A message to someone with no device in sight waits, then goes to each of theirs once they show up.
    #[test]
    fn unplaced_message_waits_for_devices() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-unplaced-{}", new_id()));
        let mut s = Store::open(&dir);
        let wire = Wire { id: "w".into(), conv: ConvId::direct("tales"), text: "x".into(), at: 1, file: None, reply_to: None, forwarded_from: None, op: None, relayed: None };
        s.outbox.push(Pending { wire, to_user: "tales".into(), node: String::new() });
        let device = |id: &str, user: &str| tailscale::Peer { id: id.into(), user: user.into(), ..Default::default() };
        s.place_unplaced(&[device("other", "bruna")]);
        assert_eq!(s.outbox.iter().map(|p| p.node.as_str()).collect::<Vec<_>>(), [""]);
        s.place_unplaced(&[device("pc", "tales"), device("phone", "tales"), device("other", "bruna")]);
        assert_eq!(s.outbox.iter().map(|p| p.node.as_str()).collect::<Vec<_>>(), ["pc", "phone"]);
        assert_eq!(Store::open(&dir).outbox.len(), 2);
        // Both devices drop out of sight while another person's stays: it waits for them again, once.
        let items = s.outbox.clone();
        s.unplace(&items, &[device("other", "bruna")]);
        assert_eq!(s.outbox.iter().map(|p| p.node.as_str()).collect::<Vec<_>>(), [""]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A renamed group keeps its history, merged without repeats, and its outbox.
    #[test]
    fn renamed_group_keeps_history() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-rename-{}", new_id()));
        let (old, new) = (ConvId::group("familia"), ConvId::group("familia-silva"));
        let mut s = Store::open(&dir);
        let msg = |id: &str, at: u64| Message {
            id: id.into(),
            from: "bruna".into(),
            device: String::new(),
            text: "oi".into(),
            at,
            mine: false,
            delivered: vec![],
            file: None,
            reply_to: None,
            forwarded_from: None,
            reactions: BTreeMap::new(),
            edited_at: 0,
            deleted: false,
            call: None,
        };
        s.add(&old, msg("m1", 1), true);
        s.add(&old, msg("m3", 3), true);
        s.add(&new, msg("m2", 2), false);
        s.add(&new, msg("m3", 3), false);
        let wire = Wire { id: "w".into(), conv: old.clone(), text: "x".into(), at: 4, file: None, reply_to: None, forwarded_from: None, op: None, relayed: None };
        s.outbox.push(Pending { wire, to_user: "bruna".into(), node: "n".into() });

        assert!(s.rename_conv(&old, &new));
        assert!(!s.history.conversations().iter().any(|(id, _)| *id == old));
        assert!(s.history.recent(&old, 10).is_empty());
        assert_eq!(ids_of(&s, &new), ["m1", "m2", "m3"]);
        assert_eq!(s.history.unread(&new), 2);
        assert_eq!(s.outbox[0].wire.conv, new);
        let again = Store::open(&dir);
        assert_eq!(ids_of(&again, &new).len(), 3);
        assert!(!s.rename_conv(&old, &new), "nothing left to move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reactions toggle per person; only the author edits or deletes;
    /// pins follow what's there.
    #[test]
    fn ops_change_messages_by_the_rules() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-ops-{}", new_id()));
        let conv = ConvId::group("familia");
        let mut s = Store::open(&dir);
        let msg = |id: &str, from: &str| Message {
            id: id.into(),
            from: from.into(),
            device: String::new(),
            text: "oi".into(),
            at: 1,
            mine: false,
            delivered: vec![],
            file: None,
            reply_to: None,
            forwarded_from: None,
            reactions: BTreeMap::new(),
            edited_at: 0,
            deleted: false,
            call: None,
        };
        s.add(&conv, msg("m1", "bruna"), false);
        let react = |emoji: &str, on: bool| Op::React { target: "m1".into(), emoji: emoji.into(), on };
        assert!(s.apply_op(&conv, "tales", &react("❤️", true)));
        assert!(!s.apply_op(&conv, "tales", &react("❤️", true)), "already there");
        assert!(s.apply_op(&conv, "bruna", &react("❤️", true)));
        let m1 = |s: &Store| s.history.message(&conv, "m1").unwrap();
        assert_eq!(m1(&s).reactions["❤️"], ["tales", "bruna"]);
        assert!(s.apply_op(&conv, "tales", &react("❤️", false)));
        assert_eq!(m1(&s).reactions["❤️"], ["bruna"]);
        assert!(!s.apply_op(&conv, "tales", &react("abc", true)), "not an emoji");
        // Only Bruna edits or deletes her message.
        let edit = Op::Edit { target: "m1".into(), text: "olá".into() };
        assert!(!s.apply_op(&conv, "tales", &edit));
        assert!(s.apply_op(&conv, "bruna", &edit));
        assert_eq!(m1(&s).text, "olá");
        assert!(m1(&s).edited_at > 0);
        assert!(s.apply_op(&conv, "tales", &Op::Pin { target: Some("m1".into()) }));
        assert!(!s.apply_op(&conv, "tales", &Op::Pin { target: Some("nope".into()) }), "unknown message");
        let del = Op::Delete { target: "m1".into() };
        assert!(!s.apply_op(&conv, "tales", &del));
        assert!(s.apply_op(&conv, "bruna", &del));
        let m = m1(&s);
        assert!(m.deleted && m.text.is_empty() && m.reactions.is_empty());
        assert_eq!(s.history.pinned(&conv), None, "a deleted message isn't pinned");
        // Saved: reopened, the same.
        let again = Store::open(&dir);
        assert!(m1(&again).deleted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Older versions read an op as a plain message (its stand-in text).
    #[test]
    fn ops_stay_readable_by_older_versions() {
        let mut w = wire(ConvId::direct("bruna"));
        w.text = "❤️ (reaction to: oi)".into();
        w.op = Some(Op::React { target: "x".into(), emoji: "❤️".into(), on: true });
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["text"], "❤️ (reaction to: oi)");
        assert_eq!(json["op"]["type"], "react");
        let back: Wire = serde_json::from_value(json).unwrap();
        assert_eq!(back.op, w.op);
    }

    /// The call step as the phone app reads and writes it.
    #[test]
    fn call_step_matches_phone_wire() {
        let op = Op::Call { call: "ab12".into(), action: crate::call::Action::Ring, video: true };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json, serde_json::json!({"type": "call", "call": "ab12", "action": "ring", "video": true}));
        let back: Op = serde_json::from_str(r#"{"type":"call","call":"ab12","action":"end"}"#).unwrap();
        assert_eq!(back, Op::Call { call: "ab12".into(), action: crate::call::Action::End, video: false });
    }

    #[test]
    fn history_is_kept_in_order_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-test-{}", new_id()));
        let conv = ConvId::direct("bruna");
        let m = |id: &str, at: u64| Message {
            id: id.into(),
            from: "bruna".into(),
            device: "nb".into(),
            text: id.into(),
            at,
            mine: false,
            delivered: vec![],
            file: None,
            reply_to: None,
            forwarded_from: None,
            reactions: BTreeMap::new(),
            edited_at: 0,
            deleted: false,
            call: None,
        };
        {
            let mut s = Store::open(&dir);
            assert!(s.add(&conv, m("b", 20), true));
            assert!(s.add(&conv, m("a", 10), true));
            assert!(!s.add(&conv, m("a", 10), true), "duplicate");
            assert!(s.add(&conv, m("c", 30), true));
            s.outbox.push(Pending { wire: wire(conv.clone()), to_user: "bruna".into(), node: "n1".into() });
            s.save_outbox();
            assert!(s.delivered(&conv, "c", "bruna"));
            assert!(!s.delivered(&conv, "c", "bruna"), "already marked");
        }
        // Reopened from disk: same history, same outbox, files private.
        let s = Store::open(&dir);
        assert_eq!(ids_of(&s, &conv), ["a", "b", "c"]);
        assert_eq!(s.history.unread(&conv), 3);
        assert_eq!(s.history.message(&conv, "c").unwrap().delivered, ["bruna"]);
        assert_eq!(s.outbox.len(), 1);
        let summaries = s.summaries();
        assert_eq!(summaries[0].last.as_ref().map(|m| m.id.as_str()), Some("c"));
        use std::os::unix::fs::PermissionsExt;
        for file in ["history.db", "outbox"] {
            let mode = std::fs::metadata(dir.join(file)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_elsewhere_keeps_later_unread() {
        let dir = std::env::temp_dir().join(format!("biglace-chat-test-{}", new_id()));
        let conv = ConvId::direct("bruna");
        let mut s = Store::open(&dir);
        for (id, at) in [("a", 10), ("b", 20), ("c", 30)] {
            let mut w = wire(conv.clone());
            w.id = id.into();
            let m = Message {
                id: id.into(),
                from: "bruna".into(),
                device: "nb".into(),
                text: w.text,
                at,
                mine: false,
                delivered: vec![],
                file: None,
                reply_to: None,
                forwarded_from: None,
                reactions: BTreeMap::new(),
                edited_at: 0,
                deleted: false,
                call: None,
            };
            assert!(s.add(&conv, m, true));
        }
        assert!(s.read_until(&conv, 20), "read up to b on the phone: c, later, stays unread");
        assert_eq!(s.history.unread(&conv), 1);
        assert!(!s.read_until(&conv, 20), "nothing new");
        assert!(!s.read_until(&conv, 5), "an older read never adds unread messages back");
        assert!(!s.read_until(&ConvId::direct("nobody"), 30));
        assert_eq!(Store::open(&dir).history.unread(&conv), 1, "saved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only our own devices may say someone else wrote what they pass on.
    #[test]
    fn relayed_author_only_from_our_devices() {
        let relayed = |from: &str| {
            let mut w = wire(ConvId::direct("tales"));
            w.relayed = Some(Relayed { from: from.into(), device: "phone".into() });
            w
        };
        let plain = wire(ConvId::direct("tales"));
        for (w, caller, expected) in [
            (relayed("bruna"), "tales", ("bruna", "phone")),
            (relayed("bruna"), "mallory", ("mallory", "nb")),
            (relayed("tales"), "tales", ("tales", "nb")),
            (relayed("../x"), "tales", ("tales", "nb")),
            (plain.clone(), "tales", ("tales", "nb")),
            (plain, "bruna", ("bruna", "nb")),
        ] {
            assert_eq!(author_of(&w, caller, "nb", "tales"), (expected.0.to_string(), expected.1.to_string()), "caller {caller}");
        }
    }

    /// A passed-on message as the phone app reads and writes it.
    #[test]
    fn relayed_matches_phone_wire() {
        let mut w = wire(ConvId::direct("tales"));
        w.relayed = Some(Relayed { from: "bruna".into(), device: "phone".into() });
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["relayed"], serde_json::json!({"from": "bruna", "device": "phone"}));
        assert!(serde_json::to_value(wire(ConvId::direct("tales"))).unwrap().get("relayed").is_none(), "absent when not passed on");
    }

    #[test]
    fn call_conversation_follows_network() {
        assert_eq!(conversation_with(&lan_address("abc")), ConvId::lan("abc"));
        assert_eq!(conversation_with("bruna"), ConvId::direct("bruna"));
    }

    #[test]
    fn lan_accepts_only_our_messages() {
        let own = "K".repeat(43);
        let base = || Wire { conv: ConvId::lan(&own), ..wire(ConvId::lan(&own)) };
        let with = |f: &dyn Fn(&mut Wire)| {
            let mut w = base();
            f(&mut w);
            w
        };
        let offer = Offer { name: "a.txt".into(), size: 1, kind: FileKind::File, thumb: None, width: 0, height: 0, duration_ms: 0, wave: vec![], sha256: None };
        for (w, ok, case) in [
            (base(), true, "a message to us"),
            (with(&|w| w.conv = ConvId::lan(&"X".repeat(43))), false, "written to another device"),
            (with(&|w| w.conv = ConvId::direct(&own)), false, "not a local network conversation"),
            (with(&|w| w.file = Some(offer.clone())), true, "a file offered"),
            (with(&|w| w.file = Some(Offer { name: "../x".into(), ..offer.clone() })), false, "a file with a bad name"),
            (with(&|w| w.relayed = Some(Relayed { from: "bruna".into(), device: "nb".into() })), false, "passed on"),
            (with(&|w| w.id = "../x".into()), false, "a bad id"),
            (with(&|w| w.text = "x".repeat(MAX_TEXT + 1)), false, "too long"),
            (with(&|w| w.forwarded_from = Some("a b".into())), false, "a bad forwarded name"),
        ] {
            assert_eq!(lan_wire_ok(&w, &own), ok, "{case}");
        }
    }

    /// The read step as the phone app reads and writes it.
    #[test]
    fn read_step_matches_phone_wire() {
        let json = serde_json::to_value(Op::Read { at: 1_700_000_000_123 }).unwrap();
        assert_eq!(json, serde_json::json!({"type": "read", "at": 1_700_000_000_123_u64}));
    }
}
