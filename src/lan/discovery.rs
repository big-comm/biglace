//! Finding BigLace on the local network through Avahi (mDNS): we announce
//! `_biglace._tcp` with our device key and name, and list the others.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::message::Type;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

const SERVICE: &str = "_biglace._tcp";
const AVAHI: &str = "org.freedesktop.Avahi";
const SERVER: &str = "org.freedesktop.Avahi.Server";
const BROWSER: &str = "org.freedesktop.Avahi.ServiceBrowser";
/// Any interface; IPv4 (the agent listens on IPv4).
const IF_UNSPEC: i32 = -1;
const PROTO_INET: i32 = 0;

/// Another BigLace on the network, as it announced itself (not yet proven: see `super::handshake`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nearby {
    pub key: String,
    pub name: String,
    pub address: String,
    pub port: u16,
}

/// Our announcement, up while this is held: Avahi drops the group with the connection.
pub struct Announcement {
    _conn: Connection,
}

pub async fn announce(key: &str, name: &str) -> zbus::Result<Announcement> {
    let conn = Connection::system().await?;
    let group: OwnedObjectPath = conn.call_method(Some(AVAHI), "/", Some(SERVER), "EntryGroupNew", &()).await?.body().deserialize()?;
    let txt = vec![format!("key={key}").into_bytes(), format!("name={name}").into_bytes()];
    let instance = format!("BigLace {}", key.get(..8).unwrap_or(key));
    let entry = Some("org.freedesktop.Avahi.EntryGroup");
    conn.call_method(Some(AVAHI), &group, entry, "AddService", &(IF_UNSPEC, PROTO_INET, 0u32, instance, SERVICE, "", "", super::PORT, txt))
        .await?;
    conn.call_method(Some(AVAHI), &group, entry, "Commit", &()).await?;
    Ok(Announcement { _conn: conn })
}

/// `key=…` and `name=…` from a TXT record.
fn parse_txt(txt: &[Vec<u8>]) -> Option<(String, String)> {
    let mut key = None;
    let mut name = None;
    for entry in txt {
        let entry = std::str::from_utf8(entry).ok()?;
        if let Some(v) = entry.strip_prefix("key=") {
            key = Some(v.to_string());
        } else if let Some(v) = entry.strip_prefix("name=") {
            name = Some(v.to_string());
        }
    }
    Some((key?, name?))
}

/// An address on the local network itself: not loopback, nor the tailnet
/// (100.64.0.0/10), which Avahi also announces on.
pub fn on_this_network(address: &str) -> bool {
    match address.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => !ip.is_loopback() && !(ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64),
        Err(_) => false,
    }
}

/// Each time the BigLace devices on the network change, `seen` gets the
/// whole list (without us). Runs until the system bus goes away.
pub async fn browse(own_key: &str, seen: impl Fn(Vec<Nearby>)) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    // Listen before the browser exists, so its first announcements aren't lost
    // (older Avahi has no ServiceBrowserPrepare); then keep only its signals.
    let rule = MatchRule::builder().msg_type(Type::Signal).sender(AVAHI)?.interface(BROWSER)?.build();
    let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
    let browser: OwnedObjectPath = conn
        .call_method(Some(AVAHI), "/", Some(SERVER), "ServiceBrowserNew", &(IF_UNSPEC, PROTO_INET, SERVICE, "", 0u32))
        .await?
        .body()
        .deserialize()?;
    let mut found: HashMap<String, Nearby> = HashMap::new();
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let header = msg.header();
        if header.path().is_none_or(|p| p.as_str() != browser.as_str()) {
            continue;
        }
        let Some(member) = header.member() else { continue };
        let Ok((iface, proto, instance, kind, domain, _flags)) = msg.body().deserialize::<(i32, i32, String, String, String, u32)>() else { continue };
        match member.as_str() {
            "ItemNew" => {
                let resolved = conn
                    .call_method(Some(AVAHI), "/", Some(SERVER), "ResolveService", &(iface, proto, &instance, &kind, &domain, PROTO_INET, 0u32))
                    .await;
                let Ok(reply) = resolved else { continue };
                type Resolved = (i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32);
                let Ok((.., address, port, txt, _)) = reply.body().deserialize::<Resolved>() else { continue };
                let Some((key, name)) = parse_txt(&txt) else { continue };
                if key == own_key || port == 0 || !on_this_network(&address) {
                    continue;
                }
                found.insert(instance, Nearby { key, name, address, port });
            }
            "ItemRemove" => {
                found.remove(&instance);
            }
            _ => continue,
        }
        let mut list: Vec<Nearby> = found.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list.dedup_by(|a, b| a.key == b.key);
        seen(list);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_needs_key_and_name() {
        let txt = |items: &[&str]| items.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria"])), Some(("abc".into(), "Maria".into())));
        assert_eq!(parse_txt(&txt(&["name=Maria"])), None);
        assert_eq!(parse_txt(&[vec![0xff, 0xfe]]), None, "not UTF-8");
    }

    #[test]
    fn only_local_network_addresses() {
        for (address, local) in [
            ("192.168.1.20", true),
            ("10.0.0.5", true),
            ("100.64.0.13", false),
            ("100.127.255.1", false),
            ("100.128.0.1", true),
            ("127.0.0.1", false),
            ("fe80::1", false),
        ] {
            assert_eq!(on_this_network(address), local, "{address}");
        }
    }
}
