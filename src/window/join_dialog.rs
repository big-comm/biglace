//! "Join with an invite": the newcomer's way onto the network. Paste (or
//! open) the invite link, type your name, done — the panel creates the
//! account and BigLace connects right away.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::config;
use crate::invite::{self, JoinError};
use crate::tr;
use crate::trf;

use super::dialogs::{sanitize_hostname, sanitize_hostname_while_typing, sanitize_server_url, validate_hostname};
use super::sidebar::Sidebar;

/// Sentence explaining a failed join, for the dialog's error label.
fn explain(err: &JoinError) -> String {
    match err {
        JoinError::NameTaken(Some(s)) => trf!(
            "That name is already in use on this network. How about \"{name}\"?",
            "name" => s
        ),
        JoinError::NameTaken(None) => tr!("That name is already in use on this network. Try another one."),
        JoinError::InvalidName => tr!("Type your name using at least two letters or digits."),
        JoinError::InviteUnusable(reason) => match reason.as_str() {
            "expired" => tr!("This invite has expired. Ask for a new one."),
            "used" => tr!("This invite was already used. Ask for a new one."),
            "revoked" => tr!("This invite was cancelled. Ask for a new one."),
            _ => tr!("This invite is not valid. Check the link or ask for a new one."),
        },
        JoinError::TooManyAttempts => tr!("Too many attempts. Wait a minute and try again."),
        JoinError::Other(detail) if detail == "no_invites" => {
            tr!("This server doesn't support invites yet. Sign in with a panel account instead.")
        }
        JoinError::Other(detail) => trf!("Could not reach the panel: {error}", "error" => detail),
    }
}

pub fn show(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
    link: Option<String>,
) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Join with an invite"))
        .default_width(480)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&libadwaita::HeaderBar::new());

    let page = libadwaita::PreferencesPage::new();
    let grp = libadwaita::PreferencesGroup::builder()
        .description(tr!(
            "Paste the invite link you received and type your name. \
             Your account is created and this device joins the network."
        ))
        .build();

    let er_link = libadwaita::EntryRow::builder()
        .title(tr!("Invite link"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    let er_name = libadwaita::EntryRow::builder().title(tr!("Your name")).build();
    er_name.set_tooltip_text(Some(&tr!(
        "How others will see you on the network, e.g. 'Maria' or 'Office PC'."
    )));
    let er_device = libadwaita::EntryRow::builder()
        .title(tr!("Device name on the network"))
        .build();
    er_device.set_tooltip_text(Some(&tr!(
        "The name this device shows to other peers. Lowercase letters, digits \
         and hyphens. Example: 'maria-laptop'."
    )));
    // Which network the link leads to, so a link from a stranger can't
    // quietly enroll this device somewhere else.
    let row_network = libadwaita::ActionRow::builder()
        .title(tr!("Network"))
        .css_classes(["property"])
        .visible(false)
        .build();
    {
        let row_network = row_network.clone();
        er_link.connect_changed(move |e| match invite::parse(&e.text()) {
            Ok(inv) => {
                row_network.set_subtitle(inv.host());
                row_network.set_visible(true);
            }
            Err(_) => row_network.set_visible(false),
        });
    }
    if let Some(l) = &link {
        er_link.set_text(l);
    }

    // The device name follows the person's name until it's edited by hand.
    let device_touched = Rc::new(Cell::new(false));
    let syncing = Rc::new(Cell::new(false));
    {
        let er_device = er_device.clone();
        let (touched, syncing) = (device_touched.clone(), syncing.clone());
        er_name.connect_changed(move |e| {
            if !touched.get() {
                syncing.set(true);
                er_device.set_text(&sanitize_hostname(&e.text()));
                syncing.set(false);
            }
        });
    }
    {
        let (touched, syncing) = (device_touched.clone(), syncing.clone());
        er_device.connect_changed(move |e| {
            if syncing.get() {
                return;
            }
            touched.set(true);
            let clean = sanitize_hostname_while_typing(&e.text());
            if clean != e.text().as_str() {
                e.set_text(&clean);
                e.set_position(-1);
            }
        });
    }

    grp.add(&er_link);
    grp.add(&row_network);
    grp.add(&er_name);
    grp.add(&er_device);
    page.add(&grp);
    outer.append(&page);

    let lbl_err = gtk4::Label::new(None);
    lbl_err.add_css_class("error");
    lbl_err.set_wrap(true);
    lbl_err.set_max_width_chars(60);
    lbl_err.set_margin_start(16);
    lbl_err.set_margin_end(16);
    lbl_err.set_selectable(true);
    outer.append(&lbl_err);

    let btn_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk4::Align::End);
    btn_box.set_margin_top(8);
    btn_box.set_margin_bottom(16);
    btn_box.set_margin_start(16);
    btn_box.set_margin_end(16);
    let btn_cancel = gtk4::Button::with_label(&tr!("Cancel"));
    let ok_idle = gtk4::Label::new(Some(&tr!("Join")));
    let ok_busy = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let spinner = gtk4::Spinner::new();
    ok_busy.append(&spinner);
    ok_busy.append(&gtk4::Label::new(Some(&tr!("Joining…"))));
    let btn_ok = gtk4::Button::builder()
        .css_classes(["suggested-action"])
        .child(&ok_idle)
        .build();
    btn_box.append(&btn_cancel);
    btn_box.append(&btn_ok);
    outer.append(&btn_box);
    dlg.set_content(Some(&outer));

    // Don't let the window close mid-request (same reason as panel login).
    let busy = Rc::new(Cell::new(false));
    {
        let busy = busy.clone();
        dlg.connect_close_request(move |_| {
            if busy.get() {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    {
        let dlg = dlg.clone();
        btn_cancel.connect_clicked(move |_| dlg.close());
    }

    let set_busy = {
        let (btn_ok, btn_cancel, ok_idle, ok_busy, spinner, busy) = (
            btn_ok.clone(),
            btn_cancel.clone(),
            ok_idle.clone(),
            ok_busy.clone(),
            spinner.clone(),
            busy.clone(),
        );
        move |on: bool| {
            busy.set(on);
            btn_ok.set_sensitive(!on);
            btn_cancel.set_sensitive(!on);
            if on {
                btn_ok.set_child(Some(&ok_busy));
                spinner.start();
            } else {
                spinner.stop();
                btn_ok.set_child(Some(&ok_idle));
            }
        }
    };

    {
        let (dlg, win, overlay, cfg, sidebar) =
            (dlg.clone(), parent.clone(), overlay.clone(), cfg.clone(), sidebar.clone());
        let (er_link, er_name, er_device, lbl_err) =
            (er_link.clone(), er_name.clone(), er_device.clone(), lbl_err.clone());
        btn_ok.connect_clicked(move |_| {
            lbl_err.set_text("");
            let invite = match invite::parse(&er_link.text()) {
                Ok(i) => i,
                Err(_) => {
                    lbl_err.set_text(&tr!(
                        "That doesn't look like an invite link. It starts with https:// and contains /join#."
                    ));
                    return;
                }
            };
            let name = er_name.text().trim().to_string();
            if name.is_empty() {
                lbl_err.set_text(&tr!("Type your name."));
                return;
            }
            let device = sanitize_hostname(&er_device.text());
            if let Err(msg) = validate_hostname(&device) {
                lbl_err.set_text(&msg);
                return;
            }
            er_device.set_text(&device);

            set_busy(true);
            let set_busy = set_busy.clone();
            let (dlg, win, overlay, cfg, sidebar, er_name, lbl_err) = (
                dlg.clone(),
                win.clone(),
                overlay.clone(),
                cfg.clone(),
                sidebar.clone(),
                er_name.clone(),
                lbl_err.clone(),
            );
            let invite_w = invite.clone();
            super::run_in_background(
                move || invite::redeem(&invite_w, &name),
                move |result| {
                    set_busy(false);
                    let joined = match result {
                        Ok(j) => j,
                        Err(e) => {
                            if let JoinError::NameTaken(Some(s)) = &e {
                                er_name.set_text(s);
                            }
                            lbl_err.set_text(&explain(&e));
                            return;
                        }
                    };
                    let server_url = sanitize_server_url(&joined.server_url, &invite.panel);
                    sidebar.entry_server.set_text(&server_url);
                    sidebar.entry_key.set_text(&joined.authkey);
                    sidebar.entry_host.set_text(&device);
                    sidebar.expander_manual.set_expanded(false);
                    let saved = {
                        let mut c = cfg.borrow_mut();
                        c.panel_url = invite.panel.clone();
                        c.panel_username.clear();
                        c.server_url = server_url;
                        c.authkey = joined.authkey.clone();
                        c.hostname = device.clone();
                        c.enrolled = true;
                        config::save_with_authkey(&c)
                    };
                    match saved {
                        Ok(None) => {
                            let text = match &joined.group {
                                Some(g) => trf!(
                                    "Welcome, {name}! You joined the group {group}. Connecting…",
                                    "name" => joined.user,
                                    "group" => g
                                ),
                                None => trf!("Welcome, {name}! Connecting…", "name" => joined.user),
                            };
                            super::show_toast(&overlay, &text, 5)
                        }
                        Ok(Some(reason)) => super::show_toast(&overlay, &super::keyring_warning(&reason), 8),
                        Err(e) => {
                            // Nothing was saved: don't try to connect with
                            // a config the next start won't have.
                            lbl_err.set_text(&trf!("Error: {error}", "error" => e));
                            return;
                        }
                    }
                    dlg.close();
                    gtk4::prelude::WidgetExt::activate_action(&win, "win.refresh", None).ok();
                    // A newcomer shouldn't need to find the Connect button.
                    sidebar.btn_connect.emit_clicked();
                },
            );
        });
    }

    dlg.present();
    if link.is_some() {
        er_name.grab_focus();
    } else {
        er_link.grab_focus();
    }
}
