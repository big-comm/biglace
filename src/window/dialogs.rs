use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::panel::{self, PanelCredentials};
use crate::secrets;
use crate::tailscale;
use crate::tr;
use crate::trf;

use super::sidebar::Sidebar;

/// Outcome of the background rename worker (see `show_rename_device`), so the
/// online probe + `tailscale set --hostname` both run off the GTK thread.
enum RenameResult {
    /// Tunnel down — config saved, nothing pushed to the daemon.
    SavedOffline,
    /// `tailscale set --hostname` succeeded.
    Renamed,
    /// `tailscale set --hostname` failed with this message.
    Failed(String),
}

pub fn show_about(parent: &libadwaita::ApplicationWindow) {
    let about = libadwaita::AboutWindow::builder()
        .transient_for(parent)
        .modal(true)
        .application_name("BigLace")
        .application_icon("org.communitybig.biglace")
        .developer_name("BigCommunity")
        .version(crate::APP_VERSION)
        .website("https://github.com/big-comm/biglace")
        .issue_url("https://github.com/big-comm/biglace/issues")
        .copyright("© 2026 BigCommunity")
        .license_type(gtk4::License::MitX11)
        .comments(tr!("Mesh VPN client for BigScale/Headscale."))
        .build();
    about.present();
}

pub fn show_set_operator(parent: &libadwaita::ApplicationWindow) {
    // `set_operator_current_user()` runs `pkexec tailscale set --operator=…`,
    // which blocks until the user answers the polkit prompt (up to minutes).
    // Run it on a worker thread and show the result via a main-loop poller so
    // the whole window doesn't freeze while the prompt is up.
    let slot: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let slot_t = slot.clone();
    std::thread::spawn(move || {
        let r = tailscale::set_operator_current_user().map_err(|e| e.to_string());
        if let Ok(mut g) = slot_t.lock() {
            *g = Some(r);
        }
    });

    let parent_w = parent.clone();
    glib::timeout_add_local(Duration::from_millis(200), move || {
        let Some(result) = slot.lock().ok().and_then(|mut g| g.take()) else {
            return glib::ControlFlow::Continue;
        };
        let body = match &result {
            Ok(()) => tr!("Done. You will no longer need a password to connect or disconnect."),
            Err(e) => trf!("Failed: {error}", "error" => e),
        };
        let dlg = libadwaita::MessageDialog::new(
            Some(&parent_w),
            Some(&tr!("Tailscale operator")),
            Some(&body),
        );
        dlg.add_response("ok", &tr!("OK"));
        dlg.present();
        glib::ControlFlow::Break
    });
}

/// Confirm with the user, then tear down the current session.
///
/// We deliberately do NOT auto-open the panel-login dialog after sign-out:
/// users who joined via a manually-pasted pre-auth key don't have panel
/// credentials, and the sidebar reverts to the "not signed in" layout where
/// they can pick freely between panel login (menu) and the manual-key
/// expander.
pub fn confirm_sign_out(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    if !cfg.borrow().enrolled && cfg.borrow().authkey.is_empty() {
        return;
    }

    let dlg = libadwaita::MessageDialog::new(
        Some(parent),
        Some(&tr!("Sign out of current account?")),
        Some(&tr!(
            "This will disconnect from the network, sign this device out of \
             the current control server, and forget the saved password. You'll \
             then be able to sign in with a different account."
        )),
    );
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("ok", &tr!("Sign out"));
    dlg.set_response_appearance("ok", libadwaita::ResponseAppearance::Destructive);
    dlg.set_default_response(Some("cancel"));
    dlg.set_close_response("cancel");

    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w = cfg.clone();
    let sidebar_w = sidebar.clone();
    dlg.connect_response(None, move |dlg, resp| {
        if resp != "ok" {
            dlg.close();
            return;
        }
        dlg.close();
        sign_out(&parent_w, &overlay_w, &cfg_w, &sidebar_w);
    });
    dlg.present();
}

/// Tear down the current session, leaving the UI in the "not signed in"
/// state. Runs `tailscale logout` on a background thread so the UI stays
/// responsive — the logout call itself can block on network when the control
/// server is unreachable.
fn sign_out(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    _sidebar: &Sidebar,
) {
    let (server_url, panel_url, panel_username) = {
        let c = cfg.borrow();
        (
            c.server_url.clone(),
            c.panel_url.clone(),
            c.panel_username.clone(),
        )
    };
    let slot: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let slot_t = slot.clone();
    std::thread::spawn(move || {
        let result = tailscale::logout().map_err(|e| e.to_string());
        if let Ok(mut guard) = slot_t.lock() {
            *guard = Some(result);
        }
    });

    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w = cfg.clone();
    glib::timeout_add_local(Duration::from_millis(200), move || {
        let Some(result) = slot.lock().ok().and_then(|mut guard| guard.take()) else {
            return glib::ControlFlow::Continue;
        };
        match result {
            Ok(()) => {
                let mut updated = cfg_w.borrow().clone();
                updated.authkey.clear();
                updated.enrolled = false;
                if let Err(error) = config::save(&updated) {
                    overlay_w.add_toast(
                        libadwaita::Toast::builder()
                            .title(
                                glib::markup_escape_text(&trf!("Error: {error}", "error" => error))
                                    .as_str(),
                            )
                            .timeout(6)
                            .build(),
                    );
                    return glib::ControlFlow::Break;
                }
                *cfg_w.borrow_mut() = updated;
                secrets::clear(&panel_url, &panel_username);
                secrets::clear_authkey(&server_url);
                overlay_w.add_toast(
                    libadwaita::Toast::builder()
                        .title(tr!("Signed out."))
                        .timeout(3)
                        .build(),
                );
                gtk4::prelude::WidgetExt::activate_action(&parent_w, "win.refresh", None).ok();
            }
            Err(error) => {
                overlay_w.add_toast(
                    libadwaita::Toast::builder()
                        .title(
                            glib::markup_escape_text(&trf!("Error: {error}", "error" => error))
                                .as_str(),
                        )
                        .timeout(6)
                        .build(),
                );
            }
        }
        glib::ControlFlow::Break
    });
}

pub fn show_panel_login(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Connect via panel account"))
        .default_width(480)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = libadwaita::HeaderBar::new();
    outer.append(&header);

    let pref_page = libadwaita::PreferencesPage::new();
    let grp = libadwaita::PreferencesGroup::builder()
        .title(tr!("Panel credentials"))
        .description(tr!(
            "Sign in with your BigScale panel username and password. \
             A device identity will be created on the network automatically — \
             no manual key needed."
        ))
        .build();

    let er_url = libadwaita::EntryRow::builder()
        .title(tr!("Panel URL"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    er_url.set_tooltip_text(Some(&tr!(
        "Address of your BigScale panel (e.g. panel.example.org). \
         The https:// scheme is added automatically if you omit it."
    )));
    let er_user = libadwaita::EntryRow::builder()
        .title(tr!("Username"))
        .build();
    er_user.set_tooltip_text(Some(&tr!(
        "Your panel username — the one you use to sign in to the web panel."
    )));
    let er_pass = libadwaita::PasswordEntryRow::builder()
        .title(tr!("Password"))
        .build();
    er_pass.set_tooltip_text(Some(&tr!(
        "Your panel password. Stored in the OS keyring (libsecret on Linux, \
         Keychain on macOS, Credential Manager on Windows)."
    )));
    let er_node = libadwaita::EntryRow::builder()
        .title(tr!("Device name on the network"))
        .build();
    er_node.connect_changed(|entry| {
        let clean = sanitize_hostname(&entry.text());
        if clean != entry.text().as_str() {
            entry.set_text(&clean);
            entry.set_position(-1);
        }
    });
    er_node.set_tooltip_text(Some(&tr!(
        "The name this device shows to other peers on the VPN — this is the \
         Tailscale hostname, NOT your computer's name and NOT your user. \
         Lowercase letters, digits and hyphens. Example: 'tales-laptop', \
         'francois', 'office-pc'. Pick something other peers will recognise."
    )));

    {
        let c = cfg.borrow();
        er_url.set_text(&c.panel_url);
        er_user.set_text(&c.panel_username);
        // Pre-fill the device identifier with whatever's saved (or the panel
        // username as a friendly default for first-time setup). This is the
        // tailscale hostname that becomes `<name>.bigscale.net`.
        let default_node = if !c.hostname.is_empty() {
            c.hostname.clone()
        } else if !c.panel_username.is_empty() {
            c.panel_username.clone()
        } else {
            config::os_user()
        };
        er_node.set_text(&default_node);
    }

    // Pre-fill the password from the OS keyring off the main thread: a locked
    // keyring can block on a D-Bus unlock prompt, which would freeze the dialog
    // the moment it opens. Load on a worker and fill via a one-shot poller.
    {
        let (panel_url, username) = {
            let c = cfg.borrow();
            (c.panel_url.clone(), c.panel_username.clone())
        };
        if !panel_url.is_empty() && !username.is_empty() {
            let slot: Arc<Mutex<Option<Option<String>>>> = Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            std::thread::spawn(move || {
                let pw = secrets::load(&panel_url, &username);
                if let Ok(mut g) = slot_t.lock() {
                    *g = Some(pw);
                }
            });
            let er_pass_w = er_pass.clone();
            glib::timeout_add_local(Duration::from_millis(150), move || {
                let Some(pw) = slot.lock().ok().and_then(|mut g| g.take()) else {
                    return glib::ControlFlow::Continue;
                };
                if let Some(pw) = pw {
                    er_pass_w.set_text(&pw);
                }
                glib::ControlFlow::Break
            });
        }
    }

    grp.add(&er_url);
    grp.add(&er_user);
    grp.add(&er_pass);
    grp.add(&er_node);
    pref_page.add(&grp);
    outer.append(&pref_page);

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

    let btn_cancel = gtk4::Button::builder().label(tr!("Cancel")).build();

    // The OK button needs to flip between "Sign in" and "spinner + Connecting…"
    // while the HTTP call is in flight. Build both children up front and just
    // swap the button's child — simpler than juggling visibility flags.
    let ok_label_idle = gtk4::Label::new(Some(&tr!("Sign in")));

    let ok_busy_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    ok_busy_box.set_halign(gtk4::Align::Center);
    let ok_spinner = gtk4::Spinner::new();
    ok_spinner.set_size_request(16, 16);
    ok_busy_box.append(&ok_spinner);
    ok_busy_box.append(&gtk4::Label::new(Some(&tr!("Connecting…"))));

    let btn_ok = gtk4::Button::builder()
        .css_classes(["suggested-action"])
        .child(&ok_label_idle)
        .build();
    btn_box.append(&btn_cancel);
    btn_box.append(&btn_ok);
    outer.append(&btn_box);

    dlg.set_content(Some(&outer));

    // Block the header-bar close button while a sign-in is in flight. Cancel
    // is already disabled during the request, but the window's own close
    // button isn't — closing there would leave the 300ms poller running
    // against a destroyed dialog (its error would land on a dead label) and
    // let the user reopen and fire a second concurrent request.
    let busy = Rc::new(RefCell::new(false));
    {
        let busy_w = busy.clone();
        dlg.connect_close_request(move |_| {
            if *busy_w.borrow() {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    {
        let dlg2 = dlg.clone();
        btn_cancel.connect_clicked(move |_| dlg2.close());
    }

    {
        let dlg2 = dlg.clone();
        let win2 = parent.clone();
        let lbl_err2 = lbl_err.clone();
        let er_url2 = er_url.clone();
        let er_user2 = er_user.clone();
        let er_pass2 = er_pass.clone();
        let er_node2 = er_node.clone();
        let cfg2 = cfg.clone();
        let sidebar2 = sidebar.clone();
        let toast2 = overlay.clone();
        let btn_ok_w = btn_ok.clone();
        let btn_cancel_w = btn_cancel.clone();
        let ok_idle_w = ok_label_idle.clone();
        let ok_busy_w = ok_busy_box.clone();
        let ok_spinner_w = ok_spinner.clone();
        let busy_c = busy.clone();

        btn_ok.connect_clicked(move |_| {
            lbl_err2.set_text("");

            let node_text = sanitize_hostname(&er_node2.text());
            if let Err(message) = validate_hostname(&node_text) {
                lbl_err2.set_text(&message);
                return;
            }
            er_node2.set_text(&node_text);
            let panel_url = match panel::normalize_panel_url(&er_url2.text()) {
                Ok(url) => url,
                Err(e) => {
                    lbl_err2.set_text(&e.to_string());
                    return;
                }
            };
            let creds = PanelCredentials {
                url: panel_url,
                username: er_user2.text().trim().to_string(),
                password: er_pass2.text().to_string(),
                node: node_text.clone(),
                // Tailscale hostname is the BigScale identifier the user
                // chose — what becomes `<hostname>.bigscale.net`. The OS user
                // is propagated separately via a `tag:user-…` ACL tag.
                hostname: node_text,
            };

            if creds.username.is_empty() || creds.password.is_empty() || creds.node.is_empty() {
                lbl_err2.set_text(&tr!("Fill in URL, username and password."));
                return;
            }

            // Visual busy state: disable OK + Cancel, swap label for spinner,
            // and block the window close button (see `busy` above).
            btn_ok_w.set_sensitive(false);
            btn_cancel_w.set_sensitive(false);
            btn_ok_w.set_child(Some(&ok_busy_w));
            ok_spinner_w.start();
            *busy_c.borrow_mut() = true;

            let slot: Arc<Mutex<Option<Result<panel::PreAuthResponse, String>>>> =
                Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            let creds_t = creds.clone();
            std::thread::spawn(move || {
                let r = panel::request_preauth(&creds_t).map_err(|e| e.to_string());
                if let Ok(mut g) = slot_t.lock() {
                    *g = Some(r);
                }
            });

            let dlg3 = dlg2.clone();
            let win3 = win2.clone();
            let lbl_err3 = lbl_err2.clone();
            let cfg3 = cfg2.clone();
            let sidebar3 = sidebar2.clone();
            let toast3 = toast2.clone();
            let btn_ok_w2 = btn_ok_w.clone();
            let btn_cancel_w2 = btn_cancel_w.clone();
            let ok_idle_w2 = ok_idle_w.clone();
            let ok_spinner_w2 = ok_spinner_w.clone();
            let busy_p = busy_c.clone();
            let panel_url = creds.url.clone();
            let username = creds.username.clone();
            let password = creds.password.clone();

            glib::timeout_add_local(Duration::from_millis(300), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(Ok(resp)) => {
                        // Clear busy before close() — the close-request guard
                        // returns Stop while busy and would otherwise block our
                        // own dlg3.close() below.
                        *busy_p.borrow_mut() = false;
                        // The server may return a loopback (127.0.0.1) URL
                        // when its public_url isn't configured — fall back
                        // to whatever the user actually typed in panel URL.
                        let server_url = sanitize_server_url(&resp.server_url, &panel_url);
                        sidebar3.entry_server.set_text(&server_url);
                        sidebar3.entry_key.set_text(&resp.authkey);
                        sidebar3.entry_host.set_text(&creds.hostname);
                        // Collapse the manual-key expander — the key was just
                        // generated for the user, no need to show the field.
                        sidebar3.expander_manual.set_expanded(false);
                        {
                            let mut c = cfg3.borrow_mut();
                            c.panel_url = panel_url.clone();
                            c.panel_username = username.clone();
                            c.server_url = server_url.clone();
                            c.authkey = resp.authkey.clone();
                            c.hostname = creds.hostname.clone();
                            c.enrolled = true;
                            config::save_or_warn(&c);
                        }
                        // Stash the password in the OS-native keyring so the
                        // user doesn't retype it next time they sign in.
                        secrets::save(&panel_url, &username, &password);
                        toast3.add_toast(
                            libadwaita::Toast::builder()
                                .title(tr!("Signed in. Press Connect to join the network."))
                                .timeout(3)
                                .build(),
                        );
                        dlg3.close();
                        // Force the sidebar / status to re-read config now —
                        // otherwise the UI keeps showing the "not signed in"
                        // layout until the next 30s refresh tick.
                        gtk4::prelude::WidgetExt::activate_action(&win3, "win.refresh", None).ok();
                        glib::ControlFlow::Break
                    }
                    Some(Err(e)) => {
                        *busy_p.borrow_mut() = false;
                        lbl_err3.set_text(&trf!("Error: {error}", "error" => e));
                        // Restore the idle button state so the user can retry.
                        ok_spinner_w2.stop();
                        btn_ok_w2.set_child(Some(&ok_idle_w2));
                        btn_ok_w2.set_sensitive(true);
                        btn_cancel_w2.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    dlg.present();
}

/// Prompt the user for a new tailscale hostname and apply it. The dialog
/// validates client-side (lowercase letters, digits, hyphens — same charset
/// tailscale accepts as a DNS leaf) before persisting. When the tunnel is up
/// we push the rename to the running daemon via `tailscale set --hostname=…`
/// in a worker thread so the call doesn't stall the GTK loop; when offline
/// we just save the config and the next `tailscale up` will carry the new
/// hostname through.
pub fn show_rename_device(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
) {
    let current = cfg.borrow().hostname.clone();

    let entry = libadwaita::EntryRow::builder()
        .title(tr!("Device name on the network"))
        .build();
    entry.set_text(&sanitize_hostname(&current));
    let host_icon = gtk4::Image::from_icon_name("computer-symbolic");
    host_icon.set_pixel_size(18);
    entry.add_prefix(&host_icon);

    let group = libadwaita::PreferencesGroup::new();
    group.add(&entry);

    let lbl_err = gtk4::Label::new(None);
    lbl_err.add_css_class("error");
    lbl_err.set_wrap(true);
    lbl_err.set_max_width_chars(50);
    lbl_err.set_visible(false);
    lbl_err.set_xalign(0.0);
    lbl_err.set_margin_top(8);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&group);
    outer.append(&lbl_err);

    let dlg = libadwaita::MessageDialog::new(
        Some(parent),
        Some(&tr!("Rename this device")),
        Some(&tr!(
            "The name this device shows to other peers on the VPN. \
             Lowercase letters, digits and hyphens. Example: tales-laptop."
        )),
    );
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("save", &tr!("Save & apply"));
    dlg.set_response_appearance("save", libadwaita::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("save"));
    dlg.set_close_response("cancel");
    dlg.set_extra_child(Some(&outer));

    // Validate live and gate the Save response on validity, so an invalid name
    // simply can't be submitted. The old code let Save fire on an invalid
    // value, then closed the dialog and only toasted the error — throwing away
    // whatever the user had typed. Now the dialog stays open until it's valid.
    let validate: Rc<dyn Fn(&str)> = {
        let dlg_w = dlg.clone();
        let lbl_w = lbl_err.clone();
        Rc::new(move |text: &str| match validate_hostname(text.trim()) {
            Ok(()) => {
                lbl_w.set_visible(false);
                dlg_w.set_response_enabled("save", true);
            }
            Err(msg) => {
                dlg_w.set_response_enabled("save", false);
                // Empty is "not valid yet", not worth a red banner; only show
                // the message once the user has typed something wrong.
                if text.trim().is_empty() {
                    lbl_w.set_visible(false);
                } else {
                    lbl_w.set_text(&msg);
                    lbl_w.set_visible(true);
                }
            }
        })
    };
    validate(entry.text().as_str());
    {
        let validate2 = validate.clone();
        entry.connect_changed(move |e| {
            let clean = sanitize_hostname(&e.text());
            if clean != e.text().as_str() {
                e.set_text(&clean);
                e.set_position(-1);
            }
            validate2(&clean);
        });
    }

    let entry_w = entry.clone();
    let cfg_w = cfg.clone();
    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    dlg.connect_response(None, move |dlg, resp| {
        if resp != "save" {
            dlg.close();
            return;
        }
        let new_name = entry_w.text().trim().to_string();
        // Save is only enabled when the value validates (see `validate` above),
        // so this is a defensive guard, not the primary check.
        if validate_hostname(&new_name).is_err() {
            dlg.close();
            return;
        }
        dlg.close();

        // Persist first so an interrupted apply still ends up with the right
        // value the next time we connect.
        {
            let mut c = cfg_w.borrow_mut();
            c.hostname = new_name.clone();
            config::save_or_warn(&c);
        }

        // Probe online state AND push the rename on a worker thread: the
        // is_service_active/get_status probe is a subprocess that would freeze
        // the GTK loop on a hung daemon if done here. If the tunnel is up we
        // apply now; otherwise the next `tailscale up` carries the new value.
        let name_t = new_name.clone();
        let slot: Arc<Mutex<Option<RenameResult>>> = Arc::new(Mutex::new(None));
        let slot_t = slot.clone();
        std::thread::spawn(move || {
            let online = tailscale::is_service_active() && tailscale::get_status().online;
            let outcome = if !online {
                RenameResult::SavedOffline
            } else {
                match tailscale::set_hostname(&name_t) {
                    Ok(()) => RenameResult::Renamed,
                    Err(e) => RenameResult::Failed(e.to_string()),
                }
            };
            if let Ok(mut g) = slot_t.lock() {
                *g = Some(outcome);
            }
        });

        let parent_t = parent_w.clone();
        let overlay_t = overlay_w.clone();
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let Some(outcome) = slot.lock().ok().and_then(|mut g| g.take()) else {
                return glib::ControlFlow::Continue;
            };
            match outcome {
                RenameResult::SavedOffline => {
                    overlay_t.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!(
                                "Saved. The new name will take effect the next time you connect."
                            ))
                            .timeout(3)
                            .build(),
                    );
                    gtk4::prelude::WidgetExt::activate_action(&parent_t, "win.refresh", None).ok();
                }
                RenameResult::Renamed => {
                    overlay_t.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Device renamed."))
                            .timeout(2)
                            .build(),
                    );
                    gtk4::prelude::WidgetExt::activate_action(&parent_t, "win.refresh", None).ok();
                }
                RenameResult::Failed(e) => {
                    overlay_t.add_toast(
                        libadwaita::Toast::builder()
                            // Escape tailscale stderr before it hits the
                            // markup-aware toast title.
                            .title(
                                glib::markup_escape_text(
                                    &trf!("Rename failed: {error}", "error" => e),
                                )
                                .as_str(),
                            )
                            .timeout(6)
                            .build(),
                    );
                }
            }
            glib::ControlFlow::Break
        });
    });
    dlg.present();
}

/// Validate the user-typed hostname against the same charset tailscale enforces
/// as a DNS leaf. Returns the localized error to display when the value is
/// rejected, or Ok with the trimmed name when it's good to apply.
pub(super) fn validate_hostname(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(tr!("The device name can't be empty."));
    }
    if s.len() > 63 {
        return Err(tr!("Use 63 characters or fewer."));
    }
    let bytes = s.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return Err(tr!("The device name can't start or end with a hyphen."));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(tr!(
            "Only lowercase letters, digits and hyphens are allowed."
        ));
    }
    Ok(())
}

pub(super) fn sanitize_hostname(input: &str) -> String {
    let mut output = String::with_capacity(input.len().min(63));
    let mut hyphen = false;
    for c in input.chars().flat_map(char::to_lowercase) {
        if output.len() >= 63 {
            break;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            output.push(c);
            hyphen = false;
        } else if !output.is_empty() && !hyphen {
            output.push('-');
            hyphen = true;
        }
    }
    output.trim_matches('-').to_string()
}

/// If the server returned a loopback URL (because its `server_url` config is
/// pointing at 127.0.0.1 — common when the panel runs behind a reverse proxy
/// and was set up with the default), fall back to whatever the user typed
/// as the panel URL. The user clearly reaches the panel from outside, so
/// that URL is also the right `--login-server` for tailscale.
fn sanitize_server_url(server_from_response: &str, panel_url_typed: &str) -> String {
    let s = server_from_response.trim();
    let is_loopback = panel::url_is_local_server_response(s);
    let chosen = if s.is_empty() || is_loopback {
        panel_url_typed
    } else {
        s
    };
    normalize_server_url(chosen)
}

/// Canonicalize a server URL the user typed: trim whitespace, drop trailing
/// `/`, and prepend `https://` when no scheme is present. Lets users paste
/// just the domain (`vpn.example.org`) — tailscale needs a full URL on
/// `--login-server` and would reject the bare domain otherwise. Empty input
/// passes through unchanged so callers can still detect "no server set".
pub(super) fn normalize_server_url(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name_is_a_dns_label() {
        assert_eq!(sanitize_hostname(" Office PC__01 "), "office-pc-01");
        assert_eq!(sanitize_hostname("---PHONE---"), "phone");
        assert!(validate_hostname(&"a".repeat(63)).is_ok());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
    }

    #[test]
    fn loopback_fallback_checks_the_url_host() {
        assert_eq!(
            sanitize_server_url("http://127.0.0.1:8080", "https://panel.example.org"),
            "https://panel.example.org",
        );
        assert_eq!(
            sanitize_server_url("https://example.org/localhost", "https://panel.example.org"),
            "https://example.org/localhost",
        );
    }
}
