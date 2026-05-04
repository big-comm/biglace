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
/// permission denied, etc.) are intentionally swallowed — at worst the user
/// just retypes the password next time.
pub fn save(panel_url: &str, username: &str, password: &str) {
    if let Some(e) = entry(panel_url, username) {
        let _ = e.set_password(password);
    }
}

/// Look up a previously-saved password. Returns `None` when nothing is stored
/// or the keyring is unavailable.
pub fn load(panel_url: &str, username: &str) -> Option<String> {
    entry(panel_url, username).and_then(|e| e.get_password().ok())
}

/// Forget a stored password — call when the user signs out / switches account.
#[allow(dead_code)]
pub fn clear(panel_url: &str, username: &str) {
    if let Some(e) = entry(panel_url, username) {
        let _ = e.delete_credential();
    }
}
