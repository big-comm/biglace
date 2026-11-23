//! Sending files to one device, AirDrop style.
//!
//! - The sender picks files for a device: each is shared like a chat file
//!   (served from where it is, at `/v1/file/<id>/<offset>`, only to that
//!   device's account), and an offer goes to the device over the chat
//!   connection (`Op::Send`).
//! - The receiver accepts (into `~/Downloads/BigLace`, or a folder it picks)
//!   or declines. Accepted files come straight from the sender, resume after
//!   a dropped connection, and are checked against their SHA-256.
//! - Each step tells the sender how it went.
//! - A device paired on the local network takes the same steps over the
//!   pairing's connection, named by its device key instead of an account.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::chat::{self, files, Op};
use crate::tailscale;

/// Announced by versions that take files this way.
pub const CAPABILITY: &str = "send1";
/// An offer nobody answers is dropped after this long.
const OFFER_FOR: Duration = Duration::from_secs(10 * 60);
/// Files in one offer.
const MAX_ITEMS: usize = 200;

/// A step of a transfer, as it travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Offer,
    Accept,
    Decline,
    /// Everything arrived and checked.
    Done,
    /// The receiver couldn't get them (gone, damaged, interrupted).
    Failed,
    /// The sender withdrew the offer.
    Cancel,
}

/// One file of an offer: its share id, name, size and SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl Item {
    fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 64
            && self.id.bytes().all(|b| b.is_ascii_alphanumeric())
            && files::safe_name(&self.name)
            && self.size <= files::MAX_SIZE
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

/// What the window learns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// `from`'s `device` wants to send these files.
    Offered { id: String, from: String, device: String, items: Vec<Item> },
    /// Receiving: `done` of `total` bytes.
    Progress { id: String, from: String, done: u64, total: u64 },
    /// Received and checked, `count` files in `dir`.
    Received { id: String, from: String, dir: PathBuf, count: usize },
    /// Receiving stopped short.
    Failed { id: String, from: String, reason: String },
    /// The offer is over without us (withdrawn, expired, declined): drop its notification.
    Gone { id: String },
    /// Sending: `to` answered (accepted, declined, received, or couldn't get them).
    Answered { id: String, to: String, action: Action, count: usize },
}

static LISTENERS: crate::listeners::Listeners<TransferEvent> = crate::listeners::Listeners::new();

/// Where the window hears about transfers.
pub fn events() -> async_channel::Receiver<TransferEvent> {
    LISTENERS.subscribe()
}

fn emit(e: TransferEvent) {
    LISTENERS.send(e);
}

/// How the other device is reached: on the tailnet at its address, or paired on
/// the local network (the account then is its device key).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Tailnet(String),
    Lan,
}

/// An offer to us, waiting for an answer.
struct Incoming {
    from: String,
    route: Route,
    items: Vec<Item>,
    at: Instant,
}

/// An offer we made: to whom, with how many files.
struct Outgoing {
    to: String,
    count: usize,
}

fn incoming() -> &'static Mutex<HashMap<String, Incoming>> {
    static I: OnceLock<Mutex<HashMap<String, Incoming>>> = OnceLock::new();
    I.get_or_init(Mutex::default)
}

fn outgoing() -> &'static Mutex<HashMap<String, Outgoing>> {
    static O: OnceLock<Mutex<HashMap<String, Outgoing>>> = OnceLock::new();
    O.get_or_init(Mutex::default)
}

/// Hand `op` to `user`'s device along `route`, now.
fn send_op(route: &Route, user: &str, op: Op) -> Result<(), String> {
    match route {
        Route::Tailnet(ip) => chat::send_op_now(ip, user, op),
        #[cfg(target_os = "linux")]
        Route::Lan => chat::send_op_lan(user, op),
        #[cfg(not(target_os = "linux"))]
        Route::Lan => Err("no local network here".into()),
    }
}

fn tell(route: &Route, user: &str, id: &str, action: Action) {
    let (route, user, id) = (route.clone(), user.to_string(), id.to_string());
    let _ = std::thread::Builder::new().name("biglace-send-tell".into()).spawn(move || {
        let op = Op::Send { send: id, action, items: vec![] };
        if let Err(e) = send_op(&route, &user, op) {
            tailscale::dbg(&format!("send: {action:?} to {route:?}: {e}"));
        }
    });
}

/// Offer `paths` to `user`'s device at `ip`. Blocking (reads every file to
/// hash it): off the UI thread. The number of files offered.
pub fn send(ip: &str, user: &str, paths: &[PathBuf]) -> Result<usize, String> {
    offer(&Route::Tailnet(ip.to_string()), user, paths)
}

/// [`send`] to the device `key` paired on the local network.
pub fn send_lan(key: &str, paths: &[PathBuf]) -> Result<usize, String> {
    offer(&Route::Lan, key, paths)
}

fn offer(route: &Route, user: &str, paths: &[PathBuf]) -> Result<usize, String> {
    if paths.is_empty() || paths.len() > MAX_ITEMS {
        return Err(crate::trf!("Pick between 1 and {count} files.", "count" => MAX_ITEMS.to_string()));
    }
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        items.push(share(path, user)?);
    }
    let id = chat::new_id();
    let count = items.len();
    send_op(route, user, Op::Send { send: id.clone(), action: Action::Offer, items })?;
    outgoing()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, Outgoing { to: user.to_string(), count });
    Ok(count)
}

/// Serve `path` to `user`, and describe it for the offer.
fn share(path: &Path, user: &str) -> Result<Item, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(crate::trf!("{name} isn't a file.", "name" => name));
    }
    if meta.len() > files::MAX_SIZE {
        return Err(crate::trf!("{name} is too big (4 GB at most).", "name" => name));
    }
    if !files::safe_name(&name) {
        return Err(crate::trf!("{name} has a name that can't be sent.", "name" => name));
    }
    let item = Item { id: chat::new_id(), name, size: meta.len(), sha256: files::sha256_of(path)? };
    files::share(&item.id, path, item.size, &[user.to_string()]);
    Ok(item)
}

/// A transfer step from `sender`'s device `device` at `ip` (agent thread).
pub fn received(sender: &str, device: &str, ip: &str, id: &str, action: Action, items: &[Item]) {
    step(sender, device, Route::Tailnet(ip.to_string()), id, action, items);
}

/// A transfer step from the device `key` paired on the local network.
pub fn received_lan(key: &str, id: &str, action: Action, items: &[Item]) {
    step(key, "", Route::Lan, id, action, items);
}

fn step(sender: &str, device: &str, route: Route, id: &str, action: Action, items: &[Item]) {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return;
    }
    match action {
        Action::Offer => {
            if items.is_empty() || items.len() > MAX_ITEMS || !items.iter().all(Item::valid) {
                return;
            }
            let mut offers = incoming().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            offers.retain(|_, o| o.at.elapsed() < OFFER_FOR);
            if offers.contains_key(id) {
                return;
            }
            offers.insert(
                id.to_string(),
                Incoming { from: sender.to_string(), route, items: items.to_vec(), at: Instant::now() },
            );
            drop(offers);
            emit(TransferEvent::Offered { id: id.into(), from: sender.into(), device: device.into(), items: items.to_vec() });
            expire_later(id);
        }
        Action::Cancel => {
            let mut offers = incoming().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if offers.get(id).is_some_and(|o| o.from == sender) {
                offers.remove(id);
                drop(offers);
                emit(TransferEvent::Gone { id: id.into() });
            }
        }
        Action::Accept | Action::Decline | Action::Done | Action::Failed => {
            let mut sent = outgoing().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(offer) = sent.get(id).filter(|o| o.to == sender) else { return };
            let count = offer.count;
            if action != Action::Accept {
                sent.remove(id);
            }
            drop(sent);
            emit(TransferEvent::Answered { id: id.into(), to: sender.into(), action, count });
        }
    }
}

/// After [`OFFER_FOR`], an offer still unanswered is dropped.
fn expire_later(id: &str) {
    let id = id.to_string();
    let _ = std::thread::Builder::new().name("biglace-send-expire".into()).spawn(move || {
        std::thread::sleep(OFFER_FOR);
        let gone = incoming().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id).is_some();
        if gone {
            emit(TransferEvent::Gone { id });
        }
    });
}

/// Refuse offer `id`.
pub fn decline(id: &str) {
    let Some(offer) = incoming().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id) else { return };
    tell(&offer.route, &offer.from, id, Action::Decline);
    emit(TransferEvent::Gone { id: id.into() });
}

/// Take offer `id` into `dir` (None: `~/Downloads/BigLace`), in the
/// background; progress and the outcome arrive as events.
pub fn accept(id: &str, dir: Option<PathBuf>) {
    let Some(offer) = incoming().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id) else { return };
    let id = id.to_string();
    let dir = dir.unwrap_or_else(files::downloads_dir);
    tell(&offer.route, &offer.from, &id, Action::Accept);
    let _ = std::thread::Builder::new().name("biglace-receive".into()).spawn(move || {
        match fetch_all(&id, &offer, &dir) {
            Ok(count) => {
                tell(&offer.route, &offer.from, &id, Action::Done);
                emit(TransferEvent::Received { id, from: offer.from, dir, count });
            }
            Err(reason) => {
                tailscale::dbg(&format!("receive {id}: {reason}"));
                tell(&offer.route, &offer.from, &id, Action::Failed);
                emit(TransferEvent::Failed { id, from: offer.from, reason });
            }
        }
    });
}

/// Fetch and check every file of `offer` into `dir`; how many arrived.
fn fetch_all(id: &str, offer: &Incoming, dir: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let total: u64 = offer.items.iter().map(|i| i.size).sum();
    let mut before = 0u64;
    for item in &offer.items {
        let part = dir.join(format!(".biglace-{}.part", item.id));
        let mut progress = |done: u64| {
            emit(TransferEvent::Progress { id: id.into(), from: offer.from.clone(), done: before + done, total });
        };
        match &offer.route {
            Route::Tailnet(ip) => files::fetch_part(ip, &item.id, &part, item.size, &mut progress)?,
            Route::Lan => {
                files::lan_fetch_part(&offer.from, &item.id, &part, item.size, &mut progress)?;
            }
        }
        if !files::sha256_of(&part)?.eq_ignore_ascii_case(&item.sha256) {
            let _ = std::fs::remove_file(&part);
            return Err(crate::trf!("{name} arrived different from the original (damaged on the way).", "name" => item.name));
        }
        std::fs::rename(&part, files::unique_path(dir, &item.name)).map_err(|e| e.to_string())?;
        before += item.size;
    }
    Ok(offer.items.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form the phone app reads and writes.
    #[test]
    fn offer_matches_phone_wire() {
        let item = Item { id: "f1".into(), name: "foto.jpg".into(), size: 3, sha256: "a".repeat(64) };
        let op = Op::Send { send: "ab12".into(), action: Action::Offer, items: vec![item] };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "send", "send": "ab12", "action": "offer",
                "items": [{"id": "f1", "name": "foto.jpg", "size": 3, "sha256": "a".repeat(64)}]})
        );
        let answer: Op = serde_json::from_str(r#"{"type":"send","send":"ab12","action":"decline"}"#).unwrap();
        assert_eq!(answer, Op::Send { send: "ab12".into(), action: Action::Decline, items: vec![] });
    }

    /// Offered names and sizes are checked before anything is fetched.
    #[test]
    fn hostile_items_are_refused() {
        let good = Item { id: "f1".into(), name: "relatório.pdf".into(), size: 10, sha256: "0".repeat(64) };
        assert!(good.valid());
        let bad = [
            Item { name: "../.bashrc".into(), ..good.clone() },
            Item { name: "a/b".into(), ..good.clone() },
            Item { id: "../x".into(), ..good.clone() },
            Item { size: files::MAX_SIZE + 1, ..good.clone() },
            Item { sha256: "z".repeat(64), ..good.clone() },
            Item { sha256: "0".repeat(10), ..good.clone() },
        ];
        for item in bad {
            assert!(!item.valid(), "{item:?}");
        }
    }
}
