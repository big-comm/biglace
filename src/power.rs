//! Power actions on a computer someone is helping remotely: "Restart and
//! reconnect" and "Shut down", asked for from inside a remote session (the
//! person there already let that viewer control the computer).
//!
//! Restart and reconnect:
//! 1. the host remembers the viewer (a one-time pass, 20 minutes, so nobody
//!    has to click "Allow" at a computer that just restarted);
//! 2. arms a one-time automatic sign-in (`biglace-autologin`, through
//!    pkexec — no password for the person signed in) and restarts;
//! 3. after the restart BigLace starts, **locks the screen at once** and the
//!    viewer's window connects again by itself. The boot service locks the
//!    session and undoes the automatic sign-in even if BigLace doesn't start.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Announced in `/v1/hello` when this computer can do it.
pub const CAPABILITY: &str = "power";
const HELPER: &str = "/usr/lib/biglace/biglace-autologin";
/// How long the viewer may come back without being asked.
const RESUME_SECS: u64 = 20 * 60;

/// The viewer allowed back after a restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Resume {
    login: String,
    device: String,
    /// Unix seconds.
    until: u64,
    /// When the restart was asked for (unix seconds).
    #[serde(default)]
    asked: u64,
    /// The boot (its start time) whose session was already locked.
    #[serde(default)]
    locked_boot: u64,
}

/// When this boot started (unix seconds), from the kernel.
fn boot_time() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

/// Lock now? Only on the first start after the restart that was asked for.
fn should_lock(r: &Resume, boot: u64) -> bool {
    boot != 0 && r.asked != 0 && r.asked < boot && r.locked_boot != boot
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `$XDG_CONFIG_HOME` (read each time, unlike GLib's cached value).
fn config_base() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| gtk4::glib::home_dir().join(".config"))
}

fn resume_path() -> PathBuf {
    config_base().join("biglace").join("resume.json")
}

/// One-shot autostart, when BigLace doesn't start with the session anyway.
fn autostart_once_path() -> PathBuf {
    config_base().join("autostart").join("biglace-resume.desktop")
}

/// This computer can restart and come back (Linux, helper installed).
pub fn available() -> bool {
    cfg!(target_os = "linux") && Path::new(HELPER).is_file()
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)
}

fn read_resume() -> Option<Resume> {
    let r: Resume = serde_json::from_slice(&std::fs::read(resume_path()).ok()?).ok()?;
    if r.until < now() {
        let _ = std::fs::remove_file(resume_path());
        return None;
    }
    Some(r)
}

/// Is `login`@`device` the viewer that restarted this computer (and still
/// within its time)? Then it's let back in without asking.
pub fn resume_allows(login: &str, device: &str) -> bool {
    read_resume().is_some_and(|r| r.login == login && r.device == device)
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("exit {}", out.status) } else { err })
    }
}

/// Restart this computer and come back to the desktop for `login`@`device`.
pub fn restart_and_return(login: &str, device: &str) -> Result<(), String> {
    if !available() {
        return Err(crate::tr!("this computer can't restart remotely (BigLace's helper isn't installed)"));
    }
    let resume = Resume { login: login.into(), device: device.into(), until: now() + RESUME_SECS, asked: now(), locked_boot: 0 };
    write_private(&resume_path(), &serde_json::to_vec(&resume).unwrap_or_default()).map_err(|e| e.to_string())?;
    if !crate::autostart::is_enabled() {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "biglace".into());
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=BigLace\nExec={exe} --hidden\nIcon={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            crate::APP_ID
        );
        let _ = write_private(&autostart_once_path(), entry.as_bytes());
    }
    let hint = std::env::var("XDG_SESSION_DESKTOP").unwrap_or_default().to_lowercase();
    let undo = || {
        let _ = std::fs::remove_file(resume_path());
        let _ = std::fs::remove_file(autostart_once_path());
    };
    if let Err(e) = run(Command::new("pkexec").args([HELPER, "arm", &hint])) {
        undo();
        return Err(crate::trf!("could not prepare the automatic sign-in: {error}", "error" => e));
    }
    if let Err(e) = run(Command::new("systemctl").arg("reboot")) {
        let _ = run(Command::new("pkexec").args([HELPER, "disarm"]));
        undo();
        return Err(crate::trf!("could not restart: {error}", "error" => e));
    }
    Ok(())
}

pub fn shutdown() -> Result<(), String> {
    run(Command::new("systemctl").arg("poweroff")).map_err(|e| crate::trf!("could not shut down: {error}", "error" => e))
}

/// This boot came from "Restart and reconnect" (signed in automatically:
/// the keyring is still locked, so nothing should ask it for secrets).
pub fn just_restarted() -> bool {
    let boot = boot_time();
    read_resume().is_some_and(|r| boot != 0 && r.asked != 0 && r.asked < boot)
}

/// At startup: back from "Restart and reconnect"? Lock the screen first
/// thing, and undo the one-time setup.
pub fn after_start() {
    let _ = std::fs::remove_file(autostart_once_path());
    let Some(mut resume) = read_resume() else { return };
    let boot = boot_time();
    if !should_lock(&resume, boot) {
        return;
    }
    resume.locked_boot = boot;
    let _ = write_private(&resume_path(), &serde_json::to_vec(&resume).unwrap_or_default());
    // GNOME refuses remote access while locked: there the screen stays as
    // it was before the restart (open to the person helping) and locks
    // when their session ends — or if they don't come back soon.
    let defer = locked_screen_blocks_remote();
    let _ = std::thread::Builder::new().name("biglace-power".into()).spawn(move || {
        if defer {
            defer_lock();
        } else {
            lock_screen();
        }
        if Path::new("/var/lib/biglace/autologin.state").exists() {
            let _ = run(Command::new("pkexec").args([HELPER, "disarm"]));
        }
    });
}

/// GNOME (Wayland): its lock screen stops remote desktop sessions.
fn locked_screen_blocks_remote() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    desktop.split(':').any(|d| d == "gnome") && std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// GNOME with its screen locked right now: remote access is refused.
pub fn screen_locked_blocks_remote() -> bool {
    locked_screen_blocks_remote()
        && output(Command::new("gdbus").args([
            "call",
            "--session",
            "--dest",
            "org.gnome.ScreenSaver",
            "--object-path",
            "/org/gnome/ScreenSaver",
            "--method",
            "org.gnome.ScreenSaver.GetActive",
        ]))
        .is_some_and(|s| s.contains("true"))
}

/// How long the person who restarted the computer has to come back
/// before it locks anyway (GNOME).
const DEFERRED_LOCK: std::time::Duration = std::time::Duration::from_secs(180);

/// A lock waiting for the returning session to end (GNOME).
struct Deferred {
    since: std::time::Instant,
    /// The returning viewer is connected now.
    active: bool,
}

fn deferred() -> &'static std::sync::Mutex<Option<Deferred>> {
    static D: std::sync::OnceLock<std::sync::Mutex<Option<Deferred>>> = std::sync::OnceLock::new();
    D.get_or_init(Default::default)
}

fn defer_lock() {
    if let Ok(mut d) = deferred().lock() {
        *d = Some(Deferred { since: std::time::Instant::now(), active: false });
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let due = match deferred().lock().ok().and_then(|d| d.as_ref().map(|d| (d.active, d.since.elapsed()))) {
            None => return,
            Some((active, waited)) => !active && waited >= DEFERRED_LOCK,
        };
        if due {
            if let Ok(mut d) = deferred().lock() {
                *d = None;
            }
            lock_screen();
            return;
        }
    }
}

/// A remote session started here (the host side tells us).
pub fn session_started(login: &str, device: &str) {
    if !resume_allows(login, device) {
        return;
    }
    if let Ok(mut d) = deferred().lock() {
        if let Some(d) = d.as_mut() {
            d.active = true;
        }
    }
}

/// A remote session ended: if it was the returning one, lock now.
pub fn session_ended(login: &str, device: &str) {
    let lock = deferred().lock().ok().is_some_and(|mut d| {
        let returning = d.as_ref().is_some_and(|d| d.active) && resume_allows(login, device);
        if returning {
            *d = None;
        }
        returning
    });
    if lock {
        lock_screen();
    }
}

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
}

/// Lock this session's screen, whatever the desktop.
pub fn lock_screen() {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.contains("xfce") {
        if has("xfce4-screensaver-command") && lock_xfce() {
            return;
        }
        if has("light-locker-command") && run(Command::new("light-locker-command").arg("-l")).is_ok() {
            return;
        }
    }
    if run(Command::new("loginctl").arg("lock-session")).is_ok() {
        return;
    }
    let _ = run(Command::new("xdg-screensaver").arg("lock"));
}

fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn xfce_locked() -> bool {
    output(Command::new("xfce4-screensaver-command").arg("-q")).is_some_and(|s| s.contains("is active"))
}

/// XFCE's screensaver locks only while it runs and with locking turned on
/// (BigLinux ships it off). Turn it on for this one lock, and back off once
/// the person unlocks.
fn lock_xfce() -> bool {
    let wait = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
    // Right after sign-in the session may still be starting.
    for _ in 0..20 {
        if output(Command::new("xfce4-screensaver-command").arg("-q")).is_some() {
            break;
        }
        if run(Command::new("pgrep").args(["-u", &whoami(), "-f", "^xfce4-screensaver$"])).is_err() {
            let _ = Command::new("xfce4-screensaver").spawn();
        }
        wait(500);
    }
    let prop = ["-c", "xfce4-screensaver", "-p", "/lock/enabled"];
    let was_on = output(Command::new("xfconf-query").args(prop)).as_deref() == Some("true");
    if !was_on {
        let _ = run(Command::new("xfconf-query").args(prop).args(["-n", "-t", "bool", "-s", "true"]))
            .or_else(|_| run(Command::new("xfconf-query").args(prop).args(["-s", "true"])));
    }
    let mut locked = false;
    for _ in 0..5 {
        let _ = run(Command::new("xfce4-screensaver-command").arg("--lock"));
        wait(800);
        if xfce_locked() {
            locked = true;
            break;
        }
    }
    if !was_on {
        // Back to how it was once unlocked (or now, if it didn't lock).
        let _ = std::thread::Builder::new().name("biglace-unlock-watch".into()).spawn(move || {
            while locked && xfce_locked() {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            let _ = run(Command::new("xfconf-query").args(prop).args(["-s", "false"]));
        });
    }
    locked
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pass_is_for_that_viewer_only_and_expires() {
        let _env = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!("biglace-power-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let r = Resume { login: "tales".into(), device: "notebook".into(), until: now() + 60, asked: 100, locked_boot: 0 };
        write_private(&resume_path(), &serde_json::to_vec(&r).unwrap()).unwrap();
        assert!(resume_allows("tales", "notebook"));
        assert!(!resume_allows("tales", "outro"));
        assert!(!resume_allows("bruna", "notebook"));
        let old = Resume { until: now() - 1, ..r };
        write_private(&resume_path(), &serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(!resume_allows("tales", "notebook"), "expired");
        assert!(!resume_path().exists(), "and forgotten");
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Locked once, on the first start after the restart — not when BigLace
    /// is merely reopened (or reinstalled) before or after.
    #[test]
    fn locks_only_once_after_the_restart() {
        let r = Resume { login: "t".into(), device: "d".into(), until: 0, asked: 1_000, locked_boot: 0 };
        assert!(should_lock(&r, 1_100), "restarted after asking");
        assert!(!should_lock(&r, 900), "same boot as the request");
        assert!(!should_lock(&Resume { locked_boot: 1_100, ..r.clone() }, 1_100), "already locked this boot");
        assert!(!should_lock(&r, 0));
    }
}
