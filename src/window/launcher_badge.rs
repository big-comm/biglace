//! The unread count on BigLace's icon in the task manager or dock (Plasma,
//! Dash to Dock, Plank...): they draw it from the Unity LauncherEntry signal.

use gtk4::{glib, prelude::*};

const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const PATH: &str = "/org/communitybig/biglace/LauncherEntry";
/// The launcher it's for: our desktop file.
const APP_URI: &str = "application://org.communitybig.biglace.desktop";

/// Show `count` on our launcher (hidden at 0).
pub fn show_launcher_badge(app: &gtk4::Application, count: u32) {
    let Some(bus) = app.dbus_connection() else { return };
    let props = glib::VariantDict::new(None);
    props.insert("count", i64::from(count));
    props.insert("count-visible", count > 0);
    let args = glib::Variant::tuple_from_iter([APP_URI.to_variant(), props.end()]);
    if let Err(e) = bus.emit_signal(None, PATH, INTERFACE, "Update", Some(&args)) {
        crate::tailscale::dbg(&format!("launcher entry: {e}"));
    }
}
