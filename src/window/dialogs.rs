use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::config;
use crate::panel::{self, PanelCredentials};
use crate::secrets;
use crate::tailscale;
use crate::tr;
use crate::trf;

use super::sidebar::Sidebar;

/// Esc closes `win` (a plain window, which has no such key by itself).
pub(super) fn close_on_escape(win: &impl IsA<gtk4::Window>) {
    let win: &gtk4::Window = win.upcast_ref();
    let keys = gtk4::EventControllerKey::new();
    let weak = win.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key != gtk4::gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(w) = weak.upgrade() {
            w.close();
        }
        glib::Propagation::Stop
    });
    win.add_controller(keys);
}

pub fn show_about(parent: &libadwaita::ApplicationWindow) {
    let about = libadwaita::AboutWindow::builder()
        .transient_for(parent)
        .modal(true)
        .application_name("BigLace")
        .application_icon("org.communitybig.biglace")
        .developer_name("BigCommunity")
        .version(crate::APP_VERSION)
        // The package's date stamp says exactly which release this is.
        .debug_info(match crate::BUILD_VERSION {
            Some(build) => format!("Build {build}"),
            None => "Development build".to_string(),
        })
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
    let parent_w = parent.clone();
    super::run_in_background(
        || tailscale::set_operator_current_user().map_err(|e| e.to_string()),
        move |result| {
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
        },
    );
}

/// Confirm with the user, then tear down the current session.
///
/// We deliberately do NOT auto-open the panel-login dialog after sign-out:
/// users who joined via a manually-pasted pre-auth key don't have panel
/// credentials, and the sidebar reverts to the "not signed in" layout where
/// they can pick freely between panel login (menu) and the manual-key
/// expander.
///
/// An account an invite created gets a choice: keep it (sign in again later
/// as the same name, from this computer) or delete it (the name is freed).
pub fn confirm_sign_out(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
    online: bool,
) {
    if !cfg.borrow().enrolled && cfg.borrow().authkey.is_empty() {
        return;
    }
    let account = cfg.borrow().account_user.clone();

    let dlg = if account.is_empty() {
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
        dlg
    } else {
        let dlg = libadwaita::MessageDialog::new(
            Some(parent),
            Some(&tr!("Leave the network?")),
            Some(&trf!(
                "This device leaves the network and its name becomes free. Your account {user} is kept: you can come back later from this computer, without an invite.\n\nIf you won't come back, delete the account and the name {user} becomes free too.",
                "user" => account
            )),
        );
        dlg.add_response("cancel", &tr!("Cancel"));
        dlg.add_response("delete", &tr!("Leave and delete account"));
        dlg.add_response("ok", &tr!("Leave"));
        dlg.set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
        dlg.set_response_appearance("ok", libadwaita::ResponseAppearance::Suggested);
        // Deleting goes through the panel, reached over the tunnel.
        if !online {
            dlg.set_response_enabled("delete", false);
            dlg.set_extra_child(Some(
                &gtk4::Label::builder()
                    .label(tr!("Connect first to delete the account."))
                    .css_classes(["dim-label", "caption"])
                    .wrap(true)
                    .build(),
            ));
        }
        dlg
    };
    dlg.set_default_response(Some("cancel"));
    dlg.set_close_response("cancel");

    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w = cfg.clone();
    let sidebar_w = sidebar.clone();
    dlg.connect_response(None, move |dlg, resp| {
        dlg.close();
        match resp {
            "ok" => sign_out(&parent_w, &overlay_w, &cfg_w, &sidebar_w, false, online),
            "delete" => sign_out(&parent_w, &overlay_w, &cfg_w, &sidebar_w, true, online),
            _ => {}
        }
    });
    dlg.present();
}

/// Time for the panel's reply to arrive before it removes this device (and
/// with it the tunnel the reply travels on).
const LEAVE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Tear down the current session, leaving the UI in the "not signed in"
/// state. First asks the panel to remove this device (frees its name; with
/// `delete_account`, the account too), then runs `tailscale logout` — on a
/// background thread, as both can block on the network.
fn sign_out(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    _sidebar: &Sidebar,
    delete_account: bool,
    online: bool,
) {
    // Both steps below can take seconds: say something right away.
    super::show_toast(overlay, &tr!("Signing out…"), 3);
    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w = cfg.clone();
    super::run_in_background(
        move || {
            // Offline, the panel is unreachable: asking would only stall the
            // sign-out on a connect timeout.
            let left = if online {
                crate::invite::leave(delete_account)
            } else {
                Err(crate::invite::MemberError::Unsupported)
            };
            if delete_account && left.is_err() {
                // Signing out anyway would leave the account behind while
                // the person believes it's gone.
                return Err(None);
            }
            match left {
                Ok(left) => {
                    std::thread::sleep(LEAVE_GRACE);
                    // The server already dropped the device: logging out
                    // only has to clear local state, its reply is moot.
                    if tailscale::logout().is_err() {
                        if let Err(e) = tailscale::disconnect() {
                            eprintln!("[biglace] sign-out: disconnect failed: {e}");
                        }
                    }
                    Ok(Some(left))
                }
                Err(_) => tailscale::logout().map(|()| None).map_err(|e| Some(e.to_string())),
            }
        },
        move |result| match result {
            Ok(left) => {
                let account_gone = left.as_ref().is_some_and(|l| l.account_deleted);
                forget_local_identity(&parent_w, &overlay_w, &cfg_w, account_gone);
                if let Some(l) = left.filter(|l| delete_account && !l.account_deleted) {
                    super::show_toast(
                        &overlay_w,
                        &trf!(
                            "Signed out. Your account was kept: {count} other device(s) still use it.",
                            "count" => l.other_devices.to_string()
                        ),
                        8,
                    );
                }
            }
            Err(None) => super::show_toast(
                &overlay_w,
                &tr!("Couldn't delete the account: the panel can't be reached. Connect and try again."),
                8,
            ),
            // `tailscale logout` has to reach the control server. When that
            // server is down — exactly when someone may want to switch
            // accounts — the old code just toasted the error and left the user
            // stuck signed in. Offer to forget the identity on this computer.
            Err(Some(error)) => offer_local_sign_out(&parent_w, &overlay_w, &cfg_w, &error),
        },
    );
}

/// The config after signing out: no enrollment key, not enrolled.
fn signed_out(cfg: &config::Config) -> config::Config {
    let mut updated = cfg.clone();
    updated.authkey.clear();
    updated.enrolled = false;
    updated.pending_key = false;
    updated
}

/// Drop the saved identity (config + keyring entries) and refresh the UI.
/// The invite-made account (and its secret) stays unless `account_gone`.
fn forget_local_identity(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    account_gone: bool,
) {
    let (server_url, panel_url, panel_username) = {
        let c = cfg.borrow();
        (
            c.server_url.clone(),
            c.panel_url.clone(),
            c.panel_username.clone(),
        )
    };
    let account_user = cfg.borrow().account_user.clone();
    let mut updated = signed_out(&cfg.borrow());
    if account_gone {
        updated.account_user.clear();
    }
    if let Err(error) = config::save(&updated) {
        super::show_toast(overlay, &trf!("Error: {error}", "error" => error), 6);
        return;
    }
    *cfg.borrow_mut() = updated;
    secrets::clear(&panel_url, &panel_username);
    secrets::clear_authkey(&server_url);
    if account_gone {
        secrets::clear_reclaim(&panel_url, &account_user);
        super::show_toast(overlay, &tr!("Signed out and account deleted."), 4);
    } else {
        super::show_toast(overlay, &tr!("Signed out."), 3);
    }
    gtk4::prelude::WidgetExt::activate_action(parent, "win.refresh", None).ok();
}

fn offer_local_sign_out(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    error: &str,
) {
    let dlg = libadwaita::MessageDialog::new(
        Some(parent),
        Some(&tr!("Couldn't reach the server")),
        Some(&trf!(
            "The control server didn't confirm the sign-out: {error}\n\nYou can still sign out on this computer: BigLace forgets the saved key and account and disconnects. The server keeps listing this device until an administrator removes it.",
            "error" => error
        )),
    );
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("local", &tr!("Sign out on this computer"));
    dlg.set_response_appearance("local", libadwaita::ResponseAppearance::Destructive);
    dlg.set_default_response(Some("cancel"));
    dlg.set_close_response("cancel");

    let parent_w = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w = cfg.clone();
    dlg.connect_response(None, move |dlg, resp| {
        dlg.close();
        if resp != "local" {
            return;
        }
        // Stop the tunnel too (best effort; it may well be down already).
        let parent_t = parent_w.clone();
        let overlay_t = overlay_w.clone();
        let cfg_t = cfg_w.clone();
        super::run_in_background(
            || {
                if let Err(e) = tailscale::disconnect() {
                    eprintln!("[biglace] sign-out: disconnect failed: {e}");
                }
            },
            move |()| forget_local_identity(&parent_t, &overlay_t, &cfg_t, false),
        );
    });
    dlg.present();
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
        .title(tr!("Sign in as administrator"))
        .default_width(480)
        .build();
    close_on_escape(&dlg);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = libadwaita::HeaderBar::new();
    outer.append(&header);

    let pref_page = libadwaita::PreferencesPage::new();
    let grp = libadwaita::PreferencesGroup::builder()
        .title(tr!("Panel administrator"))
        .description(tr!(
            "Only for whoever administers the BigScale panel: use the panel's admin \
             username and password. This device joins the network under the name \
             below. Everyone else joins with an invite or an access key."
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
        .title(tr!("Panel admin username"))
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
        let clean = sanitize_hostname_while_typing(&entry.text());
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
            let er_pass_w = er_pass.clone();
            super::run_in_background(
                move || secrets::load(&panel_url, &username),
                move |pw| {
                    if let Some(pw) = pw {
                        er_pass_w.set_text(&pw);
                    }
                },
            );
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

            let creds_t = creds.clone();

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

            super::run_in_background(
                move || panel::request_preauth(&creds_t).map_err(|e| e.to_string()),
                move |result| match result {
                    Ok(resp) => {
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
                        let saved = {
                            let mut c = cfg3.borrow_mut();
                            c.panel_url = panel_url.clone();
                            c.panel_username = username.clone();
                            // Signed in as a panel account now, not as the
                            // invite-made one this installation had.
                            c.account_user.clear();
                            c.server_url = server_url.clone();
                            c.authkey = resp.authkey.clone();
                            c.hostname = creds.hostname.clone();
                            c.enrolled = true;
                            c.pending_key = true;
                            config::save_with_authkey(&c)
                        };
                        // Stash the password in the OS-native keyring so the
                        // user doesn't retype it next time they sign in.
                        secrets::save(&panel_url, &username, &password);
                        match saved {
                            Ok(None) => super::show_toast(
                                &toast3,
                                &tr!("Signed in. Press Connect to join the network."),
                                3,
                            ),
                            Ok(Some(reason)) => {
                                super::show_toast(&toast3, &super::keyring_warning(&reason), 8)
                            }
                            Err(e) => {
                                super::show_toast(&toast3, &trf!("Error: {error}", "error" => e), 6)
                            }
                        }
                        dlg3.close();
                        // Force the sidebar / status to re-read config now —
                        // otherwise the UI keeps showing the "not signed in"
                        // layout until the next 30s refresh tick.
                        gtk4::prelude::WidgetExt::activate_action(&win3, "win.refresh", None).ok();
                    }
                    Err(e) => {
                        *busy_p.borrow_mut() = false;
                        lbl_err3.set_text(&trf!("Error: {error}", "error" => e));
                        // Restore the idle button state so the user can retry.
                        ok_spinner_w2.stop();
                        btn_ok_w2.set_child(Some(&ok_idle_w2));
                        btn_ok_w2.set_sensitive(true);
                        btn_cancel_w2.set_sensitive(true);
                    }
                },
            );
        });
    }

    dlg.present();
}

/// One name being typed: what the panel said about it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Typed {
    /// The name already in use.
    Unchanged,
    /// Not a name yet (empty, or ends in a hyphen mid-word).
    Incomplete,
    Checking,
    Free,
    Taken,
    /// The panel couldn't be asked.
    Unknown,
}

/// What each of the two entries holds, account first.
type Typing = Rc<Cell<(Typed, Typed)>>;

/// The pencil on our identity row: our username (with a BigScale panel
/// behind the network) and this device's name. Each is checked as it is
/// typed; a name someone else holds can't be saved.
pub fn show_names(parent: &libadwaita::ApplicationWindow, overlay: &libadwaita::ToastOverlay, cfg: &Rc<RefCell<config::Config>>) {
    use crate::invite::NameKind;

    let account_now = crate::chat::my_user();
    let device_now = sanitize_hostname(&cfg.borrow().hostname);
    let panel = account_now.is_some();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let at = gtk4::Label::new(Some("@"));
    at.add_css_class("dim-label");
    let (account, account_note, account_group) = name_entry(
        &outer,
        &tr!("Username"),
        &at,
        account_now.as_deref().unwrap_or_default(),
        &tr!("How everyone sees you in conversations."),
    );
    account_group.set_visible(panel);
    let host_icon = gtk4::Image::from_icon_name("computer-symbolic");
    host_icon.set_pixel_size(18);
    let (device, device_note, _) = name_entry(
        &outer,
        &tr!("Device name on the network"),
        &host_icon,
        &device_now,
        &tr!("How this computer appears in the device list."),
    );

    let dlg = libadwaita::MessageDialog::new(Some(parent), Some(&tr!("Your names")), None);
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("save", &tr!("Save"));
    dlg.set_response_appearance("save", libadwaita::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("save"));
    dlg.set_close_response("cancel");
    dlg.set_extra_child(Some(&outer));

    let checks = NameChecks::new(panel, &dlg);
    let typing = checks.typing.clone();
    checks.watch(&account, &account_note, NameKind::Account, account_now.unwrap_or_default(), 32);
    checks.watch(&device, &device_note, NameKind::Device, device_now, 63);

    let (cfg, parent, overlay) = (cfg.clone(), parent.clone(), overlay.clone());
    dlg.connect_response(None, move |dlg, resp| {
        dlg.close();
        let (a, d) = typing.get();
        if resp != "save" {
            return;
        }
        let new_account = (a == Typed::Free).then(|| account.text().to_string());
        let new_device = (d == Typed::Free).then(|| sanitize_hostname(&device.text()));
        save_names(&parent, &overlay, &cfg, panel, new_account, new_device);
    });
    dlg.present();
}

/// Checking each name as it is typed.
struct NameChecks {
    panel: bool,
    typing: Typing,
    /// Enables Save when both names can be kept.
    gate: Rc<dyn Fn()>,
}

impl NameChecks {
    /// Save in `dlg` stays off until both names can be kept and one changed.
    fn new(panel: bool, dlg: &libadwaita::MessageDialog) -> Self {
        let typing: Typing = Rc::new(Cell::new((Typed::Unchanged, Typed::Unchanged)));
        let gate: Rc<dyn Fn()> = {
            let (dlg, typing) = (dlg.clone(), typing.clone());
            Rc::new(move || {
                let (a, d) = typing.get();
                let fine = |t: Typed| matches!(t, Typed::Unchanged | Typed::Free);
                dlg.set_response_enabled("save", fine(a) && fine(d) && (a, d) != (Typed::Unchanged, Typed::Unchanged));
            })
        };
        gate();
        NameChecks { panel, typing, gate }
    }

    /// Check what `entry` holds against `now` (asking the panel when there is
    /// one) and say so in `note`. A check that comes back after the text
    /// changed again is dropped.
    fn watch(&self, entry: &libadwaita::EntryRow, note: &gtk4::Label, kind: crate::invite::NameKind, now: String, max: usize) {
        use crate::invite::{self, NameKind};

        let asked: Rc<Cell<u32>> = Rc::default();
        let (note, typing, gate, panel) = (note.clone(), self.typing.clone(), self.gate.clone(), self.panel);
        let set = move |t: Typed, text: &str| {
            let (a, d) = typing.get();
            typing.set(if kind == NameKind::Account { (t, d) } else { (a, t) });
            show_typed(&note, t, text);
            gate();
        };
        entry.connect_changed(move |e| {
            let mut clean = sanitize_hostname_while_typing(&e.text());
            clean.truncate(max);
            if clean != e.text().as_str() {
                e.set_text(&clean);
                e.set_position(-1);
                return;
            }
            let ticket = asked.get().wrapping_add(1);
            asked.set(ticket);
            if clean == now {
                return set(Typed::Unchanged, &clean);
            }
            if clean.len() < 2 || clean.ends_with('-') {
                return set(Typed::Incomplete, &clean);
            }
            if !panel {
                // No panel to ask: the network takes any well-formed name.
                return set(Typed::Free, &clean);
            }
            set(Typed::Checking, &clean);
            let (asked, set, entry) = (asked.clone(), set.clone(), e.clone());
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                if asked.get() != ticket {
                    return;
                }
                let raw = clean.clone();
                super::run_in_background(
                    move || invite::check_name(kind, &raw),
                    move |r| {
                        if asked.get() != ticket || entry.text() != clean {
                            return;
                        }
                        set(r.map_or(Typed::Unknown, |c| Typed::from(c.state)), &clean);
                    },
                );
            });
        });
    }
}

impl From<crate::invite::NameState> for Typed {
    fn from(state: crate::invite::NameState) -> Self {
        use crate::invite::NameState;
        match state {
            NameState::Free => Typed::Free,
            NameState::Yours => Typed::Unchanged,
            NameState::Taken => Typed::Taken,
            NameState::Invalid => Typed::Incomplete,
        }
    }
}

/// The line under an entry for what it holds.
fn show_typed(note: &gtk4::Label, t: Typed, text: &str) {
    note.remove_css_class("error");
    note.remove_css_class("success");
    let shown = match t {
        Typed::Unchanged | Typed::Incomplete => String::new(),
        Typed::Checking => tr!("Checking…"),
        Typed::Free => {
            note.add_css_class("success");
            trf!("{name} is free.", "name" => text)
        }
        Typed::Taken => {
            note.add_css_class("error");
            trf!("{name} is already taken. Choose another name.", "name" => text)
        }
        Typed::Unknown => {
            note.add_css_class("error");
            tr!("Couldn't check the name with the panel. Try again in a moment.")
        }
    };
    note.set_visible(!shown.is_empty());
    note.set_text(&shown);
}

/// Take the new names: the account through the panel; the device through the
/// panel too when there is one, and as this device's own name.
fn save_names(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    panel: bool,
    new_account: Option<String>,
    new_device: Option<String>,
) {
    use crate::invite::{self, NameKind};

    if let Some(name) = &new_device {
        // Kept for the next `tailscale up`, whatever happens now.
        let mut c = cfg.borrow_mut();
        c.hostname.clone_from(name);
        config::save_or_warn(&c);
    }
    let (parent, overlay) = (parent.clone(), overlay.clone());
    super::run_in_background(
        move || -> Result<(), String> {
            if let Some(name) = &new_account {
                invite::take_name(NameKind::Account, name).map_err(|e| name_failure(&e))?;
                crate::chat::my_groups_reload();
            }
            if let Some(name) = &new_device {
                if panel {
                    invite::take_name(NameKind::Device, name).map_err(|e| name_failure(&e))?;
                }
                // The device's own name too, so the network name follows it from now on.
                if tailscale::is_service_active() && tailscale::get_status().registered {
                    tailscale::set_hostname(name).map_err(|e| trf!("Rename failed: {error}", "error" => e))?;
                }
            }
            Ok(())
        },
        move |r| {
            let text = match r {
                Ok(()) => tr!("Names saved."),
                Err(e) => e,
            };
            super::show_toast(&overlay, &text, 5);
            gtk4::prelude::WidgetExt::activate_action(&parent, "win.refresh", None).ok();
        },
    );
}

/// One name's entry in `outer`: its group, saying what the name is for, and
/// the line under it for what was typed.
fn name_entry(
    outer: &gtk4::Box,
    title: &str,
    prefix: &impl IsA<gtk4::Widget>,
    text: &str,
    what: &str,
) -> (libadwaita::EntryRow, gtk4::Label, libadwaita::PreferencesGroup) {
    let entry = libadwaita::EntryRow::builder().title(title).build();
    entry.add_prefix(prefix);
    entry.set_text(text);
    let group = libadwaita::PreferencesGroup::builder().description(what).build();
    group.add(&entry);
    let note = note_label();
    outer.append(&group);
    outer.append(&note);
    (entry, note, group)
}

fn note_label() -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_max_width_chars(50);
    l.add_css_class("caption");
    l.set_visible(false);
    l
}

/// Sentence for a name the panel refused.
fn name_failure(e: &crate::invite::MemberError) -> String {
    use crate::invite::MemberError;
    match e {
        MemberError::Other(code) if code == "name_taken" => tr!("Someone just took that name. Choose another name."),
        MemberError::TooMany => tr!("You changed your name a moment ago. Wait a minute and try again."),
        MemberError::Unsupported => tr!("Changing names needs a newer BigScale server. Ask your administrator to update it."),
        MemberError::Other(code) => trf!("Could not reach the panel: {error}", "error" => code),
        _ => tr!("The panel couldn't confirm this device. Make sure BigLace is up to date and stays open while connected, then try again."),
    }
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
    sanitize_hostname_while_typing(input)
        .trim_end_matches('-')
        .to_string()
}

/// The same normalization for an entry that is still being edited: a trailing
/// hyphen is kept, because it is how "tales-cel" gets typed one key at a time.
/// Stripping it on every keystroke made the hyphen impossible to type;
/// [`sanitize_hostname`] removes it once the value is used.
pub(super) fn sanitize_hostname_while_typing(input: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

    let mut output = String::with_capacity(input.len().min(63));
    let mut hyphen = false;
    // Decompose and drop the combining marks first, so "José" becomes "jose"
    // (as on the Android app) instead of losing the accented letter.
    let folded = input.nfd().filter(|c| !is_combining_mark(*c));
    for c in folded.flat_map(char::to_lowercase) {
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
    output
}

/// If the server returned a loopback URL (because its `server_url` config is
/// pointing at 127.0.0.1 — common when the panel runs behind a reverse proxy
/// and was set up with the default), fall back to whatever the user typed
/// as the panel URL. The user clearly reaches the panel from outside, so
/// that URL is also the right `--login-server` for tailscale.
pub(super) fn sanitize_server_url(server_from_response: &str, panel_url_typed: &str) -> String {
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
    fn signing_out_forgets_the_identity_but_keeps_preferences() {
        let cfg = config::Config {
            server_url: "https://vpn.example.org".into(),
            authkey: "hskey-auth-secret".into(),
            enrolled: true,
            pending_key: true,
            favorites: vec!["box".into()],
            ..config::Config::default()
        };
        let out = signed_out(&cfg);
        assert!(out.authkey.is_empty());
        assert!(!out.enrolled);
        assert!(!out.pending_key);
        assert_eq!(out.server_url, cfg.server_url);
        assert_eq!(out.favorites, cfg.favorites);
    }

    #[test]
    fn a_hyphen_can_be_typed_and_is_only_trimmed_on_use() {
        // Typing "tales-cel" one key at a time must keep the hyphen in between.
        let typed = "tales-cel".chars().fold(String::new(), |field, key| {
            sanitize_hostname_while_typing(&format!("{field}{key}"))
        });
        assert_eq!(typed, "tales-cel");
        assert_eq!(sanitize_hostname_while_typing("Tales "), "tales-");
        assert_eq!(sanitize_hostname("tales-"), "tales");
        assert_eq!(sanitize_hostname_while_typing("--tales"), "tales");
    }

    #[test]
    fn device_name_is_a_dns_label() {
        assert_eq!(sanitize_hostname(" Office PC__01 "), "office-pc-01");
        assert_eq!(sanitize_hostname("---PHONE---"), "phone");
        // Accents are folded, not dropped.
        assert_eq!(sanitize_hostname("José Ñandú"), "jose-nandu");
        assert_eq!(sanitize_hostname("  Sálá PC__01  "), "sala-pc-01");
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
