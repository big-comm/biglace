//! Native OS credential storage for panel passwords and enrollment keys.
//!
//! Backed by the `keyring` crate, which speaks to whatever the user's OS
//! provides: Windows Credential Manager, macOS Keychain, or any D-Bus Secret
//! Service implementation on Linux (gnome-keyring, KWallet, KeePassXC, …).
//!
//! When no backend is reachable (e.g. a minimal Linux session with no keyring
//! daemon), every call here returns gracefully — the dialog just won't
//! pre-fill the password field next time.

use keyring::Entry;

const SERVICE: &str = "biglace";
const AUTHKEY_SERVICE: &str = "biglace-authkey";
const RECLAIM_SERVICE: &str = "biglace-reclaim";

fn account_id(panel_url: &str, username: &str) -> String {
    // Compose the panel URL + username so a user with multiple BigScale
    // accounts (different servers) can store each independently.
    format!("{}@{}", username.trim(), panel_url.trim_end_matches('/'))
}

fn entry(panel_url: &str, username: &str) -> Option<Entry> {
    if panel_url.is_empty() || username.is_empty() {
        return None;
    }
    Entry::new(SERVICE, &account_id(panel_url, username)).ok()
}

fn authkey_entry(server_url: &str) -> Option<Entry> {
    let server = server_url.trim().trim_end_matches('/');
    if server.is_empty() {
        return None;
    }
    Entry::new(AUTHKEY_SERVICE, server).ok()
}

pub fn save_authkey(server_url: &str, authkey: &str) -> keyring::Result<()> {
    let Some(entry) = authkey_entry(server_url) else {
        return Ok(());
    };
    entry.set_password(authkey.trim())
}

/// The key stored for `server_url`; `Ok(None)` when none is stored.
pub fn load_authkey(server_url: &str) -> keyring::Result<Option<String>> {
    let Some(entry) = authkey_entry(server_url) else {
        return Ok(None);
    };
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn clear_authkey(server_url: &str) {
    let Some(entry) = authkey_entry(server_url) else {
        return;
    };
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(err) => eprintln!("[biglace] secrets: authkey clear failed: {err}"),
    }
}

// ── Reclaim secret: signs this installation back in as the account an
// invite created (see `invite::rejoin`). Keyed like panel passwords.

fn reclaim_entry(panel_url: &str, user: &str) -> Option<Entry> {
    if panel_url.trim().is_empty() || user.trim().is_empty() {
        return None;
    }
    Entry::new(RECLAIM_SERVICE, &account_id(panel_url, user)).ok()
}

// Without a keyring (common on minimal desktops) the secret goes to a 0600
// file next to the config instead: losing it would silently take away
// "sign in again as …", which is the whole point of keeping it.
const RECLAIM_FILE: &str = "account.secret";

fn reclaim_file_read(panel_url: &str, user: &str) -> Option<String> {
    let text = std::fs::read_to_string(crate::config::private_file(RECLAIM_FILE)).ok()?;
    let mut lines = text.lines();
    let (id, secret) = (lines.next()?, lines.next()?);
    (id == account_id(panel_url, user) && !secret.is_empty()).then(|| secret.to_string())
}

fn reclaim_file_clear() {
    match std::fs::remove_file(crate::config::private_file(RECLAIM_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[biglace] secrets: reclaim file clear failed: {e}"),
    }
}

pub fn save_reclaim(panel_url: &str, user: &str, secret: &str) -> std::io::Result<()> {
    if let Some(entry) = reclaim_entry(panel_url, user) {
        match entry.set_password(secret.trim()) {
            Ok(()) => {
                reclaim_file_clear();
                return Ok(());
            }
            Err(err) => eprintln!("[biglace] secrets: keyring unavailable for the account secret ({err}); using a private file"),
        }
    }
    let data = format!("{}\n{}\n", account_id(panel_url, user), secret.trim());
    crate::config::write_private_file(RECLAIM_FILE, data.as_bytes())
}

pub fn load_reclaim(panel_url: &str, user: &str) -> Option<String> {
    if let Some(entry) = reclaim_entry(panel_url, user) {
        match entry.get_password() {
            Ok(value) => return Some(value),
            Err(keyring::Error::NoEntry) => {}
            Err(err) => eprintln!("[biglace] secrets: reclaim load failed: {err}"),
        }
    }
    reclaim_file_read(panel_url, user)
}

pub fn clear_reclaim(panel_url: &str, user: &str) {
    if let Some(entry) = reclaim_entry(panel_url, user) {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(err) => eprintln!("[biglace] secrets: reclaim clear failed: {err}"),
        }
    }
    reclaim_file_clear();
}

/// Persist `password` for the given panel/user. Errors (no keyring daemon,
/// permission denied, etc.) are logged but otherwise tolerated — at worst the
/// user just retypes the password next time.
pub fn save(panel_url: &str, username: &str, password: &str) {
    if let Some(e) = entry(panel_url, username) {
        if let Err(err) = e.set_password(password) {
            eprintln!("[biglace] secrets: save failed: {err}");
        }
    }
}

/// Look up a previously-saved password. Returns `None` when nothing is stored
/// or the keyring is unavailable. `NoEntry` is the expected miss path and
/// stays quiet; any other failure (locked keyring, D-Bus error) is logged.
pub fn load(panel_url: &str, username: &str) -> Option<String> {
    let e = entry(panel_url, username)?;
    match e.get_password() {
        Ok(p) => Some(p),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            eprintln!("[biglace] secrets: load failed: {err}");
            None
        }
    }
}

/// Forget a stored password — call when the user signs out / switches account.
/// Sign-out is the user telling us "drop my credentials"; if the keyring
/// refuses, the sign-in dialog will silently re-fill the password field next
/// time, which feels like sign-out lied. Log loud enough to debug.
pub fn clear(panel_url: &str, username: &str) {
    if let Some(e) = entry(panel_url, username) {
        match e.delete_credential() {
            Ok(()) => {}
            Err(keyring::Error::NoEntry) => {}
            Err(err) => eprintln!("[biglace] secrets: clear failed: {err}"),
        }
    }
}
