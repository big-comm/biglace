mod autostart;
mod config;
mod i18n;
mod panel;
mod secrets;
mod tailscale;
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
pub const APP_VERSION: &str = "1.11.1";

/// Date-based build stamp (`YY.MM.DD-HHMM`) — the scheme the GitHub release
/// tags use. The PKGBUILD exports it as `BIGLACE_VERSION`; the update check
/// compares it with the latest release tag. `None` for a plain `cargo build`,
/// which the update check treats as a development build.
pub const BUILD_VERSION: Option<&str> = option_env!("BIGLACE_VERSION");

/// Serializes tests that mutate process-wide environment variables (XDG
/// dirs, D-Bus address): cargo runs tests on parallel threads, and one test
/// changing `XDG_CONFIG_HOME` under another makes both flaky.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn main() {
    i18n::init();

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

    let app = libadwaita::Application::builder()
        .application_id("org.communitybig.biglace")
        .build();

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

    std::process::exit(app.run_with_args(&["biglace"]).into());
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
