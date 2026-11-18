//! Joining a network with an invite from a BigScale panel.
//!
//! An admin creates the invite in the panel and shares either the web link
//! (`https://<panel>/join#<token>`, also encoded in the QR code) or the app
//! link (`biglace://join?panel=<panel>&invite=<token>`). Both carry the same
//! two things: which panel to talk to and the one-time token. Redeeming the
//! token creates this person's account, puts it in the invite's group and
//! returns a pre-auth key — no admin password involved.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::panel;

/// An invite parsed out of whatever the user pasted or clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Normalized panel base URL (HTTPS, or HTTP on loopback).
    pub panel: String,
    pub token: String,
}

/// Same bounds the panel enforces: base64url, 20–128 chars.
fn valid_token(token: &str) -> bool {
    (20..=128).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Decode `%XX` escapes (query values in the app link). Invalid escapes are
/// an error rather than passed through, so a mangled link fails loudly.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Parse an invite link: `biglace://join?panel=…&invite=…` or
/// `https://<panel>/join#<token>`. Surrounding whitespace is ignored.
pub fn parse(input: &str) -> Result<Invite> {
    let input = input.trim();
    let bad = || anyhow!("invalid_link");

    if let Some(rest) = input.strip_prefix("biglace://join") {
        let query = rest.strip_prefix('?').or_else(|| rest.strip_prefix("/?")).ok_or_else(bad)?;
        let (mut panel_url, mut token) = (None, None);
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "panel" => panel_url = percent_decode(v),
                "invite" => token = percent_decode(v),
                _ => {}
            }
        }
        let token = token.ok_or_else(bad)?;
        let panel_url = panel::normalize_panel_url(&panel_url.ok_or_else(bad)?).map_err(|_| bad())?;
        if !valid_token(&token) {
            return Err(bad());
        }
        return Ok(Invite { panel: panel_url, token });
    }

    // Web link: everything before `/join#` is the panel.
    let (base, token) = input.split_once("/join#").ok_or_else(bad)?;
    let token = token.trim();
    if !valid_token(token) {
        return Err(bad());
    }
    let panel_url = panel::normalize_panel_url(base).map_err(|_| bad())?;
    Ok(Invite {
        panel: panel_url,
        token: token.to_string(),
    })
}

impl Invite {
    /// Host of the panel, shown to the user so a link can't silently enroll
    /// this device into a network they didn't mean to join.
    pub fn host(&self) -> &str {
        let rest = self.panel.split_once("://").map_or(self.panel.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }
}

/// The coordinator URL the panel answered with is acceptable for an invite
/// from `panel`: an HTTPS panel must hand out an HTTPS coordinator (the
/// pre-auth key and all control traffic go there). Empty or loopback answers
/// are fine — the caller falls back to the panel URL for those.
pub fn server_url_ok(panel: &str, server_url: &str) -> bool {
    let s = server_url.trim();
    if s.is_empty() || panel::url_is_local_server_response(s) || !panel.starts_with("https://") {
        return true;
    }
    s.starts_with("https://")
}

#[derive(Serialize)]
struct RedeemBody<'a> {
    invite: &'a str,
    name: &'a str,
    /// Pinned by the panel to the device that registers with the key it
    /// hands back (see device_key).
    #[serde(skip_serializing_if = "Option::is_none")]
    device_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Joined {
    pub authkey: String,
    pub server_url: String,
    /// Account name the panel created (sanitized from what the user typed).
    pub user: String,
    #[serde(default)]
    pub group: Option<String>,
    /// Secret for signing in again as `user` from this installation later
    /// (kept in the OS keyring). Absent on panels older than 0.4.
    #[serde(default)]
    pub reclaim: Option<String>,
}

fn check_joined(panel: &str, joined: &Joined) -> std::result::Result<(), JoinError> {
    if joined.authkey.trim().is_empty()
        || joined.authkey.len() > 4096
        || joined.authkey.chars().any(char::is_control)
    {
        return Err(JoinError::Other("panel returned an invalid pre-auth key".into()));
    }
    if !server_url_ok(panel, &joined.server_url) {
        return Err(JoinError::Other(
            "panel returned a coordinator address without HTTPS".into(),
        ));
    }
    Ok(())
}

fn status_error(code: u16, resp: ureq::Response) -> JoinError {
    let body = panel::read_json_limited::<ErrorBody>(resp).ok();
    let (err, suggestion) = body
        .map(|b| (b.error, b.suggestion))
        .unwrap_or((None, None));
    // An older panel without invites answers 404 with no error code.
    let err = err.unwrap_or_else(|| {
        if code == 404 {
            "no_invites".into()
        } else {
            format!("HTTP {code}")
        }
    });
    classify(&err, suggestion)
}

/// Why redeeming failed, in terms the dialog can explain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The name is in use; the panel may suggest a free one.
    NameTaken(Option<String>),
    InvalidName,
    /// Unknown, expired, used up or revoked.
    InviteUnusable(String),
    TooManyAttempts,
    /// Network/TLS/server failure, with detail for the log.
    Other(String),
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    suggestion: Option<String>,
}

fn classify(code: &str, suggestion: Option<String>) -> JoinError {
    match code {
        "name_taken" => JoinError::NameTaken(suggestion),
        "invalid_name" => JoinError::InvalidName,
        "unknown" | "expired" | "used" | "revoked" | "pending" => JoinError::InviteUnusable(code.to_string()),
        "too_many_attempts" => JoinError::TooManyAttempts,
        other => JoinError::Other(other.to_string()),
    }
}

/// Redeem `invite` for the person called `name`. Blocking.
pub fn redeem(invite: &Invite, name: &str) -> std::result::Result<Joined, JoinError> {
    let agent = panel::shared_agent().map_err(|e| JoinError::Other(e.to_string()))?;
    let body = serde_json::to_value(RedeemBody {
        invite: &invite.token,
        name,
        device_key: crate::device_key::public_key(),
    })
    .map_err(|e| JoinError::Other(e.to_string()))?;
    match agent
        .post(&format!("{}/api/join", invite.panel))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_json(body)
    {
        Ok(resp) => {
            let joined: Joined =
                panel::read_json_limited(resp).map_err(|e| JoinError::Other(e.to_string()))?;
            check_joined(&invite.panel, &joined)?;
            Ok(joined)
        }
        Err(ureq::Error::Status(code, resp)) => Err(status_error(code, resp)),
        Err(e) => Err(JoinError::Other(e.to_string())),
    }
}

/// Sign in again as `user`, the account an invite created from this
/// installation, with the secret kept from that join. `hostname` lets the
/// panel drop this device's old offline entry so the name comes back as is.
/// Blocking. `InviteUnusable("revoked")` means the account is gone.
pub fn rejoin(
    panel_url: &str,
    user: &str,
    secret: &str,
    hostname: &str,
) -> std::result::Result<Joined, JoinError> {
    let panel_url = panel_url.trim_end_matches('/');
    if !(panel_url.starts_with("https://") || panel::url_is_loopback(panel_url)) {
        return Err(JoinError::Other("panel address without HTTPS".into()));
    }
    let agent = panel::shared_agent().map_err(|e| JoinError::Other(e.to_string()))?;
    let body = serde_json::json!({
        "user": user,
        "reclaim": secret,
        "hostname": hostname,
        "device_key": crate::device_key::public_key(),
    });
    match agent
        .post(&format!("{panel_url}/api/rejoin"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_json(body)
    {
        Ok(resp) => {
            let joined: Joined =
                panel::read_json_limited(resp).map_err(|e| JoinError::Other(e.to_string()))?;
            check_joined(panel_url, &joined)?;
            Ok(joined)
        }
        Err(ureq::Error::Status(code, resp)) => Err(status_error(code, resp)),
        Err(e) => Err(JoinError::Other(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_settings_reach_the_app() {
        let mine: MyInvites = serde_json::from_str(r#"{"invites":[],"groups":[]}"#).unwrap();
        assert!(mine.can_invite, "older panels let every member invite");
        let mine: MyInvites =
            serde_json::from_str(r#"{"invites":[],"groups":[],"policy":"admins","can_invite":false}"#).unwrap();
        assert!(!mine.can_invite);
        let made: CreatedInvite =
            serde_json::from_str(r#"{"link":"https://p/join#x","invite":{"status":"pending","id":"a"}}"#).unwrap();
        assert!(made.awaits_approval());
        let made: CreatedInvite = serde_json::from_str(r#"{"link":"https://p/join#x"}"#).unwrap();
        assert!(!made.awaits_approval());
        let found: People = serde_json::from_str(r#"{"people":[]}"#).unwrap();
        assert_eq!(found.directory, "");
    }

    #[test]
    fn typed_user_names() {
        assert!(user_name_ok("bruna"));
        assert!(user_name_ok("bigrato-2"));
        for bad in ["", "Bruna", "a b", "../x", "a@b", &"a".repeat(65)] {
            assert!(!user_name_ok(bad), "{bad}");
        }
    }

    #[test]
    fn request_ids_cannot_reshape_the_path() {
        assert!(request_id("a1b2c3d4e5f6a1b2c3d4e5f6").is_ok());
        for bad in ["", "../x", "ab/cd", "abc?x=1", &"a".repeat(65)] {
            assert!(request_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn group_requests_parse_without_outgoing() {
        let r: GroupRequests = serde_json::from_str(
            r#"{"incoming":[{"id":"ab","group":"g","from":"a","to":"b","createdAt":"x"}],"outgoing":[]}"#,
        )
        .unwrap();
        assert_eq!(r.incoming[0].from, "a");
        let empty: GroupRequests = serde_json::from_str("{}").unwrap();
        assert!(empty.incoming.is_empty());
    }

    const TOKEN: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";

    #[test]
    fn both_link_forms_yield_the_same_invite() {
        let web = parse(&format!("  https://vpn.example.org/join#{TOKEN}\n")).unwrap();
        let app = parse(&format!(
            "biglace://join?panel=https%3A%2F%2Fvpn.example.org&invite={TOKEN}"
        ))
        .unwrap();
        assert_eq!(web, app);
        assert_eq!(web.panel, "https://vpn.example.org");
        assert_eq!(web.token, TOKEN);
        // Bare domain in the web link gets https, like the panel URL field.
        assert_eq!(parse(&format!("vpn.example.org/join#{TOKEN}")).unwrap(), web);
        // Order of query parameters doesn't matter; unknown ones are ignored.
        let swapped = parse(&format!(
            "biglace://join/?invite={TOKEN}&v=2&panel=https://vpn.example.org"
        ))
        .unwrap();
        assert_eq!(swapped, web);
    }

    #[test]
    fn malformed_or_unsafe_links_are_rejected() {
        for bad in [
            "",
            "hello",
            "https://vpn.example.org/join#",
            "https://vpn.example.org/join#short",
            &format!("https://vpn.example.org/join#{TOKEN}!"),
            &format!("https://vpn.example.org/join?x={TOKEN}"),
            // Cleartext to a remote host would leak the token.
            &format!("http://vpn.example.org/join#{TOKEN}"),
            &format!("biglace://join?panel=http%3A%2F%2Fevil.example&invite={TOKEN}"),
            &format!("biglace://join?invite={TOKEN}"),
            "biglace://join?panel=https%3A%2F%2Fvpn.example.org",
            &format!("biglace://join?panel=%ZZ&invite={TOKEN}"),
            &format!("biglace://other?panel=https%3A%2F%2Fvpn.example.org&invite={TOKEN}"),
            &format!("https://user:pw@vpn.example.org/join#{TOKEN}"),
        ] {
            assert!(parse(bad).is_err(), "accepted: {bad:?}");
        }
        // Loopback HTTP is fine (local testing).
        assert!(parse(&format!("http://127.0.0.1:3000/join#{TOKEN}")).is_ok());
    }

    #[test]
    fn the_host_is_shown_and_https_panels_require_an_https_coordinator() {
        let inv = parse(&format!("https://vpn.example.org:8443/join#{TOKEN}")).unwrap();
        assert_eq!(inv.host(), "vpn.example.org:8443");
        let https = "https://vpn.example.org";
        assert!(server_url_ok(https, "https://vpn.example.org"));
        assert!(server_url_ok(https, ""));
        assert!(server_url_ok(https, "http://127.0.0.1:8080"));
        assert!(!server_url_ok(https, "http://vpn.example.org"));
        assert!(!server_url_ok(https, "evil.example"));
        // A loopback test panel may point at a plain-HTTP coordinator.
        assert!(server_url_ok("http://127.0.0.1:3000", "http://bs-e2e:8080"));
    }

    /// Shapes copied from the panel's /api/me/invites answers.
    #[test]
    fn member_api_answers_parse() {
        let mine: MyInvites = serde_json::from_str(
            r#"{"user":"tales","groups":["familia"],"invites":[{"id":"9f2c","group":"familia",
            "mutual":false,"note":"vm","createdAt":"2026-11-17T20:01:02.000Z","expiresAt":null,
            "maxUses":1,"uses":[{"user":"vm-teste","at":"2026-11-17T20:02:00.000Z"}],
            "inviter":{"user":"tales","device":"tales"},"status":"used"}]}"#,
        )
        .unwrap();
        assert_eq!(mine.groups, ["familia"]);
        assert_eq!(mine.invites[0].uses[0].user, "vm-teste");
        assert_eq!(mine.invites[0].status, "used");
        let created: CreatedInvite = serde_json::from_str(
            r#"{"invite":{"id":"a1","group":null,"note":"","createdAt":"2026-11-17T20:01:02.000Z",
            "status":"active","uses":[]},"token":"x","link":"https://vpn.example.org/join#x"}"#,
        )
        .unwrap();
        assert_eq!(created.link, "https://vpn.example.org/join#x");
        assert!(cancel("../../admin").is_err(), "ids that could reshape the path are refused");
    }

    #[test]
    fn qr_codes_are_square_and_decodable_sized() {
        let m = qr_modules("https://vpn.example.org/join#AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde").unwrap();
        assert!(m.len() >= 21 && m.iter().all(|row| row.len() == m.len()));
        // Finder pattern corner is dark.
        assert!(m[0][0]);
    }

    #[test]
    fn panel_error_codes_map_to_explainable_errors() {
        assert_eq!(
            classify("name_taken", Some("maria-2".into())),
            JoinError::NameTaken(Some("maria-2".into()))
        );
        assert_eq!(classify("invalid_name", None), JoinError::InvalidName);
        for c in ["unknown", "expired", "used", "revoked", "pending"] {
            assert_eq!(classify(c, None), JoinError::InviteUnusable(c.into()));
        }
        assert_eq!(classify("too_many_attempts", None), JoinError::TooManyAttempts);
        assert!(matches!(classify("server_error", None), JoinError::Other(_)));
    }

    /// Read one whole HTTP request (head + Content-Length body).
    fn read_request(s: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    return text;
                }
            }
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0, "client closed early");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Redeem against a fake panel on loopback: success and a name clash.
    #[test]
    fn redeem_talks_to_the_panel_join_endpoint() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in [
                (200, r#"{"authkey":"hskey-auth-x","server_url":"https://vpn.example.org","user":"maria","group":"familia"}"#),
                (409, r#"{"error":"name_taken","suggestion":"maria-2"}"#),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                seen.push(read_request(&mut s));
                let (code, body) = answer;
                let reason = if code == 200 { "OK" } else { "Conflict" };
                write!(
                    s,
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });

        let invite = parse(&format!("http://{addr}/join#{TOKEN}")).unwrap();
        let joined = redeem(&invite, "Maria").unwrap();
        assert_eq!(joined.user, "maria");
        assert_eq!(joined.group.as_deref(), Some("familia"));
        assert_eq!(
            redeem(&invite, "Maria").unwrap_err(),
            JoinError::NameTaken(Some("maria-2".into()))
        );

        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /api/join HTTP/1.1"));
        assert!(seen[0].contains(TOKEN) && seen[0].contains("\"name\":\"Maria\""));
    }

    /// Rejoin with the kept secret: success (new secret), then the account
    /// is gone; a plain-HTTP panel off loopback is refused before any call.
    #[test]
    fn rejoin_talks_to_the_panel_rejoin_endpoint() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (code, reason, body) in [
                (200, "OK", r#"{"authkey":"hskey-auth-y","server_url":"https://vpn.example.org","user":"maria","reclaim":"new-secret"}"#),
                (410, "Gone", r#"{"error":"revoked"}"#),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                seen.push(read_request(&mut s));
                write!(
                    s,
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });

        let panel = format!("http://{addr}");
        let joined = rejoin(&panel, "maria", "old-secret", "maria-laptop").unwrap();
        assert_eq!(joined.authkey, "hskey-auth-y");
        assert_eq!(joined.reclaim.as_deref(), Some("new-secret"));
        assert_eq!(
            rejoin(&panel, "maria", "old-secret", "maria-laptop").unwrap_err(),
            JoinError::InviteUnusable("revoked".into())
        );
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /api/rejoin HTTP/1.1"));
        assert!(seen[0].contains("\"reclaim\":\"old-secret\"") && seen[0].contains("\"hostname\":\"maria-laptop\""));

        assert!(matches!(
            rejoin("http://vpn.example.org", "maria", "s", "h"),
            Err(JoinError::Other(_))
        ));
    }
}

// ─── Inviting (any member, over the tailnet) ─────────────────────────────────
//
// Talks to the panel's peer API on its tailnet address: the panel knows who
// we are from the tunnel itself, so no password is involved. What an invite
// grants is decided by the panel and bounded by what we have: the newcomer
// reaches our devices (and we theirs), plus at most one group we're in.

/// One invite we created.
#[derive(Debug, Clone, Deserialize)]
pub struct SentInvite {
    pub id: String,
    pub group: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// `active`, `used`, `expired` or `revoked`.
    pub status: String,
    #[serde(default)]
    pub uses: Vec<InviteUse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InviteUse {
    pub user: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MyInvites {
    pub invites: Vec<SentInvite>,
    /// Groups we're in — the only ones we can invite into.
    pub groups: Vec<String>,
    /// Whether we may create invites (false: administrators only).
    #[serde(default = "yes")]
    pub can_invite: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedInvite {
    /// Link to share (also what the QR code encodes).
    pub link: String,
    #[serde(default)]
    pub invite: Option<CreatedStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedStatus {
    /// `pending`: works only after an administrator approves it.
    pub status: String,
}

impl CreatedInvite {
    pub fn awaits_approval(&self) -> bool {
        self.invite.as_ref().is_some_and(|i| i.status == "pending")
    }
}

/// Why the member API can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberError {
    /// Not on a BigScale network, or its panel predates member invites.
    Unsupported,
    /// The panel doesn't take this device as a person (tagged device…).
    NotAMember,
    /// The panel couldn't confirm this device's key with its BigLace agent
    /// (agent not running or not reachable from the panel).
    Unverified,
    /// The panel knows this device by another key (BigLace was reinstalled
    /// or its key file lost): sign out and join again.
    KeyMismatch,
    NotInGroup,
    /// The network has no access rules (everyone reaches everyone), so only
    /// the administrator may let people in.
    OpenNetwork,
    TooMany,
    Other(String),
}

fn member_call(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> std::result::Result<ureq::Response, MemberError> {
    let base = panel::panel_tailnet_url().ok_or(MemberError::Unsupported)?;
    let agent = panel::shared_agent().map_err(|e| MemberError::Other(e.to_string()))?;
    let body = body.map(|b| b.to_string());
    // Signed with this device's key: the source address alone is not proof
    // (see device_key).
    let res = crate::device_key::send(&agent, method, &format!("{base}{path}"), body.as_deref());
    match res {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, resp)) => {
            let err = panel::read_json_limited::<ErrorBody>(resp)
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_default();
            Err(match (code, err.as_str()) {
                (404, "") | (405, _) => MemberError::Unsupported,
                (_, "not_a_member") => MemberError::NotAMember,
                // Unsigned (no device key could be made) or not provable.
                (401, "signature_required" | "device_unverified") => MemberError::Unverified,
                (401, "bad_signature" | "stale_request" | "replayed") => MemberError::Other(err),
                (401, "device_key_mismatch") => MemberError::KeyMismatch,
                (_, "not_in_group") => MemberError::NotInGroup,
                (_, "open_network") => MemberError::OpenNetwork,
                (429, _) => MemberError::TooMany,
                _ => MemberError::Other(if err.is_empty() { format!("HTTP {code}") } else { err }),
            })
        }
        Err(e) => Err(MemberError::Other(e.to_string())),
    }
}

/// What the panel did when this device left the network.
#[derive(Debug, Clone, Deserialize)]
pub struct Left {
    #[serde(default)]
    pub account_deleted: bool,
    /// Other devices still signed in to the account (it isn't deleted then).
    #[serde(default)]
    pub other_devices: u32,
}

/// Tell the panel this device is leaving: it removes the device (freeing its
/// name) shortly after answering and, with `delete_account`, the account too
/// when this was its last device. Blocking; needs the tunnel up.
pub fn leave(delete_account: bool) -> std::result::Result<Left, MemberError> {
    let body = serde_json::json!({ "delete_account": delete_account });
    let r = member_call("POST", "/api/me/leave", Some(body))?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

/// Our invites and the groups we can invite into. Blocking.
pub fn my_invites() -> std::result::Result<MyInvites, MemberError> {
    let r = member_call("GET", "/api/me/invites", None)?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

/// Create an invite (optionally also into `group`). Blocking.
pub fn create(group: Option<&str>, note: &str) -> std::result::Result<CreatedInvite, MemberError> {
    let body = serde_json::json!({ "group": group, "note": note });
    let r = member_call("POST", "/api/me/invites", Some(body))?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

/// Cancel one of our invites. Blocking.
pub fn cancel(id: &str) -> std::result::Result<(), MemberError> {
    // Ids are hex from the panel; refuse anything that could reshape the path.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MemberError::Other("invalid invite id".into()));
    }
    member_call("DELETE", &format!("/api/me/invites/{id}"), None).map(|_| ())
}

// ─── Groups (any member, over the tailnet) ──────────────────────────────────

/// A group we're in.
#[derive(Debug, Clone, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    /// `member`: created by someone from BigLace; `network`: by the admin.
    pub kind: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
}

impl GroupInfo {
    pub fn is_member_group(&self) -> bool {
        self.kind == "member"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MyGroups {
    pub user: String,
    /// In the network's `admins` group: may open the admin panel.
    #[serde(default)]
    pub is_admin: bool,
    pub groups: Vec<GroupInfo>,
}

/// Group names the panel accepts: lowercase letters, digits and hyphens.
pub fn group_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && name != "admins"
}

/// Path segment for a name or user, refusing anything that could reshape it.
fn segment(s: &str) -> std::result::Result<&str, MemberError> {
    if !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        Ok(s)
    } else {
        Err(MemberError::Other("invalid name".into()))
    }
}

pub fn my_groups() -> std::result::Result<MyGroups, MemberError> {
    let r = member_call("GET", "/api/me/groups", None)?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

pub fn create_group(name: &str) -> std::result::Result<GroupInfo, MemberError> {
    let r = member_call("POST", "/api/me/groups", Some(serde_json::json!({ "name": name })))?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

pub fn delete_group(name: &str) -> std::result::Result<(), MemberError> {
    member_call("DELETE", &format!("/api/me/groups/{}", segment(name)?), None).map(|_| ())
}

/// The owner removing `user`, or ourselves leaving.
pub fn remove_member(group: &str, user: &str) -> std::result::Result<(), MemberError> {
    member_call(
        "DELETE",
        &format!("/api/me/groups/{}/members/{}", segment(group)?, segment(user)?),
        None,
    )
    .map(|_| ())
}

// ─── Voice rooms (presence on the panel; sound goes peer to peer) ──────────

/// Someone in a group's voice room.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Participant {
    pub user: String,
    pub device: String,
    /// Tailnet IPv4 — where the others connect for the sound.
    pub ip: String,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomInfo {
    pub group: String,
    #[serde(default)]
    pub participants: Vec<Participant>,
}

#[derive(Deserialize)]
struct RoomsResp {
    rooms: Vec<RoomInfo>,
}

#[derive(Deserialize)]
struct Roster {
    participants: Vec<Participant>,
}

/// The rooms of our groups and who's in them.
pub fn rooms() -> std::result::Result<Vec<RoomInfo>, MemberError> {
    let r = member_call("GET", "/api/me/rooms", None)?;
    panel::read_json_limited::<RoomsResp>(r)
        .map(|r| r.rooms)
        .map_err(|e| MemberError::Other(e.to_string()))
}

/// Enter (or stay in — repeat every few seconds) `group`'s room.
pub fn room_join(group: &str, muted: bool) -> std::result::Result<Vec<Participant>, MemberError> {
    let body = serde_json::json!({ "muted": muted });
    let r = member_call("POST", &format!("/api/me/rooms/{}/join", segment(group)?), Some(body))?;
    panel::read_json_limited::<Roster>(r)
        .map(|r| r.participants)
        .map_err(|e| MemberError::Other(e.to_string()))
}

pub fn room_leave(group: &str) -> std::result::Result<(), MemberError> {
    member_call("POST", &format!("/api/me/rooms/{}/leave", segment(group)?), Some(serde_json::json!({}))).map(|_| ())
}

// ─── Asking people already on the network into a group ──────────────────────
//
// Nothing changes until the person asked accepts, from their own BigLace.

/// Someone on the network we could ask into a group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Person {
    pub user: String,
    #[serde(default)]
    pub online: bool,
    /// `available`, `member` (already in) or `pending` (already asked).
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct People {
    pub people: Vec<Person>,
    /// `contacts`: only people we're connected to are listed; anyone else
    /// by exact name. `network`: everyone.
    #[serde(default)]
    pub directory: String,
}

/// "Come into my group?" — sent or received, not answered yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GroupRequest {
    pub id: String,
    pub group: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupRequests {
    #[serde(default)]
    pub incoming: Vec<GroupRequest>,
}

#[derive(Deserialize)]
struct Sent {
    sent: u32,
}

/// Request ids are hex from the panel; refuse anything that could reshape the path.
fn request_id(id: &str) -> std::result::Result<&str, MemberError> {
    if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(MemberError::Other("invalid request id".into()))
    }
}

/// People on the network we could ask into `group` (one we're in).
pub fn people(group: &str) -> std::result::Result<People, MemberError> {
    let r = member_call("GET", &format!("/api/me/groups/{}/people", segment(group)?), None)?;
    panel::read_json_limited::<People>(r).map_err(|e| MemberError::Other(e.to_string()))
}

/// An account name as the panel spells them (what someone types to invite
/// a person by name).
pub fn user_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Ask `users` into `group`. Returns how many requests went out.
pub fn ask_into_group(group: &str, users: &[String]) -> std::result::Result<u32, MemberError> {
    let body = serde_json::json!({ "users": users });
    let r = member_call("POST", &format!("/api/me/groups/{}/requests", segment(group)?), Some(body))?;
    panel::read_json_limited::<Sent>(r)
        .map(|s| s.sent)
        .map_err(|e| MemberError::Other(e.to_string()))
}

pub fn group_requests() -> std::result::Result<GroupRequests, MemberError> {
    let r = member_call("GET", "/api/me/requests", None)?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

/// Accept (we join the group) or decline a request sent to us.
pub fn answer_request(id: &str, accept: bool) -> std::result::Result<(), MemberError> {
    let body = serde_json::json!({ "accept": accept });
    member_call("POST", &format!("/api/me/requests/{}", request_id(id)?), Some(body)).map(|_| ())
}

/// What an invite link does for someone already on its network.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GroupLink {
    pub group: Option<String>,
    pub invited_by: Option<String>,
}

/// An invite link opened while already signed in to its network: it brings
/// us into its group (no new account). With `preview`, only says what it
/// would do.
pub fn redeem_as_member(invite: &Invite, preview: bool) -> std::result::Result<GroupLink, MemberError> {
    let body = serde_json::json!({ "invite": invite.token, "preview": preview });
    let r = member_call("POST", "/api/me/redeem", Some(body))?;
    panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))
}

#[derive(Deserialize)]
struct AdminLink {
    url: String,
}

/// One-time link signing the browser in to the admin panel (admins only).
pub fn admin_login() -> std::result::Result<String, MemberError> {
    let r = member_call("POST", "/api/me/admin-login", Some(serde_json::json!({})))?;
    let link: AdminLink = panel::read_json_limited(r).map_err(|e| MemberError::Other(e.to_string()))?;
    if !(link.url.starts_with("https://") || panel::url_is_loopback(&link.url)) {
        return Err(MemberError::Other("panel returned a link without HTTPS".into()));
    }
    Ok(link.url)
}

/// The QR code for `text` as a square grid of dark (true) modules, without
/// the quiet zone. Medium error correction, like the panel's QR.
pub fn qr_modules(text: &str) -> Option<Vec<Vec<bool>>> {
    let qr = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()?;
    let n = qr.size();
    Some((0..n).map(|y| (0..n).map(|x| qr.get_module(x, y)).collect()).collect())
}
