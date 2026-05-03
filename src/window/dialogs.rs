use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::panel::{self, PanelCredentials};
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
        .version(env!("CARGO_PKG_VERSION"))
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

pub fn show_panel_login(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg:    &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Sign in with panel account"))
        .default_width(460)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = libadwaita::HeaderBar::new();
    outer.append(&header);

    let pref_page = libadwaita::PreferencesPage::new();
    let grp = libadwaita::PreferencesGroup::builder()
        .title(tr!("Panel credentials"))
        .description(tr!(
            "Use your BigScale panel username and password to generate a key automatically."
        ))
        .build();

    let er_url = libadwaita::EntryRow::builder()
        .title(tr!("Panel URL"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    let er_user = libadwaita::EntryRow::builder().title(tr!("Username")).build();
    let er_pass = libadwaita::PasswordEntryRow::builder().title(tr!("Password")).build();
    let er_node = libadwaita::EntryRow::builder().title(tr!("Network user identifier")).build();

    {
        let c = cfg.borrow();
        er_url.set_text(&c.panel_url);
        er_node.set_text(&c.hostname);
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
    let btn_ok = gtk4::Button::builder()
        .label(tr!("Sign in"))
        .css_classes(["suggested-action"])
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
        let lbl_err2 = lbl_err.clone();
        let er_url2 = er_url.clone();
        let er_user2 = er_user.clone();
        let er_pass2 = er_pass.clone();
        let er_node2 = er_node.clone();
        let cfg2 = cfg.clone();
        let sidebar2 = sidebar.clone();
        let toast2 = overlay.clone();
        let btn_ok_w = btn_ok.clone();

        btn_ok.connect_clicked(move |_| {
            lbl_err2.set_text("");
            btn_ok_w.set_sensitive(false);

            let creds = PanelCredentials {
                url:      er_url2.text().to_string(),
                username: er_user2.text().to_string(),
                password: er_pass2.text().to_string(),
                node:     er_node2.text().to_string(),
                hostname: er_node2.text().to_string(),
            };

            if creds.url.is_empty() || creds.username.is_empty() || creds.password.is_empty() {
                lbl_err2.set_text(&tr!("Fill in URL, username and password."));
                btn_ok_w.set_sensitive(true);
                return;
            }

            let slot: Arc<Mutex<Option<Result<panel::PreAuthResponse, String>>>> =
                Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            let creds_t = creds.clone();
            std::thread::spawn(move || {
                let r = panel::request_preauth(&creds_t).map_err(|e| e.to_string());
                if let Ok(mut g) = slot_t.lock() { *g = Some(r); }
            });

            let dlg3 = dlg2.clone();
            let lbl_err3 = lbl_err2.clone();
            let cfg3 = cfg2.clone();
            let sidebar3 = sidebar2.clone();
            let toast3 = toast2.clone();
            let btn_ok_w2 = btn_ok_w.clone();
            let panel_url = creds.url.clone();
            let node = creds.node.clone();

            glib::timeout_add_local(Duration::from_millis(300), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(Ok(resp)) => {
                        sidebar3.entry_server.set_text(&resp.server_url);
                        sidebar3.entry_key.set_text(&resp.authkey);
                        sidebar3.entry_host.set_text(&node);
                        {
                            let mut c = cfg3.borrow_mut();
                            c.panel_url  = panel_url.clone();
                            c.server_url = resp.server_url.clone();
                            c.authkey    = resp.authkey.clone();
                            c.hostname   = node.clone();
                            config::save(&c).ok();
                        }
                        toast3.add_toast(
                            libadwaita::Toast::builder()
                                .title(tr!("Signed in. Press Connect to join the network."))
                                .timeout(3)
                                .build(),
                        );
                        dlg3.close();
                        glib::ControlFlow::Break
                    }
                    Some(Err(e)) => {
                        lbl_err3.set_text(&trf!("Error: {error}", "error" => e));
                        btn_ok_w2.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    dlg.present();
}
