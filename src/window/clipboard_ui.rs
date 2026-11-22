//! The clipboard shared with our phones (see `crate::clipboard`) in the
//! window: following it from startup, its setting, and on GNOME the shell
//! extension it needs.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{glib, prelude::*};

use crate::clipboard::{self, gnome};
use crate::config;

/// Start following the clipboard and wire its setting (once, at startup).
pub fn wire(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, sidebar: &super::sidebar::Sidebar, cfg: &Rc<RefCell<config::Config>>) {
    let on = !cfg.borrow().no_clipboard_sharing;
    clipboard::set_enabled(on);
    // Nobody looks for an extension to turn on: we do it once, then leave it to the person.
    if on && !cfg.borrow().gnome_extension_turned_on && gnome::turn_on() {
        let mut c = cfg.borrow_mut();
        c.gnome_extension_turned_on = true;
        config::save_or_warn(&c);
    }
    clipboard::watch(&WidgetExt::display(win));
    if let Some(conn) = win.application().and_then(|a| a.dbus_connection()) {
        if let Err(e) = gnome::export(&conn) {
            crate::tailscale::dbg(&format!("clipboard: no D-Bus interface for the GNOME extension: {e}"));
        }
    }
    sidebar.switch_clipboard.set_active(on);
    let row = sidebar.gnome_clipboard_row.clone();
    let cfg = cfg.clone();
    sidebar.switch_clipboard.connect_state_set(move |_, active| {
        let mut c = cfg.borrow_mut();
        c.no_clipboard_sharing = !active;
        config::save_or_warn(&c);
        clipboard::set_enabled(active);
        offer_extension(&row, active);
        glib::Propagation::Proceed
    });
    offer_extension(&sidebar.gnome_clipboard_row, on);
    let (row, toast) = (sidebar.gnome_clipboard_row.clone(), toast.clone());
    sidebar.btn_gnome_extension.connect_clicked(move |_| {
        let (row, toast) = (row.clone(), toast.clone());
        glib::spawn_future_local(async move {
            match gnome::enable_extension().await {
                Ok(()) => row.set_visible(false),
                Err(e) => super::show_toast(&toast, &e.to_string(), 8),
            }
        });
    });
}

/// Show the GNOME extension's row while sharing is on and GNOME has the extension off.
fn offer_extension(row: &libadwaita::ActionRow, sharing: bool) {
    if !sharing {
        row.set_visible(false);
        return;
    }
    let row = row.clone();
    glib::spawn_future_local(async move {
        row.set_visible(gnome::extension_on().await == Some(false));
    });
}
