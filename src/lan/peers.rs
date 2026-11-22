//! Devices paired on the local network: the key each proved at pairing and
//! the name it gave. Kept in a private file next to the config.

use serde::{Deserialize, Serialize};

const FILE: &str = "lan-peers.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    /// Its device key (base64url): who it is.
    pub key: String,
    pub name: String,
    /// When we paired, seconds since the epoch.
    pub since: u64,
}

pub fn list() -> Vec<Paired> {
    let Ok(bytes) = std::fs::read(crate::config::private_file(FILE)) else { return vec![] };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        crate::tailscale::dbg(&format!("lan: {FILE} unreadable, no paired devices: {e}"));
        vec![]
    })
}

pub fn is_paired(key: &str) -> bool {
    list().iter().any(|p| p.key == key)
}

/// Remember `paired`, replacing an earlier pairing with the same device.
pub fn add(paired: Paired) -> std::io::Result<()> {
    let mut all = list();
    all.retain(|p| p.key != paired.key);
    all.push(paired);
    save(&all)
}

fn save(all: &[Paired]) -> std::io::Result<()> {
    crate::config::write_private_file(FILE, &serde_json::to_vec_pretty(all)?)
}
