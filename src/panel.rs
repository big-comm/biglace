use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

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

    // ureq 2.x doesn't auto-wire native-tls — must pass the connector explicitly.
    let tls = native_tls::TlsConnector::new().context("init TLS connector")?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .tls_connector(Arc::new(tls))
        .build();

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
