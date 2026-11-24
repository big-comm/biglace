//! BigLace: a private network of your devices and people, on Tailscale.
//!
//! - `agent`: the HTTP service through which one BigLace talks to another.
//! - `access`: who may use this computer remotely, and what happened.
//! - `autostart`: starting BigLace at login.
//! - `power`, `wake`: restart or shut down a helped computer; turn one on (Wake-on-LAN).
//! - `avatars`: profile photos, served by each person's own computer.
//! - `config`: this device's settings and identity on the network.
//! - `device_key`: the key proving to the panel that a request comes from this device.
//! - `i18n`: gettext translation.
//! - `lan`: the local network without a server: pairing by code, then TLS between paired devices.
//! - `invite`, `panel`: joining a network and talking to the BigScale panel.
//! - `launchers`: the terminal, file manager and log viewer this app spawns.
//! - `ssh_watch`: people coming into this computer over SSH (terminal or files).
//! - `link`: our phones' connection (battery, calls, links, notifications).
//! - `clipboard`: the clipboard shared with our phones.
//! - `remote`: remote desktop between devices.
//! - `chat`, `room`: messages and voice rooms between people.
//! - `call`: one-to-one voice and video calls (ringing, answering, the history).
//! - `transfer`: sending files to a device, accepted or declined there.
//! - `listeners`: events every part of the window hears.
//! - `secrets`: the OS credential store.
//! - `tailscale`: the tailscaled daemon and its state.
//! - `tray`, `window`: the tray indicator and the main window.

mod agent;
mod access;
mod autostart;
mod power;
mod wake;
mod avatars;
mod call;
mod clipboard;
mod listeners;
mod config;
mod device_key;
mod i18n;
mod invite;
#[cfg(target_os = "linux")]
mod lan;
mod launchers;
#[cfg(target_os = "linux")]
mod ssh_watch;
mod link;
mod panel;
mod remote;
mod chat;
mod room;
mod secrets;
mod tailscale;
mod transfer;
// Cross-platform façade: ksni (StatusNotifierItem / D-Bus) on Linux,
// Shell_NotifyIcon on Windows, no-op stub elsewhere. The submodule that
// actually compiles is selected inside `tray.rs` via `#[cfg(target_os = …)]`,
// so this `mod tray;` works for every supported target.
mod tray;
mod window;

use gtk4::prelude::*;

// jemalloc as the global allocator on Linux/macOS. glibc's malloc keeps
// freed pages indefinitely, so a long-idle GTK app drifts upward in RSS
// even when it's doing nothing. jemalloc's background thread runs madvise
// over decayed dirty pages, returning them to the kernel.
//
// Gated behind the `jemalloc` cargo feature (default on) so packagers whose
// linker can't resolve tikv-jemalloc-sys's prefixed `_rjem_*` symbols can
// fall back to the system allocator with `--no-default-features`.
#[cfg(all(feature = "jemalloc", any(target_os = "linux", target_os = "macos")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Tune jemalloc for an interactive desktop app: a 1 s dirty-page decay
// (vs the 10 s default) means free pages are returned to the kernel fast
// after bursts (e.g. tailscale status parsing) instead of lingering in
// the process's working set. `background_thread:true` activates the
// builtin reclaimer so we don't have to drive `mallctl("epoch")` ourselves.
#[cfg(all(feature = "jemalloc", any(target_os = "linux", target_os = "macos")))]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

/// The app version shown in About.
///
/// Must stay a plain string literal: gitrepo bumps it on every commit it
/// creates (feat → minor, fix/perf/docs/… → patch, breaking → major) by
/// rewriting this assignment. Reading it from `CARGO_PKG_VERSION` hid it from
/// gitrepo, which is how the version sat at 1.0.0 through dozens of releases.
pub const APP_VERSION: &str = "1.74.19";
pub const APP_ID: &str = "org.communitybig.biglace";

/// Date-based build stamp (`YY.MM.DD-HHMM`) — the scheme the GitHub release
/// tags use. The PKGBUILD exports it as `BIGLACE_VERSION`; the update check
/// compares it with the latest release tag. `None` for a plain `cargo build`,
/// which the update check treats as a development build.
pub const BUILD_VERSION: Option<&str> = option_env!("BIGLACE_VERSION");

/// In a virtual machine GTK's Vulkan renderer ends up on Mesa's software
/// Vulkan (lavapipe), which crashed opening the screen viewer there: use
/// GTK's GL renderer instead, unless the person chose one (GSK_RENDERER).
fn avoid_software_vulkan() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("GSK_RENDERER").is_some() {
            return;
        }
        let vm = std::process::Command::new("systemd-detect-virt")
            .args(["--vm", "--quiet"])
            .status()
            .is_ok_and(|s| s.success());
        if vm {
            // Before GTK starts (no other threads yet).
            std::env::set_var("GSK_RENDERER", "ngl");
        }
    }
}

/// Serializes tests that mutate process-wide environment variables (XDG
/// dirs, D-Bus address): cargo runs tests on parallel threads, and one test
/// changing `XDG_CONFIG_HOME` under another makes both flaky.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// SIGTERM, SIGINT and SIGHUP quit the app the normal way (shutdown runs:
/// keys a remote viewer holds are let go, the room is left).
#[cfg(unix)]
fn quit_on_signals(app: &libadwaita::Application) {
    use gtk4::prelude::*;
    thread_local! {
        static APP: std::cell::RefCell<Option<gtk4::glib::WeakRef<libadwaita::Application>>> = const { std::cell::RefCell::new(None) };
    }
    // GLib's own (async-signal-safe) signal sources; the Rust binding left
    // glib 0.22, the C function is in the library we already use.
    extern "C" {
        fn g_unix_signal_add(
            signum: std::ffi::c_int,
            handler: extern "C" fn(*mut std::ffi::c_void) -> std::ffi::c_int,
            data: *mut std::ffi::c_void,
        ) -> std::ffi::c_uint;
    }
    extern "C" fn on_signal(_: *mut std::ffi::c_void) -> std::ffi::c_int {
        APP.with(|a| {
            if let Some(app) = a.borrow().as_ref().and_then(|w| w.upgrade()) {
                app.quit();
            }
        });
        1 // keep the source
    }
    APP.with(|a| *a.borrow_mut() = Some(app.downgrade()));
    for signum in [1, 2, 15] {
        // SAFETY: a plain C callback with no data, run on the main loop.
        unsafe {
            g_unix_signal_add(signum, on_signal, std::ptr::null_mut());
        }
    }
}

fn main() {
    i18n::init();
    avoid_software_vulkan();

    // Autostart launch mode. The desktop-session autostart entry (Linux
    // .desktop, Windows "Run" key, macOS LaunchAgent) appends `--hidden`, so a
    // login-triggered launch comes up straight into the system tray without
    // ever flashing the window. A manual launch from the app menu omits the
    // flag and opens normally.
    //
    // We parse argv ourselves and hand GApplication a clean argv via
    // `run_with_args`, so GTK's own option parser never sees `--hidden` and
    // aborts with "Unknown option".
    // args_os, not args: the latter panics mid-iteration on any non-UTF-8
    // argument, which would crash biglace before a single window appears if a
    // launcher passed odd bytes. OsString compares against &str directly.
    let start_hidden = std::env::args_os().skip(1).any(|a| a == "--hidden");

    // Migrate a stale autostart entry written by a pre-`--hidden` build, so an
    // in-place upgrade actually starts in the tray at the next login instead of
    // re-opening the window from an old entry that lacks the flag.
    autostart::migrate_if_enabled();

    // Back from "Restart and reconnect": lock the screen before anything else.
    power::after_start();

    // An invite link (`biglace://join?…`) clicked in a browser or chat app
    // arrives as an argument. It's the only argument handed to GApplication:
    // with HANDLES_OPEN it's forwarded to the already-running instance, if
    // any, and delivered to `open` below.
    let join_link = std::env::args_os()
        .skip(1)
        .filter_map(|a| a.into_string().ok())
        .find(|a| a.starts_with("biglace://"));

    // On X11 the window's WM_CLASS comes from the program name; panels like
    // XFCE's match it against StartupWMClass to find the .desktop entry (and
    // so the app icon). With the binary name ("biglace") they don't, and fall
    // back to a generic icon.
    gtk4::glib::set_prgname(Some(APP_ID));
    let app = libadwaita::Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    // Quitting leaves the voice room (the others see it at once instead of
    // after the panel's 30 s timeout).
    app.connect_shutdown(|_| {
        // Keys a remote viewer still holds are let go before anything else.
        remote::host::release_all_inputs();
        room::leave();
    });
    // Asked to stop (logout, systemd, kill): quit the normal way, so the above runs.
    #[cfg(unix)]
    quit_on_signals(&app);

    // The window icon too (X11 _NET_WM_ICON): docks that draw the window's
    // own icon (XFCE's docklike, via libwnck) show the app icon, not a
    // generic one.
    app.connect_startup(|_| gtk4::Window::set_default_icon_name(APP_ID));

    app.connect_activate(move |app| {
        // Single-instance: launching biglace again (or the desktop entry being
        // triggered a second time) re-emits `activate` on the already-running
        // process. Don't build a second window + tray + worker set — just bring
        // the existing window to the front. This also means clicking the app
        // icon while we're hidden in the tray surfaces the window as expected.
        if let Some(win) = app.windows().first() {
            win.present();
            return;
        }
        window::build(app, start_hidden);
    });

    app.connect_open(move |app, files, _hint| {
        let win = match app.windows().first() {
            Some(w) => w.clone(),
            None => {
                // Opened by a link: show the window even if autostart asked
                // for the tray, since the user is about to type in it.
                window::build(app, false);
                match app.windows().first() {
                    Some(w) => w.clone(),
                    None => return,
                }
            }
        };
        win.present();
        for file in files {
            let uri = file.uri();
            if uri.starts_with("biglace://join") {
                let _ = gtk4::prelude::WidgetExt::activate_action(
                    &win,
                    "win.open-invite-link",
                    Some(&gtk4::glib::Variant::from(uri.as_str())),
                );
            }
        }
    });

    let mut argv = vec!["biglace".to_string()];
    argv.extend(join_link);
    std::process::exit(app.run_with_args(&argv).into());
}

#[cfg(test)]
mod tests {
    /// gitrepo can only bump a literal `MAJOR.MINOR.PATCH`; anything else
    /// (a `concat!`, an `env!`, a suffix) silently freezes the version again.
    #[test]
    fn app_version_is_a_plain_semantic_version() {
        let parts: Vec<&str> = super::APP_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "APP_VERSION must be MAJOR.MINOR.PATCH");
        assert!(parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())));
    }
}
