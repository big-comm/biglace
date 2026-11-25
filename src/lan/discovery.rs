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
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Nearby {
    pub key: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    /// The system account it signs in to over SSH; announced by computers only.
    pub login: Option<String>,
    /// It takes files sent outside a conversation (see `crate::transfer`).
    pub takes_files: bool,
    /// It shares its screen, and whether it fits its resolution to the viewer's window.
    pub screen: Option<Screen>,
    /// It takes calls (see `crate::call`).
    pub calls: bool,
    /// A phone that can be our webcam (see `crate::webcam`).
    pub webcam: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Screen {
    Shared,
    Fits,
}

/// TXT `screen=` values.
const SCREEN_SHARED: &str = "shared";
const SCREEN_FITS: &str = "fits";

/// Our announcement, up while this is held: Avahi drops the group with the connection.
pub struct Announcement {
    _conn: Connection,
}

pub async fn announce(key: &str, name: &str) -> zbus::Result<Announcement> {
    let conn = Connection::system().await?;
    let group: OwnedObjectPath = conn.call_method(Some(AVAHI), "/", Some(SERVER), "EntryGroupNew", &()).await?.body().deserialize()?;
    let mut txt = vec![format!("key={key}").into_bytes(), format!("name={name}").into_bytes()];
    // Where a terminal or the files of this computer sign in (SSH).
    let login = gtk4::glib::user_name().to_string_lossy().into_owned();
    if login_ok(&login) {
        txt.push(format!("login={login}").into_bytes());
    }
    txt.push(format!("send={}", crate::transfer::CAPABILITY).into_bytes());
    if crate::remote::can_host() {
        let screen = if crate::remote::display::available() { SCREEN_FITS } else { SCREEN_SHARED };
        txt.push(format!("screen={screen}").into_bytes());
    }
    txt.push(format!("call={}", crate::call::CAPABILITY).into_bytes());
    // Our own phones link to us (see `crate::link`).
    txt.push(format!("link={}", crate::link::CAPABILITY).into_bytes());
    let instance = format!("BigLace {}", key.get(..8).unwrap_or(key));
    let entry = Some("org.freedesktop.Avahi.EntryGroup");
    conn.call_method(Some(AVAHI), &group, entry, "AddService", &(IF_UNSPEC, PROTO_INET, 0u32, instance, SERVICE, "", "", super::PORT, txt))
        .await?;
    conn.call_method(Some(AVAHI), &group, entry, "Commit", &()).await?;
    Ok(Announcement { _conn: conn })
}

/// What a TXT record says: `key=…` and `name=…` (both needed), `login=…` when
/// it is a valid account name, and whether `send=…` names our way of sending files.
#[derive(Debug, PartialEq, Eq)]
struct Txt {
    key: String,
    name: String,
    login: Option<String>,
    takes_files: bool,
    screen: Option<Screen>,
    calls: bool,
    webcam: bool,
}

fn parse_txt(txt: &[Vec<u8>]) -> Option<Txt> {
    let mut key = None;
    let mut name = None;
    let mut login = None;
    let mut takes_files = false;
    let mut screen = None;
    let mut calls = false;
    let mut webcam = false;
    for entry in txt {
        let entry = std::str::from_utf8(entry).ok()?;
        if let Some(v) = entry.strip_prefix("key=") {
            key = Some(v.to_string());
        } else if let Some(v) = entry.strip_prefix("name=") {
            name = Some(v.to_string());
        } else if let Some(v) = entry.strip_prefix("login=") {
            login = Some(v.to_string()).filter(|l| login_ok(l));
        } else if let Some(v) = entry.strip_prefix("send=") {
            takes_files = v == crate::transfer::CAPABILITY;
        } else if let Some(v) = entry.strip_prefix("screen=") {
            screen = match v {
                SCREEN_SHARED => Some(Screen::Shared),
                SCREEN_FITS => Some(Screen::Fits),
                _ => None,
            };
        } else if let Some(v) = entry.strip_prefix("call=") {
            calls = v == crate::call::CAPABILITY;
        } else if let Some(v) = entry.strip_prefix("webcam=") {
            webcam = v == crate::webcam::CAPABILITY;
        }
    }
    Some(Txt { key: key?, name: name?, login, takes_files, screen, calls, webcam })
}

/// A system account name as `useradd` takes it: it goes into an SSH command line.
pub fn login_ok(login: &str) -> bool {
    let mut bytes = login.bytes();
    login.len() <= 32
        && bytes.next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && bytes.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-' | b'.'))
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
                let Some(Txt { key, name, login, takes_files, screen, calls, webcam }) = parse_txt(&txt) else { continue };
                if key == own_key || port == 0 || !on_this_network(&address) {
                    continue;
                }
                found.insert(instance, Nearby { key, name, address, port, login, takes_files, screen, calls, webcam });
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
        let maria = |login: Option<&str>, takes_files| Some(Txt { key: "abc".into(), name: "Maria".into(), login: login.map(Into::into), takes_files, screen: None, calls: false, webcam: false });
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria"])), maria(None, false));
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria", "login=maria", "send=send1"])), maria(Some("maria"), true));
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria", "login=-oX", "send=send9"])), maria(None, false));
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria", "screen=fits"])).and_then(|t| t.screen), Some(Screen::Fits));
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria", "screen=shared"])).and_then(|t| t.screen), Some(Screen::Shared));
        assert_eq!(parse_txt(&txt(&["key=abc", "name=Maria", "screen=later"])).and_then(|t| t.screen), None);
        assert!(parse_txt(&txt(&["key=abc", "name=Maria", &format!("call={}", crate::call::CAPABILITY)])).is_some_and(|t| t.calls));
        assert!(parse_txt(&txt(&["key=abc", "name=Maria", &format!("webcam={}", crate::webcam::CAPABILITY)])).is_some_and(|t| t.webcam));
        assert_eq!(parse_txt(&txt(&["name=Maria"])), None);
        assert_eq!(parse_txt(&[vec![0xff, 0xfe]]), None, "not UTF-8");
    }

    /// A login reaches `ssh user@host`: nothing an option or a second argument can hide in.
    #[test]
    fn login_names_ssh_accepts() {
        for (login, ok) in [
            ("maria", true),
            ("_svc", true),
            ("joao.silva", true),
            ("user-2", true),
            ("", false),
            ("-oProxyCommand=x", false),
            ("Maria", false),
            ("maria root", false),
            ("maria@host", false),
            ("2maria", false),
            (&"a".repeat(33), false),
        ] {
            assert_eq!(login_ok(login), ok, "{login:?}");
        }
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
