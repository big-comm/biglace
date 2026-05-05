mod content;
mod dialogs;
mod peer_row;
mod sidebar;
mod style;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::tailscale::{self, Peer, Status};
use crate::tr;
use crate::trf;
#[cfg(target_os = "linux")]
use crate::tray;

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

/// Cross-thread handle to the tray indicator. Wrapped in `Rc<RefCell<Option<…>>>`
/// so callers can update it from the GTK loop and so the optional case
/// (headless / no D-Bus / non-Linux) is a single `borrow().as_ref()` away.
#[cfg(target_os = "linux")]
type TrayHandle = Rc<RefCell<Option<ksni::blocking::Handle<tray::BigLaceTray>>>>;
#[cfg(not(target_os = "linux"))]
type TrayHandle = Rc<RefCell<Option<()>>>;

/// Aggregates everything the UI flow needs into one struct so signal
/// handlers can capture a single `Ctx` clone instead of juggling six Rcs.
///
/// Fields fall into two categories:
/// - **Main-thread only** (`Rc<RefCell<…>>`): mutated only from GTK callbacks.
/// - **Cross-thread** (`Arc<Mutex<…>>`): written from worker threads and read
///   on the main loop. Workers must not capture `Ctx` directly — it isn't
///   `Send`. Instead they clone the specific Arc + the `refresh_tx` sender
///   they need.
#[derive(Clone)]
struct Ctx {
    ui:    Ui,
    cfg:   Rc<RefCell<config::Config>>,
    tray:  TrayHandle,
    /// hostname → most recent ping, if any. None = "queried but timed out".
    /// Missing = "haven't measured yet".
    latency: Arc<Mutex<HashMap<String, Option<f64>>>>,
    /// hostname → online flag from the previous render. Diff'd against the
    /// current render to fire desktop notifications on transitions.
    last_peer_states: Rc<RefCell<HashMap<String, bool>>>,
    /// Most recent Headscale `/health` result. Drives the badge in the header.
    /// Defaults to true so a fresh launch doesn't briefly flash a red banner.
    health_ok: Arc<Mutex<bool>>,
    /// Auto-reconnect attempt counter — used to compute the exponential
    /// backoff delay. Cleared whenever a connect succeeds.
    reconnect_attempts: Rc<RefCell<u32>>,
    /// Human-readable text of the latest biglace release on Github, when a
    /// version newer than the running build is found. None means "no update
    /// to advertise" (offline check, same version, or check failed).
    update_available: Arc<Mutex<Option<String>>>,
    /// Worker threads send `()` here to ask the main loop to call
    /// `refresh_state`. A poller in `build()` drains the receiver. This is
    /// our cross-thread bridge — the closure captured by `idle_add_once` only
    /// needs to be `Send`-friendly types, and we never pass `Ctx` itself
    /// across the thread boundary.
    refresh_tx: mpsc::Sender<()>,
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

    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

    let ctx = Ctx {
        ui:    ui.clone(),
        cfg:   cfg.clone(),
        tray:  Rc::new(RefCell::new(None)),
        latency: Arc::new(Mutex::new(HashMap::new())),
        last_peer_states: Rc::new(RefCell::new(HashMap::new())),
        health_ok: Arc::new(Mutex::new(true)),
        reconnect_attempts: Rc::new(RefCell::new(0)),
        update_available: Arc::new(Mutex::new(None)),
        refresh_tx,
    };

    apply_config_to_widgets(&ui, &cfg.borrow());
    setup_menu(&ctx);
    wire_signals(&ctx);
    let tray_active = setup_tray(&ctx);
    setup_close_to_tray(&ctx, tray_active);

    // Drain refresh signals from background workers and apply on the main
    // thread. Coalesce bursts (`while try_recv()`) so a flurry of pings only
    // triggers one refresh.
    {
        let ctx2 = ctx.clone();
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let mut got = false;
            while refresh_rx.try_recv().is_ok() {
                got = true;
            }
            if got {
                refresh_state(&ctx2);
            }
            glib::ControlFlow::Continue
        });
    }

    win.present();

    refresh_state(&ctx);

    // Periodic refresh of tailscaled status. Keep at 30s so we don't spam
    // `tailscale status --json`; transitions still get caught quickly because
    // explicit user actions also call refresh_state directly.
    {
        let ctx2 = ctx.clone();
        glib::timeout_add_seconds_local(30, move || {
            refresh_state(&ctx2);
            glib::ControlFlow::Continue
        });
    }

    // Background workers (latency, health, auto-reconnect, self-update).
    spawn_latency_worker(&ctx);
    spawn_health_worker(&ctx);
    spawn_reconnect_worker(&ctx);
    spawn_update_check(&ctx);

    {
        let c = cfg.borrow().clone();
        if c.auto_connect && !c.authkey.is_empty()
            && tailscale::is_service_active() && !tailscale::get_status().online
        {
            do_connect(&ctx, c.server_url, c.authkey, c.hostname);
        }
    }
}

// ─── Tray (StatusNotifierItem) ───────────────────────────────────────────────

/// Spawn the system-tray indicator and wire its actions to the existing UI
/// flows. Returns true when the tray was registered successfully — callers
/// use this to decide whether the close button should hide instead of quit.
#[cfg(target_os = "linux")]
fn setup_tray(ctx: &Ctx) -> bool {
    let Some((rx, handle)) = tray::spawn() else { return false; };
    *ctx.tray.borrow_mut() = Some(handle);

    let ctx2 = ctx.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                tray::TrayCommand::Show => {
                    ctx2.ui.win.present();
                }
                tray::TrayCommand::Connect => {
                    if ctx2.ui.sidebar.btn_connect.is_sensitive()
                        && !ctx2.ui.sidebar.btn_connect.has_css_class("destructive-action")
                    {
                        ctx2.ui.sidebar.btn_connect.emit_clicked();
                    }
                }
                tray::TrayCommand::Disconnect => {
                    if ctx2.ui.sidebar.btn_connect.has_css_class("destructive-action") {
                        ctx2.ui.sidebar.btn_connect.emit_clicked();
                    }
                }
                tray::TrayCommand::Quit => {
                    if let Some(app) = ctx2.ui.win.application() {
                        app.quit();
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
    true
}

#[cfg(not(target_os = "linux"))]
fn setup_tray(_ctx: &Ctx) -> bool { false }

/// Push the current connection state into the tray indicator (tooltip + the
/// connect/disconnect menu label flip). No-op when the tray failed to spawn.
#[cfg(target_os = "linux")]
fn update_tray(ctx: &Ctx, connected: bool, peer_count: usize) {
    if let Some(h) = ctx.tray.borrow().as_ref() {
        h.update(|t| {
            t.connected = connected;
            t.peer_count = peer_count;
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn update_tray(_ctx: &Ctx, _connected: bool, _peer_count: usize) {}

/// Hijack the window's close button so it hides the window instead of
/// destroying it — the tray icon is the live presence and quitting only
/// happens via the tray's "Quit" item or the in-app menu. Without an active
/// tray we leave the default behavior alone (otherwise closing would orphan
/// the user with no way back).
fn setup_close_to_tray(ctx: &Ctx, tray_active: bool) {
    if !tray_active {
        return;
    }
    let win_w = ctx.ui.win.clone();
    ctx.ui.win.connect_close_request(move |_| {
        win_w.set_visible(false);
        glib::Propagation::Stop
    });
}

// ─── Background workers ──────────────────────────────────────────────────────

/// Re-measure latency to every online peer roughly every 20s. Each ping has
/// a hard 2s cap inside `tailscale::ping_ms`, so a frota of 50 dead peers
/// can't stall the worker for longer than ~100s — and dead peers won't be
/// pinged anyway because we filter on `peer.online`.
fn spawn_latency_worker(ctx: &Ctx) {
    let latency = ctx.latency.clone();
    let tx = ctx.refresh_tx.clone();
    glib::timeout_add_seconds_local(20, move || {
        let online_targets: Vec<(String, String)> = tailscale::get_peers()
            .into_iter()
            .filter(|p| p.online && !p.ip.is_empty())
            .map(|p| (p.hostname, p.ip))
            .collect();

        let lat = latency.clone();
        let tx_t = tx.clone();
        std::thread::spawn(move || {
            for (host, ip) in online_targets {
                let ms = tailscale::ping_ms(&ip);
                if let Ok(mut g) = lat.lock() {
                    g.insert(host, ms);
                }
            }
            let _ = tx_t.send(());
        });
        glib::ControlFlow::Continue
    });
}

/// Poll Headscale's `/health` every 60s and update the badge in the header.
/// Skipped while the user has no server configured.
fn spawn_health_worker(ctx: &Ctx) {
    let kick = |url: String, health_ok: Arc<Mutex<bool>>, tx: mpsc::Sender<()>| {
        std::thread::spawn(move || {
            let ok = tailscale::headscale_healthy(&url);
            if let Ok(mut g) = health_ok.lock() {
                *g = ok;
            }
            let _ = tx.send(());
        });
    };

    // First check on startup.
    {
        let url = ctx.cfg.borrow().server_url.clone();
        if !url.is_empty() {
            kick(url, ctx.health_ok.clone(), ctx.refresh_tx.clone());
        }
    }

    let cfg = ctx.cfg.clone();
    let health_ok = ctx.health_ok.clone();
    let tx = ctx.refresh_tx.clone();
    glib::timeout_add_seconds_local(60, move || {
        let url = cfg.borrow().server_url.clone();
        if !url.is_empty() {
            kick(url, health_ok.clone(), tx.clone());
        }
        glib::ControlFlow::Continue
    });
}

/// Watch tailscaled's online state and, when the user opted into auto-reconnect,
/// trigger a connect with exponential backoff after a drop. Capped at 5 min
/// so a permanent outage doesn't burn a connect attempt every second.
fn spawn_reconnect_worker(ctx: &Ctx) {
    let ctx2 = ctx.clone();
    let was_online = Rc::new(RefCell::new(false));
    glib::timeout_add_seconds_local(15, move || {
        let cfg = ctx2.cfg.borrow().clone();
        let now_online = tailscale::is_service_active() && tailscale::get_status().online;
        let prev = *was_online.borrow();
        *was_online.borrow_mut() = now_online;

        if now_online {
            // Reset backoff on successful connect, even if user reconnected manually.
            *ctx2.reconnect_attempts.borrow_mut() = 0;
            return glib::ControlFlow::Continue;
        }
        if !cfg.auto_reconnect || cfg.authkey.is_empty() || !prev {
            return glib::ControlFlow::Continue;
        }
        // Exponential backoff with cap: 15s, 30s, 1m, 2m, 4m, 5m, 5m, ...
        let n = *ctx2.reconnect_attempts.borrow();
        let delay = (15u64 << n.min(5)).min(300);
        eprintln!("[biglace] auto-reconnect: drop detected, retrying in {delay}s (attempt {})", n + 1);
        *ctx2.reconnect_attempts.borrow_mut() = n + 1;

        let ctx_inner = ctx2.clone();
        glib::timeout_add_seconds_local_once(delay as u32, move || {
            // Double-check we're still offline before firing.
            if tailscale::is_service_active() && !tailscale::get_status().online {
                let c = ctx_inner.cfg.borrow().clone();
                do_connect(&ctx_inner, c.server_url, c.authkey, c.hostname);
            }
        });
        glib::ControlFlow::Continue
    });
}

/// One-shot Github releases check. Compares `crate::APP_VERSION` against the
/// `tag_name` returned by the API and stashes the new version string in
/// `update_available` for the header badge to pick up. Silent on failure —
/// no internet, rate-limit, or repo rename should never spam the user.
fn spawn_update_check(ctx: &Ctx) {
    let update_available = ctx.update_available.clone();
    let tx = ctx.refresh_tx.clone();
    std::thread::spawn(move || {
        let url = "https://api.github.com/repos/communitybig/biglace/releases/latest";
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(8))
            .build();
        let Ok(resp) = agent
            .get(url)
            .set("User-Agent", "biglace-update-check")
            .set("Accept", "application/vnd.github+json")
            .call()
        else {
            return;
        };
        let Ok(body) = resp.into_string() else { return; };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else { return; };
        let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else { return; };
        let latest = tag.trim_start_matches('v').to_string();
        let current = crate::APP_VERSION;
        if version_is_newer(&latest, current) {
            if let Ok(mut g) = update_available.lock() {
                *g = Some(latest);
            }
            let _ = tx.send(());
        }
    });
}

/// Naive semver-ish comparison: split on `.`, parse u32 components,
/// compare lexicographically. Pre-release tags (`-rc1` etc.) make this
/// imprecise but biglace doesn't currently ship those, so good enough.
fn version_is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect()
    };
    parse(candidate) > parse(current)
}

// ─── Apply config to UI ──────────────────────────────────────────────────────

fn apply_config_to_widgets(ui: &Ui, c: &config::Config) {
    ui.sidebar.entry_server.set_text(&c.server_url);
    ui.sidebar.entry_key.set_text(&c.authkey);
    ui.sidebar.entry_host.set_text(&c.hostname);
    ui.sidebar.switch_auto.set_active(c.auto_connect);
    ui.sidebar.switch_auto_reconnect.set_active(c.auto_reconnect);
    ui.sidebar.switch_notify.set_active(c.notify_peer_changes);
}

// ─── Menu (hamburger) ────────────────────────────────────────────────────────

fn setup_menu(ctx: &Ctx) {
    let menu = gtk4::gio::Menu::new();
    menu.append(Some(&tr!("Refresh")),                              Some("win.refresh"));
    menu.append(Some(&tr!("Sign in with panel account")),           Some("win.panel-login"));
    menu.append(Some(&tr!("Sign out")),                             Some("win.sign-out"));
    menu.append(Some(&tr!("Make this user the tailscale operator")), Some("win.set-operator"));
    menu.append(Some(&tr!("View tailscaled logs")),                 Some("win.view-logs"));
    menu.append(Some(&tr!("About BigLace")),                        Some("win.about"));
    ctx.ui.content.btn_menu.set_menu_model(Some(&menu));

    {
        let action = gtk4::gio::SimpleAction::new("about", None);
        let win_w = ctx.ui.win.clone();
        action.connect_activate(move |_, _| dialogs::show_about(&win_w));
        ctx.ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("set-operator", None);
        let win_w = ctx.ui.win.clone();
        action.connect_activate(move |_, _| dialogs::show_set_operator(&win_w));
        ctx.ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("view-logs", None);
        action.connect_activate(move |_, _| tailscale::open_logs());
        ctx.ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("refresh", None);
        let ctx2 = ctx.clone();
        action.connect_activate(move |_, _| refresh_state(&ctx2));
        ctx.ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("panel-login", None);
        let win_w = ctx.ui.win.clone();
        let toast_w = ctx.ui.toast.clone();
        let cfg2 = ctx.cfg.clone();
        let sidebar_w = ctx.ui.sidebar.clone();
        action.connect_activate(move |_, _| {
            dialogs::show_panel_login(&win_w, &toast_w, &cfg2, &sidebar_w);
        });
        ctx.ui.win.add_action(&action);
    }

    {
        let action = gtk4::gio::SimpleAction::new("sign-out", None);
        let win_w = ctx.ui.win.clone();
        let toast_w = ctx.ui.toast.clone();
        let cfg2 = ctx.cfg.clone();
        let sidebar_w = ctx.ui.sidebar.clone();
        action.connect_activate(move |_, _| {
            dialogs::confirm_sign_out(&win_w, &toast_w, &cfg2, &sidebar_w);
        });
        ctx.ui.win.add_action(&action);
    }
}

// ─── Signal wiring ───────────────────────────────────────────────────────────

fn wire_signals(ctx: &Ctx) {
    // ── Save manual key/server ──
    {
        let ctx2 = ctx.clone();
        ctx.ui.sidebar.btn_save_manual.connect_clicked(move |_| {
            let server = ctx2.ui.sidebar.entry_server.text().to_string();
            let key    = ctx2.ui.sidebar.entry_key.text().to_string();
            {
                let mut c = ctx2.cfg.borrow_mut();
                c.server_url = server;
                c.authkey    = key;
                config::save(&c).ok();
            }
            ctx2.ui.sidebar.expander_manual.set_expanded(false);
            ctx2.ui.toast.add_toast(
                libadwaita::Toast::builder()
                    .title(tr!("Setup saved"))
                    .timeout(2)
                    .build(),
            );
            refresh_state(&ctx2);
        });
    }

    // ── Device name auto-save ──
    {
        let cfg2 = ctx.cfg.clone();
        let entry = ctx.ui.sidebar.entry_host.clone();
        ctx.ui.sidebar.entry_host.connect_apply(move |_| {
            let mut c = cfg2.borrow_mut();
            c.hostname = entry.text().to_string();
            config::save(&c).ok();
        });
    }

    // ── Auto-connect switch → save ──
    {
        let cfg2 = ctx.cfg.clone();
        ctx.ui.sidebar.switch_auto.connect_state_set(move |_, active| {
            let mut c = cfg2.borrow_mut();
            c.auto_connect = active;
            config::save(&c).ok();
            glib::Propagation::Proceed
        });
    }

    // ── Auto-reconnect switch ──
    {
        let cfg2 = ctx.cfg.clone();
        ctx.ui.sidebar.switch_auto_reconnect.connect_state_set(move |_, active| {
            let mut c = cfg2.borrow_mut();
            c.auto_reconnect = active;
            config::save(&c).ok();
            glib::Propagation::Proceed
        });
    }

    // ── Peer-change notifications switch ──
    {
        let cfg2 = ctx.cfg.clone();
        ctx.ui.sidebar.switch_notify.connect_state_set(move |_, active| {
            let mut c = cfg2.borrow_mut();
            c.notify_peer_changes = active;
            config::save(&c).ok();
            glib::Propagation::Proceed
        });
    }

    // ── Connect / Disconnect button ──
    {
        let ctx2 = ctx.clone();
        ctx.ui.sidebar.btn_connect.connect_clicked(move |btn| {
            // The button's role depends on current state, encoded by its CSS classes.
            if btn.has_css_class("destructive-action") {
                eprintln!("[biglace] disconnect: button clicked");
                let ctx3 = ctx2.clone();
                std::thread::spawn(|| {
                    if let Err(e) = tailscale::disconnect() {
                        eprintln!("[biglace] disconnect: failed: {e}");
                    } else {
                        eprintln!("[biglace] disconnect: ok");
                    }
                });
                glib::timeout_add_local(Duration::from_millis(400), move || {
                    refresh_state(&ctx3);
                    glib::ControlFlow::Break
                });
            } else {
                let c = ctx2.cfg.borrow().clone();
                if c.authkey.is_empty() {
                    ctx2.ui.toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Sign in or paste a pre-auth key first."))
                            .timeout(3)
                            .build(),
                    );
                    return;
                }
                {
                    let mut cm = ctx2.cfg.borrow_mut();
                    cm.hostname = ctx2.ui.sidebar.entry_host.text().to_string();
                    config::save(&cm).ok();
                }
                do_connect(
                    &ctx2,
                    c.server_url, c.authkey,
                    ctx2.ui.sidebar.entry_host.text().to_string(),
                );
            }
        });
    }

    // ── Start service button ──
    {
        let ctx2 = ctx.clone();
        ctx.ui.content.btn_start_service.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            apply_state(&ctx2, AppState::Connecting, &Status::default(), &[]);

            let slot: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
            let slot_t = slot.clone();
            std::thread::spawn(move || {
                let ok = std::process::Command::new("pkexec")
                    .args(["systemctl", "enable", "--now", "tailscaled"])
                    .status().map(|s| s.success()).unwrap_or(false);
                if let Ok(mut g) = slot_t.lock() { *g = Some(ok); }
            });

            let ctx3 = ctx2.clone();
            let btn3 = btn.clone();
            glib::timeout_add_local(Duration::from_millis(400), move || {
                match slot.lock().ok().and_then(|mut g| g.take()) {
                    None => glib::ControlFlow::Continue,
                    Some(_) => {
                        btn3.set_sensitive(true);
                        refresh_state(&ctx3);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    // ── Copy "this device" IP ──
    {
        let row = ctx.ui.content.self_row.clone();
        let toast = ctx.ui.toast.clone();
        ctx.ui.content.btn_copy_self_ip.connect_clicked(move |btn| {
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
    ctx: &Ctx,
    server: String,
    authkey: String,
    hostname: String,
) {
    eprintln!(
        "[biglace] connect: button clicked (server={server:?} hostname={hostname:?})"
    );
    apply_state(ctx, AppState::Connecting, &Status::default(), &[]);

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

    let ctx2 = ctx.clone();
    glib::timeout_add_local(Duration::from_millis(300), move || {
        match slot.lock().ok().and_then(|mut g| g.take()) {
            None => glib::ControlFlow::Continue,
            Some(Ok(())) => {
                refresh_state(&ctx2);
                ctx2.ui.toast.add_toast(
                    libadwaita::Toast::builder()
                        .title(tr!("Connected"))
                        .timeout(2)
                        .build(),
                );
                glib::ControlFlow::Break
            }
            Some(Err(e)) => {
                refresh_state(&ctx2);
                let toast = libadwaita::Toast::builder()
                    .title(trf!("Error: {error}", "error" => e))
                    .timeout(6)
                    .build();
                ctx2.ui.toast.add_toast(toast);
                glib::ControlFlow::Break
            }
        }
    });
}

// ─── State refresh & UI binding ──────────────────────────────────────────────

fn refresh_state(ctx: &Ctx) {
    if !tailscale::is_service_active() {
        apply_state(ctx, AppState::ServiceStopped, &Status::default(), &[]);
        return;
    }

    let status = tailscale::get_status();
    let peers  = if status.online { tailscale::get_peers() } else { vec![] };

    let state = if status.online {
        AppState::Connected
    } else if ctx.cfg.borrow().authkey.is_empty() {
        AppState::NotSignedIn
    } else {
        AppState::Disconnected
    };

    apply_state(ctx, state, &status, &peers);
}

fn apply_state(
    ctx: &Ctx,
    state: AppState,
    status: &Status,
    peers: &[Peer],
) {
    let ui = &ctx.ui;
    let cfg_snap = ctx.cfg.borrow().clone();

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

    // ── Headscale health badge (header) ──
    // Only flag the server as unreachable when *we're not connected*. If the
    // tunnel is up, the server is obviously routable — and many Headscale
    // deployments behind nginx/Caddy don't expose /health anyway, so a
    // failed probe there is not a useful signal while connected.
    let health_ok = ctx.health_ok.lock().map(|g| *g).unwrap_or(true);
    let server_set = !cfg_snap.server_url.is_empty();
    let show_health_badge = server_set
        && !health_ok
        && matches!(state, AppState::Disconnected | AppState::NotSignedIn);
    if show_health_badge {
        ui.content.health_badge.set_visible(true);
        ui.content.health_badge.set_label(&tr!("Server unreachable"));
    } else {
        ui.content.health_badge.set_visible(false);
    }

    // ── Self-update banner ──
    let latest_release = ctx.update_available.lock().ok().and_then(|g| g.clone());
    if let Some(latest) = latest_release {
        ui.content.update_badge.set_visible(true);
        ui.content.update_badge.set_label(&trf!(
            "Update available: v{version}",
            "version" => latest
        ));
    } else {
        ui.content.update_badge.set_visible(false);
    }

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
    let signed_in = !cfg_snap.authkey.is_empty();
    if !signed_in {
        ui.sidebar.identity_row.set_title(&tr!("Not signed in"));
        ui.sidebar.identity_row.set_subtitle(&tr!("Sign in or paste a pre-auth key"));
        ui.sidebar.expander_manual.set_visible(true);
    } else {
        let host = if cfg_snap.hostname.is_empty() { "—".to_string() } else { cfg_snap.hostname.clone() };
        let url  = if cfg_snap.server_url.is_empty() {
            tr!("Server not set")
        } else {
            cfg_snap.server_url.clone()
        };
        ui.sidebar.identity_row.set_title(&host);
        ui.sidebar.identity_row.set_subtitle(&url);
        ui.sidebar.expander_manual.set_visible(false);
        ui.sidebar.expander_manual.set_expanded(false);
    }

    // ── Menu items: gate by sign-in state ──
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

        // Diff against previous render to fire libnotify on transitions.
        let mut prev = ctx.last_peer_states.borrow_mut();
        if cfg_snap.notify_peer_changes && !prev.is_empty() {
            for p in peers {
                let was = prev.get(&p.hostname).copied();
                if was == Some(true) && !p.online {
                    notify_peer_change(&p.hostname, false);
                } else if was == Some(false) && p.online {
                    notify_peer_change(&p.hostname, true);
                }
            }
        }
        prev.clear();
        for p in peers {
            prev.insert(p.hostname.clone(), p.online);
        }
        drop(prev);

        // Sort: favorites → online → alpha. We keep the get_peers() order
        // (online → alpha) and just stable-partition pinned ones to the top.
        let mut sorted: Vec<&Peer> = peers.iter().collect();
        sorted.sort_by(|a, b| {
            let af = cfg_snap.is_favorite(&a.hostname);
            let bf = cfg_snap.is_favorite(&b.hostname);
            bf.cmp(&af)
                .then(b.online.cmp(&a.online))
                .then(a.hostname.cmp(&b.hostname))
        });

        while let Some(child) = ui.content.peers_list.first_child() {
            ui.content.peers_list.remove(&child);
        }
        let refresh_cb: Rc<dyn Fn()> = {
            let ctx2 = ctx.clone();
            Rc::new(move || refresh_state(&ctx2))
        };
        let peer_ctx = peer_row::PeerCtx {
            toast:   ui.toast.clone(),
            cfg:     ctx.cfg.clone(),
            latency: ctx.latency.clone(),
            refresh: refresh_cb,
        };
        for peer in sorted {
            ui.content.peers_list.append(&peer_row::build(peer, &peer_ctx));
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
        ctx.last_peer_states.borrow_mut().clear();
    }

    // ── Tray indicator ──
    let connected = state == AppState::Connected;
    let online_peers = peers.iter().filter(|p| p.online).count();
    update_tray(ctx, connected, online_peers);
}

/// Send a desktop notification via `notify-send`. Silent on failure (no
/// notification daemon, missing binary, …) — never crash the GTK loop on
/// a missing optional integration.
fn notify_peer_change(hostname: &str, online: bool) {
    let summary = if online {
        format!("{hostname} {}", tr!("is online"))
    } else {
        format!("{hostname} {}", tr!("went offline"))
    };
    let icon = if online { "network-transmit-receive-symbolic" } else { "network-offline-symbolic" };
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=BigLace", "--icon", icon, "--", &summary])
        .spawn();
}

