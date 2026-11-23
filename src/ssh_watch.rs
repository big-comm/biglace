//! Someone coming into this computer over SSH: the terminal, or the file
//! manager browsing `sftp://`. Neither goes through BigLace, so nothing on
//! screen would show it; logind announces each SSH login as a session, and we
//! follow those.

use std::time::Duration;

use futures_util::StreamExt;
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

/// What the visitor is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Files,
}

impl Kind {
    /// The access history's name for it.
    pub fn what(self) -> &'static str {
        match self {
            Kind::Terminal => "terminal",
            Kind::Files => "files",
        }
    }
}

/// One SSH session on this computer.
#[derive(Debug, Clone)]
pub struct Visit {
    /// logind's session id.
    pub id: String,
    /// The account behind the address it came from, or the address itself.
    pub who: String,
    /// That account's device, when known.
    pub device: String,
    pub kind: Kind,
}

pub enum Event {
    Started(Visit),
    /// The logind session id.
    Ended(String),
}

/// Follow SSH sessions until the system bus goes away; `event` runs on the
/// watching thread. Sessions already open count as started.
pub fn watch(event: impl Fn(Event) + Send + 'static) {
    let spawned = std::thread::Builder::new().name("biglace-ssh-watch".into()).spawn(move || {
        if let Err(e) = async_io::block_on(follow(&event)) {
            crate::tailscale::dbg(&format!("ssh watch stopped: {e}"));
        }
    });
    if let Err(e) = spawned {
        crate::tailscale::dbg(&format!("ssh watch not started: {e}"));
    }
}

async fn follow(event: &impl Fn(Event)) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    // Listen before listing, so a session opening in between isn't missed.
    let rule = MatchRule::builder().msg_type(Type::Signal).sender(LOGIND)?.interface(MANAGER)?.path(MANAGER_PATH)?.build();
    let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
    type Listed = Vec<(String, u32, String, String, OwnedObjectPath)>;
    let open: Listed = conn.call_method(Some(LOGIND), MANAGER_PATH, Some(MANAGER), "ListSessions", &()).await?.body().deserialize()?;
    for (id, .., path) in open {
        if let Some(visit) = visit(&conn, &id, &path, false).await {
            event(Event::Started(visit));
        }
    }
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let Some(member) = msg.header().member().map(|m| m.to_string()) else { continue };
        let Ok((id, path)) = msg.body().deserialize::<(String, OwnedObjectPath)>() else { continue };
        match member.as_str() {
            "SessionNew" => {
                if let Some(visit) = visit(&conn, &id, &path, true).await {
                    event(Event::Started(visit));
                }
            }
            "SessionRemoved" => event(Event::Ended(id)),
            _ => {}
        }
    }
    Ok(())
}

/// The session as a visit, if someone logged in over SSH from elsewhere.
/// `fresh`: just opened, so its programs may still be starting.
async fn visit(conn: &Connection, id: &str, path: &OwnedObjectPath, fresh: bool) -> Option<Visit> {
    let props: std::collections::HashMap<String, OwnedValue> = conn
        .call_method(Some(LOGIND), path.as_str(), Some("org.freedesktop.DBus.Properties"), "GetAll", &(SESSION,))
        .await
        .ok()?
        .body()
        .deserialize()
        .ok()?;
    let text = |name: &str| props.get(name).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let remote = props.get("Remote").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
    if text("Service") != "sshd" || !remote {
        return None;
    }
    let leader = props.get("Leader").and_then(|v| u32::try_from(v.clone()).ok())?;
    let kind = kind_of(leader, fresh).await;
    let host = text("RemoteHost");
    let (who, device) = match crate::tailscale::get_peers().into_iter().find(|p| p.ipv4 == host || p.ipv6 == host) {
        Some(p) if !p.user.is_empty() => (p.user, p.hostname),
        Some(p) => (p.hostname, String::new()),
        None => (host, String::new()),
    };
    Some(Visit { id: id.to_string(), who, device, kind })
}

/// Files when the session runs the SFTP server, else a terminal. A new
/// session gets a few seconds for it to start.
async fn kind_of(leader: u32, fresh: bool) -> Kind {
    let tries = if fresh { 6 } else { 1 };
    for _ in 0..tries {
        if fresh {
            async_io::Timer::after(Duration::from_millis(500)).await;
        }
        if session_programs(leader).iter().any(|c| c == "sftp-server") {
            return Kind::Files;
        }
    }
    Kind::Terminal
}

/// The program names running in the session leader's cgroup (its scope).
fn session_programs(leader: u32) -> Vec<String> {
    let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{leader}/cgroup")) else { return Vec::new() };
    let Some(scope) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) else { return Vec::new() };
    let Ok(procs) = std::fs::read_to_string(format!("/sys/fs/cgroup{scope}/cgroup.procs")) else { return Vec::new() };
    procs
        .lines()
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .map(|c| c.trim_end().to_string())
        .collect()
}

/// End session `id`: logind lets its own user do it, and asks anyone else to
/// authenticate. Blocking.
pub fn end(id: &str) -> Result<(), String> {
    async_io::block_on(async {
        let conn = Connection::system().await.map_err(|e| e.to_string())?;
        conn.call_method(Some(LOGIND), MANAGER_PATH, Some(MANAGER), "TerminateSession", &(id,))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}
