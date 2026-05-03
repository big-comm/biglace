use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

use crate::{tr, trf};

// ─── Logging ─────────────────────────────────────────────────────────────────
// Info-level lines are always printed to stderr so the user can follow what
// the app is doing. Setting BIGLACE_DEBUG=1 additionally dumps the full
// stdout/stderr of every tailscale invocation.

fn debug_enabled() -> bool {
    std::env::var("BIGLACE_DEBUG").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

pub(crate) fn dbg(msg: &str) {
    eprintln!("[biglace] {msg}");
}

fn dbg_output(label: &str, args: &[&str], status: i32, stdout: &str, stderr: &str) {
    eprintln!("[biglace] {label} exit={status} args={args:?}");
    if debug_enabled() {
        if !stdout.trim().is_empty() {
            for line in stdout.lines() {
                eprintln!("[biglace]   stdout: {line}");
            }
        }
        if !stderr.trim().is_empty() {
            for line in stderr.lines() {
                eprintln!("[biglace]   stderr: {line}");
            }
        }
    } else {
        // Without verbose mode, still surface a single-line stderr summary
        // so the user sees *something* when a command fails.
        let trimmed = stderr.trim();
        if !trimmed.is_empty() {
            let first = trimmed.lines().next().unwrap_or("");
            eprintln!("[biglace]   stderr: {first}");
        }
    }
}

// ─── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub online:   bool,
    pub ip:       Option<String>,
    pub hostname: Option<String>,
    pub dns_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub hostname: String,
    pub ip:       String,
    pub dns_name: String,
    pub online:   bool,
    pub os:       String,
}

// ─── Tailscale JSON structs ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct TsStatus {
    #[serde(rename = "Self")]
    self_node: Option<TsNode>,
    #[serde(rename = "Peer")]
    peers: Option<std::collections::HashMap<String, TsNode>>,
    #[serde(rename = "Health")]
    health: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TsNode {
    #[serde(rename = "HostName")]
    hostname: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "OS")]
    os: Option<String>,
}

// ─── Detection ───────────────────────────────────────────────────────────────

pub fn is_installed() -> bool {
    Command::new("which")
        .arg("tailscale")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_service_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "tailscaled"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── Status ──────────────────────────────────────────────────────────────────

/// Returns the most relevant health-check message reported by tailscaled, if
/// any. `tailscale up` exits 0 even when the coordinator rejects the auth key
/// — the failure only shows up here. Login-related messages are prioritized.
pub fn get_health_issue() -> Option<String> {
    let out = Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let ts: TsStatus = serde_json::from_slice(&out.stdout).ok()?;
    let health = ts.health?;
    if health.is_empty() {
        return None;
    }
    // Prefer login/auth errors over generic connectivity warnings.
    let login_issue = health.iter().find(|h| {
        let l = h.to_lowercase();
        l.contains("logged out")
            || l.contains("auth-key")
            || l.contains("auth key")
            || l.contains("login error")
            || l.contains("not authorized")
    });
    Some(login_issue.cloned().unwrap_or_else(|| health[0].clone()))
}

pub fn get_status() -> Status {
    if !is_installed() {
        return Status::default();
    }

    let out = match Command::new("tailscale").args(["status", "--json"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Status::default(),
    };

    let ts: TsStatus = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return Status::default(),
    };

    let node = ts.self_node.as_ref();
    Status {
        online:   node.and_then(|n| n.online).unwrap_or(false),
        ip:       node.and_then(|n| n.ips.as_ref()?.first().cloned()),
        hostname: node.and_then(|n| n.hostname.clone()),
        dns_name: node
            .and_then(|n| n.dns_name.clone())
            .map(|d| d.trim_end_matches('.').to_string()),
    }
}

// ─── Peers ───────────────────────────────────────────────────────────────────

pub fn get_peers() -> Vec<Peer> {
    let out = match Command::new("tailscale").args(["status", "--json"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let ts: TsStatus = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut peers: Vec<Peer> = ts
        .peers
        .unwrap_or_default()
        .into_values()
        .map(|n| Peer {
            hostname: n.hostname.unwrap_or_default(),
            ip:       n.ips.as_ref().and_then(|v| v.first().cloned()).unwrap_or_default(),
            dns_name: n.dns_name.map(|d| d.trim_end_matches('.').to_string()).unwrap_or_default(),
            online:   n.online.unwrap_or(false),
            os:       n.os.unwrap_or_default(),
        })
        .collect();

    // online first, then alphabetical
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.hostname.cmp(&b.hostname)));
    peers
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Run `tailscale <args>`; if it fails because the user is not the configured
/// operator, retry once via `pkexec` and use that single elevation to also
/// mark the current user as operator — so subsequent calls don't need a
/// password at all.
fn run_tailscale_with_fallback(args: &[&str]) -> Result<()> {
    dbg(&format!("running: tailscale {}", args.join(" ")));
    let out = Command::new("tailscale")
        .args(args)
        .output()
        .with_context(|| tr!("Failed to run tailscale"))?;

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    dbg_output("tailscale", args, out.status.code().unwrap_or(-1), &stdout, &stderr);

    if out.status.success() {
        return Ok(());
    }

    let needs_root = stderr.contains("access denied")
        || stdout.contains("access denied")
        || stderr.contains("Access denied")
        || stdout.contains("Access denied")
        || stderr.contains("operator")
        || stderr.contains("must be root");

    if !needs_root {
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!(tr!("Failed to connect — check the server URL and key."));
        } else {
            bail!(msg.to_string());
        }
    }

    // Single pkexec invocation that (1) sets the current user as operator
    // (so future calls bypass elevation) and (2) runs the original command.
    // We use `sh -c` with positional args to avoid shell-escaping pitfalls:
    // `$0` is the username, `"$@"` expands to the original tailscale args.
    let user = std::env::var("USER").unwrap_or_default();
    let script =
        "tailscale set --operator=\"$0\" >/dev/null 2>&1 || true; exec tailscale \"$@\"";

    let mut sh_args: Vec<&str> = vec!["sh", "-c", script, &user];
    sh_args.extend_from_slice(args);

    dbg(&format!("running: pkexec sh -c '<set-operator + exec tailscale>' {} {}", user, args.join(" ")));
    let pk = Command::new("pkexec")
        .args(&sh_args)
        .output()
        .with_context(|| tr!("Failed to run pkexec tailscale"))?;

    let pk_err = String::from_utf8_lossy(&pk.stderr);
    let pk_out = String::from_utf8_lossy(&pk.stdout);
    dbg_output("pkexec tailscale", args, pk.status.code().unwrap_or(-1), &pk_out, &pk_err);

    if pk.status.success() {
        return Ok(());
    }

    let msg = pk_err.trim();
    if msg.is_empty() {
        bail!(tr!("Failed to connect — check the server URL and key."));
    } else {
        bail!(msg.to_string());
    }
}

pub fn connect(server: &str, authkey: &str, hostname: &str) -> Result<()> {
    dbg(&format!(
        "connect: server={server:?} hostname={hostname:?} authkey={}",
        if authkey.is_empty() { "<empty>" } else { "<provided>" }
    ));
    let pre_status = get_status();
    dbg(&format!(
        "connect: pre-status online={} hostname={:?} ip={:?}",
        pre_status.online, pre_status.hostname, pre_status.ip
    ));
    // Always try with the user-provided authkey first. If it succeeds, great.
    // If it fails because the key was consumed/expired, fall back to a
    // keyless attempt that relies on cached registration on disk.
    let first = try_connect(server, authkey, hostname);
    if let Err(e) = first {
        dbg(&format!("connect: first attempt failed: {e}"));
        let msg = e.to_string().to_lowercase();
        let auth_failed = msg.contains("auth-key")
            || msg.contains("authkey")
            || msg.contains("not found")
            || msg.contains("expired")
            || msg.contains("already used")
            || msg.contains("invalid key");
        if authkey.is_empty() || !auth_failed {
            return Err(e);
        }
        dbg("connect: retrying without authkey (cached registration)");
        try_connect(server, "", hostname)?;
    }

    // `tailscale up` exits 0 even when the coordinator rejects the key.
    // The real failure only appears in Self.Online and the Health array.
    // Wait briefly for the daemon to settle, then verify.
    for _ in 0..10 {
        let post = get_status();
        if post.online {
            dbg(&format!("connect: post-status online=true hostname={:?} ip={:?}",
                post.hostname, post.ip));
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    let post = get_status();
    dbg(&format!("connect: post-status online={} hostname={:?} ip={:?}",
        post.online, post.hostname, post.ip));
    if let Some(h) = get_health_issue() {
        dbg(&format!("connect: tailscaled health: {h}"));
        let lower = h.to_lowercase();
        if lower.contains("auth-key not found") || lower.contains("auth key not found") {
            bail!(tr!("The pre-auth key was rejected by the server. Generate a new key in the BigScale panel and try again."));
        }
        if lower.contains("logged out") || lower.contains("login error") {
            bail!(trf!("Login rejected by server: {detail}", "detail" => h));
        }
        bail!(h);
    }
    bail!(tr!("Failed to connect — check the server URL and key."))
}

fn try_connect(server: &str, authkey: &str, hostname: &str) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut args = vec!["up", "--accept-routes"];

    let s_arg;
    let a_arg;
    let h_arg;
    let op_arg;

    if !server.is_empty() {
        s_arg = format!("--login-server={server}");
        args.push(&s_arg);
    }
    if !authkey.is_empty() {
        a_arg = format!("--authkey={authkey}");
        args.push(&a_arg);
        // Required when switching to a login-server that differs from the
        // one cached locally — without this, tailscale aborts with
        // "can't change --login-server without --force-reauth".
        args.push("--force-reauth");
    }
    if !hostname.is_empty() {
        h_arg = format!("--hostname={hostname}");
        args.push(&h_arg);
    }
    // Without --reset, `tailscale up` requires every non-default pref to be
    // mentioned explicitly. Operator is one of those once we set it, so pass
    // it back on every up to keep the call self-consistent.
    if !user.is_empty() {
        op_arg = format!("--operator={user}");
        args.push(&op_arg);
    }

    run_tailscale_with_fallback(&args)
}

pub fn disconnect() -> Result<()> {
    run_tailscale_with_fallback(&["down"])
}

/// One-time setup: make `$USER` the tailscale operator so subsequent
/// `up`/`down` calls don't need pkexec. Always runs through pkexec.
pub fn set_operator_current_user() -> Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        bail!(tr!("Could not determine the current user."));
    }
    let arg = format!("--operator={user}");
    let st = Command::new("pkexec")
        .args(["tailscale", "set", &arg])
        .status()
        .with_context(|| tr!("Failed to run pkexec tailscale set"))?;
    if !st.success() {
        bail!(tr!("Failed to set tailscale operator."));
    }
    Ok(())
}

pub fn open_files(ip: &str) {
    let _ = Command::new("xdg-open")
        .arg(format!("sftp://{ip}/"))
        .spawn();
}

pub fn open_terminal(ip: &str) {
    // Try common terminals in preference order.
    for (term, args) in &[
        ("ashyterm",         vec!["-e", &format!("ssh {ip}")]),
        ("xterm",            vec!["-e", &format!("ssh {ip}")]),
        ("konsole",          vec!["-e", &format!("ssh {ip}")]),
        ("gnome-terminal",   vec!["--", "ssh", ip]),
        ("xfce4-terminal",   vec!["-e", &format!("ssh {ip}")]),
    ] {
        if Command::new(term).args(args).spawn().is_ok() {
            return;
        }
    }
}
