//! "Turn on" (Wake-on-LAN): a switched-off computer is woken by a magic
//! packet sent on its own local network — by this computer when it's on the
//! same network, otherwise by another BigLace computer that is (asked over
//! the tunnel, `/v1/wake/<mac>`).
//!
//! While a computer is on, its BigLace announces its wired network cards
//! (hardware address and local network) in `/v1/hello`; the others keep
//! that, so they still know it once it's off. The computer must have
//! Wake-on-LAN turned on (BIOS/UEFI and network card) and a cable.

use std::collections::HashMap;
use std::net::{Ipv4Addr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Announced by computers that can send a magic packet for others.
pub const CAPABILITY: &str = "wake";
pub const PATH_PREFIX: &str = "/v1/wake/";
/// Magic packets sent for others, per minute.
const RATE_PER_MINUTE: usize = 20;

/// One wired network card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lan {
    /// `aa:bb:cc:dd:ee:ff`.
    pub mac: String,
    pub ip: String,
    pub prefix: u8,
}

fn network(ip: &str, prefix: u8) -> Option<u32> {
    let ip: Ipv4Addr = ip.parse().ok()?;
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    Some(u32::from(ip) & mask)
}

/// Both on the same local network.
pub fn same_network(a: &Lan, b: &Lan) -> bool {
    a.prefix == b.prefix && a.prefix >= 8 && network(&a.ip, a.prefix).is_some_and(|n| Some(n) == network(&b.ip, b.prefix))
}

fn mac_bytes(mac: &str) -> Option<[u8; 6]> {
    let hex: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 || mac.chars().any(|c| !(c.is_ascii_hexdigit() || c == ':' || c == '-')) {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    (out != [0; 6] && out != [0xff; 6]).then_some(out)
}

/// 6 × 0xFF, then the address 16 times.
pub fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0xffu8; 6];
    for _ in 0..16 {
        p.extend_from_slice(&mac);
    }
    p
}

/// This computer's wired cards with an IPv4 address (no Wi-Fi, no virtual
/// interfaces: Wake-on-LAN needs a real cable).
pub fn local_lans() -> Vec<Lan> {
    #[derive(Deserialize)]
    struct Addr {
        local: String,
        prefixlen: u8,
    }
    #[derive(Deserialize)]
    struct Iface {
        ifname: String,
        #[serde(default)]
        addr_info: Vec<Addr>,
    }
    if !cfg!(target_os = "linux") {
        return vec![];
    }
    let Ok(out) = std::process::Command::new("ip").args(["-j", "-4", "addr", "show"]).output() else { return vec![] };
    let ifaces: Vec<Iface> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let sys = PathBuf::from("/sys/class/net");
    let mut lans = Vec::new();
    for i in ifaces {
        let dir = sys.join(&i.ifname);
        let physical = dir.join("device").exists() && !dir.join("wireless").exists();
        let ether = std::fs::read_to_string(dir.join("type")).map(|t| t.trim() == "1").unwrap_or(false);
        if !physical || !ether {
            continue;
        }
        let Ok(mac) = std::fs::read_to_string(dir.join("address")) else { continue };
        let mac = mac.trim().to_lowercase();
        if mac_bytes(&mac).is_none() {
            continue;
        }
        for a in i.addr_info {
            lans.push(Lan { mac: mac.clone(), ip: a.local, prefix: a.prefixlen });
        }
    }
    lans
}

/// Send the magic packet for `mac` out of the card at `from`.
fn send_magic(mac: [u8; 6], from: &Lan) -> std::io::Result<()> {
    let ip: Ipv4Addr = from.ip.parse().map_err(|_| std::io::Error::other("bad address"))?;
    let sock = UdpSocket::bind((ip, 0))?;
    sock.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let net = network(&from.ip, from.prefix).unwrap_or(0);
    let host_bits = if from.prefix >= 32 { 0 } else { u32::MAX >> from.prefix };
    let broadcast = Ipv4Addr::from(net | host_bits);
    for port in [9u16, 7] {
        let _ = sock.send_to(&packet, (broadcast, port));
        let _ = sock.send_to(&packet, (Ipv4Addr::BROADCAST, port));
    }
    Ok(())
}

// ─── What we know about the others ──────────────────────────────────────────

fn known_path() -> PathBuf {
    gtk4::glib::user_cache_dir().join("biglace").join("wake.json")
}

fn known_map() -> &'static Mutex<HashMap<String, Vec<Lan>>> {
    static K: OnceLock<Mutex<HashMap<String, Vec<Lan>>>> = OnceLock::new();
    K.get_or_init(|| {
        let map = std::fs::read(known_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(map)
    })
}

/// The peer at tailnet address `ip` announced these cards (it's on now).
pub fn remember(ip: &str, lans: &[Lan]) {
    if lans.is_empty() {
        return;
    }
    let Ok(mut k) = known_map().lock() else { return };
    if k.get(ip).map(Vec::as_slice) == Some(lans) {
        return;
    }
    k.insert(ip.to_string(), lans.to_vec());
    if let Ok(json) = serde_json::to_vec(&*k) {
        let path = known_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
}

/// We know how to wake the peer at `ip`.
pub fn can_wake(ip: &str) -> bool {
    known_map().lock().is_ok_and(|k| k.get(ip).is_some_and(|l| !l.is_empty()))
}

/// Wake the peer at tailnet address `ip`. Blocking. Returns who sent the
/// packet (this computer, or another one on that network).
pub fn wake(ip: &str) -> Result<String, String> {
    let targets = known_map().lock().ok().and_then(|k| k.get(ip).cloned()).unwrap_or_default();
    if targets.is_empty() {
        return Err(crate::tr!("BigLace doesn't know that computer's network card yet"));
    }
    // On the same network ourselves?
    let mine = local_lans();
    let mut sent = false;
    for t in &targets {
        let Some(mac) = mac_bytes(&t.mac) else { continue };
        for m in mine.iter().filter(|m| same_network(m, t)) {
            sent |= send_magic(mac, m).is_ok();
        }
    }
    if sent {
        return Ok(crate::tr!("this computer"));
    }
    // Another computer of ours (or of a contact) on that network.
    for p in crate::tailscale::get_peers().iter().filter(|p| p.online && !p.ipv4.is_empty() && p.ipv4 != ip) {
        let Some(hello) = crate::agent::client::cached(&p.ipv4) else { continue };
        if !hello.supports(CAPABILITY) || !hello.lan.iter().any(|l| targets.iter().any(|t| same_network(l, t))) {
            continue;
        }
        for t in &targets {
            let path = format!("{PATH_PREFIX}{}", t.mac.replace(':', ""));
            if let Ok((stream, _)) = crate::remote::client::dial(&p.ipv4, &path, &[], Duration::from_secs(6)) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Ok(p.display_name());
            }
        }
    }
    Err(crate::tr!("no computer on its network is on to send the signal"))
}

// ─── Sending it for someone else ────────────────────────────────────────────

fn rate_ok() -> bool {
    static SEEN: OnceLock<Mutex<Vec<Instant>>> = OnceLock::new();
    let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() else { return false };
    let now = Instant::now();
    seen.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if seen.len() >= RATE_PER_MINUTE {
        return false;
    }
    seen.push(now);
    true
}

fn respond(mut stream: &std::net::TcpStream, status: u16, reason: &str) {
    use std::io::Write;
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// Another computer asks us to wake one on our network (agent thread).
pub fn serve(
    stream: std::net::TcpStream,
    peer: std::net::IpAddr,
    headers: &HashMap<String, String>,
    identity: &crate::tailscale::Identity,
    path: &str,
) {
    let local = stream.local_addr().map(|a| format!("{}:{}", a.ip(), a.port())).unwrap_or_default();
    if let Err(why) = crate::remote::host::verify_caller(headers, peer, &local, path) {
        crate::tailscale::dbg(&format!("wake: refused {}@{}: {why}", identity.login, identity.device));
        return respond(&stream, 401, "Unauthorized");
    }
    let Some(mac) = path.strip_prefix(PATH_PREFIX).and_then(mac_bytes) else {
        return respond(&stream, 400, "Bad Request");
    };
    if !rate_ok() {
        return respond(&stream, 429, "Too Many Requests");
    }
    let mut sent = false;
    for lan in local_lans() {
        sent |= send_magic(mac, &lan).is_ok();
    }
    crate::tailscale::dbg(&format!("wake: magic packet for {} from {} ({sent})", path, identity.login));
    if sent {
        respond(&stream, 200, "OK");
    } else {
        respond(&stream, 503, "Unavailable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packets_and_networks() {
        let mac = mac_bytes("52:54:00:b9:4e:ff").unwrap();
        assert_eq!(mac_bytes("525400b94eff"), Some(mac));
        assert!(mac_bytes("52:54:00:b9:4e").is_none());
        assert!(mac_bytes("00:00:00:00:00:00").is_none());
        assert!(mac_bytes("../../x").is_none());
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert_eq!(&p[6..12], &mac);
        assert_eq!(&p[96..], &mac);
        let lan = |ip: &str, prefix| Lan { mac: String::new(), ip: ip.into(), prefix };
        assert!(same_network(&lan("192.168.131.10", 24), &lan("192.168.131.200", 24)));
        assert!(!same_network(&lan("192.168.131.10", 24), &lan("192.168.68.10", 24)));
        assert!(!same_network(&lan("10.0.0.1", 0), &lan("192.168.0.1", 0)));
    }

    /// The packet really leaves through a card and is what WoL expects.
    #[test]
    fn sends_a_magic_packet() {
        let rx = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = rx.local_addr().unwrap().port();
        let tx = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let mac = mac_bytes("aa:bb:cc:dd:ee:01").unwrap();
        tx.send_to(&magic_packet(mac), ("127.0.0.1", port)).unwrap();
        let mut buf = [0u8; 200];
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(n, 102);
        assert_eq!(&buf[6..12], &mac);
    }
}
