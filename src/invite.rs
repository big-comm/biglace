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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Joined {
    pub authkey: String,
    pub server_url: String,
    /// Account name the panel created (sanitized from what the user typed).
    pub user: String,
    pub group: Option<String>,
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
        "unknown" | "expired" | "used" | "revoked" => JoinError::InviteUnusable(code.to_string()),
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
            if joined.authkey.trim().is_empty()
                || joined.authkey.len() > 4096
                || joined.authkey.chars().any(char::is_control)
            {
                return Err(JoinError::Other("panel returned an invalid pre-auth key".into()));
            }
            if !server_url_ok(&invite.panel, &joined.server_url) {
                return Err(JoinError::Other(
                    "panel returned a coordinator address without HTTPS".into(),
                ));
            }
            Ok(joined)
        }
        Err(ureq::Error::Status(code, resp)) => {
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
            Err(classify(&err, suggestion))
        }
        Err(e) => Err(JoinError::Other(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn panel_error_codes_map_to_explainable_errors() {
        assert_eq!(
            classify("name_taken", Some("maria-2".into())),
            JoinError::NameTaken(Some("maria-2".into()))
        );
        assert_eq!(classify("invalid_name", None), JoinError::InvalidName);
        for c in ["unknown", "expired", "used", "revoked"] {
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
}
