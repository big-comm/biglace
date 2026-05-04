mod content;
mod dialogs;
mod peer_row;
mod sidebar;
mod style;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::tailscale::{self, Peer, Status};
use crate::tr;
use crate::trf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AppState {
    ServiceStopped,
    NotSignedIn,
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Clone)]
struct Ui {
    win:     libadwaita::ApplicationWindow,
    toast:   libadwaita::ToastOverlay,
    sidebar: sidebar::Sidebar,
    content: content::Content,
}

pub fn build(app: &libadwaita::Application) {
    style::install();

    let win = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("BigLace")
        .default_width(960)
        .default_height(640)
        .build();

    let toast = libadwaita::ToastOverlay::new();

    let split = libadwaita::OverlaySplitView::builder()
        .min_sidebar_width(280.0)
        .max_sidebar_width(340.0)
        .sidebar_width_fraction(0.36)
        .show_sidebar(true)
        .build();

    let sidebar_widgets = sidebar::build();
    let content_widgets = content::build();

    split.set_sidebar(Some(&sidebar_widgets.toolbar));
    split.set_content(Some(&content_widgets.toolbar));

    toast.set_child(Some(&split));
    win.set_content(Some(&toast));

    let cfg = Rc::new(RefCell::new(config::load()));

    let ui = Ui {
        win:     win.clone(),
        toast:   toast.clone(),
        sidebar: sidebar_widgets,
        content: content_widgets,
    };

    apply_config_to_widgets(&ui, &cfg.borrow());
    setup_menu(&ui, &cfg);
    wire_signals(&ui, &cfg);

    win.present();

    refresh_state(&ui, &cfg);

    {
        let ui2 = ui.clone();
        let cfg2 = cfg.clone();
        glib::timeout_add_seconds_local(30, move || {
            refresh_state(&ui2, &cfg2);
            glib::ControlFlow::Continue
        });
    }

    {
        let c = cfg.borrow().clone();
        if c.auto_connect && !c.authkey.is_empty()
            && tailscale::is_service_active() && !tailscale::get_status().online
        {
            do_connect(&ui, &cfg, c.server_url, c.authkey, c.hostname);
        }
    }
}

// ─── Apply config to UI ──────────────────────────────────────────────────────

fn apply_config_to_widgets(ui: &Ui, c: &config::Config) {
    ui.sidebar.entry_server.set_text(&c.server_url);
    ui.sidebar.entry_key.set_text(&c.authkey);
    ui.sidebar.entry_host.set_text(&c.hostname);
    ui.sidebar.switch_auto.set_active(c.auto_connect);
}

// ─── Menu (hamburger) ────────────────────────────────────────────────────────

fn setup_menu(ui: &Ui, cfg: &Rc<RefCell<config::Config>>) {
    let menu = gtk4::gio::Menu::new();
    menu.append(Some(&tr!("Refresh")),                              Some("win.refresh"));
    menu.append(Some(&tr!("Sign in with panel account")),           Some("win.panel-login"));
    menu.append(Some(&tr!("Sign out")),                             Some("win.sign-out"));
    menu.append(Some(&tr!("Make this user the tailscale operator")), Some("win.set-operator"));
    menu.append(Some(&tr!("About BigLace")),                        Some("win.about"));
    ui.content.btn_menu.set_menu_model(Some(&menu));

    {
        let action = gtk4::gio::SimpleAction::new("about", None);
        let win_w = ui.win.clone();
        action.connect_activate(move |_, _| dialogs::show_about(&win_w));
        ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("set-operator", None);
        let win_w = ui.win.clone();
        action.connect_activate(move |_, _| dialogs::show_set_operator(&win_w));
        ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("refresh", None);
        let ui2 = ui.clone();
        let cfg2 = cfg.clone();
        action.connect_activate(move |_, _| refresh_state(&ui2, &cfg2));
        ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("panel-login", None);
        let win_w = ui.win.clone();
        let toast_w = ui.toast.clone();
        let cfg2 = cfg.clone();
        let sidebar_w = ui.sidebar.clone();
        action.connect_activate(move |_, _| {
            dialogs::show_panel_login(&win_w, &toast_w, &cfg2, &sidebar_w);
        });
        ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("sign-out", None);
        let win_w = ui.win.clone();
        let toast_w = ui.toast.clone();
        let cfg2 = cfg.clone();
        let sidebar_w = ui.sidebar.clone();
        action.connect_activate(move |_, _| {
            dialogs::confirm_sign_out(&win_w, &toast_w, &cfg2, &sidebar_w);
        });
        ui.win.add_action(&action);
    }
}

// ─── Signal wiring ───────────────────────────────────────────────────────────

fn wire_signals(ui: &Ui, cfg: &Rc<RefCell<config::Config>>) {
    // ── Save manual key/server ──
    {
        let ui2 = ui.clone();
        let cfg2 = cfg.clone();
        ui.sidebar.btn_save_manual.connect_clicked(move |_| {
            let server = ui2.sidebar.entry_server.text().to_string();
            let key    = ui2.sidebar.entry_key.text().to_string();
            {
                let mut c = cfg2.borrow_mut();
                c.server_url = server;
                c.authkey    = key;
                config::save(&c).ok();
            }
            ui2.sidebar.expander_manual.set_expanded(false);
            ui2.toast.add_toast(
                libadwaita::Toast::builder()
                    .title(tr!("Setup saved"))
                    .timeout(2)
                    .build(),
            );
            refresh_state(&ui2, &cfg2);
        });
    }

    // ── Device name auto-save ──
    {
        let cfg2 = cfg.clone();
        let entry = ui.sidebar.entry_host.clone();
        ui.sidebar.entry_host.connect_apply(move |_| {
            let mut c = cfg2.borrow_mut();
            c.hostname = entry.text().to_string();
            config::save(&c).ok();
        });
    }

    // ── Auto-connect switch → save ──
    {
        let cfg2 = cfg.clone();
        ui.sidebar.switch_auto.connect_state_set(move |_, active| {
            let mut c = cfg2.borrow_mut();
            c.auto_connect = active;
            config::save(&c).ok();
            glib::Propagation::Proceed
        });
    }

    // ── Connect / Disconnect button ──
    {
        let ui2 = ui.clone();
        let cfg2 = cfg.clone();
        ui.sidebar.btn_connect.connect_clicked(move |btn| {
            // The button's role depends on current state, encoded by its CSS classes.
            if btn.has_css_class("destructive-action") {
                eprintln!("[biglace] disconnect: button clicked");
                let ui3 = ui2.clone();
                let cfg3 = cfg2.clone();
                std::thread::spawn(|| {
                    if let Err(e) = tailscale::disconnect() {
                        eprintln!("[biglace] disconnect: failed: {e}");
                    } else {
                        eprintln!("[biglace] disconnect: ok");
                    }
                });
                glib::timeout_add_local(Duration::from_millis(400), move || {
                    refresh_state(&ui3, &cfg3);
                    glib::ControlFlow::Break
                });
            } else {
                let c = cfg2.borrow().clone();
                if c.authkey.is_empty() {
                    ui2.toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Sign in or paste a pre-auth key first."))
                            .timeout(3)
                            .build(),
                    );
                    return;
                }
                {
                    let mut cm = cfg2.borrow_mut();
                    cm.hostname = ui2.sidebar.entry_host.text().to_string();
                    config::save(&cm).ok();
                }
                do_connect(
                    &ui2, &cfg2,
                    c.server_url, c.authkey,
                    ui2.sidebar.entry_host.text().to_string(),
                );
            }
        });
    }

    // ── Start service button ──
    {
        let ui2 = ui.clone();
        let cfg2 = cfg.clone();
        ui.content.btn_start_service.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            apply_state(&ui2, &cfg2.borrow(), AppState::Connecting, &Status::default(), &[]);

            let slot: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            std::thread::spawn(move || {
                let ok = std::process::Command::new("pkexec")
                    .args(["systemctl", "enable", "--now", "tailscaled"])
                    .status().map(|s| s.success()).unwrap_or(false);
                if let Ok(mut g) = slot_t.lock() { *g = Some(ok); }
            });

            let ui3 = ui2.clone();
            let cfg3 = cfg2.clone();
            let btn3 = btn.clone();
            glib::timeout_add_local(Duration::from_millis(400), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(_) => {
                        btn3.set_sensitive(true);
                        refresh_state(&ui3, &cfg3);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    // ── Copy "this device" IP ──
    {
        let row = ui.content.self_row.clone();
        let toast = ui.toast.clone();
        ui.content.btn_copy_self_ip.connect_clicked(move |btn| {
            let ip = row.subtitle().map(|s| s.to_string()).unwrap_or_default();
            if ip.is_empty() { return; }
            btn.display().clipboard().set_text(&ip);
            toast.add_toast(
                libadwaita::Toast::builder()
                    .title(tr!("IP address copied"))
                    .timeout(2)
                    .build(),
            );
        });
    }
}

// ─── Connect flow ────────────────────────────────────────────────────────────

fn do_connect(
    ui: &Ui,
    cfg: &Rc<RefCell<config::Config>>,
    server: String,
    authkey: String,
    hostname: String,
) {
    eprintln!(
        "[biglace] connect: button clicked (server={server:?} hostname={hostname:?})"
    );
    apply_state(ui, &cfg.borrow(), AppState::Connecting, &Status::default(), &[]);

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

    let ui2 = ui.clone();
    let cfg2 = cfg.clone();
    glib::timeout_add_local(Duration::from_millis(300), move || {
        match slot.lock().ok().and_then(|mut g| g.take()) {
            None => glib::ControlFlow::Continue,
            Some(Ok(())) => {
                refresh_state(&ui2, &cfg2);
                ui2.toast.add_toast(
                    libadwaita::Toast::builder()
                        .title(tr!("Connected"))
                        .timeout(2)
                        .build(),
                );
                glib::ControlFlow::Break
            }
            Some(Err(e)) => {
                refresh_state(&ui2, &cfg2);
                let toast = libadwaita::Toast::builder()
                    .title(trf!("Error: {error}", "error" => e))
                    .timeout(6)
                    .build();
                ui2.toast.add_toast(toast);
                glib::ControlFlow::Break
            }
        }
    });
}

// ─── State refresh & UI binding ──────────────────────────────────────────────

fn refresh_state(ui: &Ui, cfg: &Rc<RefCell<config::Config>>) {
    if !tailscale::is_service_active() {
        apply_state(ui, &cfg.borrow(), AppState::ServiceStopped, &Status::default(), &[]);
        return;
    }

    let status = tailscale::get_status();
    let peers  = if status.online { tailscale::get_peers() } else { vec![] };

    let state = if status.online {
        AppState::Connected
    } else if cfg.borrow().authkey.is_empty() {
        AppState::NotSignedIn
    } else {
        AppState::Disconnected
    };

    apply_state(ui, &cfg.borrow(), state, &status, &peers);
}

fn apply_state(
    ui: &Ui,
    cfg: &config::Config,
    state: AppState,
    status: &Status,
    peers: &[Peer],
) {
    // ── Status dot + label in content header ──
    for c in ["idle", "connected", "connecting", "error"] {
        ui.content.status_dot.remove_css_class(c);
    }
    let (dot_class, status_text) = match state {
        AppState::ServiceStopped => ("error",      tr!("Service stopped")),
        AppState::NotSignedIn    => ("idle",       tr!("Not signed in")),
        AppState::Disconnected   => ("idle",       tr!("Disconnected")),
        AppState::Connecting     => ("connecting", tr!("Connecting…")),
        AppState::Connected      => ("connected",  tr!("Connected")),
    };
    ui.content.status_dot.add_css_class(dot_class);
    ui.content.status_label.set_text(&status_text);

    // ── Stack page ──
    let page = match state {
        AppState::ServiceStopped => "service",
        AppState::NotSignedIn    => "not-signed-in",
        AppState::Disconnected   => "disconnected",
        AppState::Connecting     => "connecting",
        AppState::Connected      => "connected",
    };
    ui.content.stack.set_visible_child_name(page);

    // ── Identity card ──
    let signed_in = !cfg.authkey.is_empty();
    if !signed_in {
        ui.sidebar.identity_row.set_title(&tr!("Not signed in"));
        ui.sidebar.identity_row.set_subtitle(&tr!("Sign in or paste a pre-auth key"));
        ui.sidebar.expander_manual.set_visible(true);
    } else {
        let host = if cfg.hostname.is_empty() { "—".to_string() } else { cfg.hostname.clone() };
        let url  = if cfg.server_url.is_empty() {
            tr!("Server not set")
        } else {
            cfg.server_url.clone()
        };
        ui.sidebar.identity_row.set_title(&host);
        ui.sidebar.identity_row.set_subtitle(&url);
        // Already signed in — hide the manual-key advanced row to avoid
        // suggesting a key still needs to be pasted. It'll come back if the
        // user signs out (clears authkey).
        ui.sidebar.expander_manual.set_visible(false);
        ui.sidebar.expander_manual.set_expanded(false);
    }

    // ── Menu items: gate by sign-in state ──
    // Sign-in actions only make sense when out; sign-out only when in.
    if let Some(act) = ui.win.lookup_action("panel-login").and_downcast::<gtk4::gio::SimpleAction>() {
        act.set_enabled(!signed_in);
    }
    if let Some(act) = ui.win.lookup_action("sign-out").and_downcast::<gtk4::gio::SimpleAction>() {
        act.set_enabled(signed_in);
    }

    // ── Connect / Disconnect button ──
    ui.sidebar.btn_connect.remove_css_class("suggested-action");
    ui.sidebar.btn_connect.remove_css_class("destructive-action");
    match state {
        AppState::Connected => {
            ui.sidebar.btn_connect.set_label(&tr!("Disconnect"));
            ui.sidebar.btn_connect.add_css_class("destructive-action");
            ui.sidebar.btn_connect.set_sensitive(true);
        }
        AppState::Connecting => {
            ui.sidebar.btn_connect.set_label(&tr!("Connecting…"));
            ui.sidebar.btn_connect.add_css_class("suggested-action");
            ui.sidebar.btn_connect.set_sensitive(false);
        }
        AppState::Disconnected => {
            ui.sidebar.btn_connect.set_label(&tr!("Connect"));
            ui.sidebar.btn_connect.add_css_class("suggested-action");
            ui.sidebar.btn_connect.set_sensitive(true);
        }
        AppState::NotSignedIn | AppState::ServiceStopped => {
            ui.sidebar.btn_connect.set_label(&tr!("Connect"));
            ui.sidebar.btn_connect.add_css_class("suggested-action");
            ui.sidebar.btn_connect.set_sensitive(false);
        }
    }

    // ── Connected: this device, peers, bottom bar ──
    if state == AppState::Connected {
        let host = status
            .dns_name
            .as_deref()
            .or(status.hostname.as_deref())
            .unwrap_or("—");
        let ip = status.ip.as_deref().unwrap_or("—");
        ui.content.self_row.set_title(host);
        ui.content.self_row.set_subtitle(ip);

        while let Some(child) = ui.content.peers_list.first_child() {
            ui.content.peers_list.remove(&child);
        }
        for peer in peers {
            ui.content.peers_list.append(&peer_row::build(peer, &ui.toast));
        }

        let online  = peers.iter().filter(|p| p.online).count();
        let offline = peers.len().saturating_sub(online);
        let counts = if peers.is_empty() {
            tr!("no other devices")
        } else if offline == 0 {
            trf!("{online} online", "online" => online)
        } else {
            trf!("{online} online · {offline} offline",
                "online" => online, "offline" => offline)
        };
        ui.content.bottom_label.set_text(&format!("{ip}  ·  {counts}"));
    } else {
        ui.content.bottom_label.set_text("");
    }
}
