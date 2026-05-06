use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Build a ureq agent that knows how to do TLS via native-tls. The default
/// builder doesn't wire it in — calls to https:// fail without this.
fn build_agent() -> Result<ureq::Agent> {
    let tls = native_tls::TlsConnector::new().context("init TLS connector")?;
    Ok(ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .tls_connector(Arc::new(tls))
        .build())
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

    let agent = build_agent()?;

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

/// Tailnet-reachable address of the BigScale panel. The os-user POST and the
/// node-list GET both authenticate by tailnet source IP, so the request must
/// hit the panel's tailnet interface — going through the public FQDN would
/// land on the reverse proxy and the panel would see the proxy's IP.
const PANEL_TAILNET_URL: &str = "http://panel.bigscale.net:3000";

#[derive(Serialize)]
struct OsUserBody<'a> {
    os_user: &'a str,
}

/// Tell the BigScale panel which OS user runs biglace on this device. The
/// panel identifies the caller via its tailnet source IP (after `tailscale up`
/// completed), so no auth header is sent. Returns Ok(()) on 200 *and* on 304
/// (idempotent — value unchanged).
pub fn post_os_user(os_user: &str) -> Result<()> {
    let endpoint = format!("{PANEL_TAILNET_URL}/api/devices/me/os-user");
    let agent = build_agent()?;

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

/// Subset of the `/api/bs/v1/node` response we actually consume — `name`
/// matches the tailscale hostname (peer.hostname), `os_user` is the field the
/// panel adds on top of the engine's native payload. Optional everywhere so a
/// schema change (or a node missing `os_user` because it never ran biglace)
/// doesn't break the parse.
#[derive(Deserialize)]
struct NodesResponse {
    #[serde(default)]
    nodes: Vec<NodeEntry>,
}

#[derive(Deserialize)]
struct NodeEntry {
    #[serde(default)]
    name:    Option<String>,
    #[serde(default, alias = "givenName")]
    given_name: Option<String>,
    #[serde(default)]
    os_user: Option<String>,
}

/// Fetch the merged node list from the panel and return a `hostname → os_user`
/// map. Hosts without an `os_user` are skipped (caller falls back to hostname).
pub fn fetch_device_meta() -> Result<HashMap<String, String>> {
    let endpoint = format!("{PANEL_TAILNET_URL}/api/bs/v1/node");
    let agent = build_agent()?;

    let resp = agent.get(&endpoint).set("Accept", "application/json").call();
    let parsed: NodesResponse = match resp {
        Ok(r) => r.into_json().context("invalid response from panel")?,
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_json::<ErrorBody>()
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_else(|| format!("HTTP {code}"));
            return Err(anyhow!("{detail}"));
        }
        Err(e) => return Err(anyhow!("{e}")),
    };

    let mut out = HashMap::with_capacity(parsed.nodes.len());
    for n in parsed.nodes {
        let Some(os_user) = n.os_user.filter(|s| !s.is_empty()) else { continue; };
        // Prefer `name`; some headscale versions only emit `givenName`.
        let host = n.name.or(n.given_name).unwrap_or_default();
        if !host.is_empty() {
            out.insert(host, os_user);
        }
    }
    Ok(out)
}
