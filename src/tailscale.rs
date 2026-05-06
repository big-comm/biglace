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
    /// First TailscaleIP — kept as `ip` for backwards compatibility with the
    /// existing UI. Same value as `ipv4` whenever the peer has one.
    pub ip:       String,
    pub ipv4:     String,
    pub ipv6:     String,
    pub dns_name: String,
    pub online:   bool,
    pub os:       String,
    /// BigScale account that owns the device (e.g. `tales`). Shown in the UI
    /// as "Owner".
    pub user:     String,
    /// OS login on the peer's machine — the right-hand side of `ssh user@host`.
    /// Extracted from a `tag:user-<name>` ACL tag advertised by the peer on
    /// connect (see `connect()`). Empty when the peer didn't advertise the
    /// tag; callers fall back to the peer's hostname in that case.
    pub ssh_user: String,
    /// Last time tailscaled saw the peer. Empty string when unknown (e.g. the
    /// peer never came online since the daemon started).
    pub last_seen: String,
    /// Tags advertised by the peer (ACL tags), with the leading `tag:` stripped.
    pub tags:     Vec<String>,
    /// True when this peer currently advertises itself as an exit node.
    pub exit_node_offered: bool,
    /// True when biglace is using this peer as its exit node.
    pub exit_node_active: bool,
}

impl Peer {
    /// Visible label for the row title. Prefers the headscale-assigned name —
    /// the first label of `DNSName` — so renames done in the BigScale panel
    /// surface immediately, without the user having to touch the remote box's
    /// `/etc/hostname`. Falls back to `HostName` (the peer's OS hostname),
    /// then to the IP, so the row never renders blank. Display only — keys
    /// for favorites, overrides, latency cache etc. stay on `hostname`.
    pub fn display_name(&self) -> String {
        if let Some(label) = first_dns_label(&self.dns_name) {
            return label;
        }
        if !self.hostname.is_empty() {
            return self.hostname.clone();
        }
        self.ip.clone()
    }
}

impl Status {
    /// Same precedence as `Peer::display_name`, applied to the local node.
    pub fn display_name(&self) -> String {
        if let Some(label) = self.dns_name.as_deref().and_then(first_dns_label) {
            return label;
        }
        if let Some(h) = self.hostname.as_deref().filter(|h| !h.is_empty()) {
            return h.to_string();
        }
        self.ip.clone().unwrap_or_default()
    }
}

fn first_dns_label(dns: &str) -> Option<String> {
    let trimmed = dns.trim_end_matches('.');
    let label = trimmed.split('.').next()?;
    if label.is_empty() { None } else { Some(label.to_string()) }
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
    /// Map of user-id → user metadata. Tailscale serializes the keys as JSON
    /// numbers but serde decodes them as strings. Used to translate the
    /// numeric `User` on each peer into the BigScale account login.
    #[serde(rename = "User")]
    users: Option<std::collections::HashMap<String, TsUser>>,
    /// Tailnet-wide DNS suffix (e.g. `bigscale.net`). Used by the panel
    /// integration to derive `panel.<suffix>` without hardcoding `bigscale.net`,
    /// so biglace stays usable against non-BigScale headscale/tailscale tailnets.
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<TsCurrentTailnet>,
}

#[derive(Deserialize)]
struct TsCurrentTailnet {
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
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
    #[serde(rename = "UserID")]
    user_id: Option<i64>,
    #[serde(rename = "LastSeen")]
    last_seen: Option<String>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    /// `PrimaryRoutes` includes `0.0.0.0/0` and/or `::/0` when the node is an
    /// exit node. We don't deserialize the full route list — `ExitNodeOption`
    /// in tailscaled's status JSON already tells us whether the peer offers
    /// exit-node service, and `ExitNode` whether we currently route through it.
    #[serde(rename = "ExitNodeOption", default)]
    exit_node_option: bool,
    #[serde(rename = "ExitNode", default)]
    exit_node: bool,
}

#[derive(Deserialize)]
struct TsUser {
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
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

/// Tailnet IP of the BigScale panel peer (the one whose DNSName is
/// `panel.<MagicDNSSuffix>`), as advertised by tailscaled. Returns None when:
///   - tailscaled is down or unparseable,
///   - the tailnet has no MagicDNSSuffix (rare),
///   - no peer matches `panel.<suffix>` (vanilla headscale/tailscale tailnets).
///
/// Used in place of OS-level DNS resolution because biglace runs on hosts
/// where MagicDNS may not be wired into the resolver (broken resolvconf,
/// containerized userspace, etc). Asking tailscaled directly bypasses
/// /etc/resolv.conf entirely and works as long as the tunnel is up.
pub fn panel_peer_ip() -> Option<String> {
    let out = Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let ts: TsStatus = serde_json::from_slice(&out.stdout).ok()?;
    let suffix = ts
        .magic_dns_suffix
        .clone()
        .or_else(|| ts.current_tailnet.as_ref().and_then(|c| c.magic_dns_suffix.clone()))
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty())?;
    let target = format!("panel.{suffix}");

    let peers = ts.peers?;
    for n in peers.into_values() {
        let dns = n.dns_name.as_deref().unwrap_or("").trim_end_matches('.');
        if dns == target {
            // Prefer IPv4 — some hosts have v6 disabled and `connect()` to a
            // [::1]-style URL would fail before TLS even starts.
            let ips = n.ips.unwrap_or_default();
            if let Some(v4) = ips.iter().find(|i| !i.contains(':')) {
                return Some(v4.clone());
            }
            return ips.into_iter().next();
        }
    }
    None
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

/// Owner accounts that represent server-side infrastructure on the tailnet,
/// not actual user devices. Filtered out of `get_peers()` so they don't show
/// up in the device list — e.g. the BigScale panel itself joins the tailnet
/// under `_panel` to authenticate `/api/devices/me/os-user` by tunnel
/// identity, but it's not a peer the user can connect to.
///
/// The engine often hides the owning user's info from unrelated peers
/// (`tailscale status --json` only includes users you share a permission
/// with), so `resolve_user` returns an empty string for the panel. The
/// DNS name fallback in `get_peers` catches that case.
const RESERVED_OWNERS: &[&str] = &["_panel"];

/// First DNS label of infrastructure peers, applied alongside `RESERVED_OWNERS`.
/// Same hostname the BigScale entrypoint registers the panel under
/// (`BIGSCALE_PANEL_HOSTNAME`, default `panel`). Compared against the leading
/// label of `panel.<MagicDNSSuffix>` so it can't match an unrelated peer that
/// happened to be named "panel" on a different tailnet.
const RESERVED_FIRST_LABELS: &[&str] = &["panel"];

pub fn get_peers() -> Vec<Peer> {
    let out = match Command::new("tailscale").args(["status", "--json"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let ts: TsStatus = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    // Tailscale's UserMap is keyed by stringified user-id. Build a quick
    // id → login lookup, stripping any trailing "@github" / "@bigscale.net"
    // tail so it matches the local SSH username on the peer.
    let users = ts.users.unwrap_or_default();
    let resolve_user = |uid: Option<i64>| -> String {
        let uid = match uid {
            Some(v) if v > 0 => v,
            _ => return String::new(),
        };
        users
            .get(&uid.to_string())
            .and_then(|u| u.login_name.clone())
            .map(|l| l.split('@').next().unwrap_or("").to_string())
            .unwrap_or_default()
    };

    // Tailnet suffix (e.g. `bigscale.net`) used to recognize the panel peer by
    // its DNS name when the engine has hidden the owning user from us.
    let suffix = ts
        .magic_dns_suffix
        .clone()
        .or_else(|| ts.current_tailnet.as_ref().and_then(|c| c.magic_dns_suffix.clone()))
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty());

    let is_reserved = |n: &TsNode| -> bool {
        if RESERVED_OWNERS.contains(&resolve_user(n.user_id).as_str()) {
            return true;
        }
        let dns = n.dns_name.as_deref().unwrap_or("").trim_end_matches('.');
        if let Some(label) = dns.split('.').next() {
            if RESERVED_FIRST_LABELS.contains(&label) {
                // Only treat the leading label as reserved when the rest of the
                // DNS name matches our tailnet suffix — otherwise a peer
                // genuinely named "panel" on an unrelated tailnet would be
                // filtered out by accident.
                if let Some(suf) = suffix.as_deref() {
                    let rest = dns.strip_prefix(label).unwrap_or("");
                    let rest = rest.strip_prefix('.').unwrap_or(rest);
                    if rest == suf {
                        return true;
                    }
                }
            }
        }
        false
    };

    let mut peers: Vec<Peer> = ts
        .peers
        .unwrap_or_default()
        .into_values()
        .filter(|n| !is_reserved(n))
        .map(|n| {
            let ips = n.ips.unwrap_or_default();
            let ipv4 = ips.iter().find(|i| !i.contains(':')).cloned().unwrap_or_default();
            let ipv6 = ips.iter().find(|i| i.contains(':')).cloned().unwrap_or_default();
            let ip = ips.first().cloned().unwrap_or_default();
            let tags: Vec<String> = n.tags.unwrap_or_default()
                .into_iter()
                .map(|t| t.strip_prefix("tag:").unwrap_or(&t).to_string())
                .collect();
            Peer {
                hostname: n.hostname.unwrap_or_default(),
                ip,
                ipv4,
                ipv6,
                dns_name: n.dns_name.map(|d| d.trim_end_matches('.').to_string()).unwrap_or_default(),
                online:   n.online.unwrap_or(false),
                os:       n.os.unwrap_or_default(),
                user:     resolve_user(n.user_id),
                // Filled in by the window layer from the panel's device-meta
                // cache — empty here means "not known yet"; callers fall back
                // to using the peer's hostname as the SSH login.
                ssh_user: String::new(),
                last_seen: n.last_seen.unwrap_or_default(),
                tags,
                exit_node_offered: n.exit_node_option,
                exit_node_active:  n.exit_node,
            }
        })
        .collect();

    // online first, then alphabetical. Final ordering (favorites first) happens
    // in the UI layer, which knows the user's pin list — keeping it out of
    // here means tests of get_peers don't need a Config to compare against.
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
    // `--reset` clears any pref cached from a previous `tailscale up`/`set`
    // run that we don't explicitly carry over here. Without it, tailscale
    // aborts with "changing settings via 'tailscale up' requires mentioning
    // all non-default flags" whenever an old biglace (or a manual `tailscale
    // set`) left a flag enabled that the current biglace doesn't pass —
    // e.g. --ssh, --shields-up, --advertise-routes. BigLace owns the full
    // user-facing config, so resetting on each up is the right call.
    let mut args = vec!["up", "--reset", "--accept-routes"];

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

/// Sign the device out of its current control server. Use when the user wants
/// to switch accounts — `down` only stops the tunnel but keeps the node
/// registered, so the next `up` would silently rejoin the same account.
pub fn logout() -> Result<()> {
    run_tailscale_with_fallback(&["logout"])
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

/// Pick the right target for `ssh`/`sftp` at click time: prefer the tailnet
/// hostname (more readable, survives the peer reconnecting on a different
/// IP), but fall back to the IP when the local resolver can't resolve the
/// name. The fallback exists because biglace runs on every distro under the
/// sun and openresolv/systemd-resolved misconfiguration silently breaks
/// MagicDNS without the user noticing — the IP path keeps the launch
/// buttons usable in that case.
///
/// Resolution is synchronous on purpose. Modern resolvers fail fast on
/// NXDOMAIN (a few ms), so the click-handler delay is imperceptible. Only
/// when the network is really wedged could this stall, and a stalled
/// network would block the actual ssh/sftp call right after anyway.
pub fn pick_target(dns_name: &str, ip_fallback: &str) -> String {
    use std::net::ToSocketAddrs;
    if !dns_name.is_empty() {
        // Port is irrelevant — `getaddrinfo` only needs *something* to
        // attempt the lookup against. We don't open this socket.
        if (dns_name, 22_u16)
            .to_socket_addrs()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
        {
            return dns_name.to_string();
        }
    }
    ip_fallback.to_string()
}

/// Open the user's configured file manager pointed at `sftp://<user>@<host>/`.
///
/// We can't rely on `xdg-open` alone: it forwards to `gio open`, which fails
/// on a bare sftp URL with "location is not mounted" because GVfs needs an
/// explicit mount step before it'll open. So we resolve the user's preferred
/// handler ourselves:
///   1. `xdg-mime query default x-scheme-handler/sftp` → the .desktop entry
///      that the user (or distro) set as the SFTP handler. `gtk-launch` runs
///      it with the URL as argument.
///   2. Same for `inode/directory` — many file managers register only there.
///   3. Probe a known list of GUI managers in popularity order. Each one of
///      them mounts the SFTP location itself and pops up the password prompt.
///   4. Last resort: `gio mount` then `xdg-open`.
pub fn open_files(host: &str, user: &str) {
    let target = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    let url = format!("sftp://{target}/");

    for mime in &["x-scheme-handler/sftp", "inode/directory"] {
        if let Some(desktop) = default_handler_for(mime) {
            let r = Command::new("gtk-launch").args([&desktop, &url]).status();
            if matches!(r, Ok(s) if s.success()) {
                return;
            }
        }
    }

    for cmd in &["nautilus", "nemo", "caja", "dolphin", "thunar", "pcmanfm"] {
        if Command::new(cmd).arg(&url).spawn().is_ok() {
            return;
        }
    }

    let _ = Command::new("gio").args(["mount", &url]).status();
    let _ = Command::new("xdg-open").arg(&url).spawn();
}

fn default_handler_for(mime: &str) -> Option<String> {
    let out = Command::new("xdg-mime")
        .args(["query", "default", mime])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Open a terminal running `ssh <user>@<host>`. `user` should be the peer's
/// OS user (its hostname on Linux). When empty we fall back to `ssh <host>`,
/// which makes ssh use the local username — usually wrong, but better than
/// failing to launch at all.
pub fn open_terminal(host: &str, user: &str) {
    let target = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    let ssh_cmd = format!("ssh {target}");

    // Try common terminals in preference order.
    for (term, args) in &[
        ("ashyterm",         vec!["-e", ssh_cmd.as_str()]),
        ("xterm",            vec!["-e", ssh_cmd.as_str()]),
        ("konsole",          vec!["-e", ssh_cmd.as_str()]),
        ("gnome-terminal",   vec!["--", "ssh", target.as_str()]),
        ("xfce4-terminal",   vec!["-e", ssh_cmd.as_str()]),
    ] {
        if Command::new(term).args(args).spawn().is_ok() {
            return;
        }
    }
}

/// Open a terminal that tails `tailscaled`'s journal. Useful for debugging
/// connect failures without leaving biglace.
pub fn open_logs() {
    // `pkexec journalctl -fu tailscaled` would be ideal but pkexec inside a
    // terminal child often deadlocks on polkit's auth dialog. Plain
    // `journalctl` works for the user's own session journal and surfaces
    // tailscaled-routed lines via the system journal under `-u tailscaled`
    // when the user is in `systemd-journal` (Manjaro default).
    let cmd = "journalctl -fu tailscaled --no-pager; echo; echo '[press enter to close]'; read";
    for (term, args) in &[
        ("ashyterm",       vec!["-e", cmd]),
        ("xterm",          vec!["-e", cmd]),
        ("konsole",        vec!["-e", "bash", "-c", cmd]),
        ("gnome-terminal", vec!["--", "bash", "-c", cmd]),
        ("xfce4-terminal", vec!["-e", cmd]),
    ] {
        if Command::new(term).args(args).spawn().is_ok() {
            return;
        }
    }
}

// ─── Exit nodes ──────────────────────────────────────────────────────────────

/// Route this device's traffic through `host` (matched against tailscaled's
/// known peer hostnames). Pass `None` to clear the exit node and return to
/// direct routing.
pub fn set_exit_node(host: Option<&str>) -> Result<()> {
    let arg = match host {
        Some(h) => format!("--exit-node={h}"),
        None    => "--exit-node=".to_string(),
    };
    run_tailscale_with_fallback(&["set", &arg])
}

// ─── Latency ─────────────────────────────────────────────────────────────────

/// One-shot ping to `target` (IP or hostname). Returns the round-trip time in
/// milliseconds on the first reply, or None on timeout / unreachable. Caps at
/// ~2s so a dead peer doesn't hang the periodic refresh.
pub fn ping_ms(target: &str) -> Option<f64> {
    let out = Command::new("tailscale")
        .args(["ping", "--c=1", "--timeout=2s", "--until-direct=false", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Lines look like:
    //   pong from box (100.64.0.5) via 1.2.3.4:41641 in 14ms
    //   pong from box (100.64.0.5) via DERP(nyc) in 27ms
    let line = stdout.lines().find(|l| l.starts_with("pong from"))?;
    let ms_idx = line.rfind(" in ")? + 4;
    let rest = &line[ms_idx..];
    let num_end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    rest[..num_end].parse::<f64>().ok()
}

// ─── Headscale health ────────────────────────────────────────────────────────

/// Hit `<server_url>/health` and return true on any 2xx response. Headscale
/// versions and reverse proxies vary on the exact code (200 vs 204), so we
/// accept the whole 2xx range. Empty URL → false (we never reached out).
///
/// The badge that consumes this is only shown while *disconnected*, so a
/// false negative here can't surprise an already-connected user — see
/// `apply_state` in `window/mod.rs`.
pub fn headscale_healthy(server_url: &str) -> bool {
    let url = server_url.trim_end_matches('/');
    if url.is_empty() {
        return false;
    }
    let endpoint = format!("{url}/health");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    matches!(agent.get(&endpoint).call(), Ok(r) if (200..300).contains(&r.status()))
}
