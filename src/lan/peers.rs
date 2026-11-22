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
    /// Forgotten: kept only for its name in our history; it can only pair again.
    #[serde(default)]
    pub forgotten: bool,
    /// Where it last reached us or we reached it: for when mDNS can't find it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

pub fn list() -> Vec<Paired> {
    let Ok(bytes) = std::fs::read(crate::config::private_file(FILE)) else { return vec![] };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        crate::tailscale::dbg(&format!("lan: {FILE} unreadable, no paired devices: {e}"));
        vec![]
    })
}

pub fn is_paired(key: &str) -> bool {
    list().iter().any(|p| p.key == key && !p.forgotten)
}

/// The paired devices, without the forgotten ones.
pub fn paired() -> Vec<Paired> {
    list().into_iter().filter(|p| !p.forgotten).collect()
}

/// Remember `paired`, replacing an earlier pairing with the same device.
pub fn add(paired: Paired) -> std::io::Result<()> {
    let mut all = list();
    all.retain(|p| p.key != paired.key);
    all.push(paired);
    save(&all)
}

/// Forget a paired device: it can only pair again.
pub fn remove(key: &str) {
    let mut all = list();
    for p in all.iter_mut().filter(|p| p.key == key) {
        p.forgotten = true;
    }
    if let Err(e) = save(&all) {
        crate::tailscale::dbg(&format!("lan: {FILE} not saved: {e}"));
    }
}

/// Remember `key`'s current address (the file is written only when it differs).
pub fn seen_at(key: &str, address: &str) {
    let mut all = list();
    let Some(p) = all.iter_mut().find(|p| p.key == key && p.address.as_deref() != Some(address)) else { return };
    p.address = Some(address.to_string());
    if let Err(e) = save(&all) {
        crate::tailscale::dbg(&format!("lan: {FILE} not saved: {e}"));
    }
}

fn save(all: &[Paired]) -> std::io::Result<()> {
    crate::config::write_private_file(FILE, &serde_json::to_vec_pretty(all)?)
}
