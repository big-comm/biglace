use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    /// Loaded from the OS keyring; never serialized to TOML.
    #[serde(default, skip_serializing)]
    pub authkey: String,
    pub auto_connect: bool,

    /// Local tailscaled state contains a registered node key. This remains a
    /// durable reconnect marker even when no enrollment key is available.
    #[serde(default)]
    pub enrolled: bool,

    /// Tailscale hostname for this device — the BigScale account identifier
    /// (e.g. `tales`), which becomes the device's DNS name (`tales.bigscale.net`).
    /// Distinct from the local OS user (`os_user()`) — that one is propagated
    /// separately via a `tag:user-…` ACL tag so peers can compose the SSH
    /// login `<os_user>@<hostname>.bigscale.net` from the two.
    #[serde(default)]
    pub hostname: String,

    #[serde(default)]
    pub panel_url: String,
    // Last-used panel username, pre-filled in the login dialog so the user
    // doesn't retype it each time. Password is intentionally NOT persisted.
    #[serde(default)]
    pub panel_username: String,

    /// Pinned peers, identified by hostname. Pinned peers sort to the top
    /// of the list regardless of online/offline state — handy when you have
    /// a frota of clients and only care about a handful day-to-day.
    #[serde(default)]
    pub favorites: Vec<String>,

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

    /// Prevents a later UI save from overwriting a config that could not be
    /// read or backed up safely during startup.
    #[serde(skip)]
    pub recovery_blocked: bool,
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
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("biglace/config.toml")
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
    let s = match fs::read_to_string(&p) {
        Ok(s) => s,
        // Missing file is the normal first-run case — start with defaults.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            eprintln!("[biglace] config: could not read {}: {e}", p.display());
            return Config {
                recovery_blocked: true,
                ..Config::default()
            };
        }
    };
    match toml::from_str::<Config>(&s) {
        Ok(mut cfg) => {
            let legacy_key = cfg.authkey.clone();
            if !legacy_key.is_empty() {
                cfg.enrolled = true;
                if cfg.server_url.trim().is_empty() {
                    eprintln!(
                        "[biglace] secrets: deferring authkey migration until a server URL is set"
                    );
                } else {
                    match crate::secrets::save_authkey(&cfg.server_url, &legacy_key) {
                        Ok(()) => {
                            if let Err(e) = write_config_file(&cfg) {
                                eprintln!("[biglace] config: could not remove migrated plaintext key: {e}");
                            }
                        }
                        Err(e) => eprintln!("[biglace] secrets: authkey migration failed: {e}"),
                    }
                }
            } else {
                cfg.authkey = crate::secrets::load_authkey(&cfg.server_url).unwrap_or_default();
            }
            cfg
        }
        Err(e) => {
            // The file exists but doesn't parse (interrupted write, full disk,
            // hand-edit typo). Previously we silently returned defaults, and
            // the first save_or_warn() then OVERWROTE the file — destroying the
            // user's server/key/favorites without a trace. Preserve the original
            // as `.bak` so nothing is lost, and log loudly.
            let bak = p.with_extension("toml.bak");
            eprintln!(
                "[biglace] config: {} is invalid ({e}); preserving it as {} and starting fresh",
                p.display(),
                bak.display(),
            );
            if let Err(re) = fs::copy(&p, &bak) {
                eprintln!("[biglace] config: could not back up the invalid config: {re}");
                return Config {
                    recovery_blocked: true,
                    ..Config::default()
                };
            }
            Config::default()
        }
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    if cfg.recovery_blocked {
        bail!("refusing to overwrite an unreadable configuration file");
    }
    if !cfg.authkey.trim().is_empty() {
        if cfg.server_url.trim().is_empty() {
            bail!("cannot store a pre-auth key without a server URL");
        }
        crate::secrets::save_authkey(&cfg.server_url, &cfg.authkey)?;
    }
    write_config_file(cfg)
}

fn write_config_file(cfg: &Config) -> Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = toml::to_string(cfg)?;
    // Write to a sibling temp file then rename over the target: an interrupted
    // write (crash, power loss, ENOSPC) can't truncate the live config to
    // garbage — the old file stays intact until the atomic rename swaps it.
    let tmp = p.with_extension("toml.tmp");
    write_private(&tmp, data.as_bytes())?;
    fs::rename(&tmp, &p)?;
    sync_parent(&p)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Write `data` to `path`, creating the file 0600 (owner-only) on Unix. Config
/// still contains private network metadata even though enrollment keys live in
/// the OS keyring.
#[cfg(unix)]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode(0o600)` only applies when *creating* the file. Force it on the
    // handle too so a leftover temp from a previous crash (possibly created
    // with a laxer mode) can't carry world-readable perms into the renamed
    // config that holds the pre-auth key.
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    fs::write(path, data)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_key_is_never_serialized() {
        let cfg = Config {
            server_url: "https://vpn.example.org".into(),
            authkey: "tskey-secret".into(),
            enrolled: true,
            ..Config::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(!serialized.contains("tskey-secret"));
        assert!(!serialized.contains("authkey"));
        assert!(serialized.contains("enrolled = true"));
    }
}
