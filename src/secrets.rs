//! Native OS credential storage for the BigScale panel password.
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
