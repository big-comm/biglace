//! The local network running: listening on [`super::PORT`], announcing us
//! and listing the others, and pairing both ways. Whoever started a pairing
//! or got asked, both people see the code and answer.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ed25519_dalek::SigningKey;

use super::discovery::{self, Nearby};
use super::handshake::{self, Peer, Purpose};
use super::{peers, tls};
use crate::remote::{read_frame, write_frame, Kind};

/// How long a person has to answer a pairing code.
const ANSWER_WAIT: Duration = Duration::from_secs(120);
/// Reads on the network give up after this, so a silent peer can't hold a thread.
const READ_WAIT: Duration = Duration::from_secs(30);
/// A file transfer silent this long is given up, to be resumed on a fresh connection.
const STALL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub enum LanEvent {
    /// The other BigLace devices on the network now.
    Nearby(Vec<Nearby>),
    /// Pairing with `name`: show `code`, and send the person's answer.
    Code { name: String, code: String, answer: std::sync::mpsc::SyncSender<bool> },
    /// Paired with `name`, whose device key is `key`.
    Paired { key: String, name: String },
    /// A pairing that didn't happen (refused, timed out, broken off).
    PairFailed { name: String },
}

static LISTENERS: crate::listeners::Listeners<LanEvent> = crate::listeners::Listeners::new();

pub fn events() -> async_channel::Receiver<LanEvent> {
    LISTENERS.subscribe()
}

/// Who we are on the network and how we prove it.
struct Me {
    key: SigningKey,
    name: String,
    /// Set by [`stop`]: the listener and the announcement end.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// Ends the announcement and the browsing.
    quit: async_channel::Sender<()>,
}

fn me() -> &'static Mutex<Option<Arc<Me>>> {
    static ME: OnceLock<Mutex<Option<Arc<Me>>>> = OnceLock::new();
    ME.get_or_init(|| Mutex::new(None))
}

fn current() -> Option<Arc<Me>> {
    me().lock().ok()?.clone()
}

/// The devices nearby now, where [`deliver`] finds them.
fn nearby() -> &'static Mutex<Vec<Nearby>> {
    static NEARBY: Mutex<Vec<Nearby>> = Mutex::new(Vec::new());
    &NEARBY
}

/// Start, once: listen, announce us as `name` and follow the others.
pub fn start(name: &str) -> anyhow::Result<()> {
    let key = crate::device_key::signing_key().ok_or_else(|| anyhow::anyhow!("no device key"))?;
    let public = crate::device_key::public_key().ok_or_else(|| anyhow::anyhow!("no device key"))?;
    let mut slot = me().lock().map_err(|_| anyhow::anyhow!("lan state poisoned"))?;
    if slot.is_some() {
        return Ok(());
    }
    let server = Arc::new(tls::server()?);
    let listener = TcpListener::bind(("0.0.0.0", super::PORT))?;
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (quit, quitting) = async_channel::bounded::<()>(1);
    *slot = Some(Arc::new(Me { key, name: name.to_string(), stopping: stopping.clone(), quit }));
    drop(slot);
    std::thread::Builder::new().name("biglace-lan".into()).spawn(move || {
        for tcp in listener.incoming().flatten() {
            if stopping.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            let server = server.clone();
            let _ = std::thread::Builder::new().name("biglace-lan-in".into()).spawn(move || incoming(tcp, &server));
        }
    })?;
    let name = name.to_string();
    std::thread::Builder::new().name("biglace-lan-mdns".into()).spawn(move || {
        async_io::block_on(async {
            let _announced = discovery::announce(&public, &name)
                .await
                .inspect_err(|e| crate::tailscale::dbg(&format!("lan: not announced: {e}")))
                .ok();
            let browsing = Box::pin(discovery::browse(&public, |list| {
                if let Ok(mut n) = nearby().lock() {
                    n.clone_from(&list);
                }
                LISTENERS.send(LanEvent::Nearby(list));
                // Someone may be back with messages waiting for them.
                crate::chat::kick();
            }));
            if let futures_util::future::Either::Left((Err(e), _)) = futures_util::future::select(browsing, Box::pin(quitting.recv())).await {
                crate::tailscale::dbg(&format!("lan: not browsing: {e}"));
            }
        });
    })?;
    Ok(())
}

/// Stop listening and announcing us (pairings stay).
pub fn stop() {
    let Some(me) = me().lock().ok().and_then(|mut slot| slot.take()) else { return };
    me.stopping.store(true, std::sync::atomic::Ordering::Release);
    let _ = me.quit.try_send(());
    // The listener only looks at `stopping` when someone comes in.
    let _ = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], super::PORT)), Duration::from_secs(1));
    LISTENERS.send(LanEvent::Nearby(vec![]));
}

/// Show `code`, wait for our person's answer, then trade answers with the
/// peer; paired only when both said yes.
fn settle(s: &mut (impl Read + Write), peer: &Peer, code: String, address: String) {
    let (answer, answered) = std::sync::mpsc::sync_channel(1);
    LISTENERS.send(LanEvent::Code { name: peer.name.clone(), code, answer });
    let ours = answered.recv_timeout(ANSWER_WAIT).unwrap_or(false);
    let paired = handshake::agree(s, ours).unwrap_or(false);
    if paired && peers::add(peers::Paired { key: peer.key.clone(), name: peer.name.clone(), since: crate::chat::now_ms() / 1000, forgotten: false, address: Some(address) }).is_ok() {
        LISTENERS.send(LanEvent::Paired { key: peer.key.clone(), name: peer.name.clone() });
    } else {
        LISTENERS.send(LanEvent::PairFailed { name: peer.name.clone() });
    }
}

fn incoming(tcp: TcpStream, server: &tls::Server) {
    let Some(me) = current() else { return };
    let address = tcp.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    // The person has two minutes to answer; the peer's reads wait as long.
    let _ = tcp.set_read_timeout(Some(ANSWER_WAIT + READ_WAIT));
    let Ok(mut s) = server.acceptor.accept(tcp) else { return };
    let admit = |key: &str, purpose: Purpose| purpose == Purpose::Pair || peers::is_paired(key);
    let Ok((peer, purpose)) = handshake::server(&mut s, &server.fingerprint, &me.key, &me.name, admit) else { return };
    match purpose {
        Purpose::Pair => match handshake::pair_server(&mut s, &server.fingerprint, &me.key, &peer) {
            Ok(code) => settle(&mut s, &peer, code, address),
            Err(_) => LISTENERS.send(LanEvent::PairFailed { name: peer.name }),
        },
        Purpose::Talk => {
            peers::seen_at(&peer.key, &address);
            let _ = s.get_ref().set_read_timeout(Some(READ_WAIT));
            let _ = s.get_ref().set_write_timeout(Some(STALL));
            talk(&mut s, &peer);
        }
    }
}

/// A paired device asks for the file it was offered in message `file`, from `offset` on.
#[derive(serde::Serialize, serde::Deserialize)]
struct FileAsk {
    file: String,
    offset: u64,
}

/// The answer before the bytes: how many follow and the whole file's SHA-256
/// (hex; the message carries none), or why none.
#[derive(serde::Serialize, serde::Deserialize)]
struct FileAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A paired device asks for our profile photo, naming the version it has (empty: none).
#[derive(serde::Serialize, serde::Deserialize)]
struct PhotoAsk {
    photo: String,
}

/// Our photo's version, and its size when the bytes follow (the asker has another
/// version); no version without a photo. Older versions answer without one.
#[derive(serde::Serialize, serde::Deserialize)]
struct PhotoAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    photo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

const GONE: &str = "gone";

/// Messages from a paired device, each acknowledged once kept, or one file or our photo it asks for.
fn talk(s: &mut (impl Read + Write), peer: &Peer) {
    loop {
        match read_frame(s) {
            Ok((Kind::Chat, payload)) => {
                let Some(id) = crate::chat::accept_lan(&peer.key, &payload) else { break };
                let ack = serde_json::to_vec(&[id]).unwrap_or_default();
                if write_frame(s, Kind::ChatAck, &ack).is_err() {
                    break;
                }
            }
            Ok((Kind::Lan, payload)) => {
                if let Ok(ask) = serde_json::from_slice::<FileAsk>(&payload) {
                    send_file(s, &peer.key, &ask);
                } else if let Ok(ask) = serde_json::from_slice::<PhotoAsk>(&payload) {
                    send_photo(s, &ask);
                }
                break;
            }
            _ => break,
        }
    }
}

fn send_file(s: &mut (impl Read + Write), who: &str, ask: &FileAsk) {
    use crate::chat::files::{open_share, ShareError};
    match open_share(&ask.file, who, false, ask.offset) {
        Ok((file, left)) => {
            let sha256 = crate::chat::files::share_sha256(&ask.file);
            if handshake::send(s, &FileAnswer { size: Some(left), sha256, error: None }).is_ok() {
                let _ = std::io::copy(&mut std::io::Read::take(file, left), s);
                let _ = s.flush();
            }
        }
        Err(e) => {
            let why = if e == ShareError::Gone { GONE } else { "missing" };
            let _ = handshake::send(s, &FileAnswer { size: None, sha256: None, error: Some(why.into()) });
        }
    }
}

fn send_photo(s: &mut (impl Read + Write), ask: &PhotoAsk) {
    let photo = crate::avatars::own_photo();
    let version = photo.as_deref().map(crate::avatars::hash_of);
    let fresh = photo.filter(|_| version.as_deref() != Some(ask.photo.as_str()));
    let answer = PhotoAnswer { photo: version, size: fresh.as_ref().map(|b| b.len() as u64) };
    if handshake::send(s, &answer).is_ok() {
        if let Some(bytes) = fresh {
            let _ = s.write_all(&bytes).and_then(|()| s.flush());
        }
    }
}

/// Why a paired device wasn't reached, or wouldn't send a file.
pub(crate) enum LanError {
    /// The local network is off here.
    Off,
    /// The device is out of reach, or isn't the paired one.
    Unreachable,
    /// It answered that it won't send the file.
    Refused(crate::chat::files::ShareError),
}

/// Fetch (the rest of) file `id` from the paired device `key` into `part`,
/// which holds `size` once whole: how many bytes it holds now, also when
/// the transfer broke off (another call resumes it), and the SHA-256 the
/// sender gave, if any.
pub(crate) fn fetch_file(key: &str, id: &str, part: &std::path::Path, size: u64, progress: &mut dyn FnMut(u64)) -> Result<(u64, Option<String>), LanError> {
    use crate::chat::files::ShareError;
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0).min(size);
    let mut s = talk_to(key)?;
    // The sender may hash a large file before it answers.
    let _ = s.get_ref().set_read_timeout(Some(READ_WAIT));
    handshake::send(&mut s, &FileAsk { file: id.to_string(), offset: have }).map_err(|_| LanError::Unreachable)?;
    let answer: FileAnswer = handshake::recv(&mut s).map_err(|_| LanError::Unreachable)?;
    let _ = s.get_ref().set_read_timeout(Some(STALL));
    match (answer.size, answer.error.as_deref()) {
        (Some(left), None) if have + left == size => {
            let held = crate::chat::files::append_part(&mut s, part, have, size, progress)
                .unwrap_or_else(|_| std::fs::metadata(part).map(|m| m.len()).unwrap_or(have));
            Ok((held, answer.sha256))
        }
        (_, Some(GONE)) => Err(LanError::Refused(ShareError::Gone)),
        _ => Err(LanError::Refused(ShareError::Missing)),
    }
}

/// A paired device's profile photo: its version, and the bytes when ours differ.
pub(crate) struct Photo {
    pub version: String,
    pub bytes: Option<Vec<u8>>,
}

/// What the paired device `key` says about its profile photo, given the version
/// we hold (empty: none): `None` when it has none. Bytes are at most `max`.
pub(crate) fn fetch_photo(key: &str, have: &str, max: u64) -> Result<Option<Photo>, LanError> {
    let mut s = talk_to(key)?;
    let _ = s.get_ref().set_read_timeout(Some(STALL));
    handshake::send(&mut s, &PhotoAsk { photo: have.to_string() }).map_err(|_| LanError::Unreachable)?;
    let answer: PhotoAnswer = handshake::recv(&mut s).map_err(|_| LanError::Unreachable)?;
    let Some(version) = answer.photo else { return Ok(None) };
    let bytes = match answer.size {
        Some(n) if n <= max => {
            let mut b = vec![0; usize::try_from(n).map_err(|_| LanError::Unreachable)?];
            s.read_exact(&mut b).map_err(|_| LanError::Unreachable)?;
            Some(b)
        }
        Some(_) => return Err(LanError::Unreachable),
        None => None,
    };
    Ok(Some(Photo { version, bytes }))
}

/// A proven TLS connection to the paired device `key`, at its announced
/// address or, failing that, where it was last.
fn talk_to(key: &str) -> Result<openssl::ssl::SslStream<TcpStream>, LanError> {
    let me = current().ok_or(LanError::Off)?;
    let found = nearby().lock().ok().and_then(|n| n.iter().find(|d| d.key == key).map(|d| format!("{}:{}", d.address, d.port)));
    let remembered = peers::list().into_iter().find(|p| p.key == key).and_then(|p| p.address).map(|a| format!("{a}:{}", super::PORT));
    let mut places: Vec<String> = found.into_iter().chain(remembered).collect();
    places.dedup();
    for place in places {
        let Ok(addr) = place.parse::<SocketAddr>() else { continue };
        let Some(mut s) = connect(addr) else { continue };
        let Ok(fingerprint) = tls::peer_fingerprint(&s) else { continue };
        if handshake::client(&mut s, &fingerprint, &me.key, &me.name, Purpose::Talk, Some(key)).is_ok() {
            return Ok(s);
        }
    }
    Err(LanError::Unreachable)
}

/// Send `payloads` (chat wires) to the paired device `key`, if it's nearby;
/// the ids it acknowledged.
pub(crate) fn deliver(key: &str, payloads: &[Vec<u8>]) -> Result<HashSet<String>, LanError> {
    let mut s = talk_to(key)?;
    let _ = s.get_ref().set_read_timeout(Some(READ_WAIT));
    for payload in payloads {
        write_frame(&mut s, Kind::Chat, payload).map_err(|_| LanError::Unreachable)?;
    }
    let mut acked = HashSet::new();
    while acked.len() < payloads.len() {
        match read_frame(&mut s) {
            Ok((Kind::ChatAck, payload)) => acked.extend(serde_json::from_slice::<Vec<String>>(&payload).unwrap_or_default()),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(acked)
}

/// What the phone scans to pair with us: our address on the local network,
/// port and device key; `None` without a local network address.
pub fn link() -> Option<String> {
    let key = crate::device_key::public_key()?;
    // Connecting a UDP socket sends nothing: it only picks the interface toward the local network.
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("224.0.0.251:5353").ok()?;
    let ip = probe.local_addr().ok()?.ip().to_string();
    discovery::on_this_network(&ip).then(|| format!("{LINK_PREFIX}{ip}:{}:{key}", super::PORT))
}

/// Starts what [`link`] gives.
pub const LINK_PREFIX: &str = "biglace-lan:";

/// TLS to the BigLace at `addr` (who it is gets proven by the handshake).
fn connect(addr: SocketAddr) -> Option<openssl::ssl::SslStream<TcpStream>> {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    tcp.set_read_timeout(Some(ANSWER_WAIT + READ_WAIT)).ok()?;
    let mut config = tls::connector().ok()?.configure().ok()?;
    config.set_verify_hostname(false);
    config.connect("biglace", tcp).ok()
}

/// Ask `nearby` to pair (on a thread of its own); `nearby.key` empty when
/// it's not known yet.
pub fn pair_with(nearby: Nearby) {
    let _ = std::thread::Builder::new().name("biglace-lan-pair".into()).spawn(move || {
        let failed = || LISTENERS.send(LanEvent::PairFailed { name: nearby.name.clone() });
        let Some(me) = current() else { return failed() };
        let Ok(addr) = format!("{}:{}", nearby.address, nearby.port).parse::<SocketAddr>() else { return failed() };
        let Some(mut s) = connect(addr) else { return failed() };
        let Ok(fingerprint) = tls::peer_fingerprint(&s) else { return failed() };
        let Ok(peer) = handshake::client(&mut s, &fingerprint, &me.key, &me.name, Purpose::Pair, None) else { return failed() };
        if !nearby.key.is_empty() && peer.key != nearby.key {
            // It announced one key and proved another.
            return failed();
        }
        match handshake::pair_client(&mut s, &fingerprint, &me.key, &peer) {
            Ok(code) => settle(&mut s, &peer, code, nearby.address.clone()),
            Err(_) => failed(),
        }
    });
}
