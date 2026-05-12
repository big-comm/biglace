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
pub static MALLOC_CONF: &[u8] =
    b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

/// Single source of truth for the app version. Wired to `Cargo.toml` via
/// `CARGO_PKG_VERSION` so a `cargo set-version` (or manual bump) flows
/// through to the About dialog and the GitHub releases self-update check
/// without an extra place to keep in sync.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    i18n::init();

    let app = libadwaita::Application::builder()
        .application_id("org.communitybig.biglace")
        .build();

    app.connect_activate(|app| {
        window::build(app);
    });

    std::process::exit(app.run().into());
}
