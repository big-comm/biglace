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
    let result = tailscale::set_operator_current_user();
    let body = match &result {
        Ok(()) => tr!("Done. You will no longer need a password to connect or disconnect."),
        Err(e) => trf!("Failed: {error}", "error" => e.to_string()),
    };
    let dlg = libadwaita::MessageDialog::new(
        Some(parent),
        Some(&tr!("Tailscale operator")),
        Some(&body),
    );
    dlg.add_response("ok", &tr!("OK"));
    dlg.present();
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
    cfg:    &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    if cfg.borrow().authkey.is_empty() {
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

    let parent_w  = parent.clone();
    let overlay_w = overlay.clone();
    let cfg_w     = cfg.clone();
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
    cfg:    &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    // Snapshot credentials we need to clear from the keyring, then wipe the
    // identity-bearing fields from the on-disk config. We deliberately keep
    // `server_url`, `panel_url`, `hostname` and `auto_connect`: typically
    // the user is switching accounts on the same control server, so making
    // them retype URLs each time is friction. They can still edit them
    // manually when moving to a different server.
    let (panel_url, panel_username) = {
        let mut c = cfg.borrow_mut();
        let pu = c.panel_url.clone();
        let pn = c.panel_username.clone();
        c.authkey        = String::new();
        c.panel_username = String::new();
        config::save(&c).ok();
        (pu, pn)
    };
    secrets::clear(&panel_url, &panel_username);

    sidebar.entry_key.set_text("");

    std::thread::spawn(|| {
        let _ = tailscale::logout();
    });

    overlay.add_toast(
        libadwaita::Toast::builder()
            .title(tr!("Signed out."))
            .timeout(3)
            .build(),
    );

    // Refresh so the sidebar flips to the "not signed in" layout, exposing
    // both the manual-key expander and the panel-login button.
    gtk4::prelude::WidgetExt::activate_action(parent, "win.refresh", None).ok();
}

pub fn show_panel_login(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg:    &Rc<RefCell<config::Config>>,
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
    let er_user = libadwaita::EntryRow::builder().title(tr!("Username")).build();
    let er_pass = libadwaita::PasswordEntryRow::builder().title(tr!("Password")).build();
    let er_node = libadwaita::EntryRow::builder()
        .title(tr!("Network identifier (will be created if new)"))
        .build();

    {
        let c = cfg.borrow();
        er_url.set_text(&c.panel_url);
        er_user.set_text(&c.panel_username);
        er_node.set_text(&c.hostname);
        // Pre-fill the password from the OS keyring if we've seen this user
        // on this panel before. Falls back to empty silently when no keyring
        // backend is available.
        if !c.panel_url.is_empty() && !c.panel_username.is_empty() {
            if let Some(pw) = secrets::load(&c.panel_url, &c.panel_username) {
                er_pass.set_text(&pw);
            }
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

        btn_ok.connect_clicked(move |_| {
            lbl_err2.set_text("");

            let creds = PanelCredentials {
                url:      er_url2.text().to_string(),
                username: er_user2.text().to_string(),
                password: er_pass2.text().to_string(),
                node:     er_node2.text().to_string(),
                hostname: er_node2.text().to_string(),
            };

            if creds.url.is_empty() || creds.username.is_empty() || creds.password.is_empty() {
                lbl_err2.set_text(&tr!("Fill in URL, username and password."));
                return;
            }

            // Visual busy state: disable OK + Cancel, swap label for spinner.
            btn_ok_w.set_sensitive(false);
            btn_cancel_w.set_sensitive(false);
            btn_ok_w.set_child(Some(&ok_busy_w));
            ok_spinner_w.start();

            let slot: Arc<Mutex<Option<Result<panel::PreAuthResponse, String>>>> =
                Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            let creds_t = creds.clone();
            std::thread::spawn(move || {
                let r = panel::request_preauth(&creds_t).map_err(|e| e.to_string());
                if let Ok(mut g) = slot_t.lock() { *g = Some(r); }
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
            let panel_url = creds.url.clone();
            let username  = creds.username.clone();
            let password  = creds.password.clone();
            let node      = creds.node.clone();

            glib::timeout_add_local(Duration::from_millis(300), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(Ok(resp)) => {
                        // The server may return a loopback (127.0.0.1) URL
                        // when its public_url isn't configured — fall back
                        // to whatever the user actually typed in panel URL.
                        let server_url = sanitize_server_url(
                            &resp.server_url,
                            &panel_url,
                        );
                        sidebar3.entry_server.set_text(&server_url);
                        sidebar3.entry_key.set_text(&resp.authkey);
                        sidebar3.entry_host.set_text(&node);
                        // Collapse the manual-key expander — the key was just
                        // generated for the user, no need to show the field.
                        sidebar3.expander_manual.set_expanded(false);
                        {
                            let mut c = cfg3.borrow_mut();
                            c.panel_url      = panel_url.clone();
                            c.panel_username = username.clone();
                            c.server_url     = server_url.clone();
                            c.authkey        = resp.authkey.clone();
                            c.hostname       = node.clone();
                            config::save(&c).ok();
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
                        gtk4::prelude::WidgetExt::activate_action(
                            &win3, "win.refresh", None,
                        ).ok();
                        glib::ControlFlow::Break
                    }
                    Some(Err(e)) => {
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

/// If the server returned a loopback URL (because its `server_url` config is
/// pointing at 127.0.0.1 — common when the panel runs behind a reverse proxy
/// and was set up with the default), fall back to whatever the user typed
/// as the panel URL. The user clearly reaches the panel from outside, so
/// that URL is also the right `--login-server` for tailscale.
fn sanitize_server_url(server_from_response: &str, panel_url_typed: &str) -> String {
    let s = server_from_response.trim();
    let is_loopback = s.contains("127.0.0.1") || s.contains("localhost") || s.contains("0.0.0.0");
    if s.is_empty() || is_loopback {
        panel_url_typed.trim_end_matches('/').to_string()
    } else {
        s.to_string()
    }
}
