//! GNOME's clipboard, through the BigLace shell extension
//! (`usr/share/gnome-shell/extensions/biglace@biglinux.com.br`): GNOME gives
//! no other program the clipboard without focus. The extension calls
//! `Attach`, then `Copied` for each copy; what our phones send goes back to
//! it alone, as the signal `Set`.

use std::cell::RefCell;

use gtk4::{gio, glib, prelude::*};

use super::{copied, Content, PNG, UTF8_TEXT};

pub const EXTENSION_UUID: &str = "biglace@biglinux.com.br";
const PATH: &str = "/org/communitybig/biglace/Clipboard";
const INTERFACE: &str = "org.communitybig.biglace.Clipboard";
const XML: &str = r#"<node>
  <interface name="org.communitybig.biglace.Clipboard">
    <method name="Attach"/>
    <method name="Copied">
      <arg type="s" name="mime" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <signal name="Set">
      <arg type="s" name="mime"/>
      <arg type="ay" name="data"/>
    </signal>
  </interface>
</node>"#;

/// The extension that attached: its connection, its bus name, and the watch that forgets it when it goes.
struct Attached {
    conn: gio::DBusConnection,
    name: String,
    /// Ends the name watch (gio's watch id type can't be named outside gio).
    unwatch: Box<dyn FnOnce()>,
}

thread_local! {
    static ATTACHED: RefCell<Option<Attached>> = const { RefCell::new(None) };
}

fn forget() {
    if let Some(a) = ATTACHED.with(|a| a.borrow_mut().take()) {
        (a.unwatch)();
    }
}

fn attach(conn: &gio::DBusConnection, name: &str) {
    forget();
    let watch = gio::bus_watch_name_on_connection(conn, name, gio::BusNameWatcherFlags::NONE, |_, _, _| {}, |_, _| forget());
    let unwatch = Box::new(move || gio::bus_unwatch_name(watch));
    ATTACHED.with(|a| *a.borrow_mut() = Some(Attached { conn: conn.clone(), name: name.to_string(), unwatch }));
}

fn is_attached(name: &str) -> bool {
    ATTACHED.with(|a| a.borrow().as_ref().is_some_and(|a| a.name == name))
}

/// Offer the interface on the app's session bus connection (main thread, once).
pub fn export(conn: &gio::DBusConnection) -> Result<(), glib::Error> {
    let node = gio::DBusNodeInfo::for_xml(XML)?;
    let Some(interface) = node.lookup_interface(INTERFACE) else {
        return Err(glib::Error::new(gio::IOErrorEnum::Failed, "interface missing from its XML"));
    };
    conn.register_object(PATH, &interface)
        .method_call(|conn, sender, _, _, method, params, invocation| {
            let Some(sender) = sender else { return };
            match method {
                "Attach" => attach(&conn, sender),
                "Copied" if is_attached(sender) => {
                    if let Some((mime, data)) = params.get::<(String, Vec<u8>)>() {
                        received(&mime, data);
                    }
                }
                _ => {}
            }
            invocation.return_value(None);
        })
        .build()?;
    Ok(())
}

fn received(mime: &str, data: Vec<u8>) {
    if mime == PNG {
        copied(Content::Png(data));
    } else if let Ok(t) = String::from_utf8(data) {
        copied(Content::Text(t));
    }
}

/// Have the extension put `c` on GNOME's clipboard; false when no extension is attached.
pub fn set(c: &Content) -> bool {
    ATTACHED.with(|a| {
        let a = a.borrow();
        let Some(a) = a.as_ref() else { return false };
        let (mime, data) = match c {
            Content::Text(t) => (UTF8_TEXT, t.as_bytes().to_vec()),
            Content::Png(p) => (PNG, p.clone()),
        };
        a.conn.emit_signal(Some(&a.name), PATH, INTERFACE, "Set", Some(&(mime, data).to_variant())).is_ok()
    })
}

/// Put the extension in GNOME's list of extensions to run, as its package
/// can't for each person: true when done (it starts now, or at the next login
/// for a shell that started before the extension was installed). False where
/// GNOME Shell or the extension isn't installed.
pub fn turn_on() -> bool {
    let known = gio::SettingsSchemaSource::default().and_then(|s| s.lookup("org.gnome.shell", true)).is_some();
    let installed = std::iter::once(glib::user_data_dir())
        .chain(glib::system_data_dirs())
        .any(|d| d.join("gnome-shell/extensions").join(EXTENSION_UUID).join("metadata.json").is_file());
    if !known || !installed {
        return false;
    }
    let shell = gio::Settings::new("org.gnome.shell");
    let on = shell.strv("enabled-extensions");
    if !on.iter().any(|u| u == EXTENSION_UUID) {
        let mut list: Vec<&str> = on.iter().map(glib::GStringPtr::as_str).collect();
        list.push(EXTENSION_UUID);
        if shell.set_strv("enabled-extensions", list).is_err() {
            return false;
        }
    }
    let off = shell.strv("disabled-extensions");
    if off.iter().any(|u| u == EXTENSION_UUID) {
        let list: Vec<&str> = off.iter().map(glib::GStringPtr::as_str).filter(|u| *u != EXTENSION_UUID).collect();
        if shell.set_strv("disabled-extensions", list).is_err() {
            return false;
        }
    }
    true
}

/// Whether GNOME Shell runs and has the extension, and whether it is on; `None` outside GNOME.
pub async fn extension_on() -> Option<bool> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let extension = conn
        .call_future(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            "org.gnome.Shell.Extensions",
            "GetExtensionInfo",
            Some(&(EXTENSION_UUID,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
        .ok()?;
    // An unknown extension comes back as an empty dictionary.
    let fields = glib::VariantDict::new(Some(&extension.child_value(0)));
    let state = fields.lookup_value("state", None)?.get::<f64>()?;
    // ExtensionState.ACTIVE (GNOME 46+), ENABLED before it.
    Some(state == 1.0)
}

/// Ask GNOME Shell to turn the extension on.
pub async fn enable_extension() -> Result<(), glib::Error> {
    let conn = gio::bus_get_future(gio::BusType::Session).await?;
    conn.call_future(
        Some("org.gnome.Shell"),
        "/org/gnome/Shell",
        "org.gnome.Shell.Extensions",
        "EnableExtension",
        Some(&(EXTENSION_UUID,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
    )
    .await?;
    Ok(())
}
