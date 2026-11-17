//! This installation's device key: proves to the BigScale panel that a
//! request really comes from BigLace on this device.
//!
//! The tunnel's source address only names the *node*. Anything the node
//! forwards into the tailnet — a VM or container NATed through this machine —
//! reaches the panel with the same address. So every request to the panel's
//! peer API is signed with an Ed25519 key that never leaves this machine, and
//! the panel accepts that key for this node only after asking this node's
//! BigLace agent (over the tunnel) to sign a fresh challenge with it
//! ([`prove`], served at `/v1/proof/<challenge>`). A VM behind the host can't
//! get the host's BigLace to vouch for a key it doesn't hold.
//!
//! The key lives in a 0600 file next to the config (like an SSH key), so no
//! keyring prompt can block a background request. The message formats are
//! mirrored in the panel's `src/lib/server/device-auth.ts`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};

const KEY_FILE: &str = "device.key";

fn cell() -> &'static Mutex<Option<SigningKey>> {
    static KEY: OnceLock<Mutex<Option<SigningKey>>> = OnceLock::new();
    KEY.get_or_init(|| Mutex::new(None))
}

fn parse_seed(text: &str) -> Option<[u8; 32]> {
    let bytes = B64.decode(text.trim()).ok()?;
    bytes.try_into().ok()
}

fn load_or_create() -> Option<SigningKey> {
    // Tests never touch the user's real key file.
    if cfg!(test) {
        return Some(SigningKey::from_bytes(&[9u8; 32]));
    }
    let path = crate::config::private_file(KEY_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        match parse_seed(&text) {
            Some(seed) => return Some(SigningKey::from_bytes(&seed)),
            // Don't overwrite: a key we can't read may still be pinned on the
            // panel, and replacing it is a deliberate act (delete the file).
            None => {
                eprintln!("[biglace] device key: {} is unreadable; not replacing it", path.display());
                return None;
            }
        }
    }
    let mut seed = [0u8; 32];
    if let Err(e) = getrandom::getrandom(&mut seed) {
        eprintln!("[biglace] device key: no randomness: {e}");
        return None;
    }
    if let Err(e) = crate::config::write_private_file(KEY_FILE, B64.encode(seed).as_bytes()) {
        eprintln!("[biglace] device key: could not save it: {e}");
        return None;
    }
    Some(SigningKey::from_bytes(&seed))
}

fn key() -> Option<SigningKey> {
    let mut guard = cell().lock().ok()?;
    if guard.is_none() {
        *guard = load_or_create();
    }
    guard.clone()
}

/// Public key, base64url (43 chars), sent when enrolling so the panel can
/// pin it to the new device.
pub fn public_key() -> Option<String> {
    key().map(|k| B64.encode(k.verifying_key().to_bytes()))
}

/// What gets signed for a request. Mirrors `requestMessage` in the panel.
pub fn request_message(method: &str, host: &str, path: &str, time: u64, nonce: &str, body: &str) -> String {
    let body_hash = hex(&Sha256::digest(body.as_bytes()));
    format!(
        "biglace-req-v1\n{}\n{}\n{path}\n{time}\n{nonce}\n{body_hash}",
        method.to_ascii_uppercase(),
        host.to_ascii_lowercase()
    )
}

/// What the agent signs to prove it holds the key. Mirrors `proofMessage`.
pub fn proof_message(challenge: &str) -> String {
    format!("biglace-proof-v1\n{challenge}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `host[:port]` and `path?query` of an http(s) URL, as the server sees them.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.split_once("://")?.1;
    match rest.find('/') {
        Some(i) => Some((&rest[..i], &rest[i..])),
        None => Some((rest, "/")),
    }
}

/// Headers that sign `method url` with `body` (exact bytes sent).
pub fn sign_headers(method: &str, url: &str, body: &str) -> Option<Vec<(&'static str, String)>> {
    let k = key()?;
    let (host, path) = split_url(url)?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let mut raw = [0u8; 18];
    getrandom::getrandom(&mut raw).ok()?;
    let nonce = B64.encode(raw);
    let sig = k.sign(request_message(method, host, path, time, &nonce, body).as_bytes());
    Some(vec![
        ("X-BigLace-Key", B64.encode(k.verifying_key().to_bytes())),
        ("X-BigLace-Time", time.to_string()),
        ("X-BigLace-Nonce", nonce),
        ("X-BigLace-Sig", B64.encode(sig.to_bytes())),
    ])
}

/// Answer to the panel's challenge: `{ key, sig }` as JSON, or None when the
/// challenge is malformed or there's no key.
pub fn prove(challenge: &str) -> Option<String> {
    if challenge.len() != 43 || !challenge.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return None;
    }
    let k = key()?;
    let sig = k.sign(proof_message(challenge).as_bytes());
    serde_json::to_string(&serde_json::json!({
        "key": B64.encode(k.verifying_key().to_bytes()),
        "sig": B64.encode(sig.to_bytes()),
    }))
    .ok()
}

/// Send a signed request to the panel's peer API. `body` is sent verbatim
/// (JSON) and is what the signature covers.
// `ureq::Error` is large, but it's what every caller already matches on.
#[allow(clippy::result_large_err)]
pub fn send(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<ureq::Response, ureq::Error> {
    let mut req = agent.request(method, url).set("Accept", "application/json");
    let payload = body.unwrap_or("");
    // Without a key the request goes unsigned; the panel answers 401
    // (signature_required) and the caller reports it.
    if let Some(headers) = sign_headers(method, url, payload) {
        for (name, value) in headers {
            req = req.set(name, &value);
        }
    }
    match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(b),
        None => req.call(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn messages_match_the_panel_format() {
        let m = request_message("post", "100.64.0.7:3000", "/api/me/invites?x=1", 1_700_000_000, "n0nce", "{}");
        assert_eq!(
            m,
            "biglace-req-v1\nPOST\n100.64.0.7:3000\n/api/me/invites?x=1\n1700000000\nn0nce\n\
             44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(proof_message("abc"), "biglace-proof-v1\nabc");
        assert_eq!(split_url("http://100.64.0.7:3000/api/me/x?y"), Some(("100.64.0.7:3000", "/api/me/x?y")));
        assert_eq!(split_url("http://h"), Some(("h", "/")));
    }

    /// Same key and message signed by Node's crypto (what the panel runs):
    /// both sides must produce and accept the very same bytes.
    #[test]
    fn signature_matches_the_panel_implementation() {
        let k = SigningKey::from_bytes(&[7u8; 32]);
        assert_eq!(B64.encode(k.verifying_key().to_bytes()), "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw");
        let m = request_message("POST", "100.64.0.7:3000", "/api/me/invites?x=1", 1_700_000_000, "n0nce", "{}");
        assert_eq!(
            B64.encode(k.sign(m.as_bytes()).to_bytes()),
            "618YLuhBTTq72sZOVI2DpEyvu3-GhFwLl32MUVAB0FR-uagL3qfbokX703awIp6NP9kcOzMsKLBYzVnWafdKCA"
        );
    }

    #[test]
    fn signatures_verify_and_proofs_reject_junk() {
        let k = SigningKey::from_bytes(&[7u8; 32]);
        let msg = request_message("GET", "h", "/p", 1, "n", "");
        let sig = k.sign(msg.as_bytes());
        assert!(k.verifying_key().verify(msg.as_bytes(), &Signature::from_bytes(&sig.to_bytes())).is_ok());
        assert!(prove("short").is_none());
        assert!(prove(&"a/".repeat(22)[..43]).is_none());
    }
}
