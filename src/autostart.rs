#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{fs, path::PathBuf};

#[cfg(any(target_os = "linux", target_os = "macos"))]
const APP_ID: &str = "org.communitybig.biglace";
#[cfg(any(target_os = "linux", target_os = "windows"))]
const APP_NAME: &str = "BigLace";

pub fn is_enabled() -> bool {
    is_enabled_impl()
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    set_enabled_impl(enabled)
}

/// Re-emit the autostart entry on startup when it's already enabled, so an
/// in-place upgrade picks up the current entry format.
///
/// Older biglace versions wrote the autostart entry without the `--hidden`
/// flag — the start-in-tray feature didn't exist yet. Installing a newer
/// binary over such a setup leaves that stale `.desktop` / registry value in
/// place (nothing rewrites it while the switch stays on), so the login launch
/// keeps popping the window open instead of going to the tray. Rewriting the
/// entry here migrates those files to the `--hidden` form on the first run of
/// the new binary. No-op when start-at-login is off. Best-effort: any failure
/// (read-only HOME, missing `reg`, …) is swallowed so app startup never breaks
/// over an autostart housekeeping write.
pub fn migrate_if_enabled() {
    if is_enabled() {
        if let Err(e) = set_enabled(true) {
            eprintln!("[biglace] autostart: failed to refresh entry: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
fn autostart_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("autostart").join(format!("{APP_ID}.desktop"))
}

#[cfg(target_os = "linux")]
fn is_enabled_impl() -> bool {
    let Ok(data) = fs::read_to_string(autostart_path()) else {
        return false;
    };
    data.lines().all(|line| {
        line.trim() != "Hidden=true" && line.trim() != "X-GNOME-Autostart-enabled=false"
    })
}

#[cfg(target_os = "linux")]
fn set_enabled_impl(enabled: bool) -> Result<()> {
    let path = autostart_path();
    if !enabled {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| crate::tr!("Failed to disable start at login"))
            }
        }
        return Ok(());
    }

    let exec = autostart_exec()?;
    // `--hidden` makes the login-triggered launch come up directly in the tray
    // instead of popping the window open every time the user logs in. A manual
    // launch from the app menu (the installed .desktop without this flag) still
    // opens the window normally.
    let data = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=Connect your machine to the mesh network\n\
         Exec={exec} --hidden\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Network;System;\n\
         X-GNOME-Autostart-enabled=true\n"
    );

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| crate::tr!("Failed to create the autostart folder"))?;
    }
    fs::write(&path, data).with_context(|| crate::tr!("Failed to enable start at login"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn autostart_exec() -> Result<String> {
    let installed = PathBuf::from("/usr/bin/biglace");
    let path = if installed.exists() {
        installed
    } else {
        std::env::current_exe().context("current executable")?
    };
    Ok(escape_desktop_exec(&path.to_string_lossy()))
}

#[cfg(target_os = "linux")]
fn escape_desktop_exec(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' | ' ' | '\t' | '\n' | '"' | '\'' | '>' | '<' | '~' | '|' | '&' | ';' | '$'
            | '*' | '?' | '#' | '(' | ')' | '`' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(target_os = "windows")]
fn is_enabled_impl() -> bool {
    std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            APP_NAME,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn set_enabled_impl(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe().context("current executable")?;
        // Quote the path (it can contain spaces, e.g. C:\Program Files\…) and
        // append `--hidden` so the login launch starts in the tray instead of
        // opening the window. The whole thing is one REG_SZ value.
        let value = format!("\"{}\" --hidden", exe.to_string_lossy());
        let st = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                APP_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()
            .with_context(|| crate::tr!("Failed to enable start at login"))?;
        if st.success() {
            Ok(())
        } else {
            anyhow::bail!(crate::tr!("Failed to enable start at login"))
        }
    } else {
        let st = std::process::Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                APP_NAME,
                "/f",
            ])
            .status()
            .with_context(|| crate::tr!("Failed to disable start at login"))?;
        if st.success() || !is_enabled_impl() {
            Ok(())
        } else {
            anyhow::bail!(crate::tr!("Failed to disable start at login"))
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{APP_ID}.plist"))
}

#[cfg(target_os = "macos")]
fn is_enabled_impl() -> bool {
    launch_agent_path().exists()
}

#[cfg(target_os = "macos")]
fn set_enabled_impl(enabled: bool) -> Result<()> {
    let path = launch_agent_path();
    if !enabled {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| crate::tr!("Failed to disable start at login"))
            }
        }
        return Ok(());
    }

    let exe = std::env::current_exe().context("current executable")?;
    let exe = exe.to_string_lossy();
    let data = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
             <key>Label</key><string>{APP_ID}</string>\n\
             <key>ProgramArguments</key><array><string>{exe}</string><string>--hidden</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
         </dict>\n\
         </plist>\n"
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| crate::tr!("Failed to create the autostart folder"))?;
    }
    fs::write(&path, data).with_context(|| crate::tr!("Failed to enable start at login"))?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn is_enabled_impl() -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn set_enabled_impl(_enabled: bool) -> Result<()> {
    anyhow::bail!(crate::tr!(
        "Start at login is not supported on this platform."
    ))
}
