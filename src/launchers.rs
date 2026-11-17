//! Launchers for the desktop's own apps: the SSH terminal, the SFTP file
//! manager and the BigLace log viewer. Everything here only *spawns* user
//! programs — talking to the tailscale daemon lives in `crate::tailscale`.

use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::tailscale::{command_output_timeout, dbg};
use crate::tr;

// ─── Desktop integration (Linux) ─────────────────────────────────────────────

/// Desktop environment family, used to pick the file manager / terminal the
/// user actually runs instead of whatever happens to be installed first.
///
/// This matters more than it looks: distros ship GTK and Qt file managers side
/// by side (a Plasma install commonly pulls `nautilus` in as a dependency of
/// some GNOME app), so a fixed "nautilus, nemo, caja, dolphin…" probe order
/// hands a KDE user a Nautilus window for a button they pressed in a Qt-less
/// GTK app. Ordering by the running session fixes that.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desktop {
    Kde,
    Gnome,
    Xfce,
    Cinnamon,
    Mate,
    Lxqt,
    Other,
}

#[cfg(unix)]
fn desktop_env() -> Desktop {
    // XDG_CURRENT_DESKTOP is the spec'd variable and can hold a colon-separated
    // list ("ubuntu:GNOME"); the other two are the practical fallbacks for
    // sessions that don't set it (some minimal WM setups, older SDDM).
    let raw = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("XDG_SESSION_DESKTOP"))
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    for token in raw.split(':') {
        // DESKTOP_SESSION can be a path (`/usr/share/xsessions/plasma`).
        let token = token.rsplit('/').next().unwrap_or(token).trim();
        match token {
            "kde" | "plasma" | "plasma5" | "plasma6" => return Desktop::Kde,
            "gnome" | "gnome-classic" | "gnome-xorg" | "ubuntu" | "pantheon" | "budgie"
            | "budgie-desktop" => return Desktop::Gnome,
            "xfce" | "xubuntu" => return Desktop::Xfce,
            "x-cinnamon" | "cinnamon" => return Desktop::Cinnamon,
            "mate" => return Desktop::Mate,
            "lxqt" => return Desktop::Lxqt,
            _ => {}
        }
    }
    Desktop::Other
}

/// GUI file managers to try, session-native one first. Every entry here can
/// browse `sftp://` on its own (KIO on Qt, GVfs on GTK) — we never depend on
/// an external mount step.
#[cfg(unix)]
fn file_manager_candidates() -> Vec<&'static str> {
    let native: &[&str] = match desktop_env() {
        Desktop::Kde | Desktop::Lxqt => &["dolphin"],
        Desktop::Gnome => &["nautilus"],
        Desktop::Xfce => &["thunar"],
        Desktop::Cinnamon => &["nemo"],
        Desktop::Mate => &["caja"],
        Desktop::Other => &[],
    };
    let generic = ["nautilus", "dolphin", "nemo", "caja", "thunar", "pcmanfm"];
    let mut out: Vec<&'static str> = native.to_vec();
    for candidate in generic {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Extra flags a given file manager needs to open the URL in a window the user
/// will actually see.
///
/// Dolphin is the one that matters: it reuses an already-running instance, so a
/// plain `dolphin sftp://…` adds the location as a *tab* to whatever window is
/// already open — leaving the visible view on the local folder it was showing.
/// `--new-window` forces its own window at the remote location.
#[cfg(unix)]
fn file_manager_args(program: &str) -> &'static [&'static str] {
    match program {
        "dolphin" => &["--new-window"],
        "nemo" => &["--no-desktop"],
        _ => &[],
    }
}

/// Launch a known GUI file manager at `url`, with whatever flags that
/// particular one needs (see `file_manager_args`).
#[cfg(unix)]
fn launch_file_manager(program: &str, url: &str) -> bool {
    let mut command = Command::new(program);
    command.args(file_manager_args(program));
    command.arg(url);
    spawn_and_settle(&mut command)
}

/// Spawn a launcher and give it a moment to reject the argument.
///
/// `spawn()` alone only proves the binary exists — a file manager that can't
/// handle the scheme still spawns fine and then exits non-zero, which the old
/// code counted as success and used to stop trying alternatives. Waiting
/// briefly distinguishes "opened a window" (still running, or handed off to an
/// existing instance and exited 0) from "refused the URL" (exited non-zero).
#[cfg(unix)]
fn spawn_and_settle(command: &mut Command) -> bool {
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            // Still alive: it took the URL and is drawing a window. Reap it in
            // the background so the finished child doesn't linger as a zombie
            // for the lifetime of the app.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Full path of a `.desktop` entry, searched across the XDG data dirs in
/// precedence order (user overrides first).
#[cfg(unix)]
fn desktop_entry_path(desktop_id: &str) -> Option<std::path::PathBuf> {
    // Reject anything that could climb out of the applications directories —
    // `desktop_id` comes from `xdg-mime`, which reads user-writable config.
    if desktop_id.contains('/') || desktop_id.contains("..") {
        return None;
    }
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("XDG_DATA_HOME") {
        roots.push(std::path::PathBuf::from(home));
    } else if let Ok(home) = std::env::var("HOME") {
        roots.push(std::path::PathBuf::from(home).join(".local/share"));
    }
    let dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    roots.extend(dirs.split(':').filter(|d| !d.is_empty()).map(Into::into));

    for root in roots {
        let candidate = root.join("applications").join(desktop_id);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The `Exec=` line of a `.desktop` entry's `[Desktop Entry]` group.
#[cfg(unix)]
fn desktop_entry_exec(desktop_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(desktop_entry_path(desktop_id)?).ok()?;
    let mut in_main_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
            continue;
        }
        if in_main_group {
            if let Some(exec) = line.strip_prefix("Exec=") {
                return Some(exec.to_string());
            }
        }
    }
    None
}

/// Hand a URL to a `.desktop` handler. Returns true when it was actually
/// launched with the URL.
///
/// The `%u`/`%U` check is the crux: `gtk-launch` goes through GIO, and GIO
/// **silently drops** non-local arguments for entries that declare the
/// local-file field codes `%f`/`%F` — it launches the app with no argument at
/// all and exits 0. The old code read that 0 as success, which is exactly how
/// a click on "Open files" ended up showing the user their own home directory
/// instead of the peer. When the entry can't take a URL we skip `gtk-launch`
/// and run its program directly, which at least gives the app a chance to
/// parse the URL itself (Dolphin, Nemo and Thunar all do).
#[cfg(unix)]
fn launch_desktop_handler(desktop_id: &str, url: &str) -> bool {
    let exec = desktop_entry_exec(desktop_id);
    let program = exec
        .as_deref()
        .and_then(|e| e.split_whitespace().next())
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or_default()
        .to_string();

    // When the user's handler is a file manager we know, invoke it ourselves so
    // it gets the flags it needs (`--new-window` for Dolphin, see
    // `file_manager_args`). `gtk-launch` can only run the entry's plain Exec
    // line, which for Dolphin means the URL may land as a background tab of an
    // already-open window.
    if !file_manager_args(&program).is_empty() && launch_file_manager(&program, url) {
        return true;
    }

    let accepts_uris = exec
        .as_deref()
        .is_some_and(|e| e.contains("%u") || e.contains("%U"));

    if accepts_uris {
        let mut command = Command::new("gtk-launch");
        command.args([desktop_id, url]);
        if spawn_and_settle(&mut command) {
            return true;
        }
    }

    // Fall back to the entry's own program, dropping the field codes.
    let Some(exec) = exec else { return false };
    let mut parts = exec.split_whitespace().filter(|a| !a.starts_with('%'));
    let Some(program) = parts.next() else {
        return false;
    };
    let mut command = Command::new(program);
    command.args(parts);
    command.arg(url);
    spawn_and_settle(&mut command)
}

/// Compose `sftp://<user>@<host>/`, bracketing a bare IPv6 literal.
///
/// `pick_target` falls back to the peer's IP when MagicDNS can't resolve its
/// name, and on an IPv6-only peer that IP is a bare `fd7a:115c::1`. Dropped
/// into a URL unbracketed, the colons read as a port separator and every file
/// manager rejects the location.
fn sftp_url(target: &str) -> String {
    match target.rsplit_once('@') {
        Some((user, host)) => format!("sftp://{user}@{}/", bracket_ipv6(host)),
        None => format!("sftp://{}/", bracket_ipv6(target)),
    }
}

/// Wrap `host` in `[]` when it's a bare IPv6 literal (already-bracketed and
/// non-IPv6 values pass through untouched).
fn bracket_ipv6(host: &str) -> String {
    if host.starts_with('[') || !host.contains(':') {
        return host.to_string();
    }
    format!("[{host}]")
}

/// Open the user's file manager pointed at `sftp://<user>@<host>/`.
///
/// We can't rely on `xdg-open` alone: it forwards to `gio open`, which fails
/// on a bare sftp URL with "location is not mounted" because GVfs needs an
/// explicit mount step before it'll open. So we resolve the handler ourselves:
///   1. `xdg-mime query default x-scheme-handler/sftp` → the .desktop entry
///      the user (or distro) set as the SFTP handler.
///   2. Same for `inode/directory`. Most file managers register only there —
///      on Plasma the sftp scheme typically has no handler at all, so this is
///      the branch that actually fires on KDE.
///   3. Probe GUI managers, session-native first.
///   4. Last resort: `gio mount` then `xdg-open`.
///
/// Returns Err with a message to show the user when every path failed —
/// silently doing nothing was indistinguishable from the app being broken.
#[cfg(unix)]
pub fn open_files(host: &str, user: &str) -> Result<(), String> {
    let Some(target) = ssh_target(host, user) else {
        return Err(tr!("Invalid device address."));
    };
    let url = sftp_url(&target);
    dbg(&format!("open_files: {url} (desktop={:?})", desktop_env()));

    for mime in &["x-scheme-handler/sftp", "inode/directory"] {
        if let Some(desktop) = default_handler_for(mime) {
            if launch_desktop_handler(&desktop, &url) {
                return Ok(());
            }
            dbg(&format!("open_files: handler {desktop} for {mime} failed"));
        }
    }

    for cmd in file_manager_candidates() {
        if launch_file_manager(cmd, &url) {
            return Ok(());
        }
    }

    let _ = command_output_timeout(
        Command::new("gio").args(["mount", &url]),
        Duration::from_secs(20),
    );
    let mut command = Command::new("xdg-open");
    command.arg(&url);
    if spawn_and_settle(&mut command) {
        return Ok(());
    }
    Err(tr!(
        "No file manager could open the SFTP location. Install one (Dolphin, Nautilus, Nemo, Thunar…) and try again."
    ))
}

/// Windows has no native SFTP support in Explorer — the closest UX is
/// WinSCP (a third-party tool with a two-pane file-manager UI). We try
/// known install locations of WinSCP first; if absent we fall back to
/// `sshfs-win` (mounts SFTP as a drive letter via WinFSP) and lastly to
/// `start sftp://...` so the user at least sees what URL would have opened.
/// The README explains the WinSCP install step.
#[cfg(windows)]
pub fn open_files(host: &str, user: &str) -> Result<(), String> {
    let Some(target) = ssh_target(host, user) else {
        return Err(tr!("Invalid device address."));
    };
    let url = sftp_url(&target);

    // 1. WinSCP. Standard MSI installs land in Program Files; the portable
    // `winscp.com` console launcher also accepts `sftp://` URLs identically.
    const WINSCP: &[&str] = &[
        r"C:\Program Files\WinSCP\WinSCP.exe",
        r"C:\Program Files (x86)\WinSCP\WinSCP.exe",
    ];
    for path in WINSCP {
        if std::path::Path::new(path).exists() && Command::new(path).arg(&url).spawn().is_ok() {
            return Ok(());
        }
    }
    // PATH lookup, in case the user installed WinSCP via choco/scoop/portable.
    if Command::new("winscp.exe").arg(&url).spawn().is_ok() {
        return Ok(());
    }

    // 2. sshfs-win — opens File Explorer at \\sshfs.r\<user>@<host>. Requires
    // both `sshfs-win` and WinFSP to be installed; we don't probe for them
    // explicitly since spawn() will just fail silently if absent.
    if !user.is_empty()
        && Command::new("explorer.exe")
            .arg(format!(r"\\sshfs.r\{user}@{host}"))
            .spawn()
            .is_ok()
    {
        return Ok(());
    }

    // 3. Last resort: ask the shell to handle the URL. This usually pops the
    // "How do you want to open this?" dialog, which at least surfaces to the
    // user that no SFTP client is installed.
    if Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    Err(tr!(
        "No file manager could open the SFTP location. Install WinSCP and try again."
    ))
}

#[cfg(unix)]
fn default_handler_for(mime: &str) -> Option<String> {
    let mut command = Command::new("xdg-mime");
    command.args(["query", "default", mime]);
    let out = command_output_timeout(&mut command, Duration::from_secs(5)).ok()?;
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

/// Resolve `program` the way `execvp` would: a path is checked as-is,
/// a bare name is looked up in `$PATH`. Done in-process — it used to fork a
/// `sh -c "command -v …"` per candidate, which also put configuration values
/// (`$TERMINAL`, GSettings, kdeglobals) through a shell.
#[cfg(unix)]
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    fn executable(path: &std::path::Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = std::path::PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

/// Whether `program` is on PATH.
#[cfg(unix)]
fn program_exists(program: &str) -> bool {
    find_in_path(program).is_some()
}

/// Split a configured command (`kitty --single-instance`) into the program
/// and its own arguments. Settings like `$TERMINAL` or GNOME's
/// `default-applications.terminal exec` legitimately carry flags; treating the
/// whole string as a program name made those setups fall through to probing.
#[cfg(unix)]
fn split_command(value: &str) -> Option<(String, Vec<String>)> {
    let mut parts = value.split_whitespace().map(str::to_string);
    let program = parts.next()?;
    Some((program, parts.collect()))
}

/// File name of a program path, for comparing against emulator names.
#[cfg(unix)]
fn program_name(program: &str) -> String {
    program.rsplit('/').next().unwrap_or(program).to_string()
}

/// Desktop-entry id of the terminal named in `xdg-terminals.list` (first
/// non-comment line), per the freedesktop terminal-execution draft.
#[cfg(unix)]
fn xdg_terminals_list_entry() -> Option<String> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("XDG_CONFIG_HOME") {
        roots.push(std::path::PathBuf::from(home));
    } else if let Ok(home) = std::env::var("HOME") {
        roots.push(std::path::PathBuf::from(home).join(".config"));
    }
    let dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".to_string());
    roots.extend(dirs.split(':').filter(|d| !d.is_empty()).map(Into::into));

    for root in roots {
        let Ok(text) = std::fs::read_to_string(root.join("xdg-terminals.list")) else {
            continue;
        };
        if let Some(entry) = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
        {
            return Some(entry.to_string());
        }
    }
    None
}

/// How to start the user's terminal.
#[cfg(unix)]
struct TerminalLauncher {
    /// Program we invoke, and the args that precede the command to run.
    program: String,
    args: Vec<String>,
    /// Program of the emulator that will actually appear on screen. Differs
    /// from `program` when we go through `xdg-terminal-exec`, and lets callers
    /// reach for that emulator's native flags.
    emulator: String,
}

/// The terminal the user has actually configured.
///
/// Probing a hardcoded list is a last resort, not the first move: every desktop
/// has a real "this is my terminal" setting and honouring it is the difference
/// between the button opening the terminal the user expects and it opening
/// whichever emulator happens to sort first in our array. Sources, in order of
/// how authoritative they are:
///
///   1. `$TERMINAL` — explicit user intent, respected by most CLI tooling.
///   2. `xdg-terminal-exec` — the freedesktop mechanism. It reads
///      `xdg-terminals.list` and launches the entry named there, so it covers
///      every desktop at once. This is what BigLinux uses to point at
///      Ashyterm, and it's why the button must not second-guess it.
///   3. GNOME's `default-applications.terminal` GSettings pair.
///   4. Plasma's `TerminalApplication` in `kdeglobals`.
#[cfg(unix)]
fn configured_terminal() -> Option<TerminalLauncher> {
    fn have(program: &str) -> bool {
        program_exists(program)
    }
    fn read_setting(program: &str, args: &[&str]) -> Option<String> {
        let mut command = Command::new(program);
        command.args(args);
        let out = command_output_timeout(&mut command, Duration::from_secs(5)).ok()?;
        if !out.status.success() {
            return None;
        }
        // GSettings quotes string values ('xdg-terminal-exec').
        let value = std::str::from_utf8(&out.stdout)
            .ok()?
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .trim()
            .to_string();
        (!value.is_empty()).then_some(value)
    }

    if let Some((program, mut args)) = std::env::var("TERMINAL")
        .ok()
        .and_then(|t| split_command(&t))
    {
        if have(&program) {
            args.push("-e".into());
            return Some(TerminalLauncher {
                emulator: program_name(&program),
                program,
                args,
            });
        }
    }

    if have("xdg-terminal-exec") {
        // Resolve which emulator it will pick so callers can use that program's
        // native flags — going through the generic "run this command" path
        // costs real quality on some terminals (see `launch_ssh_in_terminal`).
        let emulator = xdg_terminals_list_entry()
            .and_then(|id| desktop_entry_exec(&id))
            .and_then(|exec| {
                exec.split_whitespace()
                    .next()
                    .and_then(|p| p.rsplit('/').next())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "xdg-terminal-exec".into());
        // `--` ends xdg-terminal-exec's own options, so a command starting with
        // a dash can't be mistaken for one.
        return Some(TerminalLauncher {
            program: "xdg-terminal-exec".into(),
            args: vec!["--".into()],
            emulator,
        });
    }

    if let Some((exec, mut args)) = read_setting(
        "gsettings",
        &[
            "get",
            "org.gnome.desktop.default-applications.terminal",
            "exec",
        ],
    )
    .and_then(|v| split_command(&v))
    {
        if have(&exec) {
            let flag = read_setting(
                "gsettings",
                &[
                    "get",
                    "org.gnome.desktop.default-applications.terminal",
                    "exec-arg",
                ],
            )
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| "-e".into());
            args.push(flag);
            return Some(TerminalLauncher {
                emulator: program_name(&exec),
                program: exec,
                args,
            });
        }
    }

    for kread in ["kreadconfig6", "kreadconfig5"] {
        if !have(kread) {
            continue;
        }
        if let Some((exec, mut args)) = read_setting(
            kread,
            &[
                "--file",
                "kdeglobals",
                "--group",
                "General",
                "--key",
                "TerminalApplication",
            ],
        )
        .and_then(|v| split_command(&v))
        {
            if have(&exec) {
                args.push("-e".into());
                return Some(TerminalLauncher {
                    emulator: program_name(&exec),
                    program: exec,
                    args,
                });
            }
        }
    }

    None
}

/// Terminal emulators to probe when nothing is configured, paired with the flag
/// each one uses to mean "run this command instead of a shell".
///
/// Ashyterm leads because it's BigCommunity's own terminal and ships as the
/// default on BigLinux — on a BigLinux box that never wrote an
/// `xdg-terminals.list`, this is what the user means by "the terminal". After
/// that comes the session's native emulator, and `xterm` sits last: probing it
/// early (as the original list did) handed a Plasma user a bare 1980s xterm for
/// a button they pressed in a themed app.
#[cfg(unix)]
fn terminal_candidates() -> Vec<(&'static str, &'static str)> {
    let native: &[(&str, &str)] = match desktop_env() {
        Desktop::Kde => &[("konsole", "-e")],
        Desktop::Gnome => &[("kgx", "--"), ("gnome-terminal", "--")],
        Desktop::Xfce => &[("xfce4-terminal", "--command")],
        Desktop::Cinnamon => &[("gnome-terminal", "--")],
        Desktop::Mate => &[("mate-terminal", "--")],
        Desktop::Lxqt => &[("qterminal", "-e")],
        Desktop::Other => &[],
    };
    let generic = [
        ("konsole", "-e"),
        ("gnome-terminal", "--"),
        ("kgx", "--"),
        ("xfce4-terminal", "--command"),
        ("mate-terminal", "--"),
        ("qterminal", "-e"),
        ("alacritty", "-e"),
        ("kitty", "--"),
        ("foot", "-e"),
        ("tilix", "-e"),
        ("terminator", "-x"),
        ("xterm", "-e"),
    ];
    let mut out: Vec<(&'static str, &'static str)> = vec![("ashyterm", "-e")];
    out.extend_from_slice(native);
    for (name, flag) in generic {
        if !out.iter().any(|(existing, _)| *existing == name) {
            out.push((name, flag));
        }
    }
    out
}

/// Open an SSH session to `target` in the user's terminal.
///
/// Prefers the emulator's own SSH mode over the generic "run this command"
/// path, because the generic path is not equivalent. Ashyterm's `-e`
/// implementation *types* the command into an interactive shell rather than
/// exec'ing it, so the session opened with a stray
/// `^[[200~ssh user@host^[[201~` line — the shell echoing bracketed-paste
/// markers. `ashyterm --ssh` spawns the client directly and adds connection
/// multiplexing, keepalives and OSC7 remote-directory reporting on top:
///
///   -e      python3 __init__.py → /bin/bash → ssh user@host      (pasted)
///   --ssh   python3 __init__.py → ssh -t -o ControlMaster=auto …  (exec'd)
#[cfg(unix)]
fn launch_ssh_in_terminal(target: &str) -> bool {
    let configured = configured_terminal();
    let emulator = configured
        .as_ref()
        .map(|t| t.emulator.as_str())
        .unwrap_or_default();
    // Either ashyterm is the configured terminal, or nothing is configured and
    // it heads the probe list below — same outcome, so use its native mode.
    if emulator == "ashyterm" || (emulator.is_empty() && program_exists("ashyterm")) {
        let mut command = Command::new("ashyterm");
        command.args(["--ssh", target]);
        if spawn_and_settle(&mut command) {
            return true;
        }
        dbg("terminal: ashyterm --ssh failed, falling back");
    }
    launch_in_terminal(&["ssh", target])
}

/// Run `argv` inside the user's terminal emulator.
#[cfg(unix)]
fn launch_in_terminal(argv: &[&str]) -> bool {
    if let Some(TerminalLauncher { program, args, .. }) = configured_terminal() {
        dbg(&format!("terminal: using configured {program} {args:?}"));
        let mut command = Command::new(&program);
        command.args(&args);
        command.args(argv);
        if spawn_and_settle(&mut command) {
            return true;
        }
        dbg(&format!("terminal: configured {program} failed, probing"));
    }
    for (term, flag) in terminal_candidates() {
        let mut command = Command::new(term);
        command.arg(flag);
        command.args(argv);
        if spawn_and_settle(&mut command) {
            return true;
        }
    }
    false
}

/// Open a terminal running `ssh <user>@<host>`. `user` should be the peer's
/// OS user (its hostname on Linux). When empty we fall back to `ssh <host>`,
/// which makes ssh use the local username — usually wrong, but better than
/// failing to launch at all.
#[cfg(unix)]
pub fn open_terminal(host: &str, user: &str) -> Result<(), String> {
    let Some(target) = ssh_target(host, user) else {
        return Err(tr!("Invalid device address."));
    };

    // Pass program and target as separate argv values. No shell is involved.
    if launch_ssh_in_terminal(target.as_str()) {
        return Ok(());
    }
    Err(tr!(
        "No terminal emulator found. Install one (Konsole, GNOME Terminal, xterm…) and try again."
    ))
}

/// Windows variant. Preference order:
///   1. Windows Terminal (`wt.exe`) — modern, tabbed, ships with Win11 and
///      is a free Store install on Win10.
///   2. PowerShell — kept open via `-NoExit` so the SSH session output stays
///      visible after the connection closes.
///   3. cmd.exe `/K` — last resort, same "stay open" behavior.
///
/// `ssh.exe` itself ships with the OpenSSH client, an optional Windows
/// feature that's enabled by default on Win10 (1809+) and Win11.
#[cfg(windows)]
pub fn open_terminal(host: &str, user: &str) -> Result<(), String> {
    let Some(target) = ssh_target(host, user) else {
        return Err(tr!("Invalid device address."));
    };

    // wt.exe new-tab ssh user@host
    if Command::new("wt.exe")
        .args(["new-tab", "ssh", &target])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    // PowerShell with -NoExit so the window stays open after ssh exits.
    if Command::new("powershell.exe")
        .args(["-NoExit", "-Command", &format!("ssh.exe --% {target}")])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    // Fall back to cmd /K so the window doesn't vanish on disconnect.
    if Command::new("cmd.exe")
        .args(["/K", &format!("ssh {target}")])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    Err(tr!(
        "No terminal emulator found. Install one (Konsole, GNOME Terminal, xterm…) and try again."
    ))
}

/// Reject control/shell syntax before composing SFTP URLs or Windows fallback
/// commands. Tailscale DNS names, IP literals and normal OS users fit this set.
fn ssh_target(host: &str, user: &str) -> Option<String> {
    fn safe(value: &str, is_host: bool) -> bool {
        !value.is_empty()
            && value.len() <= 255
            && value.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '.' | '-' | '_')
                    // Colons and brackets belong to IPv6 literals and are only
                    // valid on the host side. On the user side a colon would
                    // split the URL's userinfo into `user:password`, so a login
                    // propagated by the panel (network-controlled) could smuggle
                    // a password field into the SFTP URL.
                    || (is_host && matches!(c, ':' | '[' | ']'))
            })
    }
    if !safe(host, true) || (!user.is_empty() && !safe(user, false)) {
        return None;
    }
    Some(if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    })
}

/// Open a terminal that tails `tailscaled`'s journal. Useful for debugging
/// connect failures without leaving biglace.
#[cfg(unix)]
pub fn open_logs() {
    // `pkexec journalctl -fu tailscaled` would be ideal but pkexec inside a
    // terminal child often deadlocks on polkit's auth dialog. Plain
    // `journalctl` works for the user's own session journal and surfaces
    // tailscaled-routed lines via the system journal under `-u tailscaled`
    // when the user is in `systemd-journal` (Manjaro default).
    let cmd = "journalctl -fu tailscaled --no-pager; echo; echo '[press enter to close]'; read";
    // Always go through `bash -c`: the script uses shell syntax (`;`, `read`),
    // and emulators differ on whether a single `-e` argument is re-parsed by a
    // shell or exec'd directly. Naming the shell explicitly makes it uniform.
    launch_in_terminal(&["bash", "-c", cmd]);
}

/// Windows variant — Tailscale's Windows daemon writes to its own log file
/// under `%ProgramData%\Tailscale\Logs\`, not to the Windows Event Log. We
/// open a PowerShell window that tails the most recent log file using
/// `Get-Content -Wait`, which is the closest equivalent to `journalctl -f`.
#[cfg(windows)]
pub fn open_logs() {
    // `-NoExit` keeps the window open after Get-Content is interrupted.
    let cmd = r#"$d = Join-Path $env:ProgramData 'Tailscale\Logs'; if (Test-Path $d) { $f = Get-ChildItem $d -Filter *.txt | Sort-Object LastWriteTime -Descending | Select-Object -First 1; if ($f) { Get-Content $f.FullName -Wait -Tail 200 } else { Write-Host 'no log files yet' } } else { Write-Host 'Tailscale log dir not found' }"#;
    if Command::new("wt.exe")
        .args(["new-tab", "powershell.exe", "-NoExit", "-Command", cmd])
        .spawn()
        .is_ok()
    {
        return;
    }
    let _ = Command::new("powershell.exe")
        .args(["-NoExit", "-Command", cmd])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn programs_are_resolved_without_a_shell() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("biglace-no-such-program-xyz").is_none());
        // Present but not executable.
        let plain = std::env::temp_dir().join(format!("biglace-noexec-{}", std::process::id()));
        std::fs::write(&plain, "not a program").unwrap();
        assert!(find_in_path(plain.to_str().unwrap()).is_none());
        let _ = std::fs::remove_file(&plain);
        // Shell syntax is just an (absent) file name now, never executed.
        assert!(find_in_path("sh; touch /tmp/biglace-pwned").is_none());
        assert!(!std::path::Path::new("/tmp/biglace-pwned").exists());

        assert_eq!(
            split_command("kitty --single-instance"),
            Some(("kitty".to_string(), vec!["--single-instance".to_string()]))
        );
        assert_eq!(split_command("   "), None);
        assert_eq!(program_name("/usr/bin/ashyterm"), "ashyterm");
    }

    #[test]
    fn ssh_target_accepts_network_names_and_rejects_shell_syntax() {
        assert_eq!(
            ssh_target("host.example", "alice"),
            Some("alice@host.example".into())
        );
        assert_eq!(
            ssh_target("[fd7a:115c::1]", ""),
            Some("[fd7a:115c::1]".into())
        );
        assert_eq!(ssh_target("host;touch-pwned", "alice"), None);
        assert_eq!(ssh_target("host", "$(id)"), None);
        // A colon is legal in an IPv6 host but never in the login: on the user
        // side it would split the URL's userinfo into `user:password`.
        assert_eq!(ssh_target("host.example", "alice:secret"), None);
    }

    #[cfg(unix)]
    #[test]
    fn sftp_url_brackets_bare_ipv6_literals() {
        assert_eq!(sftp_url("alice@host.example"), "sftp://alice@host.example/");
        assert_eq!(sftp_url("alice@100.64.0.6"), "sftp://alice@100.64.0.6/");
        // Unbracketed IPv6 would read as host `fd7a` + port — every file
        // manager rejects that location.
        assert_eq!(
            sftp_url("alice@fd7a:115c:a1e0::6"),
            "sftp://alice@[fd7a:115c:a1e0::6]/"
        );
        assert_eq!(sftp_url("fd7a:115c:a1e0::6"), "sftp://[fd7a:115c:a1e0::6]/");
        // Already bracketed (what `ssh_target` yields for an IPv6 peer) passes
        // through untouched instead of double-bracketing.
        assert_eq!(sftp_url("[fd7a:115c::1]"), "sftp://[fd7a:115c::1]/");
    }

    #[cfg(unix)]
    #[test]
    fn launcher_candidates_are_deduplicated_and_lead_with_the_session_native_app() {
        let managers = file_manager_candidates();
        let mut seen = managers.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), managers.len(), "duplicate file manager probed");

        let terminals = terminal_candidates();
        let mut names: Vec<&str> = terminals.iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate terminal probed");

        // Ashyterm leads (BigCommunity's own terminal, the BigLinux default)
        // and xterm is the universal last resort — never ahead of a desktop's
        // own emulator, whichever session this runs under.
        assert_eq!(terminals.first().map(|(n, _)| *n), Some("ashyterm"));
        assert_eq!(terminals.last().map(|(n, _)| *n), Some("xterm"));
    }

    #[cfg(unix)]
    #[test]
    fn xdg_terminals_list_skips_comments_and_blank_lines() {
        let _env = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!("biglace-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("xdg-terminals.list"),
            "# the user's terminal\n\norg.communitybig.ashyterm.desktop\nkonsole.desktop\n",
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        assert_eq!(
            xdg_terminals_list_entry().as_deref(),
            Some("org.communitybig.ashyterm.desktop")
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn desktop_entry_lookup_rejects_path_traversal() {
        let _env = crate::test_env_lock();
        assert!(desktop_entry_path("../../etc/passwd").is_none());
        assert!(desktop_entry_path("/etc/passwd").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn desktop_entry_exec_reads_the_main_group_only() {
        let _env = crate::test_env_lock();
        // A `[Desktop Action …]` group's Exec must not be mistaken for the
        // entry's own — actions like Dolphin's "open-home" would send the file
        // manager to the local home directory instead of the peer.
        let dir = std::env::temp_dir().join(format!("biglace-test-{}", std::process::id()));
        let apps = dir.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(
            apps.join("probe.desktop"),
            "[Desktop Entry]\nName=Probe\nExec=probe-fm %U\n\n\
             [Desktop Action open-home]\nExec=probe-fm --home\n",
        )
        .unwrap();
        std::env::set_var("XDG_DATA_HOME", &dir);

        assert_eq!(
            desktop_entry_exec("probe.desktop").as_deref(),
            Some("probe-fm %U")
        );

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
