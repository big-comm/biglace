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
            talk(&mut s, &peer);
        }
    }
}

/// Messages from a paired device, each acknowledged once kept.
fn talk(s: &mut (impl Read + Write), peer: &Peer) {
    while let Ok((Kind::Chat, payload)) = read_frame(s) {
        let Some(id) = crate::chat::accept_lan(&peer.key, &payload) else { break };
        let ack = serde_json::to_vec(&[id]).unwrap_or_default();
        if write_frame(s, Kind::ChatAck, &ack).is_err() {
            break;
        }
    }
}

/// Send `payloads` (chat wires) to the paired device `key`, if it's nearby;
/// the ids it acknowledged.
pub fn deliver(key: &str, payloads: &[Vec<u8>]) -> Result<HashSet<String>, String> {
    let me = current().ok_or("the local network is off")?;
    let found = nearby().lock().ok().and_then(|n| n.iter().find(|d| d.key == key).map(|d| (d.address.clone(), d.port)));
    let remembered = || peers::list().into_iter().find(|p| p.key == key).and_then(|p| p.address).map(|a| (a, super::PORT));
    let (address, port) = found.or_else(remembered).ok_or("not nearby")?;
    let addr: SocketAddr = format!("{address}:{port}").parse().map_err(|_| "bad address")?;
    let mut s = connect(addr).ok_or("unreachable")?;
    let fingerprint = tls::peer_fingerprint(&s).map_err(|e| e.to_string())?;
    handshake::client(&mut s, &fingerprint, &me.key, &me.name, Purpose::Talk, Some(key)).map_err(|e| e.to_string())?;
    for payload in payloads {
        write_frame(&mut s, Kind::Chat, payload).map_err(|e| e.to_string())?;
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
