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
    let data = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=Connect your machine to the mesh network\n\
         Exec={exec}\n\
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
        let exe = exe.to_string_lossy().to_string();
        let st = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                APP_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &exe,
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
             <key>ProgramArguments</key><array><string>{exe}</string></array>\n\
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
