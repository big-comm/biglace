mod config;
mod i18n;
mod panel;
mod secrets;
mod tailscale;
mod tray;
mod window;

use gtk4::prelude::*;

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
