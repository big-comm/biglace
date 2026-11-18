//! The calling side of the agent: discover which peers run BigLace and what
//! they support, without ever blocking the UI.
//!
//! Probes run on background threads and land in a process-wide cache keyed
//! by the peer's tailnet IPv4. The window reads the cache when it builds the
//! peer list and asks for a re-render when a probe changes what it knows.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{Hello, APP_ID, PORT};
use crate::tailscale::Peer;

/// Re-probe a peer that answered after this long (it may have upgraded).
const FOUND_TTL: Duration = Duration::from_secs(300);
/// Re-probe a peer that didn't answer after this long (it may have
/// installed or started BigLace since).
const ABSENT_TTL: Duration = Duration::from_secs(120);
/// A peer without the agent just drops the SYN or resets; either way this
/// is plenty for a tailnet round trip, even through a relay.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
/// Concurrent probes per batch.
const PARALLELISM: usize = 6;
/// The hello answer is a few hundred bytes; never buffer more than this.
const MAX_BODY: u64 = 16 * 1024;

#[derive(Clone)]
struct Entry {
    at: Instant,
    hello: Option<Hello>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    /// Addresses with a probe currently running, so overlapping refreshes
    /// don't probe the same peer twice.
    in_flight: std::collections::HashSet<String>,
}

/// Clears a peer's in-flight mark when dropped — including on panic, so a
/// crashed probe can't leave the peer "being probed" (never probed again).
struct InFlight(String);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut c) = cache().lock() {
            c.in_flight.remove(&self.0);
        }
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// What we last learned about the agent at `ip`. `None` both for "no agent"
/// and "not probed yet" — either way the UI shows the classic actions only.
pub fn cached(ip: &str) -> Option<Hello> {
    cache().lock().ok()?.entries.get(ip)?.hello.clone()
}

fn is_stale(entry: Option<&Entry>, now: Instant) -> bool {
    match entry {
        None => true,
        Some(e) => {
            let ttl = if e.hello.is_some() { FOUND_TTL } else { ABSENT_TTL };
            now.duration_since(e.at) >= ttl
        }
    }
}

/// Probe, in the background, every online peer whose cache entry is stale.
/// `on_change` runs (on a worker thread) once per batch if any peer's answer
/// differs from what was cached — the window uses it to re-render.
pub fn refresh<F>(peers: &[Peer], on_change: F)
where
    F: Fn() + Send + 'static,
{
    let now = Instant::now();
    let targets: Vec<String> = {
        let Ok(mut c) = cache().lock() else { return };
        let wanted: Vec<String> = peers
            .iter()
            .filter(|p| p.online && !p.ipv4.is_empty())
            .map(|p| p.ipv4.clone())
            .filter(|ip| !c.in_flight.contains(ip) && is_stale(c.entries.get(ip), now))
            .collect();
        for ip in &wanted {
            c.in_flight.insert(ip.clone());
        }
        wanted
    };
    if targets.is_empty() {
        return;
    }
    // Each target owns its in-flight mark from here on: whichever way it
    // leaves (probed, panicked, never spawned) the mark is cleared.
    let targets: Vec<InFlight> = targets.into_iter().map(InFlight).collect();
    let _ = std::thread::Builder::new().name("biglace-probe".into()).spawn(move || {
        let queue = Arc::new(Mutex::new(targets));
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers = PARALLELISM.min(queue.lock().map(|q| q.len()).unwrap_or(1));
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let queue = queue.clone();
                let changed = changed.clone();
                std::thread::spawn(move || loop {
                    let Some(mark) = queue.lock().ok().and_then(|mut q| q.pop()) else {
                        return;
                    };
                    let ip = mark.0.clone();
                    let hello = probe(&ip);
                    if let Some(h) = &hello {
                        crate::wake::remember(&ip, &h.lan);
                    }
                    if let Ok(mut c) = cache().lock() {
                        let before = c.entries.get(&ip).and_then(|e| e.hello.clone());
                        if before != hello {
                            changed.store(true, std::sync::atomic::Ordering::Release);
                        }
                        c.entries.insert(
                            ip,
                            Entry {
                                at: Instant::now(),
                                hello,
                            },
                        );
                    }
                    // Clears the in-flight mark (takes the lock again).
                    drop(mark);
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        if changed.load(std::sync::atomic::Ordering::Acquire) {
            on_change();
        }
    });
}

/// Ask the agent on `ip` (a tailnet IPv4) who it is. Blocking.
pub fn probe(ip: &str) -> Option<Hello> {
    fetch_hello(&format!("http://{ip}:{PORT}/v1/hello"))
}

/// GET a hello document and validate it. Separate from [`probe`] so tests
/// can point it at a loopback server on an ephemeral port.
pub fn fetch_hello(url: &str) -> Option<Hello> {
    let agent = ureq::AgentBuilder::new()
        .timeout(PROBE_TIMEOUT)
        .redirects(0)
        .build();
    let response = agent.get(url).call().ok()?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut body)
        .ok()?;
    let hello: Hello = serde_json::from_slice(&body).ok()?;
    (hello.app == APP_ID).then_some(hello)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_peers_are_rechecked_less_often_than_absent_ones() {
        let now = Instant::now();
        let ago = |s: u64| now.checked_sub(Duration::from_secs(s)).expect("clock far from boot");
        let found = |at| Entry {
            at,
            hello: Some(Hello::local()),
        };
        let absent = |at| Entry { at, hello: None };

        assert!(is_stale(None, now));
        assert!(!is_stale(Some(&found(ago(200))), now));
        assert!(is_stale(Some(&found(ago(301))), now));
        assert!(!is_stale(Some(&absent(ago(60))), now));
        assert!(is_stale(Some(&absent(ago(121))), now));
    }

    #[test]
    fn a_foreign_service_on_the_port_is_not_an_agent() {
        // Something answering JSON that isn't a BigLace hello.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let body = r#"{"app":"other","protocol":1,"version":"1","os":"linux"}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        assert!(fetch_hello(&format!("http://{addr}/v1/hello")).is_none());
    }

    #[test]
    fn nobody_listening_is_answered_quickly_with_none() {
        // Grab a free port, then close it so nothing listens there.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let start = Instant::now();
        assert!(fetch_hello(&format!("http://{addr}/v1/hello")).is_none());
        assert!(start.elapsed() < PROBE_TIMEOUT);
    }
}
