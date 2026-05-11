use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Process-wide ureq agent. Building one allocates a TlsConnector and an
/// internal connection pool — both are reusable across requests, so we keep
/// a single instance for the lifetime of the app. The OnceLock holds a
/// Result so a failed TLS init can still be surfaced to each caller without
/// retrying the (unlikely-to-recover) initialization.
fn shared_agent() -> Result<ureq::Agent> {
    static AGENT: OnceLock<std::result::Result<ureq::Agent, String>> = OnceLock::new();
    let cell = AGENT.get_or_init(|| {
        let tls = native_tls::TlsConnector::new()
            .map_err(|e| format!("init TLS connector: {e}"))?;
        Ok(ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .tls_connector(Arc::new(tls))
            .build())
    });
    cell.clone().map_err(|e| anyhow!("{e}"))
}

#[derive(Debug, Clone)]
pub struct PanelCredentials {
    pub url:      String,
    pub username: String,
    pub password: String,
    pub node:     String,
    pub hostname: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreAuthResponse {
    pub authkey:    String,
    pub server_url: String,
}

#[derive(Serialize)]
struct PreAuthRequest<'a> {
    username:  &'a str,
    password:  &'a str,
    node_user: &'a str,
    hostname:  &'a str,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

pub fn request_preauth(creds: &PanelCredentials) -> Result<PreAuthResponse> {
    let base = creds.url.trim_end_matches('/');
    let endpoint = format!("{base}/api/v1/preauth-key");

    let agent = shared_agent()?;

    let body = PreAuthRequest {
        username:  &creds.username,
        password:  &creds.password,
        node_user: &creds.node,
        hostname:  &creds.hostname,
    };

    let response = agent
        .post(&endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_json(serde_json::to_value(&body).context("serialize body")?);

    match response {
        Ok(resp) => {
            let parsed: PreAuthResponse = resp
                .into_json()
                .context("invalid response from panel")?;
            Ok(parsed)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_json::<ErrorBody>()
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(anyhow!("{detail}"))
        }
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
    let Some(base) = panel_tailnet_url() else { return Ok(()); };
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
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_json::<ErrorBody>()
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(anyhow!("{detail}"))
        }
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
    let Some(base) = panel_tailnet_url() else { return Ok(HashMap::new()); };
    let endpoint = format!("{base}/api/devices/os-users");
    let agent = shared_agent()?;

    let resp = agent.get(&endpoint).set("Accept", "application/json").call();
    match resp {
        Ok(r) => Ok(r.into_json::<HashMap<String, String>>().unwrap_or_default()),
        // 401 on this endpoint means "panel doesn't recognize me as a tailnet
        // peer" — typically because the panel is older than this client.
        // Silent fallback keeps biglace usable.
        Err(ureq::Error::Status(401, _)) => Ok(HashMap::new()),
        // 404: endpoint missing on older BigScale builds. Same fallback.
        Err(ureq::Error::Status(404, _)) => Ok(HashMap::new()),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_json::<ErrorBody>()
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(anyhow!("{detail}"))
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}
