//! The listening side of the agent.
//!
//! A supervisor thread keeps one HTTP server bound to this device's tailnet
//! IPv4 address: it binds when the tailnet comes up, rebinds if the address
//! changes, and closes the socket when the device disconnects. Nothing is
//! ever bound to `0.0.0.0`, so the agent is invisible from the LAN and the
//! internet.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{Hello, PORT};
use crate::tailscale::{self, Identity};

/// How often the supervisor re-checks the tailnet address.
const SUPERVISE_EVERY: Duration = Duration::from_secs(15);
/// The accept loop polls a non-blocking listener this often, which is also
/// the longest a shutdown waits. Polling (rather than a blocking `accept`
/// woken by a self-connect) is what lets the socket close promptly even
/// after the tailnet address has vanished from the machine.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// Requests handled at once; anything beyond gets a 503 instead of a thread.
const MAX_IN_FLIGHT: usize = 32;
/// Per-connection socket timeouts: a caller that stalls mid-request is cut.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Request line + headers. Nothing the agent accepts comes near this.
const MAX_HEAD: usize = 8 * 1024;
/// `whois` shells out to tailscale — cache answers per caller address.
const WHOIS_TTL: Duration = Duration::from_secs(60);
const WHOIS_NEGATIVE_TTL: Duration = Duration::from_secs(10);

/// Resolves a caller's address to a tailnet identity. Injected so tests can
/// run the real server on loopback without a tailnet.
pub type Authenticator = Arc<dyn Fn(IpAddr) -> Option<Identity> + Send + Sync>;

/// A bound listener plus its accept thread. Dropping it stops accepting and
/// closes the socket within [`ACCEPT_POLL`].
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
}

impl Running {
    /// Address actually bound (the port is resolved when binding to `:0`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Releases one in-flight slot when dropped — also on panic, so a crashing
/// handler can never leak capacity until the agent refuses everyone.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bind `addr` and serve until the returned handle is dropped.
pub fn serve(addr: SocketAddr, auth: Authenticator) -> io::Result<Running> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let bound = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let thread = std::thread::Builder::new()
        .name("biglace-agent".into())
        .spawn(move || {
            // The listener lives (and dies) with this thread.
            while !stop_flag.load(Ordering::Acquire) {
                let (stream, peer) = match listener.accept() {
                    Ok(conn) => conn,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        // WouldBlock (idle) or a transient error such as
                        // EMFILE: wait instead of spinning.
                        std::thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                };
                if in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    let _ = prepare(&stream).and_then(|_| respond(&stream, 503, "busy"));
                    continue;
                }
                let slot = Slot(in_flight.clone());
                let auth = auth.clone();
                // On spawn failure the closure (and its slot) is dropped,
                // which releases the slot.
                let _ = std::thread::Builder::new()
                    .name("biglace-agent-req".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle(stream, peer, &auth);
                    });
            }
        })?;
    Ok(Running {
        stop,
        thread: Some(thread),
        addr: bound,
    })
}

/// Accepted sockets inherit non-blocking mode on some platforms (Windows,
/// BSDs); handlers want plain blocking I/O bounded by timeouts.
fn prepare(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
}

fn handle(stream: TcpStream, peer: SocketAddr, auth: &Authenticator) {
    if prepare(&stream).is_err() {
        return;
    }
    let Some((head, method, path)) = read_request_head(&stream) else {
        let _ = respond(&stream, 400, "bad request");
        return;
    };
    let identity = auth(peer.ip());
    // Chat messages from another device.
    if path == crate::chat::PATH {
        match (&identity, method.as_str()) {
            (Some(id), "GET") => crate::chat::accept(stream, peer.ip(), &parse_headers(&head), id, &path),
            _ => {
                let _ = respond(&stream, 403, "forbidden");
            }
        }
        return;
    }
    // Our profile photo (anyone the network lets reach us).
    if path == crate::avatars::PATH || path == crate::avatars::HASH_PATH {
        match (&identity, method.as_str()) {
            (Some(_), "GET") => crate::avatars::serve(stream, &path),
            _ => {
                let _ = respond(&stream, 403, "forbidden");
            }
        }
        return;
    }
    // A file someone was sent in a chat, fetched from here.
    if path.starts_with(crate::chat::files::PATH_PREFIX) {
        match (&identity, method.as_str()) {
            (Some(id), "GET") => crate::chat::files::serve(stream, peer.ip(), &parse_headers(&head), id, &path),
            _ => {
                let _ = respond(&stream, 403, "forbidden");
            }
        }
        return;
    }
    // Someone in our voice room watches the screen we're showing.
    if path.starts_with(crate::room::STAGE_PREFIX) {
        match (&identity, method.as_str()) {
            (Some(id), "GET") => crate::room::stage::accept(stream, peer.ip(), &parse_headers(&head), id, &path),
            _ => {
                let _ = respond(&stream, 403, "forbidden");
            }
        }
        return;
    }
    // A voice-room peer keeps the connection too.
    if path.starts_with(crate::room::PATH_PREFIX) {
        match (&identity, method.as_str()) {
            (Some(id), "GET") => crate::room::accept(stream, peer.ip(), &parse_headers(&head), id, &path),
            _ => {
                let _ = respond(&stream, 403, "forbidden");
            }
        }
        return;
    }
    // A remote-screen session keeps the connection: hand it over.
    if path == crate::remote::SCREEN_PATH {
        match (&identity, method.as_str()) {
            (Some(id), "GET") => crate::remote::host::serve(stream, peer.ip(), &parse_headers(&head), id.clone()),
            (None, _) => {
                let _ = respond(&stream, 403, "forbidden");
            }
            _ => {
                let _ = respond(&stream, 405, "method not allowed");
            }
        }
        return;
    }
    let (status, body) = route(&method, &path, identity.as_ref());
    if let Some(id) = &identity {
        tailscale::dbg(&format!(
            "agent: {method} {path} from {}@{} → {status}",
            id.login, id.device
        ));
    }
    let _ = respond(&stream, status, &body);
}

/// Read the request head and return `(head, method, path-without-query)`.
/// Bodies are not read: no route takes one.
fn read_request_head(mut stream: &TcpStream) -> Option<(Vec<u8>, String, String)> {
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return None;
        }
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let (method, path) = parse_request_line(&head)?;
    Some((head, method, path))
}

/// Header fields of a request head, names lowercased. Repeated or malformed
/// lines are ignored (the first value wins).
fn parse_headers(head: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(text) = std::str::from_utf8(head) else {
        return out;
    };
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            out.entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    out
}

fn parse_request_line(head: &[u8]) -> Option<(String, String)> {
    let line_end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&head[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next().filter(|m| !m.is_empty())?;
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return None;
    }
    let path = target.split('?').next().unwrap_or(target);
    Some((method.to_string(), path.to_string()))
}

fn respond(mut stream: &TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let content_type = if status == 200 {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Pure routing: method + path + who's asking → status + body. Every route
/// requires a tailnet identity; unauthenticated callers learn nothing, not
/// even which paths exist.
fn route(method: &str, path: &str, identity: Option<&Identity>) -> (u16, String) {
    if identity.is_none() {
        return (403, "forbidden".into());
    }
    // The panel, or a peer this device asked for its screen, checking that
    // the request was signed by this device's key (see device_key). Any
    // tailnet caller may ask: a proof only says "the BigLace at this
    // address holds key K", which is public, and can't be replayed as a
    // signed request (different message prefix).
    if let Some(challenge) = path.strip_prefix("/v1/proof/") {
        if method != "GET" {
            return (405, "method not allowed".into());
        }
        return match crate::device_key::prove(challenge) {
            Some(json) => (200, json),
            None => (400, "bad request".into()),
        };
    }
    match (method, path) {
        ("GET", "/v1/hello") => match serde_json::to_string(&Hello::local()) {
            Ok(json) => (200, json),
            Err(_) => (500, "internal error".into()),
        },
        (_, "/v1/hello") => (405, "method not allowed".into()),
        _ => (404, "not found".into()),
    }
}

// ─── Tailnet identity ────────────────────────────────────────────────────────

type WhoisCache = Mutex<HashMap<IpAddr, (Instant, Option<Identity>)>>;

fn whois_cache() -> &'static WhoisCache {
    static CACHE: OnceLock<WhoisCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `tailscale whois`, cached. Only tailnet addresses are ever looked up.
fn tailnet_identity(ip: IpAddr) -> Option<Identity> {
    if !is_tailnet_addr(ip) {
        return None;
    }
    let now = Instant::now();
    if let Some((at, id)) = whois_cache().lock().ok()?.get(&ip) {
        let ttl = if id.is_some() { WHOIS_TTL } else { WHOIS_NEGATIVE_TTL };
        if now.duration_since(*at) < ttl {
            return id.clone();
        }
    }
    let id = tailscale::whois(ip);
    if let Ok(mut cache) = whois_cache().lock() {
        cache.retain(|_, (at, _)| now.duration_since(*at) < WHOIS_TTL);
        cache.insert(ip, (now, id.clone()));
    }
    id
}

/// 100.64.0.0/10 (CGNAT, tailnet IPv4) or fd7a:115c:a1e0::/48 (tailnet IPv6).
pub fn is_tailnet_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

// ─── Supervisor ──────────────────────────────────────────────────────────────

/// Start the agent for the lifetime of the process. Idempotent.
///
/// Linux only for now: on Windows, the first non-loopback listener pops a
/// Defender Firewall prompt out of nowhere. That platform waits until the
/// installer ships a firewall rule scoped to the tailnet range.
pub fn start() {
    if !cfg!(target_os = "linux") {
        return;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let auth: Authenticator = Arc::new(tailnet_identity);
    let _ = std::thread::Builder::new()
        .name("biglace-agent-supervisor".into())
        .spawn(move || {
            let mut current: Option<Running> = None;
            loop {
                let wanted = local_tailnet_v4().map(|ip| SocketAddr::new(IpAddr::V4(ip), PORT));
                let bound = current.as_ref().map(Running::addr);
                if wanted != bound {
                    // Drop first: closes the old socket before binding anew.
                    current = None;
                    if let Some(addr) = wanted {
                        match serve(addr, auth.clone()) {
                            Ok(running) => {
                                tailscale::dbg(&format!("agent: listening on {addr}"));
                                current = Some(running);
                            }
                            Err(e) => tailscale::dbg(&format!("agent: bind {addr} failed: {e}")),
                        }
                    }
                }
                std::thread::sleep(SUPERVISE_EVERY);
            }
        });
}

fn local_tailnet_v4() -> Option<Ipv4Addr> {
    let status = tailscale::get_status();
    if !status.online {
        return None;
    }
    status
        .ip
        .as_deref()
        .and_then(|ip| ip.parse::<Ipv4Addr>().ok())
        .filter(|ip| is_tailnet_addr(IpAddr::V4(*ip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn someone() -> Identity {
        Identity {
            login: "bruno".into(),
            display_name: "Bruno".into(),
            device: "celbruno".into(),
        }
    }

    #[test]
    fn key_proofs_need_a_tailnet_identity_and_a_valid_challenge() {
        let challenge = "A".repeat(43);
        let path = format!("/v1/proof/{challenge}");
        assert_eq!(route("GET", &path, None).0, 403);
        assert_eq!(route("POST", &path, Some(&someone())).0, 405);
        assert_eq!(route("GET", "/v1/proof/short", Some(&someone())).0, 400);
        assert_eq!(route("GET", &path, Some(&someone())).0, 200);
    }

    #[test]
    fn headers_are_parsed_case_insensitively() {
        let h = parse_headers(b"GET /v1/screen HTTP/1.1\r\nHost: 100.64.0.3:41980\r\nX-BigLace-Key: abc\r\nbad line\r\n\r\n");
        assert_eq!(h.get("host").map(String::as_str), Some("100.64.0.3:41980"));
        assert_eq!(h.get("x-biglace-key").map(String::as_str), Some("abc"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn every_route_requires_a_tailnet_identity() {
        assert_eq!(route("GET", "/v1/hello", None).0, 403);
        assert_eq!(route("GET", "/nope", None).0, 403);
        let (status, body) = route("GET", "/v1/hello", Some(&someone()));
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Hello>(&body).unwrap(), Hello::local());
        assert_eq!(route("POST", "/v1/hello", Some(&someone())).0, 405);
        assert_eq!(route("GET", "/v1/other", Some(&someone())).0, 404);
    }

    #[test]
    fn only_tailnet_ranges_count_as_tailnet() {
        let yes = ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::5"];
        let no = ["100.63.255.255", "100.128.0.1", "192.168.0.10", "127.0.0.1", "fd7a:115c:a1e1::5", "::1"];
        for ip in yes {
            assert!(is_tailnet_addr(ip.parse().unwrap()), "{ip}");
        }
        for ip in no {
            assert!(!is_tailnet_addr(ip.parse().unwrap()), "{ip}");
        }
        // Loopback is never looked up, so it can never authenticate.
        assert!(tailnet_identity("127.0.0.1".parse().unwrap()).is_none());
    }

    /// Against the real tailnet: binds this machine's tailnet address and
    /// asks the real `tailscale whois`. Run with
    /// `cargo test -- --ignored live_agent_answers_on_the_tailnet`.
    #[test]
    #[ignore = "needs a connected tailnet"]
    fn live_agent_answers_on_the_tailnet() {
        let ip = local_tailnet_v4().expect("this device is on a tailnet");
        let auth: Authenticator = Arc::new(tailnet_identity);
        let running = serve(SocketAddr::new(IpAddr::V4(ip), 0), auth).unwrap();
        let url = format!("http://{}/v1/hello", running.addr());
        assert_eq!(super::super::client::fetch_hello(&url), Some(Hello::local()));
        // The caller was identified by whois, not trusted blindly.
        let me = tailnet_identity(IpAddr::V4(ip)).expect("whois knows this device");
        assert!(!me.login.is_empty() && !me.device.is_empty());
        eprintln!("live: {} answered for {}@{}", running.addr(), me.login, me.device);
        // Never reachable on loopback: it is bound to the tailnet address only.
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), running.addr().port());
        assert!(TcpStream::connect_timeout(&loopback, Duration::from_millis(200)).is_err());

        // `BIGLACE_LIVE_HOLD_SECS=60` keeps an agent up on the real port so
        // another device can call it (cross-device check by hand).
        if let Some(secs) = std::env::var("BIGLACE_LIVE_HOLD_SECS").ok().and_then(|v| v.parse().ok()) {
            let _held = serve(SocketAddr::new(IpAddr::V4(ip), PORT), Arc::new(tailnet_identity)).unwrap();
            eprintln!("live: holding {ip}:{PORT} for {secs}s");
            std::thread::sleep(Duration::from_secs(secs));
        }
    }

    #[test]
    fn served_over_http_with_identity_checked_per_caller() {
        // Loopback stands in for the tailnet: the injected authenticator
        // accepts it, so this exercises the real socket, parser and handler.
        let accept: Authenticator = Arc::new(|ip: IpAddr| ip.is_loopback().then(someone));
        let running = serve("127.0.0.1:0".parse().unwrap(), accept).unwrap();
        let url = format!("http://{}/v1/hello", running.addr());
        let hello = super::super::client::fetch_hello(&url).expect("agent answers");
        assert_eq!(hello, Hello::local());

        let deny: Authenticator = Arc::new(|_| None);
        let closed = serve("127.0.0.1:0".parse().unwrap(), deny).unwrap();
        let url = format!("http://{}/v1/hello", closed.addr());
        assert!(super::super::client::fetch_hello(&url).is_none());

        // Dropping the handle closes the socket at once — and the port can
        // be bound again right away (the reconnect path of the supervisor).
        let addr = running.addr();
        let started = Instant::now();
        drop(running);
        assert!(started.elapsed() < ACCEPT_POLL * 3, "shutdown must not block");
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err());
        let again = serve(addr, Arc::new(|_| None)).expect("port free after drop");
        drop(again);
    }

    #[test]
    fn request_lines_are_parsed_strictly() {
        let ok = |s: &str| parse_request_line(s.as_bytes());
        assert_eq!(
            ok("GET /v1/hello?x=1 HTTP/1.1\r\nHost: a\r\n\r\n"),
            Some(("GET".into(), "/v1/hello".into()))
        );
        assert_eq!(ok("GET /v1/hello HTTP/1.0\r\n\r\n").map(|r| r.1), Some("/v1/hello".into()));
        assert!(ok("GET http://evil/v1/hello HTTP/1.1\r\n\r\n").is_none());
        assert!(ok("GET /v1/hello\r\n\r\n").is_none());
        assert!(ok("GET /a b HTTP/1.1\r\n\r\n").is_none());
        assert!(ok("GET /v1/hello SPDY/3\r\n\r\n").is_none());
        assert!(ok("\r\n\r\n").is_none());
    }

    #[test]
    fn oversized_or_garbage_requests_get_400_and_slots_are_released() {
        let accept: Authenticator = Arc::new(|ip: IpAddr| ip.is_loopback().then(someone));
        let running = serve("127.0.0.1:0".parse().unwrap(), accept).unwrap();
        let send = |payload: &[u8]| -> String {
            let mut s = TcpStream::connect(running.addr()).unwrap();
            let _ = s.write_all(payload);
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        };
        assert!(send(&vec![b'A'; MAX_HEAD + 10]).starts_with("HTTP/1.1 400"));
        assert!(send(b"hello there\r\n\r\n").starts_with("HTTP/1.1 400"));
        // Far more requests than MAX_IN_FLIGHT, sequentially: if slots
        // leaked, the tail would be answered 503.
        for _ in 0..(MAX_IN_FLIGHT * 2) {
            let reply = send(b"GET /v1/hello HTTP/1.1\r\n\r\n");
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            assert!(reply.contains("Content-Type: application/json"));
        }
    }
}
