use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub server_url:   String,
    pub authkey:      String,
    pub auto_connect: bool,

    /// Tailscale hostname for this device — the BigScale account identifier
    /// (e.g. `tales`), which becomes the device's DNS name (`tales.bigscale.net`).
    /// Distinct from the local OS user (`os_user()`) — that one is propagated
    /// separately via a `tag:user-…` ACL tag so peers can compose the SSH
    /// login `<os_user>@<hostname>.bigscale.net` from the two.
    #[serde(default)]
    pub hostname:     String,

    #[serde(default)]
    pub panel_url:      String,
    // Last-used panel username, pre-filled in the login dialog so the user
    // doesn't retype it each time. Password is intentionally NOT persisted.
    #[serde(default)]
    pub panel_username: String,

    /// Pinned peers, identified by hostname. Pinned peers sort to the top
    /// of the list regardless of online/offline state — handy when you have
    /// a frota of clients and only care about a handful day-to-day.
    #[serde(default)]
    pub favorites:    Vec<String>,

    /// When true, biglace will keep retrying connect() with exponential
    /// backoff after the daemon reports a drop. Independent of `auto_connect`,
    /// which only fires once at startup.
    #[serde(default)]
    pub auto_reconnect: bool,

    /// Enable libnotify (`notify-send`) toasts when a peer transitions
    /// between online and offline. Off by default to keep the desktop quiet.
    #[serde(default)]
    pub notify_peer_changes: bool,

    /// Per-peer SSH login overrides, keyed by hostname. Takes precedence over
    /// the `os_user` propagated by the panel (Option D), which itself falls
    /// back to the peer's hostname. Lets the user override the login on a
    /// multi-user server where neither the panel nor the hostname matches
    /// their actual account on that box.
    #[serde(default)]
    pub peer_overrides: HashMap<String, String>,
}

impl Config {
    pub fn is_favorite(&self, hostname: &str) -> bool {
        self.favorites.iter().any(|h| h == hostname)
    }

    /// Toggle the pin state for `hostname`. Returns the new state (true = pinned).
    pub fn toggle_favorite(&mut self, hostname: &str) -> bool {
        if let Some(pos) = self.favorites.iter().position(|h| h == hostname) {
            self.favorites.remove(pos);
            false
        } else {
            self.favorites.push(hostname.to_string());
            true
        }
    }
}

/// The local OS user, used as the device's tailscale hostname so other peers
/// can SSH/SFTP into it (`ssh <os-user>@<peer-dns>`). We always derive this
/// at runtime — never persist it — so a config copied between machines auto-
/// adapts to whoever runs biglace.
///
/// Windows doesn't set `USER` / `LOGNAME` — those are Unix conventions. The
/// native equivalent is `USERNAME`, set by `cmd.exe`/`powershell.exe` and
/// every Windows session manager.
pub fn os_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "biglace".into())
}

/// Per-OS location for the TOML config. We intentionally don't pull `dirs` or
/// `directories` for this — env vars are stable, deterministic, and what every
/// OS guideline actually documents (XDG on Linux, %APPDATA% on Windows,
/// ~/Library on macOS).
#[cfg(target_os = "linux")]
fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/biglace/config.toml")
}

#[cfg(target_os = "windows")]
fn path() -> PathBuf {
    // %APPDATA% is the per-user roaming dir (e.g. C:\Users\foo\AppData\Roaming).
    // Fall back to USERPROFILE then current dir so we never panic on a stripped
    // service account profile.
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("USERPROFILE").map(|p| format!("{p}\\AppData\\Roaming")))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("biglace").join("config.toml")
}

#[cfg(target_os = "macos")]
fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Library/Application Support/biglace/config.toml")
}

pub fn load() -> Config {
    let p = path();
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) -> Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, toml::to_string(cfg)?)?;
    Ok(())
}

/// Best-effort wrapper used by signal handlers that have no good way to
/// surface a write failure (disk full, perms, RO fs). The previous code
/// silenced these with `.ok()` and we'd find out about broken configs the
/// hard way — by the user reporting "my switch keeps flipping back". At
/// least log to stderr so it shows up in `journalctl --user` or the launch
/// terminal.
pub fn save_or_warn(cfg: &Config) {
    if let Err(e) = save(cfg) {
        eprintln!("[biglace] config save failed: {e}");
    }
}
