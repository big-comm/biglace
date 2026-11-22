//! The BigLace agent: how one BigLace talks to another.
//!
//! Every BigLace runs a small HTTP endpoint bound **only** to its own tailnet
//! address. Other BigLace peers call it to learn what this device supports
//! (and, in later protocol versions, to ask for a remote screen, a restart,
//! a voice room…). Peers without BigLace — a phone with the official app, a
//! server with plain `tailscale` — simply don't answer, and the UI keeps
//! offering what it always did (SSH and SFTP).
//!
//! Security model: the tailnet is the transport and the identity. WireGuard
//! encrypts every packet, and `tailscale whois` on the caller's source
//! address tells us which account and device is on the other end — a TCP
//! handshake from a tailnet address can only be completed by the node that
//! holds that address's key. Requests whose source isn't a tailnet node are
//! refused before any handler runs.

pub mod client;
pub mod server;

use serde::{Deserialize, Serialize};

/// TCP port the agent listens on, on the tailnet address only.
pub const PORT: u16 = 41980;

/// Wire protocol revision. Bump when a change is not backward compatible;
/// additive changes go in [`Hello::capabilities`] instead.
pub const PROTOCOL: u32 = 1;

/// Identifies a BigLace agent in the `hello` answer, so an unrelated service
/// that happens to listen on the same port is never mistaken for one.
pub const APP_ID: &str = "biglace";

/// Features this build offers to other peers. Each later phase adds its own
/// entry (`screen`, `control`, `power`, `room`, …) so older and newer
/// BigLace versions can talk to each other: a client only shows a button
/// when the peer listed the matching capability.
pub const CAPABILITIES: &[&str] = &["hello", crate::chat::OPS_CAPABILITY, crate::chat::READ_CAPABILITY, crate::chat::RELAY_CAPABILITY, crate::link::CAPABILITY, crate::call::CAPABILITY, crate::transfer::CAPABILITY];

/// Answer to `GET /v1/hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub app: String,
    pub protocol: u32,
    /// BigLace version of the peer (`APP_VERSION`).
    pub version: String,
    pub os: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Wired network cards, so others can wake this computer later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lan: Vec<crate::wake::Lan>,
}

impl Hello {
    /// What this build announces about itself.
    pub fn local() -> Self {
        Self {
            app: APP_ID.to_string(),
            protocol: PROTOCOL,
            version: crate::APP_VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            capabilities: CAPABILITIES
                .iter()
                .map(|c| c.to_string())
                .chain(crate::remote::can_host().then(|| crate::remote::CAPABILITY.to_string()))
                .chain((crate::remote::can_host() && crate::power::available()).then(|| crate::power::CAPABILITY.to_string()))
                .chain(cfg!(target_os = "linux").then(|| crate::wake::CAPABILITY.to_string()))
                .chain(
                    (crate::remote::can_host() && crate::remote::display::available())
                        .then(|| crate::remote::RESIZE_CAPABILITY.to_string()),
                )
                .collect(),
            lan: crate::wake::local_lans(),
        }
    }

    /// True when the peer announced `capability`.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips_and_tolerates_unknown_fields() {
        let local = Hello::local();
        let json = serde_json::to_string(&local).unwrap();
        assert_eq!(serde_json::from_str::<Hello>(&json).unwrap(), local);
        assert!(local.supports("hello"));

        // A newer peer may add fields; an older one may omit capabilities.
        let newer: Hello = serde_json::from_str(
            r#"{"app":"biglace","protocol":2,"version":"9.0.0","os":"linux",
                "capabilities":["hello","screen"],"room":{"name":"x"}}"#,
        )
        .unwrap();
        assert!(newer.supports("screen"));
        let older: Hello =
            serde_json::from_str(r#"{"app":"biglace","protocol":1,"version":"1.0.0","os":"windows"}"#)
                .unwrap();
        assert!(older.capabilities.is_empty());
    }
}
