use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Process-wide ureq agent. Building one allocates a TlsConnector and an
/// internal connection pool — both are reusable across requests, so we keep
/// a single instance for the lifetime of the app. The OnceLock holds a
/// Result so a failed TLS init can still be surfaced to each caller without
/// retrying the (unlikely-to-recover) initialization.
pub(crate) fn shared_agent() -> Result<ureq::Agent> {
    static AGENT: OnceLock<std::result::Result<ureq::Agent, String>> = OnceLock::new();
    AGENT
        .get_or_init(|| build_agent(0))
        .clone()
        .map_err(|e| anyhow!("{e}"))
}

/// Same agent, but following up to three redirects. Only for requests that
/// carry no credentials — panel calls keep `redirects(0)` so a hostile or
/// misconfigured redirect can't replay the user's password (or a tailnet-
/// authenticated request) against a different host.
pub(crate) fn redirecting_agent() -> Result<ureq::Agent> {
    static AGENT: OnceLock<std::result::Result<ureq::Agent, String>> = OnceLock::new();
    AGENT
        .get_or_init(|| build_agent(3))
        .clone()
        .map_err(|e| anyhow!("{e}"))
}

fn build_agent(redirects: u32) -> std::result::Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|e| format!("init TLS connector: {e}"))?;
    Ok(ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .redirects(redirects)
        .tls_connector(Arc::new(tls))
        .build())
}

#[derive(Debug, Clone)]
pub struct PanelCredentials {
    pub url: String,
    pub username: String,
    pub password: String,
    pub node: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreAuthResponse {
    pub authkey: String,
    pub server_url: String,
}

/// Normalize a credential-bearing panel URL and reject cleartext remote HTTP.
pub fn normalize_panel_url(input: &str) -> Result<String> {
    let value = input.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err(anyhow!("panel URL is empty"));
    }
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    if value.contains('?') || value.contains('#') {
        return Err(anyhow!("panel URL must not contain a query or fragment"));
    }
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| anyhow!("invalid panel URL"))?;
    if rest.is_empty() || rest.starts_with('/') || authority_host(rest).is_none_or(str::is_empty) {
        return Err(anyhow!("invalid panel URL"));
    }
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(value),
        "http" if url_is_loopback(&value) => Ok(value),
        "http" => Err(anyhow!("HTTPS is required for a remote panel")),
        _ => Err(anyhow!("panel URL must use HTTPS")),
    }
}

pub(crate) fn url_is_loopback(input: &str) -> bool {
    input
        .split_once("://")
        .and_then(|(_, rest)| authority_host(rest))
        .is_some_and(is_loopback_host)
}

pub(crate) fn url_is_local_server_response(input: &str) -> bool {
    input
        .split_once("://")
        .and_then(|(_, rest)| authority_host(rest))
        .is_some_and(|host| is_loopback_host(host) || host == "0.0.0.0" || host == "::")
}

fn authority_host(rest: &str) -> Option<&str> {
    let authority = rest.split('/').next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split_once(']').map(|(host, _)| host);
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn read_json_limited<T: DeserializeOwned>(response: ureq::Response) -> Result<T> {
    if response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|size| size > MAX_RESPONSE_BYTES)
    {
        return Err(anyhow!("panel response is too large"));
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .context("read panel response")?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(anyhow!("panel response is too large"));
    }
    serde_json::from_slice(&body).context("invalid response from panel")
}

fn response_error(code: u16, response: ureq::Response) -> anyhow::Error {
    let detail = read_json_limited::<ErrorBody>(response)
        .ok()
        .and_then(|body| body.error)
        .unwrap_or_else(|| format!("HTTP {code}"));
    anyhow!(detail)
}

#[derive(Serialize)]
struct PreAuthRequest<'a> {
    username: &'a str,
    password: &'a str,
    node_user: &'a str,
    hostname: &'a str,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

pub fn request_preauth(creds: &PanelCredentials) -> Result<PreAuthResponse> {
    let base = normalize_panel_url(&creds.url)?;
    let endpoint = format!("{base}/api/v1/preauth-key");

    let agent = shared_agent()?;

    let body = PreAuthRequest {
        username: &creds.username,
        password: &creds.password,
        node_user: &creds.node,
        hostname: &creds.hostname,
    };

    let response = agent
        .post(&endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_json(serde_json::to_value(&body).context("serialize body")?);

    match response {
        Ok(resp) => {
            let parsed: PreAuthResponse = read_json_limited(resp)?;
            if parsed.authkey.trim().is_empty()
                || parsed.authkey.len() > 4096
                || parsed.authkey.chars().any(char::is_control)
            {
                return Err(anyhow!("panel returned an invalid pre-auth key"));
            }
            Ok(parsed)
        }
        Err(ureq::Error::Status(code, resp)) => Err(response_error(code, resp)),
        Err(e) => Err(anyhow!("{e}")),
    }
}

// ─── Device metadata (Option D — OS user propagation) ────────────────────────

/// Tailnet-reachable URL of the BigScale panel. Returns None when:
///   - tailscaled isn't running / not reporting a suffix (disconnected),
///   - no tailnet peer has DNSName `panel.<suffix>` (non-BigScale tailnet).
///
/// We resolve the panel via tailscaled's status JSON instead of OS-level DNS
/// because MagicDNS isn't always wired into the host's resolver (broken
/// resolvconf, containers, NSS quirks). Going through tailscaled's known peers
/// works whenever the tunnel is up, regardless of /etc/resolv.conf.
///
/// Both endpoints below authenticate by tailnet source IP (the panel resolves
/// 100.64/10 → node via the engine), so the request MUST hit the panel's
/// tailnet interface — the public FQDN would land on a reverse proxy and the
/// panel would see the proxy's IP instead of the peer's.
fn panel_tailnet_url() -> Option<String> {
    let ip = crate::tailscale::panel_peer_ip()?;
    Some(format!("http://{ip}:3000"))
}

#[derive(Serialize)]
struct OsUserBody<'a> {
    os_user: &'a str,
}

/// Tell the BigScale panel which OS user runs biglace on this device. The
/// panel identifies the caller via its tailnet source IP (after `tailscale up`
/// completed), so no auth header is sent. Returns Ok(()) on 200 *and* on 304
/// (idempotent — value unchanged), and also Ok(()) when the current tailnet
/// is non-BigScale (no `panel.<suffix>` to talk to) — biglace must keep
/// working as a plain headscale/tailscale client in that case.
pub fn post_os_user(os_user: &str) -> Result<()> {
    let Some(base) = panel_tailnet_url() else {
        return Ok(());
    };
    let endpoint = format!("{base}/api/devices/me/os-user");
    let agent = shared_agent()?;

    let response = agent
        .post(&endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_json(serde_json::to_value(OsUserBody { os_user }).context("serialize body")?);

    match response {
        Ok(_) => Ok(()),
        // 304 surfaces as Status — treat as success.
        Err(ureq::Error::Status(304, _)) => Ok(()),
        Err(ureq::Error::Status(code, resp)) => Err(response_error(code, resp)),
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// Fetch the `hostname → os_user` map from the panel's peer-facing endpoint.
/// Auth is the tunnel itself (source IP → node via the engine), so no admin
/// session is required — works for any tailnet member.
///
/// Returns an empty map (Ok) on a non-BigScale tailnet — same rationale as
/// `post_os_user`: don't break headscale/tailscale-oficial users. Treats 401
/// the same way: a panel that doesn't expose this endpoint (older BigScale,
/// or a vanilla coordinator with `panel.<suffix>` somehow present) shouldn't
/// surface as an error in the UI — biglace just falls back to using the
/// peer's hostname as the SSH login.
pub fn fetch_device_meta() -> Result<HashMap<String, String>> {
    let Some(base) = panel_tailnet_url() else {
        return Ok(HashMap::new());
    };
    let endpoint = format!("{base}/api/devices/os-users");
    let agent = shared_agent()?;

    let resp = agent
        .get(&endpoint)
        .set("Accept", "application/json")
        .call();
    match resp {
        // A 200 whose body isn't the expected map means the panel changed its
        // response shape (e.g. an upgrade) — degrade to the hostname-as-login
        // fallback like the empty-map cases, but log it so this is diagnosable
        // without a packet capture (the 401/404 fallbacks below are deliberate
        // and stay quiet; a malformed 200 is not).
        Ok(r) => Ok(
            read_json_limited::<HashMap<String, String>>(r).unwrap_or_else(|e| {
                eprintln!("[biglace] panel: os-users response wasn't the expected JSON map: {e}");
                HashMap::new()
            }),
        ),
        // 401 on this endpoint means "panel doesn't recognize me as a tailnet
        // peer" — typically because the panel is older than this client.
        // Silent fallback keeps biglace usable.
        Err(ureq::Error::Status(401, _)) => Ok(HashMap::new()),
        // 404: endpoint missing on older BigScale builds. Same fallback.
        Err(ureq::Error::Status(404, _)) => Ok(HashMap::new()),
        Err(ureq::Error::Status(code, resp)) => Err(response_error(code, resp)),
        Err(e) => Err(anyhow!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_url_requires_tls_except_loopback() {
        assert_eq!(
            normalize_panel_url("panel.example.org").unwrap(),
            "https://panel.example.org"
        );
        assert!(normalize_panel_url("http://panel.example.org").is_err());
        assert!(normalize_panel_url("http://127.0.0.1:3000").is_ok());
        assert!(normalize_panel_url("https://user:pass@example.org").is_err());
    }

    #[test]
    fn loopback_check_parses_authority_only() {
        assert!(url_is_loopback("https://localhost:8443/path"));
        assert!(!url_is_loopback("https://example.org/localhost"));
    }
}
