mod config;
mod i18n;
mod panel;
mod secrets;
mod tailscale;
mod window;

use gtk4::prelude::*;

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
