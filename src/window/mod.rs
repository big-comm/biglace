mod content;
mod dialogs;
mod peer_row;
mod sidebar;
mod style;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::config;
use crate::panel;
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
    /// hostname → instant of the last `notify-send` we fired for this peer.
    /// Used to debounce notifications when a peer flaps online/offline rapidly
    /// (tailscale's `Online` field can oscillate around DERP heartbeats), so
    /// the user doesn't get spammed by KDE/GNOME's notification daemon.
    last_peer_notifications: Rc<RefCell<HashMap<String, std::time::Instant>>>,
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
    /// `hostname → os_user` cache fetched from the BigScale panel. Filled by
    /// `spawn_device_meta_worker`; merged into peer rows so SSH/SFTP buttons
    /// can compose `<os_user>@<hostname>.bigscale.net`. Empty until the first
    /// successful panel call — peers fall back to using their hostname as
    /// the SSH login while empty.
    device_meta: Arc<Mutex<HashMap<String, String>>>,
    /// Hostnames of peers whose detail panel is currently expanded. Updated
    /// from each row's `notify::expanded` listener; consumed by the next
    /// rebuild to restore the user's open rows so the periodic refresh
    /// doesn't slam expanders shut.
    expanded_peers: Rc<RefCell<HashSet<String>>>,
    /// Hash of the last rendered peer set's structural fields. Compared on
    /// each refresh: if it matches, we skip rebuilding the ListBox and just
    /// update subtitles in place (which is the common path — latency polls
    /// fire every 20s and otherwise nothing has changed). u64::MAX is the
    /// sentinel for "no render yet", so the first refresh always rebuilds.
    last_peer_signature: Rc<RefCell<u64>>,
    /// Live `hostname → row` map for the currently rendered peers, kept in
    /// sync with the ListBox. The soft-refresh path looks up each peer by
    /// hostname and calls `set_subtitle` on its row without touching the
    /// rest of the widget tree.
    peer_rows: Rc<RefCell<HashMap<String, libadwaita::ExpanderRow>>>,
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
        last_peer_notifications: Rc::new(RefCell::new(HashMap::new())),
        health_ok: Arc::new(Mutex::new(true)),
        reconnect_attempts: Rc::new(RefCell::new(0)),
        update_available: Arc::new(Mutex::new(None)),
        device_meta: Arc::new(Mutex::new(HashMap::new())),
        expanded_peers: Rc::new(RefCell::new(HashSet::new())),
        last_peer_signature: Rc::new(RefCell::new(u64::MAX)),
        peer_rows: Rc::new(RefCell::new(HashMap::new())),
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
    spawn_device_meta_worker(&ctx);

    {
        let c = cfg.borrow().clone();
        if c.auto_connect && !c.authkey.is_empty()
            && tailscale::is_service_active() && !tailscale::get_status().online
        {
            do_connect(&ctx, c.server_url, c.authkey, c.hostname.clone());
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
/// a hard 2s cap inside `tailscale::ping_ms`, and we now fan out across up
/// to `PING_PARALLELISM` worker threads, so even a tailnet with dozens of
/// pingable peers finishes inside one batch instead of stretching across
/// minutes of sequential probes. Dead peers are filtered out via `peer.online`.
fn spawn_latency_worker(ctx: &Ctx) {
    /// Cap on concurrent `tailscale ping` invocations. Each ping shells out
    /// to a subprocess and tailscaled handles them concurrently anyway, so
    /// a hard ceiling avoids hammering the daemon on huge tailnets while
    /// still cutting wall-clock time by ~Nx on small ones. Six is enough
    /// for typical home/team tailnets and gentle on shared hardware.
    const PING_PARALLELISM: usize = 6;

    let latency = ctx.latency.clone();
    let tx = ctx.refresh_tx.clone();
    glib::timeout_add_seconds_local(20, move || {
        // Snapshotting peers off the GTK thread keeps the main loop free
        // even if tailscaled is slow — we don't read the result here, the
        // background thread does the parsing + pinging + send().
        let lat = latency.clone();
        let tx_t = tx.clone();
        std::thread::spawn(move || {
            let online_targets: Vec<(String, String)> = tailscale::get_peers()
                .into_iter()
                .filter(|p| p.online && !p.ip.is_empty())
                .map(|p| (p.hostname, p.ip))
                .collect();
            if online_targets.is_empty() {
                return;
            }

            // Work-stealing via a shared `Mutex<Vec<...>>` cursor. Each worker
            // pulls the next target, runs ping_ms (which can block up to 2s),
            // writes the result into the shared latency map. We use `join`
            // on the handles so we only fire one `refresh_tx.send()` at the
            // end of the batch — otherwise N pings would trigger N refreshes.
            let queue = std::sync::Arc::new(std::sync::Mutex::new(online_targets));
            let n_workers = PING_PARALLELISM.min(
                queue.lock().map(|q| q.len()).unwrap_or(1).max(1),
            );
            let mut handles = Vec::with_capacity(n_workers);
            for _ in 0..n_workers {
                let q = queue.clone();
                let lat_w = lat.clone();
                handles.push(std::thread::spawn(move || loop {
                    let next = q.lock().ok().and_then(|mut v| v.pop());
                    let Some((host, ip)) = next else { return };
                    let ms = tailscale::ping_ms(&ip);
                    if let Ok(mut g) = lat_w.lock() {
                        g.insert(host, ms);
                    }
                }));
            }
            for h in handles {
                let _ = h.join();
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
        // Cheap short-circuits first — when neither requirement to ever fire
        // a reconnect is met, we don't even need to know the online state,
        // so we can skip the tailscaled probe entirely. Most users don't
        // have auto-reconnect on, so this is the hot path most of the time.
        if !cfg.auto_reconnect || cfg.authkey.is_empty() {
            return glib::ControlFlow::Continue;
        }
        let now_online = tailscale::is_service_active() && tailscale::get_status().online;
        let prev = *was_online.borrow();
        *was_online.borrow_mut() = now_online;

        if now_online {
            // Reset backoff on successful connect, even if user reconnected manually.
            *ctx2.reconnect_attempts.borrow_mut() = 0;
            return glib::ControlFlow::Continue;
        }
        if !prev {
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
                do_connect(&ctx_inner, c.server_url, c.authkey, c.hostname.clone());
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

/// Periodic round-trip with the BigScale panel for OS-user metadata:
///   1. POST our own `$USER` to `/api/devices/me/os-user` (idempotent — the
///      panel returns 304 when the value is unchanged, so the cost stays low).
///   2. GET `/api/bs/v1/node` and update the `device_meta` cache that
///      peer rows read on each render.
///
/// Skipped while disconnected. Idle on non-BigScale tailnets — `panel::*`
/// resolves the panel via the tailnet's MagicDNS, so a vanilla headscale or
/// Tailscale-oficial tailnet (no `panel.<suffix>`) just short-circuits.
/// Errors are logged and never propagated — a temporarily unreachable panel
/// must not break the rest of the UI.
fn spawn_device_meta_worker(ctx: &Ctx) {
    let device_meta = ctx.device_meta.clone();
    let tx = ctx.refresh_tx.clone();

    let kick = move || {
        // The integration is gated by tailnet identity, not by a panel login —
        // POST /api/devices/me/os-user authenticates by tunnel source IP, so
        // any peer on a BigScale tailnet propagates its $USER automatically,
        // without the user ever opening "Sign in with panel". On non-BigScale
        // tailnets the helpers in `panel::*` short-circuit (DNS for
        // `panel.<MagicDNSSuffix>` doesn't resolve), so this stays a no-op.
        let os_user = config::os_user();
        let device_meta_t = device_meta.clone();
        let tx_t = tx.clone();
        std::thread::spawn(move || {
            // Gating moved off the GTK thread so a slow `systemctl is-active`
            // or `tailscale status --json` (cache miss) doesn't stall the
            // main loop. The cost is negligible — we exit early in the
            // common "not connected" case before any HTTP work happens.
            if !tailscale::is_service_active() || !tailscale::get_status().online {
                return;
            }
            // Best-effort POST first so the GET right after sees our own row
            // populated for the local device.
            if let Err(e) = panel::post_os_user(&os_user) {
                eprintln!("[biglace] panel: post os_user failed: {e}");
            }
            match panel::fetch_device_meta() {
                Ok(map) => {
                    if let Ok(mut g) = device_meta_t.lock() {
                        *g = map;
                    }
                    let _ = tx_t.send(());
                }
                Err(e) => eprintln!("[biglace] panel: fetch device-meta failed: {e}"),
            }
        });
    };

    // First kick a few seconds after startup — gives the daemon time to settle
    // if the user has auto-connect on, so we POST from a real tailnet IP.
    {
        let kick_once = kick.clone();
        glib::timeout_add_seconds_local_once(5, kick_once);
    }
    glib::timeout_add_seconds_local(30, move || {
        kick();
        glib::ControlFlow::Continue
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
            let host   = ctx2.ui.sidebar.entry_host.text().to_string();
            {
                let mut c = ctx2.cfg.borrow_mut();
                c.server_url = server;
                c.authkey    = key;
                c.hostname   = host;
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
                do_connect(
                    &ctx2,
                    c.server_url, c.authkey,
                    c.hostname.clone(),
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
                // Push our $USER to the panel and refresh the peer→os_user
                // cache right away so the SFTP/SSH buttons of the just-listed
                // peers compose the correct login on the very first render
                // — without this, the user would wait up to 30s for the next
                // device-meta worker tick.
                if !ctx2.cfg.borrow().panel_url.is_empty() {
                    let os_user = config::os_user();
                    let device_meta = ctx2.device_meta.clone();
                    let refresh_tx = ctx2.refresh_tx.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = panel::post_os_user(&os_user) {
                            eprintln!("[biglace] panel: post os_user failed: {e}");
                        }
                        match panel::fetch_device_meta() {
                            Ok(map) => {
                                if let Ok(mut g) = device_meta.lock() {
                                    *g = map;
                                }
                                let _ = refresh_tx.send(());
                            }
                            Err(e) => eprintln!("[biglace] panel: fetch device-meta failed: {e}"),
                        }
                    });
                }
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

    // One subprocess + JSON parse covers both. Cuts the periodic-refresh
    // tailscaled hit in half compared to separate get_status / get_peers.
    let (status, mut peers) = tailscale::get_status_and_peers();
    if !status.online {
        peers.clear();
    }

    // Enrich peers with the OS user the panel knows for each hostname. Empty
    // map = "panel hasn't been polled yet (or is unreachable)" — peer rows
    // fall back to the hostname for SSH composition.
    if let Ok(meta) = ctx.device_meta.lock() {
        for p in peers.iter_mut() {
            if let Some(u) = meta.get(&p.hostname) {
                p.ssh_user = u.clone();
            }
        }
    }

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
        // Show the BigScale identifier the user picked (or a placeholder if
        // they haven't typed one yet). The OS user is intentionally not shown
        // here — it's only relevant for SSH composition, not for "who am I".
        let host = if cfg_snap.hostname.is_empty() {
            tr!("(no device name)")
        } else {
            cfg_snap.hostname.clone()
        };
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
        let display = status.display_name();
        let title = if display.is_empty() { "—".to_string() } else { display };
        let ip = status.ip.as_deref().unwrap_or("—");
        ui.content.self_row.set_title(&title);
        ui.content.self_row.set_subtitle(ip);

        // Diff against previous render to fire libnotify on transitions.
        // Per-peer 60s cooldown swallows flaps — tailscale's `Online` field
        // can oscillate around DERP heartbeats and we don't want every blip
        // to show up as a popup.
        let mut prev = ctx.last_peer_states.borrow_mut();
        if cfg_snap.notify_peer_changes && !prev.is_empty() {
            let mut last_notif = ctx.last_peer_notifications.borrow_mut();
            let now = std::time::Instant::now();
            let cooldown = std::time::Duration::from_secs(60);
            for p in peers {
                let was = prev.get(&p.hostname).copied();
                let transition = match (was, p.online) {
                    (Some(true), false) => Some(false),
                    (Some(false), true) => Some(true),
                    _ => None,
                };
                if let Some(state) = transition {
                    let recent = last_notif
                        .get(&p.hostname)
                        .map(|t| now.duration_since(*t) < cooldown)
                        .unwrap_or(false);
                    if !recent {
                        notify_peer_change(&p.display_name(), state);
                        last_notif.insert(p.hostname.clone(), now);
                    }
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

        // Diff against the previous render. Latency changes — by far the most
        // frequent refresh trigger (20s worker) — never alter the signature,
        // so we only rewrite the subtitle of each existing row. A full rebuild
        // only happens when the structural shape changes (peer added/removed,
        // online flag flipped, pin toggled, IP/DNS/owner moved, etc.).
        let new_sig = compute_peer_signature(&sorted, &cfg_snap);
        let prev_sig = *ctx.last_peer_signature.borrow();
        let row_map_has_all = {
            let m = ctx.peer_rows.borrow();
            !m.is_empty() && sorted.iter().all(|p| m.contains_key(&p.hostname))
        };

        if new_sig == prev_sig && row_map_has_all {
            let rows = ctx.peer_rows.borrow();
            for peer in &sorted {
                if let Some(row) = rows.get(&peer.hostname) {
                    row.set_subtitle(&peer_row::compose_subtitle(peer, &ctx.latency));
                }
            }
        } else {
            *ctx.last_peer_signature.borrow_mut() = new_sig;
            while let Some(child) = ui.content.peers_list.first_child() {
                ui.content.peers_list.remove(&child);
            }
            let refresh_cb: Rc<dyn Fn()> = {
                let ctx2 = ctx.clone();
                Rc::new(move || refresh_state(&ctx2))
            };
            let peer_ctx = peer_row::PeerCtx {
                toast:    ui.toast.clone(),
                cfg:      ctx.cfg.clone(),
                latency:  ctx.latency.clone(),
                refresh:  refresh_cb,
                expanded: ctx.expanded_peers.clone(),
            };
            let mut new_rows: HashMap<String, libadwaita::ExpanderRow> =
                HashMap::with_capacity(sorted.len());
            for peer in &sorted {
                let row = peer_row::build(peer, &peer_ctx);
                ui.content.peers_list.append(&row);
                new_rows.insert(peer.hostname.clone(), row);
            }
            *ctx.peer_rows.borrow_mut() = new_rows;
            // Drop expanded entries for peers that no longer exist so the
            // set doesn't grow unbounded across long sessions.
            let live: HashSet<&str> = sorted.iter().map(|p| p.hostname.as_str()).collect();
            ctx.expanded_peers.borrow_mut().retain(|h| live.contains(h.as_str()));
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
        // Invalidate render caches so we don't try to "soft-refresh" a list
        // that no longer matches what the user sees (the connected page is
        // hidden in this branch). The next time we re-enter Connected the
        // first refresh will repopulate them with a fresh rebuild.
        ctx.peer_rows.borrow_mut().clear();
        *ctx.last_peer_signature.borrow_mut() = u64::MAX;
    }

    // ── Tray indicator ──
    let connected = state == AppState::Connected;
    let online_peers = peers.iter().filter(|p| p.online).count();
    update_tray(ctx, connected, online_peers);
}

/// Hash the structural fields of the rendered peer list, in render order.
/// Anything that would change the layout, the prefix/suffix buttons, or the
/// detail rows of an ExpanderRow goes in here; latency is intentionally
/// excluded — that's the field the soft-refresh path updates without
/// rebuilding. Favorites are folded in because pin state affects sort order.
fn compute_peer_signature(sorted: &[&Peer], cfg: &config::Config) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sorted.len().hash(&mut h);
    for p in sorted {
        p.hostname.hash(&mut h);
        p.online.hash(&mut h);
        p.ip.hash(&mut h);
        p.ipv4.hash(&mut h);
        p.ipv6.hash(&mut h);
        p.dns_name.hash(&mut h);
        p.os.hash(&mut h);
        p.user.hash(&mut h);
        p.ssh_user.hash(&mut h);
        p.exit_node_offered.hash(&mut h);
        p.exit_node_active.hash(&mut h);
        p.tags.hash(&mut h);
        // last_seen only renders while offline; include it so a stale
        // timestamp on a still-offline peer doesn't keep the row out of date.
        if !p.online {
            p.last_seen.hash(&mut h);
        }
        cfg.is_favorite(&p.hostname).hash(&mut h);
        cfg.peer_overrides.get(&p.hostname).hash(&mut h);
    }
    h.finish()
}

/// Send a desktop notification via `notify-send`. Silent on failure (no
/// notification daemon, missing binary, …) — never crash the GTK loop on
/// a missing optional integration.
fn notify_peer_change(name: &str, online: bool) {
    let summary = if online {
        format!("{name} {}", tr!("is online"))
    } else {
        format!("{name} {}", tr!("went offline"))
    };
    let icon = if online { "network-transmit-receive-symbolic" } else { "network-offline-symbolic" };
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=BigLace", "--icon", icon, "--", &summary])
        .spawn();
}

