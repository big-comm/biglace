use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{tr, trf};

/// TTL for the parsed-status cache. 500 ms is short enough that the UI never
/// shows visibly stale data (each `refresh_state` tick is at least 30 s
/// apart) yet long enough to swallow the burst of calls a single worker
/// tick fires off across threads. Bumping past ~1 s starts to interact
/// badly with the post-connect verification loop in `connect()` — which
/// already explicitly invalidates the cache via `invalidate_status_cache()`.
const STATUS_CACHE_TTL: Duration = Duration::from_millis(500);

/// How old a cached status may be and still stand in for a fetch that failed.
/// The fallback smooths over a single slow or failed `tailscale status`; it
/// must not keep painting "Connected" with live-looking peers for as long as a
/// wedged daemon (the classic post-suspend hang) keeps timing out.
const STATUS_STALE_LIMIT: Duration = Duration::from_secs(30);

/// The cached status, if it's recent enough to use in place of a fresh one.
fn usable_stale(value: Option<&CachedStatus>) -> Option<Arc<TsStatus>> {
    value
        .filter(|(when, _)| when.elapsed() < STATUS_STALE_LIMIT)
        .map(|(_, ts)| ts.clone())
}

/// Fetch time + the shared parsed payload. `Arc` so cache reads are refcount
/// bumps, never deep copies of a payload that runs to hundreds of KB.
type CachedStatus = (Instant, Arc<TsStatus>);

#[derive(Default)]
struct StatusCache {
    value: Option<CachedStatus>,
    fetching: bool,
}

fn status_cache() -> &'static (Mutex<StatusCache>, Condvar) {
    static CACHE: OnceLock<(Mutex<StatusCache>, Condvar)> = OnceLock::new();
    CACHE.get_or_init(|| (Mutex::new(StatusCache::default()), Condvar::new()))
}

/// Run `tailscale status --json` and return the parsed payload, reusing the
/// last result when it's younger than `STATUS_CACHE_TTL`. Returns None when
/// tailscale isn't installed, the call fails, or the JSON is unparseable.
///
/// Important: this is the *only* path that should shell out to `tailscale
/// status --json`. Multiple workers (latency, panel, health, refresh) hit it
/// in overlapping windows; routing them through this cache collapses an
/// occasional 4-5x burst of subprocesses into a single one.
fn cached_ts_status() -> Option<Arc<TsStatus>> {
    let (lock, ready) = status_cache();
    let mut state = lock.lock().ok()?;
    if let Some((when, ts)) = state.value.as_ref() {
        if when.elapsed() < STATUS_CACHE_TTL {
            return Some(ts.clone());
        }
    }

    // Collapse concurrent misses into one subprocess. Waiters reuse the fresh
    // value, or the stale value if the fetch failed or exceeded five seconds.
    if state.fetching {
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.fetching {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, timeout) = ready.wait_timeout(state, remaining).ok()?;
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
        return usable_stale(state.value.as_ref());
    }
    state.fetching = true;
    drop(state);

    let mut command = Command::new(tailscale_cmd());
    command.args(["status", "--json"]);
    let fetched = command_output_timeout(&mut command, Duration::from_secs(10))
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<TsStatus>(&out.stdout).ok())
        .map(Arc::new);

    let mut state = lock.lock().ok()?;
    state.fetching = false;
    if let Some(ts) = fetched.as_ref() {
        state.value = Some((Instant::now(), ts.clone()));
    }
    ready.notify_all();
    fetched.or_else(|| usable_stale(state.value.as_ref()))
}

pub(crate) fn command_output_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .map(|mut pipe| std::thread::spawn(move || read_capped(&mut pipe)));
    let stderr = child
        .stderr
        .take()
        .map(|mut pipe| std::thread::spawn(move || read_capped(&mut pipe)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = stdout {
                let _ = reader.join();
            }
            if let Some(reader) = stderr {
                let _ = reader.join();
            }
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output {
        status,
        stdout: stdout
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default(),
        stderr: stderr
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default(),
    })
}

fn read_capped(reader: &mut impl Read) -> Vec<u8> {
    const MAX_CAPTURE: usize = 16 * 1024 * 1024;
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let keep = read.min(MAX_CAPTURE.saturating_sub(captured.len()));
                captured.extend_from_slice(&buffer[..keep]);
            }
            // A signal delivered mid-read is not end-of-output. Treating EINTR
            // as EOF (which `while let Ok(..)` did) silently truncated the
            // captured stderr, so a failing `tailscale up` could surface to the
            // user as an empty error message.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    captured
}

/// Drop the cached status so the next read does a fresh fetch. Called after
/// `connect()`, `disconnect()`, `logout()` and `set_exit_node()` because the
/// post-action UI refresh would otherwise see ≤500 ms of stale state.
pub fn invalidate_status_cache() {
    if let Ok(mut state) = status_cache().0.lock() {
        state.value = None;
    }
}

// ─── Logging ─────────────────────────────────────────────────────────────────
// Info-level lines are always printed to stderr so the user can follow what
// the app is doing. Setting BIGLACE_DEBUG=1 additionally dumps the full
// stdout/stderr of every tailscale invocation.

fn debug_enabled() -> bool {
    std::env::var("BIGLACE_DEBUG")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

pub(crate) fn dbg(msg: &str) {
    eprintln!("[biglace] {msg}");
}

/// Join CLI args for logging with any secret-bearing flag value masked.
/// `--authkey=tskey-…` MUST never reach stderr/journald: those logs land in
/// `journalctl --user` / `~/.xsession-errors`, readable by other local
/// processes, and a reusable pre-auth key there lets anyone register a node
/// on the tailnet as the user.
fn redact_args(args: &[&str]) -> String {
    args.iter()
        .map(|a| {
            // `--authkey=file:/run/user/1000/…` carries no secret — the key
            // lives inside the (0600) file — and the path is worth seeing when
            // debugging a failed enrollment, so only the inline form is masked.
            match a.strip_prefix("--authkey=") {
                Some(value) if !value.starts_with("file:") => "--authkey=<redacted>".to_string(),
                _ => (*a).to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A 0600 file holding the pre-auth key for the lifetime of one `tailscale up`,
/// unlinked on drop.
///
/// `tailscale up --authkey=file:<path>` exists precisely so the key doesn't
/// have to travel in `argv`. That matters here: on Linux `/proc/<pid>/cmdline`
/// is world-readable, so an inline `--authkey=tskey-…` is visible to every
/// local user for as long as the command runs — and the elevated path runs the
/// key through `pkexec sh -c … "$@"`, which puts it in a *second* process's
/// argv as well. A reusable pre-auth key leaked that way lets anyone on the
/// machine register a node on the user's tailnet.
#[cfg(unix)]
struct AuthKeyFile {
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for AuthKeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn stage_authkey(authkey: &str) -> io::Result<AuthKeyFile> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    // XDG_RUNTIME_DIR is a per-user 0700 tmpfs — the right home for a
    // short-lived secret. The temp-dir fallback is shared, so `create_new`
    // makes a planted symlink an error instead of a write-through.
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!(
        "biglace-authkey-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(authkey.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(AuthKeyFile { path })
}

/// Build the `--authkey=` argument, staging the key in a file when we can so it
/// stays out of `argv`. The guard must outlive the command: dropping it
/// unlinks the file.
fn authkey_arg(authkey: &str) -> (String, Option<AuthKeyFile>) {
    #[cfg(unix)]
    {
        match stage_authkey(authkey) {
            Ok(staged) => {
                let arg = format!("--authkey=file:{}", staged.path.display());
                return (arg, Some(staged));
            }
            // Read-only runtime dir, full disk, exotic sandbox: fall back to the
            // inline form rather than refusing to connect at all.
            Err(e) => dbg(&format!(
                "connect: could not stage the pre-auth key ({e}); passing it inline"
            )),
        }
    }
    (format!("--authkey={authkey}"), None)
}

/// Windows placeholder so `authkey_arg`'s return type is uniform across
/// platforms. `/proc`-style argv exposure doesn't apply there.
#[cfg(not(unix))]
#[allow(dead_code)]
struct AuthKeyFile;

fn dbg_output(label: &str, args: &[&str], status: i32, stdout: &str, stderr: &str) {
    eprintln!(
        "[biglace] {label} exit={status} args=[{}]",
        redact_args(args)
    );
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
    pub online: bool,
    pub ip: Option<String>,
    pub hostname: Option<String>,
    pub dns_name: Option<String>,
    /// True when tailscaled already holds a node registration on disk
    /// (BackendState Running/Stopped/Starting) — so we can reconnect without a key.
    pub registered: bool,
    /// True when tailscaled reports it couldn't install the tailnet's DNS
    /// config (see `dns_config_failed`). Carried on `Status` rather than read
    /// on demand so the UI never has to touch the daemon from the GTK thread.
    pub dns_broken: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Peer {
    /// Stable node identity (the node's public key). Use this — not
    /// `hostname` — to tell peers apart: hostnames aren't unique on a tailnet
    /// (a reinstalled machine leaves its old offline node behind under the
    /// same name). Favorites and SSH overrides stay keyed on `hostname`
    /// because they're persisted and shared with the mobile app.
    pub id: String,
    pub hostname: String,
    /// First TailscaleIP — kept as `ip` for backwards compatibility with the
    /// existing UI. Same value as `ipv4` whenever the peer has one.
    pub ip: String,
    pub ipv4: String,
    pub ipv6: String,
    pub dns_name: String,
    pub online: bool,
    pub os: String,
    /// BigScale account that owns the device (e.g. `tales`). Shown in the UI
    /// as "Owner".
    pub user: String,
    /// OS login on the peer's machine — the right-hand side of `ssh user@host`.
    /// Extracted from a `tag:user-<name>` ACL tag advertised by the peer on
    /// connect (see `connect()`). Empty when the peer didn't advertise the
    /// tag; callers fall back to the peer's hostname in that case.
    pub ssh_user: String,
    /// Last time tailscaled saw the peer. Empty string when unknown (e.g. the
    /// peer never came online since the daemon started).
    pub last_seen: String,
    /// Tags advertised by the peer (ACL tags), with the leading `tag:` stripped.
    pub tags: Vec<String>,
    /// True when this peer currently advertises itself as an exit node.
    pub exit_node_offered: bool,
    /// True when biglace is using this peer as its exit node.
    pub exit_node_active: bool,
    /// Direct path, relay, or idle — see [`Link`].
    pub link: Link,
    /// The peer's BigLace agent, when it runs one (filled in by the window
    /// layer from `agent::client`'s cache). `None`: classic actions only.
    pub agent: Option<crate::agent::Hello>,
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
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

// ─── Tailscale JSON structs ───────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct TsStatus {
    #[serde(rename = "Self")]
    self_node: Option<TsNode>,
    /// Daemon state: "Running" | "Stopped" | "Starting" mean the node holds a
    /// registration on disk; "NeedsLogin" | "NoState" mean it must (re)auth.
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
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

#[derive(Deserialize, Clone)]
struct TsCurrentTailnet {
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
}

#[derive(Deserialize, Clone)]
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
    /// `ip:port` of the direct WireGuard path; empty while traffic goes
    /// through a relay (or the peer is idle).
    #[serde(rename = "CurAddr", default)]
    cur_addr: String,
    /// Home DERP region code (e.g. `bigscale`). Always set — only meaningful
    /// as "the relay in use" when `Active` and there is no `CurAddr`.
    #[serde(rename = "Relay", default)]
    relay: String,
    /// Peer-relay (UDP relay through another node) in use, if any.
    #[serde(rename = "PeerRelay", default)]
    peer_relay: String,
    /// True when there was recent traffic with this peer.
    #[serde(rename = "Active", default)]
    active: bool,
}

/// How packets currently reach a peer. Matters for anything heavier than a
/// terminal: a remote screen through a relay is noticeably slower than over
/// a direct WireGuard path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Link {
    /// No recent traffic — tailscaled only finds a path on demand.
    #[default]
    Idle,
    Direct,
    /// Through the named relay (a DERP region or a peer relay).
    Relay(String),
}

impl TsNode {
    fn link(&self) -> Link {
        if !self.cur_addr.is_empty() {
            Link::Direct
        } else if !self.peer_relay.is_empty() {
            Link::Relay(self.peer_relay.clone())
        } else if self.active && !self.relay.is_empty() {
            Link::Relay(self.relay.clone())
        } else {
            Link::Idle
        }
    }
}

#[derive(Deserialize, Clone)]
struct TsUser {
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
}

// ─── Detection ───────────────────────────────────────────────────────────────
//
// `tailscale` on Linux/macOS lives in PATH as a single CLI binary; on Windows
// the installer drops it under `%ProgramFiles%\Tailscale\tailscale.exe`, which
// is *not* in PATH for unprivileged user sessions. The CLI binary in turn
// talks to the local daemon over a Unix socket (Linux) or a named pipe
// (Windows) — same JSON wire format on both, so all the parsing in this file
// is platform-independent.

/// Absolute path / command name to invoke the tailscale CLI. Used everywhere
/// we shell out so the Windows-installer path is found without the user
/// having to add it to PATH manually.
fn tailscale_cmd() -> &'static str {
    #[cfg(windows)]
    {
        // Try a couple of well-known install locations, then fall back to the
        // bare name in case the user added it to PATH themselves.
        const CANDIDATES: &[&str] = &[
            r"C:\Program Files\Tailscale\tailscale.exe",
            r"C:\Program Files (x86)\Tailscale\tailscale.exe",
        ];
        for c in CANDIDATES {
            if std::path::Path::new(c).exists() {
                return c;
            }
        }
        "tailscale.exe"
    }
    #[cfg(not(windows))]
    {
        "tailscale"
    }
}

/// Whether the tailscale CLI is present. The answer can't change while biglace
/// is running — you don't install or remove tailscale mid-session — so we probe
/// once and memoize. This keeps the `which`/`where` subprocess out of the
/// per-refresh hot path (`get_status` / `get_status_and_peers` call this on
/// every tick).
pub fn is_installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(detect_installed)
}

#[cfg(unix)]
fn detect_installed() -> bool {
    // Probe the CLI directly rather than shelling out to `which`, which isn't
    // guaranteed present in minimal environments (stripped containers, NixOS).
    // A false negative here would wrongly claim "tailscale not installed" and
    // is memoized for the whole session, so the probe must not depend on an
    // optional helper binary.
    let mut command = Command::new(tailscale_cmd());
    command.arg("version");
    command_output_timeout(&mut command, Duration::from_secs(5))
        .is_ok_and(|out| out.status.success())
}

#[cfg(windows)]
fn detect_installed() -> bool {
    // Accept the hard-coded Program Files path as "installed" so the official
    // MSI works for users who never opened a fresh shell after install (PATH
    // updates don't propagate to existing processes). Otherwise probe the CLI
    // directly instead of relying on `where.exe`.
    if std::path::Path::new(r"C:\Program Files\Tailscale\tailscale.exe").exists()
        || std::path::Path::new(r"C:\Program Files (x86)\Tailscale\tailscale.exe").exists()
    {
        return true;
    }
    let mut command = Command::new(tailscale_cmd());
    command.arg("version");
    command_output_timeout(&mut command, Duration::from_secs(5))
        .is_ok_and(|out| out.status.success())
}

/// TTL for the service-active probe. Unlike `is_installed`, the daemon's running
/// state *does* change at runtime (the Start-service button, a crash, a
/// sleep/resume cycle), so we can't memoize it forever — but a short window
/// collapses the burst of probes a single refresh and its background workers
/// fire in overlapping order. Explicitly invalidated right after the
/// Start-service action so its follow-up refresh sees the truth immediately.
const SERVICE_CACHE_TTL: Duration = Duration::from_secs(2);

fn service_cache() -> &'static Mutex<Option<(Instant, bool)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Drop the cached service-active state so the next read re-probes. Called right
/// after the Start-service flow, whose whole point is to flip this state.
pub fn invalidate_service_cache() {
    if let Ok(mut g) = service_cache().lock() {
        *g = None;
    }
}

pub fn is_service_active() -> bool {
    if let Ok(g) = service_cache().lock() {
        if let Some((when, active)) = g.as_ref() {
            if when.elapsed() < SERVICE_CACHE_TTL {
                return *active;
            }
        }
    }
    let active = detect_service_active();
    if let Ok(mut g) = service_cache().lock() {
        *g = Some((Instant::now(), active));
    }
    active
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_service_active() -> bool {
    let mut command = Command::new("systemctl");
    command.args(["is-active", "--quiet", "tailscaled"]);
    let systemctl = command_output_timeout(&mut command, Duration::from_secs(5))
        .map(|out| out.status.success());
    service_active_from(systemctl, daemon_reachable)
}

/// Interpret `systemctl is-active`. Without systemd at all (Artix, Void,
/// Alpine, containers) the command doesn't exist; asking it pinned the UI to
/// "Service stopped" forever there, so fall back to whether the CLI can reach
/// the daemon — the same test macOS uses.
#[cfg(all(unix, not(target_os = "macos")))]
fn service_active_from(
    systemctl: io::Result<bool>,
    daemon_reachable: impl FnOnce() -> bool,
) -> bool {
    match systemctl {
        Ok(active) => active,
        Err(e) if e.kind() == io::ErrorKind::NotFound => daemon_reachable(),
        Err(_) => false,
    }
}

/// `tailscale status` connects to the daemon and exits 0 even while logged
/// out or stopped; it only fails when the daemon itself isn't running.
#[cfg(all(unix, not(target_os = "macos")))]
fn daemon_reachable() -> bool {
    let mut command = Command::new(tailscale_cmd());
    command.args(["status", "--json"]);
    command_output_timeout(&mut command, Duration::from_secs(10))
        .is_ok_and(|out| out.status.success())
}

#[cfg(target_os = "macos")]
fn detect_service_active() -> bool {
    // macOS has no systemctl. The tailscaled daemon (a launchd job on the
    // open-source build, or the network extension on the App Store build) is
    // "active" whenever the CLI can reach it: `tailscale status` connects and
    // exits 0 even while logged out or stopped, and only fails when the daemon
    // itself isn't running. Without this branch the whole UI would be pinned to
    // the "service stopped" screen on macOS regardless of the real state.
    let mut command = Command::new(tailscale_cmd());
    command.args(["status", "--json"]);
    command_output_timeout(&mut command, Duration::from_secs(10))
        .is_ok_and(|out| out.status.success())
}

#[cfg(windows)]
fn detect_service_active() -> bool {
    // `sc query Tailscale` always exits 0 if the service is *defined* — we
    // need to look at the STATE line. "RUNNING" is what we want.
    let mut command = Command::new("sc");
    command.args(["query", "Tailscale"]);
    let out = command_output_timeout(&mut command, Duration::from_secs(5));
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines()
                .any(|l| l.trim().to_ascii_uppercase().contains("RUNNING"))
        }
        _ => false,
    }
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
    let ts = cached_ts_status()?;
    let suffix = ts
        .magic_dns_suffix
        .clone()
        .or_else(|| {
            ts.current_tailnet
                .as_ref()
                .and_then(|c| c.magic_dns_suffix.clone())
        })
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty())?;
    let target = format!("panel.{suffix}");

    let peers = ts.peers.as_ref()?;
    for n in peers.values() {
        let dns = n.dns_name.as_deref().unwrap_or("").trim_end_matches('.');
        if dns == target {
            // Prefer IPv4 — some hosts have v6 disabled and `connect()` to a
            // [::1]-style URL would fail before TLS even starts.
            let ips = n.ips.clone().unwrap_or_default();
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
    let ts = cached_ts_status()?;
    let health = ts.health.as_ref()?;
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

/// True when tailscaled is reporting that it couldn't install the tailnet's DNS
/// configuration on this machine.
///
/// This is the failure behind "it used to connect by name and now only by IP":
/// something rewrote `/etc/resolv.conf` without openresolv's signature, so
/// tailscaled gives up on MagicDNS and every `<peer>.<tailnet>` lookup fails.
/// The tunnel still works, so nothing else in the UI goes red and the user is
/// left guessing — `pick_target` quietly degrades to raw IPs. Surfacing it as
/// its own badge turns a mystery into a one-command fix.
fn dns_config_failed(ts: &TsStatus) -> bool {
    ts.health.iter().flatten().any(|h| {
        let l = h.to_lowercase();
        l.contains("dns configuration") || l.contains("resolvconf") || l.contains("resolv.conf")
    })
}

pub fn get_status() -> Status {
    if !is_installed() {
        return Status::default();
    }
    match cached_ts_status() {
        Some(ts) => build_status(&ts),
        None => Status::default(),
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

/// Single-shot variant that returns both the local status and the peer list
/// from one `tailscale status --json` subprocess. The window's refresh path
/// is the hot caller — using this in place of separate `get_status()` +
/// `get_peers()` halves the subprocess + JSON-parse cost per tick, which
/// matters because the periodic refresh fires every ~20s and Status's JSON
/// can run a few hundred KB on busy tailnets.
pub fn get_status_and_peers() -> (Status, Vec<Peer>) {
    if !is_installed() {
        return (Status::default(), vec![]);
    }
    let Some(ts) = cached_ts_status() else {
        return (Status::default(), vec![]);
    };
    let status = build_status(&ts);
    let peers = build_peers(&ts);
    (status, peers)
}

fn build_status(ts: &TsStatus) -> Status {
    let node = ts.self_node.as_ref();
    Status {
        online: node.and_then(|n| n.online).unwrap_or(false),
        ip: node.and_then(|n| n.ips.as_ref()?.first().cloned()),
        hostname: node.and_then(|n| n.hostname.clone()),
        dns_name: node
            .and_then(|n| n.dns_name.clone())
            .map(|d| d.trim_end_matches('.').to_string()),
        registered: matches!(
            ts.backend_state.as_deref(),
            Some("Running") | Some("Stopped") | Some("Starting")
        ),
        dns_broken: dns_config_failed(ts),
    }
}

pub fn get_peers() -> Vec<Peer> {
    match cached_ts_status() {
        Some(ts) => build_peers(&ts),
        None => vec![],
    }
}

fn build_peers(ts: &TsStatus) -> Vec<Peer> {
    // Tailscale's UserMap is keyed by stringified user-id. Build a quick
    // id → login lookup, stripping any trailing "@github" / "@bigscale.net"
    // tail so it matches the local SSH username on the peer.
    let empty_users = std::collections::HashMap::new();
    let users = ts.users.as_ref().unwrap_or(&empty_users);
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
        .or_else(|| {
            ts.current_tailnet
                .as_ref()
                .and_then(|c| c.magic_dns_suffix.clone())
        })
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty());

    // `user` is resolved once per peer and reused for both the reserved-owner
    // check and the Peer we build, so we don't stringify the uid + clone the
    // login twice on every refresh tick.
    let is_reserved = |n: &TsNode, user: &str| -> bool {
        if RESERVED_OWNERS.contains(&user) {
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
        .iter()
        .flat_map(|m| m.iter())
        .filter_map(|(node_key, n)| {
            let user = resolve_user(n.user_id);
            if is_reserved(n, &user) {
                return None;
            }
            let ips = n.ips.clone().unwrap_or_default();
            let ipv4 = ips
                .iter()
                .find(|i| !i.contains(':'))
                .cloned()
                .unwrap_or_default();
            let ipv6 = ips
                .iter()
                .find(|i| i.contains(':'))
                .cloned()
                .unwrap_or_default();
            let ip = ips.first().cloned().unwrap_or_default();
            let tags: Vec<String> = n
                .tags
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.strip_prefix("tag:").unwrap_or(&t).to_string())
                .collect();
            Some(Peer {
                id: node_key.clone(),
                hostname: n.hostname.clone().unwrap_or_default(),
                ip,
                ipv4,
                ipv6,
                dns_name: n
                    .dns_name
                    .clone()
                    .map(|d| d.trim_end_matches('.').to_string())
                    .unwrap_or_default(),
                online: n.online.unwrap_or(false),
                os: n.os.clone().unwrap_or_default(),
                user,
                // Filled in by the window layer from the panel's device-meta
                // cache — empty here means "not known yet"; callers fall back
                // to using the peer's hostname as the SSH login.
                ssh_user: String::new(),
                last_seen: n.last_seen.clone().unwrap_or_default(),
                tags,
                exit_node_offered: n.exit_node_option,
                exit_node_active: n.exit_node,
                link: n.link(),
                agent: None,
            })
        })
        .collect();

    // online first, then alphabetical by the label the user actually sees.
    // Sorting on `hostname` (the peer's OS hostname) made the list look
    // unsorted whenever it differed from the headscale-assigned name shown in
    // the row. Final ordering (favorites first) happens in the UI layer, which
    // knows the user's pin list — keeping it out of here means tests of
    // get_peers don't need a Config to compare against.
    peers.sort_by_cached_key(|p| (!p.online, p.display_name().to_lowercase()));
    peers
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Run `tailscale <args>`; if it fails because the user is not the configured
/// operator, retry once via `pkexec` and use that single elevation to also
/// mark the current user as operator — so subsequent calls don't need a
/// password at all.
#[cfg(unix)]
fn run_tailscale_with_fallback(args: &[&str]) -> Result<()> {
    dbg(&format!("running: tailscale {}", redact_args(args)));
    let mut command = Command::new(tailscale_cmd());
    command.args(args);
    let out = command_output_timeout(&mut command, Duration::from_secs(30))
        .with_context(|| tr!("Failed to run tailscale"))?;

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    dbg_output(
        "tailscale",
        args,
        out.status.code().unwrap_or(-1),
        &stdout,
        &stderr,
    );

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
    let script = "tailscale set --operator=\"$0\" >/dev/null 2>&1 || true; exec tailscale \"$@\"";

    let mut sh_args: Vec<&str> = vec!["sh", "-c", script, &user];
    sh_args.extend_from_slice(args);

    dbg(&format!(
        "running: pkexec sh -c '<set-operator + exec tailscale>' {} {}",
        user,
        redact_args(args)
    ));
    let mut command = Command::new("pkexec");
    command.args(&sh_args);
    let pk = command_output_timeout(&mut command, Duration::from_secs(120))
        .with_context(|| tr!("Failed to run pkexec tailscale"))?;

    let pk_err = String::from_utf8_lossy(&pk.stderr);
    let pk_out = String::from_utf8_lossy(&pk.stdout);
    dbg_output(
        "pkexec tailscale",
        args,
        pk.status.code().unwrap_or(-1),
        &pk_out,
        &pk_err,
    );

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

/// Windows variant of `run_tailscale_with_fallback`. Tailscale on Windows
/// runs as a Windows Service under LocalSystem; the CLI talks to it over a
/// named pipe and inherits whatever rights the calling user has. There's no
/// pkexec equivalent we can drive from a GUI app — UAC elevation has to be
/// done by re-launching the whole process under `runas`, which is jarring
/// and pops a system dialog the user can't preview. So we just run the CLI
/// and surface whatever error comes back; the user fixes elevation by
/// running biglace as administrator (recorded in the README).
#[cfg(windows)]
fn run_tailscale_with_fallback(args: &[&str]) -> Result<()> {
    dbg(&format!("running: tailscale {}", redact_args(args)));
    let mut command = Command::new(tailscale_cmd());
    command.args(args);
    let out = command_output_timeout(&mut command, Duration::from_secs(30))
        .with_context(|| tr!("Failed to run tailscale"))?;

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    dbg_output(
        "tailscale",
        args,
        out.status.code().unwrap_or(-1),
        &stdout,
        &stderr,
    );

    if out.status.success() {
        return Ok(());
    }

    let msg = stderr.trim();
    if msg.is_empty() {
        bail!(tr!("Failed to connect — check the server URL and key."));
    } else {
        bail!(msg.to_string());
    }
}

/// The key to register with: the caller's, else the one stored at
/// enrollment. That one is read only here, on the connect worker thread:
/// read with the config, a locked keyring would prompt at every start, even
/// when a keyless reconnect needs no key.
fn registration_key(server: &str, authkey: &str, registered: bool) -> Result<String> {
    let key = if authkey.is_empty() {
        match crate::secrets::load_authkey(server) {
            Ok(key) => key.unwrap_or_default(),
            Err(error) => bail!(trf!(
                "Couldn't read the saved pre-auth key: {error}",
                "error" => error
            )),
        }
    } else {
        authkey.to_owned()
    };
    let key = key.trim().to_owned();
    if key.is_empty() {
        if registered {
            bail!(tr!("The pre-auth key was rejected by the server. Generate a new key in the BigScale panel and try again."));
        }
        bail!(tr!("Sign in or paste a pre-auth key first."));
    }
    Ok(key)
}

/// `tailscale up` with the key; a key the server refused says so plainly.
fn register_with_key(server: &str, authkey: &str, hostname: &str) -> Result<()> {
    let Err(e) = try_connect_with_key(server, authkey, hostname) else {
        return Ok(());
    };
    dbg(&format!("connect: key attempt failed: {e}"));
    let msg = e.to_string().to_lowercase();
    let auth_failed = msg.contains("auth-key")
        || msg.contains("authkey")
        || msg.contains("not found")
        || msg.contains("expired")
        || msg.contains("already used")
        || msg.contains("invalid key");
    if auth_failed {
        bail!(tr!("The pre-auth key was rejected by the server. Generate a new key in the BigScale panel and try again."));
    }
    Err(e)
}

pub fn connect(server: &str, authkey: &str, pending_key: bool, hostname: &str) -> Result<()> {
    // Trim once up front so a pasted key/URL with a trailing space or newline
    // (the usual copy-paste artifact) doesn't reach `tailscale up` verbatim and
    // get rejected with an opaque "invalid key". Everything below uses the
    // trimmed values.
    let server = server.trim();
    let authkey = authkey.trim();
    let hostname = hostname.trim();
    if server.is_empty() {
        bail!(tr!("Enter the server URL before connecting."));
    }
    dbg(&format!(
        "connect: server={server:?} hostname={hostname:?} authkey={}",
        if authkey.is_empty() {
            "<empty>"
        } else {
            "<provided>"
        }
    ));
    let pre_status = get_status();
    dbg(&format!(
        "connect: pre-status online={} registered={} hostname={:?} ip={:?}",
        pre_status.online, pre_status.registered, pre_status.hostname, pre_status.ip
    ));

    // Prefer a KEYLESS reconnect when this machine already holds a registration.
    // tailscale keeps the node key on disk across a `down`, so coming back up
    // needs no key and no re-auth. This makes everyday reconnects independent of
    // the pre-auth key's lifetime and — crucially — avoids the `--force-reauth`
    // that would re-register the node on every connect (which is what let a
    // one-time key expire the node between sessions). The key is only needed to
    // (re)register: first login, a logged-out/expired node, or a new server.
    // Also when a key is saved: an enrollment key is one-time, so once the
    // node is registered on this same server, re-authenticating with it
    // (--force-reauth) can only fail and log the device out. Only a
    // different server, a dead registration, or a key the person has just
    // entered (`pending_key`) needs the key.
    let same_server = current_control_url().is_some_and(|c| same_url(&c, server));
    if pre_status.registered && !pending_key && (authkey.is_empty() || same_server) {
        dbg("connect: cached registration present — trying keyless reconnect");
        if try_reconnect(server, hostname).is_ok() && wait_until_online() {
            dbg("connect: reconnected via cached registration (no key needed)");
            apply_saved_hostname(hostname);
            return Ok(());
        }
        // Registered here with only the saved one-time key: re-authenticating
        // with it can only fail and log the device out. Keep the registration.
        dbg("connect: keyless reconnect didn't take — keeping the registration");
        bail!(tr!("Couldn't reconnect to the network. Try again in a moment."));
    }

    let key = registration_key(server, authkey, pre_status.registered)?;
    register_with_key(server, &key, hostname)?;
    confirm_online()
}

/// `tailscale up` exits 0 even when the coordinator rejects the key. The real
/// failure only shows up in Self.Online and the Health array: verify.
fn confirm_online() -> Result<()> {
    if wait_until_online() {
        let post = get_status();
        dbg(&format!(
            "connect: post-status online=true hostname={:?} ip={:?}",
            post.hostname, post.ip
        ));
        return Ok(());
    }
    let post = get_status();
    dbg(&format!(
        "connect: post-status online={} hostname={:?} ip={:?}",
        post.online, post.hostname, post.ip
    ));
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

/// Control server the daemon is configured for (`tailscale debug prefs`).
fn current_control_url() -> Option<String> {
    let mut command = Command::new(tailscale_cmd());
    command.args(["debug", "prefs"]);
    let out = command_output_timeout(&mut command, Duration::from_secs(5)).ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("ControlURL")?.as_str().map(str::to_string)
}

fn same_url(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// A keyless reconnect is a bare `tailscale up`, which keeps the daemon's
/// stored hostname. When the user renamed the device while it was
/// disconnected (or before the daemon was reachable), push the saved name now
/// — otherwise the rename would only ever take effect on a re-enrollment.
fn apply_saved_hostname(wanted: &str) {
    let current = get_status().hostname;
    if hostname_needs_update(current.as_deref(), wanted) {
        dbg(&format!(
            "connect: applying saved hostname {wanted:?} (daemon has {current:?})"
        ));
        if let Err(e) = set_hostname(wanted.trim()) {
            dbg(&format!("connect: could not apply the saved hostname: {e}"));
        }
    }
}

/// Whether the daemon's hostname differs from the one the user saved.
fn hostname_needs_update(current: Option<&str>, wanted: &str) -> bool {
    let wanted = wanted.trim();
    !wanted.is_empty() && current.map(str::trim) != Some(wanted)
}

/// Poll for tailscaled to flip Online after an `up`, bypassing the status cache
/// each tick so we don't spin at cache-TTL granularity. True once online, false
/// after ~3s.
fn wait_until_online() -> bool {
    for _ in 0..10 {
        invalidate_status_cache();
        if get_status().online {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    false
}

/// Resume an existing registration without changing any stored preferences.
/// A bare `tailscale up` preserves exit-node, DNS, SSH, shields-up, and route
/// settings configured by BigLace or another Tailscale client.
fn try_reconnect(server: &str, hostname: &str) -> Result<()> {
    // `tailscale up` refuses unless every setting that differs from the
    // default is named again: the ones enrollment set, without the key.
    let (s_arg, h_arg) = (format!("--login-server={server}"), format!("--hostname={hostname}"));
    let mut args = vec!["up", "--accept-routes", "--timeout=10s"];
    if !server.is_empty() {
        args.push(&s_arg);
    }
    if !hostname.is_empty() {
        args.push(&h_arg);
    }
    run_tailscale_with_fallback(&args)
}

/// Enroll or deliberately re-authenticate against a control server. Reset is
/// limited to this explicit key-bearing path because changing login servers
/// requires a complete preference set; ordinary reconnects use `try_reconnect`.
fn try_connect_with_key(server: &str, authkey: &str, hostname: &str) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    // Enrollment intentionally starts from a complete preference set. Normal
    // reconnects go through `try_reconnect`, which keeps the registration.
    // --timeout bounds how long `tailscale up` waits to reach Running. Without
    // it, a rejected/expired key sends the daemon into a NeedsLogin state where
    // `up` blocks forever waiting for an interactive browser login — which BigLace
    // never provides, so the whole connect thread hangs and the UI is stuck on
    // "Connecting…". With the bound it fails in seconds and we surface a clear
    // "key expired, generate a new one" message.
    let mut args = vec!["up", "--reset", "--accept-routes", "--timeout=10s"];

    let s_arg;
    let h_arg;
    let op_arg;

    if !server.is_empty() {
        s_arg = format!("--login-server={server}");
        args.push(&s_arg);
    }
    // `_authkey_file` keeps the staged key alive for the duration of the call —
    // dropping the guard unlinks it, so it must stay in scope until after
    // `run_tailscale_with_fallback` returns.
    let (a_arg, _authkey_file) = authkey_arg(authkey);
    args.push(&a_arg);
    // Required when switching to a login-server that differs from the
    // one cached locally — without this, tailscale aborts with
    // "can't change --login-server without --force-reauth".
    args.push("--force-reauth");
    if !hostname.is_empty() {
        h_arg = format!("--hostname={hostname}");
        args.push(&h_arg);
    }
    // Keep the per-user operator configured after the enrollment reset.
    if !user.is_empty() {
        op_arg = format!("--operator={user}");
        args.push(&op_arg);
    }

    run_tailscale_with_fallback(&args)
}

pub fn disconnect() -> Result<()> {
    let r = run_tailscale_with_fallback(&["down"]);
    invalidate_status_cache();
    r
}

/// Sign the device out of its current control server. Use when the user wants
/// to switch accounts — `down` only stops the tunnel but keeps the node
/// registered, so the next `up` would silently rejoin the same account.
pub fn logout() -> Result<()> {
    let r = run_tailscale_with_fallback(&["logout"]);
    invalidate_status_cache();
    r
}

/// One-time setup: make `$USER` the tailscale operator so subsequent
/// `up`/`down` calls don't need pkexec. Always runs through pkexec.
#[cfg(unix)]
pub fn set_operator_current_user() -> Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        bail!(tr!("Could not determine the current user."));
    }
    let arg = format!("--operator={user}");
    let mut command = Command::new("pkexec");
    command.args(["tailscale", "set", &arg]);
    let st = command_output_timeout(&mut command, Duration::from_secs(120))
        .with_context(|| tr!("Failed to run pkexec tailscale set"))?;
    if !st.status.success() {
        bail!(tr!("Failed to set tailscale operator."));
    }
    Ok(())
}

/// On Windows there's no per-user "operator" prefs flag — the service runs as
/// LocalSystem and any logged-in admin can drive the CLI directly. Keep the
/// public API identical so window-side menu wiring doesn't have to be cfg'd
/// on every platform.
#[cfg(windows)]
pub fn set_operator_current_user() -> Result<()> {
    Ok(())
}

/// Resolve `host` through the OS resolver, giving up after `timeout`.
///
/// `getaddrinfo` has no timeout knob and, with a broken `/etc/resolv.conf`,
/// routinely blocks for the full resolver retry budget (tens of seconds on
/// some setups). Running it on a detached thread and reading the answer with a
/// deadline bounds what a click on "Open files" costs. The orphaned thread
/// finishes on its own and writes into a channel nobody reads.
fn resolve_with_timeout(host: &str, timeout: Duration) -> Option<Vec<std::net::IpAddr>> {
    use std::net::ToSocketAddrs;
    let (tx, rx) = std::sync::mpsc::channel();
    let owned = host.to_string();
    std::thread::spawn(move || {
        // The port is irrelevant — `getaddrinfo` just needs something to look
        // up against. We never open this socket.
        let addrs = (owned.as_str(), 22_u16)
            .to_socket_addrs()
            .map(|it| it.map(|s| s.ip()).collect::<Vec<_>>())
            .ok();
        let _ = tx.send(addrs);
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// Tailnet IPs tailscaled reports for the peer whose DNSName is `dns_name`.
/// Empty when the daemon doesn't know the name (or isn't reachable).
fn tailnet_ips_for(dns_name: &str) -> Vec<std::net::IpAddr> {
    let Some(ts) = cached_ts_status() else {
        return Vec::new();
    };
    let wanted = dns_name.trim_end_matches('.');
    ts.peers
        .iter()
        .flat_map(|m| m.values())
        .find(|n| n.dns_name.as_deref().unwrap_or("").trim_end_matches('.') == wanted)
        .and_then(|n| n.ips.clone())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Pick the right target for `ssh`/`sftp` at click time: prefer the tailnet
/// hostname (more readable, survives the peer reconnecting on a different IP),
/// but fall back to the IP when the name won't get the user to the right box.
///
/// Two things have to hold before we hand the name to ssh or a file manager,
/// because *they* resolve it themselves and we don't get a second chance:
///
///   1. **The local resolver answers.** biglace runs on every distro under the
///      sun and an openresolv/systemd-resolved misconfiguration silently
///      breaks MagicDNS without the user noticing. The IP path keeps the
///      launch buttons working through that.
///   2. **The answer is the peer we meant.** If MagicDNS is down but some
///      other resolver still answers for the name — a search domain, a
///      wildcard record, a captive portal's DNS — the old code accepted it and
///      would happily point ssh at a machine that isn't on the tailnet. We now
///      cross-check the resolved addresses against the IPs tailscaled reports
///      for that peer and fall back to the IP on a mismatch.
///
/// Call from a worker thread: resolution is bounded but still slow.
pub fn pick_target(dns_name: &str, ip_fallback: &str) -> String {
    if dns_name.is_empty() {
        return ip_fallback.to_string();
    }
    let Some(resolved) = resolve_with_timeout(dns_name, Duration::from_secs(3)) else {
        dbg(&format!(
            "pick_target: {dns_name} did not resolve — using {ip_fallback}"
        ));
        return ip_fallback.to_string();
    };
    if resolved.is_empty() {
        return ip_fallback.to_string();
    }

    let known = tailnet_ips_for(dns_name);
    // No opinion from the daemon (disconnected, or a peer it doesn't list):
    // keep the pre-existing behaviour and trust the resolver.
    if known.is_empty() || resolved.iter().any(|ip| known.contains(ip)) {
        return dns_name.to_string();
    }

    dbg(&format!(
        "pick_target: {dns_name} resolved to {resolved:?}, none of which is a tailnet address \
         for that peer — using {ip_fallback}"
    ));
    ip_fallback.to_string()
}

// ─── Identity ────────────────────────────────────────────────────────────────

/// Who is behind a tailnet address, as the coordination server sees it. The
/// tunnel itself is the proof: only the node holding that address's
/// WireGuard key can complete a TCP handshake from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Account login (e.g. `bruno`), with any `@domain` tail removed.
    pub login: String,
    /// Human name for the account; falls back to `login`.
    pub display_name: String,
    /// The device's tailnet name (first DNS label, e.g. `celbruno`).
    pub device: String,
}

#[derive(Deserialize)]
struct TsWhois {
    #[serde(rename = "Node")]
    node: Option<TsWhoisNode>,
    #[serde(rename = "UserProfile")]
    user: Option<TsWhoisUser>,
}

#[derive(Deserialize)]
struct TsWhoisNode {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "ComputedName", default)]
    computed_name: String,
}

#[derive(Deserialize)]
struct TsWhoisUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
}

fn parse_whois(json: &[u8]) -> Option<Identity> {
    let w: TsWhois = serde_json::from_slice(json).ok()?;
    let user = w.user?;
    let login = user.login_name.split('@').next().unwrap_or("").to_string();
    if login.is_empty() {
        return None;
    }
    let node = w.node?;
    let device = if node.computed_name.is_empty() {
        first_dns_label(&node.name).unwrap_or_default()
    } else {
        node.computed_name
    };
    let display_name = if user.display_name.trim().is_empty() {
        login.clone()
    } else {
        user.display_name
    };
    Some(Identity {
        login,
        display_name,
        device,
    })
}

/// Ask tailscaled who owns `ip`. `None` for anything that isn't a node on
/// our tailnet (or when the daemon is unreachable) — callers must treat that
/// as "unauthenticated".
pub fn whois(ip: std::net::IpAddr) -> Option<Identity> {
    let mut command = Command::new(tailscale_cmd());
    command.args(["whois", "--json", &ip.to_string()]);
    let out = command_output_timeout(&mut command, Duration::from_secs(3)).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_whois(&out.stdout)
}

// ─── Device rename ───────────────────────────────────────────────────────────

/// Push a new tailscale hostname to the running daemon. `tailscale set
/// --hostname=…` takes effect immediately, including while the tunnel is up,
/// so we don't have to drop and re-up the connection just to rename. Persist
/// the same value in our config separately so the next `tailscale up` (e.g.
/// on a fresh connect after a sign-out / sign-in cycle) still passes it on
/// the command line.
pub fn set_hostname(name: &str) -> Result<()> {
    let arg = format!("--hostname={name}");
    let r = run_tailscale_with_fallback(&["set", &arg]);
    invalidate_status_cache();
    r
}

// ─── Exit nodes ──────────────────────────────────────────────────────────────

/// Route this device's traffic through `host` (matched against tailscaled's
/// known peer hostnames). Pass `None` to clear the exit node and return to
/// direct routing.
pub fn set_exit_node(host: Option<&str>) -> Result<()> {
    let arg = match host {
        Some(h) => format!("--exit-node={h}"),
        None => "--exit-node=".to_string(),
    };
    let r = run_tailscale_with_fallback(&["set", &arg]);
    invalidate_status_cache();
    r
}

// ─── Latency ─────────────────────────────────────────────────────────────────

/// One-shot ping to `target` (IP or hostname). Returns the round-trip time in
/// milliseconds on the first reply, or None on timeout / unreachable. Caps at
/// ~2s so a dead peer doesn't hang the periodic refresh.
pub fn ping_ms(target: &str) -> Option<f64> {
    let mut command = Command::new(tailscale_cmd());
    command.args([
        "ping",
        "--c=1",
        "--timeout=2s",
        "--until-direct=false",
        target,
    ]);
    let out = command_output_timeout(&mut command, Duration::from_secs(5)).ok()?;
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
    // Reuse a shared TLS-configured agent. Building a bare `ureq::Agent` with
    // no `.tls_connector()` (as this did before) has NO TLS backend under our
    // `default-features = false` + `native-tls` setup, so every `https://`
    // health probe failed outright — the badge showed a healthy Headscale as
    // permanently unreachable. Reusing the agent also shares the connection
    // pool instead of allocating a fresh TlsConnector every 60s.
    //
    // The *redirecting* agent, specifically: this probe sends no credentials,
    // and a Headscale behind a proxy that answers `http://…/health` with a 301
    // to its HTTPS vhost would otherwise be reported as unreachable.
    let Ok(agent) = crate::panel::redirecting_agent() else {
        return false;
    };
    matches!(agent.get(&endpoint).call(), Ok(r) if (200..300).contains(&r.status()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn service_state_falls_back_to_the_daemon_without_systemd() {
        assert!(service_active_from(Ok(true), || panic!("not consulted")));
        assert!(!service_active_from(Ok(false), || panic!("not consulted")));
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(service_active_from(missing(), || true));
        assert!(!service_active_from(missing(), || false));
        // A systemctl that exists but misbehaves is not "no systemd".
        let timed_out = Err(io::Error::from(io::ErrorKind::TimedOut));
        assert!(!service_active_from(timed_out, || panic!("not consulted")));
    }

    #[test]
    fn a_failed_fetch_only_falls_back_to_a_recent_status() {
        let ts = Arc::new(serde_json::from_str::<TsStatus>("{}").unwrap());
        let fresh: CachedStatus = (Instant::now(), ts.clone());
        assert!(usable_stale(Some(&fresh)).is_some());

        let old = Instant::now()
            .checked_sub(STATUS_STALE_LIMIT + Duration::from_secs(1))
            .expect("monotonic clock far enough from boot");
        let stale: CachedStatus = (old, ts);
        assert!(
            usable_stale(Some(&stale)).is_none(),
            "a wedged daemon must not look connected"
        );
        assert!(usable_stale(None).is_none());
    }

    #[test]
    fn peers_sharing_a_hostname_keep_distinct_ids() {
        let status: TsStatus = serde_json::from_str(
            r#"{"Peer": {
                "nodekey:aaa": {"HostName": "box", "DNSName": "box.ts.net.", "Online": true,
                                "TailscaleIPs": ["100.64.0.1"]},
                "nodekey:bbb": {"HostName": "box", "DNSName": "box-1.ts.net.", "Online": false,
                                "TailscaleIPs": ["100.64.0.2"]}
            }}"#,
        )
        .unwrap();
        let peers = build_peers(&status);
        assert_eq!(peers.len(), 2);
        let ids: std::collections::HashSet<&str> = peers.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains("nodekey:aaa") && ids.contains("nodekey:bbb"));
    }

    #[test]
    fn link_tells_direct_relay_and_idle_apart() {
        let node = |json: &str| serde_json::from_str::<TsNode>(json).unwrap().link();
        assert_eq!(node(r#"{"CurAddr": "1.2.3.4:41641", "Relay": "bigscale"}"#), Link::Direct);
        assert_eq!(
            node(r#"{"CurAddr": "", "Relay": "bigscale", "Active": true}"#),
            Link::Relay("bigscale".into())
        );
        // The home relay is always reported; without traffic it means nothing.
        assert_eq!(node(r#"{"CurAddr": "", "Relay": "bigscale", "Active": false}"#), Link::Idle);
        assert_eq!(
            node(r#"{"PeerRelay": "10.0.0.2:7777", "Relay": "bigscale"}"#),
            Link::Relay("10.0.0.2:7777".into())
        );
        assert_eq!(node("{}"), Link::Idle);
    }

    #[test]
    fn whois_yields_the_account_and_device_or_nothing() {
        let id = parse_whois(
            br#"{"Node": {"Name": "celbruno.bigscale.net.", "ComputedName": "celbruno"},
                 "UserProfile": {"LoginName": "bruno@example.com", "DisplayName": "Bruno"}}"#,
        )
        .unwrap();
        assert_eq!(
            id,
            Identity {
                login: "bruno".into(),
                display_name: "Bruno".into(),
                device: "celbruno".into()
            }
        );
        // ComputedName missing: first DNS label; DisplayName blank: the login.
        let id = parse_whois(
            br#"{"Node": {"Name": "box.ts.net."}, "UserProfile": {"LoginName": "ana", "DisplayName": " "}}"#,
        )
        .unwrap();
        assert_eq!((id.device.as_str(), id.display_name.as_str()), ("box", "ana"));
        // No user, or garbage: never an identity.
        assert!(parse_whois(br#"{"Node": {"Name": "x."}}"#).is_none());
        assert!(parse_whois(br#"{"Node": {}, "UserProfile": {"LoginName": ""}}"#).is_none());
        assert!(parse_whois(b"not json").is_none());
    }

    #[test]
    fn saved_hostname_is_pushed_only_when_it_differs() {
        assert!(hostname_needs_update(Some("old-name"), "new-name"));
        assert!(hostname_needs_update(None, "new-name"));
        assert!(!hostname_needs_update(Some("same"), "same"));
        assert!(!hostname_needs_update(Some("same"), " same "));
        // Nothing saved: keep whatever the daemon has.
        assert!(!hostname_needs_update(Some("anything"), ""));
    }

    #[cfg(unix)]
    #[test]
    fn staged_authkey_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let staged = stage_authkey("tskey-secret").expect("stage the key");
        let path = staged.path.clone();

        let metadata = std::fs::metadata(&path).unwrap();
        // Owner-only: the whole point is that the key isn't readable by other
        // local users while `tailscale up` runs.
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "tskey-secret"
        );

        drop(staged);
        assert!(!path.exists(), "the staged key outlived its guard");
    }

    #[test]
    fn debug_logs_never_echo_an_inline_pre_auth_key() {
        let redacted = redact_args(&["up", "--authkey=tskey-secret", "--hostname=box"]);
        assert!(!redacted.contains("tskey-secret"));
        assert!(redacted.contains("--authkey=<redacted>"));
        // The file form carries no secret and its path is worth logging.
        let with_file = redact_args(&["up", "--authkey=file:/run/user/1000/biglace-authkey-7-0"]);
        assert!(with_file.contains("file:/run/user/1000/biglace-authkey-7-0"));
    }

    #[cfg(unix)]
    #[test]
    fn command_runner_captures_output_and_enforces_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf ok"]);
        let output = command_output_timeout(&mut command, Duration::from_secs(1)).unwrap();
        assert_eq!(output.stdout, b"ok");

        let mut command = Command::new("sleep");
        command.arg("2");
        let error = command_output_timeout(&mut command, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
