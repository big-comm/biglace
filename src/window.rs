use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::panel::{self, PanelCredentials};
use crate::tailscale::{self, Peer};
use crate::tr;
use crate::trf;

// ─── Entry point ─────────────────────────────────────────────────────────────

pub fn build(app: &libadwaita::Application) {
    let win = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("BigLace")
        .default_width(500)
        .default_height(650)
        .build();

    let toast_overlay = libadwaita::ToastOverlay::new();
    let root_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    // Header bar
    let header = libadwaita::HeaderBar::new();

    let btn_disconnect = gtk4::Button::builder()
        .label(tr!("Disconnect"))
        .css_classes(["destructive-action"])
        .visible(false)
        .build();
    header.pack_start(&btn_disconnect);

    let btn_refresh = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(tr!("Refresh list"))
        .visible(false)
        .build();
    header.pack_end(&btn_refresh);

    let btn_menu = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text(tr!("Main menu"))
        .build();
    header.pack_end(&btn_menu);

    root_box.append(&header);

    // Stack
    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.set_transition_duration(200);
    stack.set_vexpand(true);
    root_box.append(&stack);

    toast_overlay.set_child(Some(&root_box));
    win.set_content(Some(&toast_overlay));

    // Build pages
    let setup = make_setup_page();
    let busy_box = make_busy_page();
    let connected = make_connected_page();
    let (service_box, btn_start_service) = make_service_page();

    stack.add_named(&service_box,         Some("service"));
    stack.add_named(&setup.scroll,        Some("setup"));
    stack.add_named(&busy_box,            Some("busy"));
    stack.add_named(&connected.scroll,    Some("connected"));

    win.present();

    // ─── Menu (hamburger) ────────────────────────────────────────────────────
    {
        let menu_model = gtk4::gio::Menu::new();
        menu_model.append(Some(&tr!("Sign in with panel account")),       Some("win.panel-login"));
        menu_model.append(Some(&tr!("Make this user the tailscale operator")),     Some("win.set-operator"));
        menu_model.append(Some(&tr!("About BigLace")),                  Some("win.about"));
        btn_menu.set_menu_model(Some(&menu_model));

        let action_about = gtk4::gio::SimpleAction::new("about", None);
        let win_w = win.clone();
        action_about.connect_activate(move |_, _| show_about(&win_w));
        win.add_action(&action_about);

        let action_op = gtk4::gio::SimpleAction::new("set-operator", None);
        let win_o = win.clone();
        action_op.connect_activate(move |_, _| {
            let parent = win_o.clone();
            let result = tailscale::set_operator_current_user();
            let dlg = libadwaita::MessageDialog::new(
                Some(&parent),
                Some(&tr!("Tailscale operator")),
                Some(&match &result {
                    Ok(()) => tr!("Done. You will no longer need a password to connect or disconnect."),
                    Err(e) => trf!("Failed: {error}", "error" => e.to_string()),
                }),
            );
            dlg.add_response("ok", &tr!("OK"));
            dlg.present();
        });
        win.add_action(&action_op);
    }

    // ─── Shared config ───────────────────────────────────────────────────────
    let cfg = Rc::new(RefCell::new(config::load()));

    {
        let c = cfg.borrow();
        setup.entry_server.set_text(&c.server_url);
        setup.entry_key.set_text(&c.authkey);
        setup.entry_host.set_text(&c.hostname);
        setup.chk_auto.set_active(c.auto_connect);
    }

    // ─── Copy buttons ────────────────────────────────────────────────────────
    {
        let lbl = connected.lbl_ip.clone();
        let t = toast_overlay.clone();
        connected.btn_copy_ip.connect_clicked(move |_| copy_text(&lbl.text(), &t));
    }
    {
        let lbl = connected.lbl_host.clone();
        let t = toast_overlay.clone();
        connected.btn_copy_host.connect_clicked(move |_| copy_text(&lbl.text(), &t));
    }

    // ─── Initial state check ─────────────────────────────────────────────────
    check_state(&stack, &btn_disconnect, &btn_refresh,
                &connected.lbl_ip, &connected.lbl_host,
                &connected.peers_list, &connected.lbl_no_peers, true);

    // ─── Auto-connect if configured ──────────────────────────────────────────
    {
        let c = cfg.borrow().clone();
        if c.auto_connect && !c.server_url.is_empty()
            && tailscale::is_service_active() && !tailscale::get_status().online
        {
            do_connect(
                c.server_url.clone(), c.authkey.clone(), c.hostname.clone(),
                &stack, &btn_disconnect, &btn_refresh,
                &connected.lbl_ip, &connected.lbl_host,
                &connected.peers_list, &connected.lbl_no_peers,
                &setup.lbl_err,
            );
        }
    }

    // ─── Periodic refresh (30s) ──────────────────────────────────────────────
    {
        let stack2 = stack.clone();
        let btn_disc2 = btn_disconnect.clone();
        let btn_ref2 = btn_refresh.clone();
        let lbl_ip2 = connected.lbl_ip.clone();
        let lbl_host2 = connected.lbl_host.clone();
        let peers2 = connected.peers_list.clone();
        let lbl_np2 = connected.lbl_no_peers.clone();

        glib::timeout_add_seconds_local(30, move || {
            check_state(&stack2, &btn_disc2, &btn_ref2, &lbl_ip2, &lbl_host2,
                        &peers2, &lbl_np2, false);
            glib::ControlFlow::Continue
        });
    }

    // ─── Refresh button ──────────────────────────────────────────────────────
    {
        let stack2 = stack.clone();
        let btn_disc2 = btn_disconnect.clone();
        let btn_ref2 = btn_refresh.clone();
        let lbl_ip2 = connected.lbl_ip.clone();
        let lbl_host2 = connected.lbl_host.clone();
        let peers2 = connected.peers_list.clone();
        let lbl_np2 = connected.lbl_no_peers.clone();

        btn_refresh.connect_clicked(move |_| {
            check_state(&stack2, &btn_disc2, &btn_ref2, &lbl_ip2, &lbl_host2,
                        &peers2, &lbl_np2, true);
        });
    }

    // ─── Connect button ──────────────────────────────────────────────────────
    {
        let stack2 = stack.clone();
        let btn_disc2 = btn_disconnect.clone();
        let btn_ref2 = btn_refresh.clone();
        let lbl_ip2 = connected.lbl_ip.clone();
        let lbl_host2 = connected.lbl_host.clone();
        let peers2 = connected.peers_list.clone();
        let lbl_np2 = connected.lbl_no_peers.clone();
        let lbl_err2 = setup.lbl_err.clone();
        let cfg2 = cfg.clone();
        let es = setup.entry_server.clone();
        let ek = setup.entry_key.clone();
        let eh = setup.entry_host.clone();
        let ca = setup.chk_auto.clone();

        setup.btn_connect.connect_clicked(move |_| {
            let server   = es.text().to_string();
            let authkey  = ek.text().to_string();
            let hostname = eh.text().to_string();
            {
                let mut c = cfg2.borrow_mut();
                c.server_url   = server.clone();
                c.authkey      = authkey.clone();
                c.hostname     = hostname.clone();
                c.auto_connect = ca.is_active();
                config::save(&c).ok();
            }
            do_connect(server, authkey, hostname,
                &stack2, &btn_disc2, &btn_ref2,
                &lbl_ip2, &lbl_host2, &peers2, &lbl_np2, &lbl_err2);
        });
    }

    // ─── Disconnect button ───────────────────────────────────────────────────
    {
        let stack2 = stack.clone();
        let btn_disc2 = btn_disconnect.clone();
        let btn_ref2 = btn_refresh.clone();

        btn_disconnect.connect_clicked(move |_| {
            eprintln!("[biglace] disconnect: button clicked");
            std::thread::spawn(|| {
                if let Err(e) = tailscale::disconnect() {
                    eprintln!("[biglace] disconnect: failed: {e}");
                } else {
                    eprintln!("[biglace] disconnect: ok");
                }
            });
            btn_disc2.set_visible(false);
            btn_ref2.set_visible(false);
            stack2.set_visible_child_name("setup");
        });
    }

    // ─── Start service button ─────────────────────────────────────────────────
    {
        let stack2 = stack.clone();
        let btn_disc2 = btn_disconnect.clone();
        let btn_ref2 = btn_refresh.clone();
        let lbl_ip2 = connected.lbl_ip.clone();
        let lbl_host2 = connected.lbl_host.clone();
        let peers2 = connected.peers_list.clone();
        let lbl_np2 = connected.lbl_no_peers.clone();

        btn_start_service.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            stack2.set_visible_child_name("busy");

            let slot: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
            let slot_t = slot.clone();

            std::thread::spawn(move || {
                let ok = std::process::Command::new("pkexec")
                    .args(["systemctl", "enable", "--now", "tailscaled"])
                    .status().map(|s| s.success()).unwrap_or(false);
                if let Ok(mut g) = slot_t.lock() { *g = Some(ok); }
            });

            let stack3 = stack2.clone();
            let btn_disc3 = btn_disc2.clone();
            let btn_ref3 = btn_ref2.clone();
            let lbl_ip3 = lbl_ip2.clone();
            let lbl_host3 = lbl_host2.clone();
            let peers3 = peers2.clone();
            let lbl_np3 = lbl_np2.clone();
            let btn_clone = btn.clone();

            glib::timeout_add_local(Duration::from_millis(400), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(_) => {
                        btn_clone.set_sensitive(true);
                        check_state(&stack3, &btn_disc3, &btn_ref3, &lbl_ip3, &lbl_host3,
                                    &peers3, &lbl_np3, true);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    // ─── Panel-login menu action ─────────────────────────────────────────────
    {
        let action = gtk4::gio::SimpleAction::new("panel-login", None);
        let win_w = win.clone();
        let toast_w = toast_overlay.clone();
        let cfg2 = cfg.clone();
        let es = setup.entry_server.clone();
        let ek = setup.entry_key.clone();
        let eh = setup.entry_host.clone();

        action.connect_activate(move |_, _| {
            show_panel_login(&win_w, &toast_w, &cfg2, &es, &ek, &eh);
        });
        win.add_action(&action);
    }
}

// ─── do_connect ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn do_connect(
    server: String, authkey: String, hostname: String,
    stack: &gtk4::Stack, btn_disconnect: &gtk4::Button, btn_refresh: &gtk4::Button,
    lbl_ip: &gtk4::Label, lbl_host: &gtk4::Label,
    peers_list: &gtk4::ListBox, lbl_no_peers: &gtk4::Label, lbl_err: &gtk4::Label,
) {
    lbl_err.set_text("");
    stack.set_visible_child_name("busy");

    eprintln!("[biglace] connect: button clicked (server={server:?} hostname={hostname:?})");

    let slot: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let slot_t = slot.clone();

    std::thread::spawn(move || {
        let r = tailscale::connect(&server, &authkey, &hostname).map_err(|e| e.to_string());
        match &r {
            Ok(()) => eprintln!("[biglace] connect: ok"),
            Err(e) => eprintln!("[biglace] connect: failed: {e}"),
        }
        if let Ok(mut g) = slot_t.lock() { *g = Some(r); }
    });

    let stack2 = stack.clone();
    let btn_disc2 = btn_disconnect.clone();
    let btn_ref2 = btn_refresh.clone();
    let lbl_ip2 = lbl_ip.clone();
    let lbl_host2 = lbl_host.clone();
    let peers2 = peers_list.clone();
    let lbl_np2 = lbl_no_peers.clone();
    let lbl_err2 = lbl_err.clone();

    glib::timeout_add_local(Duration::from_millis(300), move || {
        match slot.lock().ok().and_then(|mut g| g.take()) {
            None => glib::ControlFlow::Continue,
            Some(Ok(())) => {
                check_state(&stack2, &btn_disc2, &btn_ref2, &lbl_ip2, &lbl_host2,
                            &peers2, &lbl_np2, true);
                glib::ControlFlow::Break
            }
            Some(Err(e)) => {
                stack2.set_visible_child_name("setup");
                lbl_err2.set_text(&trf!("Error: {error}", "error" => e));
                glib::ControlFlow::Break
            }
        }
    });
}

// ─── check_state ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn check_state(
    stack: &gtk4::Stack, btn_disconnect: &gtk4::Button, btn_refresh: &gtk4::Button,
    lbl_ip: &gtk4::Label, lbl_host: &gtk4::Label,
    peers_list: &gtk4::ListBox, lbl_no_peers: &gtk4::Label,
    force: bool,
) {
    let is_busy = stack.visible_child_name().as_deref() == Some("busy");

    if !tailscale::is_service_active() {
        btn_disconnect.set_visible(false);
        btn_refresh.set_visible(false);
        if force || !is_busy { stack.set_visible_child_name("service"); }
        return;
    }

    let status = tailscale::get_status();

    if status.online {
        lbl_ip.set_text(status.ip.as_deref().unwrap_or("—"));
        lbl_host.set_text(
            status.dns_name.as_deref().or(status.hostname.as_deref()).unwrap_or("—"),
        );
        btn_disconnect.set_visible(true);
        btn_refresh.set_visible(true);
        stack.set_visible_child_name("connected");
        refresh_peers(peers_list, lbl_no_peers);
    } else {
        btn_disconnect.set_visible(false);
        btn_refresh.set_visible(false);
        if force || !is_busy {
            stack.set_visible_child_name("setup");
        }
    }
}

// ─── refresh_peers ───────────────────────────────────────────────────────────

fn refresh_peers(list: &gtk4::ListBox, lbl_no_peers: &gtk4::Label) {
    while let Some(child) = list.first_child() { list.remove(&child); }

    let peers = tailscale::get_peers();
    if peers.is_empty() {
        lbl_no_peers.set_visible(true);
        list.set_visible(false);
        return;
    }
    lbl_no_peers.set_visible(false);
    list.set_visible(true);
    for peer in &peers { list.append(&make_peer_row(peer)); }
}

fn make_peer_row(peer: &Peer) -> gtk4::ListBoxRow {
    let row_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    row_box.set_margin_top(8); row_box.set_margin_bottom(8);
    row_box.set_margin_start(12); row_box.set_margin_end(12);

    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    top.set_hexpand(true);

    let dot = gtk4::Label::new(Some(if peer.online { "●" } else { "○" }));
    if peer.online { dot.add_css_class("success"); } else { dot.add_css_class("dim-label"); }

    let name = gtk4::Label::new(Some(&peer.hostname));
    name.set_hexpand(true); name.set_xalign(0.0); name.add_css_class("heading");

    let ip_lbl = gtk4::Label::new(Some(&peer.ip));
    ip_lbl.add_css_class("dim-label"); ip_lbl.add_css_class("caption"); ip_lbl.add_css_class("monospace");

    top.append(&dot); top.append(&name); top.append(&ip_lbl);
    row_box.append(&top);

    if peer.online {
        let btn_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        btn_box.set_halign(gtk4::Align::End);

        let host1 = if peer.dns_name.is_empty() { peer.ip.clone() } else { peer.dns_name.clone() };
        let host2 = host1.clone();

        let btn_files = gtk4::Button::builder()
            .label(tr!("Files"))
            .icon_name("folder-remote-symbolic").css_classes(["flat"]).build();
        btn_files.connect_clicked(move |_| tailscale::open_files(&host1));

        let btn_term = gtk4::Button::builder()
            .label(tr!("Terminal"))
            .icon_name("utilities-terminal-symbolic").css_classes(["flat"]).build();
        btn_term.connect_clicked(move |_| tailscale::open_terminal(&host2));

        btn_box.append(&btn_files); btn_box.append(&btn_term);
        row_box.append(&btn_box);
    }

    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_activatable(false);
    row
}

// ─── Clipboard ───────────────────────────────────────────────────────────────

fn copy_text(text: &str, overlay: &libadwaita::ToastOverlay) {
    if let Some(display) = gtk4::gdk::Display::default() {
        display.clipboard().set_text(text);
        overlay.add_toast(libadwaita::Toast::builder().title(tr!("Copied!")).timeout(2).build());
    }
}

// ─── About dialog ────────────────────────────────────────────────────────────

fn show_about(parent: &libadwaita::ApplicationWindow) {
    let about = libadwaita::AboutWindow::builder()
        .transient_for(parent)
        .modal(true)
        .application_name("BigLace")
        .application_icon("org.communitybig.biglace")
        .developer_name("BigCommunity")
        .version(env!("CARGO_PKG_VERSION"))
        .website("https://github.com/BigCommunity/biglace")
        .issue_url("https://github.com/BigCommunity/biglace/issues")
        .copyright("© 2026 BigCommunity")
        .license_type(gtk4::License::MitX11)
        .comments(tr!("Mesh VPN client for BigScale/Headscale."))
        .build();
    about.present();
}

// ─── Panel-login dialog ──────────────────────────────────────────────────────

fn show_panel_login(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    entry_server: &libadwaita::EntryRow,
    entry_key:    &libadwaita::EntryRow,
    entry_host:   &libadwaita::EntryRow,
) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Sign in with panel account"))
        .default_width(440)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let header = libadwaita::HeaderBar::new();
    outer.append(&header);

    let pref_page = libadwaita::PreferencesPage::new();
    let grp = libadwaita::PreferencesGroup::builder()
        .title(tr!("Panel credentials"))
        .description(tr!("Use your BigScale panel username and password to generate a key automatically."))
        .build();

    let er_url  = libadwaita::EntryRow::builder()
        .title(tr!("Panel URL")).input_purpose(gtk4::InputPurpose::Url).build();
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
    lbl_err.set_margin_start(16);
    lbl_err.set_margin_end(16);
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
        let es = entry_server.clone();
        let ek = entry_key.clone();
        let eh = entry_host.clone();
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
            let es2 = es.clone();
            let ek2 = ek.clone();
            let eh2 = eh.clone();
            let toast3 = toast2.clone();
            let btn_ok_w2 = btn_ok_w.clone();
            let panel_url = creds.url.clone();
            let node = creds.node.clone();

            glib::timeout_add_local(Duration::from_millis(300), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(Ok(resp)) => {
                        es2.set_text(&resp.server_url);
                        ek2.set_text(&resp.authkey);
                        eh2.set_text(&node);
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
                                .title(tr!("Key generated successfully."))
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

// ─── Page builders ───────────────────────────────────────────────────────────

fn make_service_page() -> (gtk4::Box, gtk4::Button) {
    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 20);
    bx.set_valign(gtk4::Align::Center); bx.set_halign(gtk4::Align::Center);
    bx.set_vexpand(true); bx.set_margin_start(32); bx.set_margin_end(32);

    let icon = gtk4::Image::builder().icon_name("network-offline-symbolic").pixel_size(64).build();
    icon.add_css_class("dim-label");

    let title = gtk4::Label::new(Some(&tr!("Service not found")));
    title.add_css_class("title-2");

    let desc = gtk4::Label::new(Some(
        &tr!("The tailscaled service is not running.\nClick below to start it."),
    ));
    desc.add_css_class("dim-label");
    desc.set_justify(gtk4::Justification::Center);
    desc.set_wrap(true);

    let btn = gtk4::Button::builder()
        .label(tr!("Start service"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .build();

    bx.append(&icon); bx.append(&title); bx.append(&desc); bx.append(&btn);
    (bx, btn)
}

struct SetupWidgets {
    scroll:       gtk4::ScrolledWindow,
    entry_server: libadwaita::EntryRow,
    entry_key:    libadwaita::EntryRow,
    entry_host:   libadwaita::EntryRow,
    chk_auto:     gtk4::CheckButton,
    btn_connect:  gtk4::Button,
    lbl_err:      gtk4::Label,
}

fn make_setup_page() -> SetupWidgets {
    let pref_page = libadwaita::PreferencesPage::new();

    let grp_server = libadwaita::PreferencesGroup::builder()
        .title(tr!("Server"))
        .description(tr!("Address of your BigScale/Headscale server."))
        .build();
    let entry_server = libadwaita::EntryRow::builder()
        .title(tr!("Server URL")).input_purpose(gtk4::InputPurpose::Url).build();
    grp_server.add(&entry_server);
    pref_page.add(&grp_server);

    let grp_user = libadwaita::PreferencesGroup::builder()
        .title(tr!("Your identity on the network"))
        .description(tr!("Personal key generated for you in the BigScale panel (\"New key\" in Users). Each person uses their own — this is not the server's key."))
        .build();
    let entry_key = libadwaita::EntryRow::builder()
        .title(tr!("Pre-auth key (yours)")).build();
    grp_user.add(&entry_key);
    pref_page.add(&grp_user);

    let grp_device = libadwaita::PreferencesGroup::builder()
        .title(tr!("This device"))
        .description(tr!("Name shown to other devices on the network."))
        .build();
    let entry_host = libadwaita::EntryRow::builder().title(tr!("Device name")).build();
    grp_device.add(&entry_host);
    pref_page.add(&grp_device);

    let grp_opts = libadwaita::PreferencesGroup::builder().title(tr!("Options")).build();
    let chk_auto = gtk4::CheckButton::builder()
        .label(tr!("Connect automatically on startup"))
        .margin_top(8).margin_bottom(8).margin_start(4).build();
    grp_opts.add(&chk_auto);
    pref_page.add(&grp_opts);

    let lbl_err = gtk4::Label::new(None);
    lbl_err.add_css_class("error");
    lbl_err.set_wrap(true);
    lbl_err.set_justify(gtk4::Justification::Center);
    lbl_err.set_max_width_chars(60);
    lbl_err.set_margin_start(16);
    lbl_err.set_margin_end(16);
    lbl_err.set_selectable(true);

    let btn_connect = gtk4::Button::builder()
        .label(tr!("Connect"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_top(12).margin_bottom(24)
        .build();

    let bottom = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    bottom.set_halign(gtk4::Align::Fill);
    let lbl_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    lbl_box.set_halign(gtk4::Align::Center);
    lbl_box.append(&lbl_err);
    bottom.append(&lbl_box); bottom.append(&btn_connect);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&pref_page); outer.append(&bottom);

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&outer).build();

    SetupWidgets { scroll, entry_server, entry_key, entry_host, chk_auto, btn_connect, lbl_err }
}

fn make_busy_page() -> gtk4::Box {
    let spinner = gtk4::Spinner::new();
    spinner.set_size_request(48, 48);
    spinner.start();

    let lbl = gtk4::Label::new(Some(&tr!("Please wait...")));
    lbl.add_css_class("dim-label");

    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    bx.set_valign(gtk4::Align::Center); bx.set_halign(gtk4::Align::Center); bx.set_vexpand(true);
    bx.append(&spinner); bx.append(&lbl);
    bx
}

struct ConnectedWidgets {
    scroll:        gtk4::ScrolledWindow,
    lbl_ip:        gtk4::Label,
    lbl_host:      gtk4::Label,
    btn_copy_ip:   gtk4::Button,
    btn_copy_host: gtk4::Button,
    peers_list:    gtk4::ListBox,
    lbl_no_peers:  gtk4::Label,
}

fn make_connected_page() -> ConnectedWidgets {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    outer.set_margin_top(16); outer.set_margin_bottom(16);
    outer.set_margin_start(16); outer.set_margin_end(16);

    let status_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let dot = gtk4::Label::new(Some("●")); dot.add_css_class("success");
    let conn_lbl = gtk4::Label::new(Some(&tr!("Connected to the network")));
    conn_lbl.add_css_class("heading");
    status_row.append(&dot); status_row.append(&conn_lbl);
    outer.append(&status_row);

    let card = gtk4::Frame::new(None); card.add_css_class("card");
    let card_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let (row_ip, lbl_ip, btn_copy_ip) = make_info_row(&tr!("Your IP address"));
    card_box.append(&row_ip);
    card_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    let (row_host, lbl_host, btn_copy_host) = make_info_row(&tr!("Device name"));
    card_box.append(&row_host);

    card.set_child(Some(&card_box));
    outer.append(&card);

    let peers_header = gtk4::Label::new(Some(&tr!("Devices on the network")));
    peers_header.set_xalign(0.0); peers_header.add_css_class("heading");
    outer.append(&peers_header);

    let lbl_no_peers = gtk4::Label::new(Some(&tr!("No other devices found.")));
    lbl_no_peers.add_css_class("dim-label"); lbl_no_peers.set_visible(false);
    outer.append(&lbl_no_peers);

    let peers_list = gtk4::ListBox::new();
    peers_list.add_css_class("boxed-list");
    peers_list.set_selection_mode(gtk4::SelectionMode::None);
    outer.append(&peers_list);

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&outer).build();

    ConnectedWidgets { scroll, lbl_ip, lbl_host, btn_copy_ip, btn_copy_host, peers_list, lbl_no_peers }
}

fn make_info_row(title: &str) -> (gtk4::Box, gtk4::Label, gtk4::Button) {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.set_margin_top(10); row.set_margin_bottom(10);
    row.set_margin_start(16); row.set_margin_end(8);

    let title_lbl = gtk4::Label::new(Some(title));
    title_lbl.add_css_class("dim-label"); title_lbl.set_width_chars(22); title_lbl.set_xalign(0.0);
    row.append(&title_lbl);

    let value_lbl = gtk4::Label::new(Some("—"));
    value_lbl.set_hexpand(true); value_lbl.set_xalign(0.0);
    value_lbl.set_selectable(true); value_lbl.add_css_class("monospace");
    row.append(&value_lbl);

    let copy_btn = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(tr!("Copy"))
        .build();
    row.append(&copy_btn);

    (row, value_lbl, copy_btn)
}
