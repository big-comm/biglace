mod content;
mod dialogs;
mod peer_row;
mod sidebar;
mod style;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::autostart;
use crate::config;
use crate::panel;
use crate::tailscale::{self, Peer, Status};
use crate::tr;
use crate::tray;
use crate::trf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AppState {
    ServiceStopped,
    NotSignedIn,
    Disconnected,
    Connecting,
    Connected,
}

/// Snapshot of tailscaled state gathered on a worker thread and handed back to
/// the main loop. Both variants only carry `Send` plain-data types so the
/// gather can happen off the GTK thread — a hung tailscaled (common after
/// suspend/resume) then can't freeze the UI while `refresh_state` blocks on a
/// subprocess.
enum RefreshData {
    ServiceStopped,
    Live { status: Status, peers: Vec<Peer> },
}

#[derive(Clone)]
struct Ui {
    win: libadwaita::ApplicationWindow,
    toast: libadwaita::ToastOverlay,
    sidebar: sidebar::Sidebar,
    content: content::Content,
}

/// Cross-thread handle to the tray indicator. Wrapped in `Rc<RefCell<Option<…>>>`
/// so callers can update it from the GTK loop and so the optional case
/// (headless / no D-Bus / tray init failed / unsupported target) is a single
/// `borrow().as_ref()` away.
type TrayHandle = Rc<RefCell<Option<tray::Handle>>>;

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
    ui: Ui,
    cfg: Rc<RefCell<config::Config>>,
    tray: TrayHandle,
    /// peer id → most recent ping, if any. None = "queried but timed out".
    /// Missing = "haven't measured yet".
    latency: Arc<Mutex<HashMap<String, Option<f64>>>>,
    /// peer id → online flag from the previous render. Diff'd against the
    /// current render to fire desktop notifications on transitions. Keyed by
    /// node id: with hostnames, a live node and a stale offline node sharing a
    /// name overwrote each other and announced "online" on every refresh.
    last_peer_states: Rc<RefCell<HashMap<String, bool>>>,
    /// peer id → instant of the last `notify-send` we fired for this peer.
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
    /// SourceId of the pending backoff `timeout_add_seconds_local_once` armed
    /// by the reconnect worker, when one is queued. The disconnect button
    /// takes it out and removes the source so a manual disconnect doesn't get
    /// silently undone by a stale auto-reconnect that was already in flight.
    pending_reconnect: Rc<RefCell<Option<glib::SourceId>>>,
    /// Tracks whether the last reconnect-worker tick saw the daemon online.
    /// The worker compares prev→now to detect drops; the disconnect button
    /// flips it to false so a manual disconnect doesn't look like a drop on
    /// the next 15s tick.
    reconnect_was_online: Rc<RefCell<bool>>,
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
    /// Ids of peers whose detail panel is currently expanded. Updated
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
    /// Live `peer id → row` map for the currently rendered peers, kept in
    /// sync with the ListBox. The soft-refresh path looks up each peer by id
    /// and calls `set_subtitle` on its row without touching the rest of the
    /// widget tree.
    peer_rows: Rc<RefCell<HashMap<String, libadwaita::ExpanderRow>>>,
    /// Worker threads send `()` here to ask the main loop to schedule a
    /// refresh. A main-loop task in `build()` awaits the receiver and kicks a
    /// background gather. This is our cross-thread bridge — workers only ever
    /// send `Send`-friendly types, never `Ctx` itself.
    refresh_tx: async_channel::Sender<()>,
    /// Coalesces periodic and user-triggered refreshes into one worker.
    refresh_in_flight: Arc<AtomicBool>,
    /// Records a refresh requested while the worker is gathering.
    refresh_pending: Arc<AtomicBool>,
    /// A gather thread sends its snapshot here; a main-loop task applies it.
    refresh_done_tx: async_channel::Sender<RefreshData>,
    /// True while an explicit connect/start-service action is in flight (the
    /// UI is showing "Connecting…"). A background gather that snapshotted the
    /// pre-connect state must not yank the UI back to "Disconnected" and
    /// re-enable the button — that's how a user ends up firing two concurrent
    /// `tailscale up`s. Main-thread only.
    connect_in_flight: Rc<RefCell<bool>>,
    /// Last (connected, peer_count) pushed to the tray. Used to skip redundant
    /// `Handle::update` calls — each one blocks the GTK loop on a D-Bus
    /// round-trip with the ksni service loop, so we only pay it on a real
    /// change. Main-thread only.
    tray_state: Rc<RefCell<Option<(bool, usize)>>>,
    /// Whether the last applied state was Connected. Written by `apply_state`
    /// (which runs on the main thread after a background gather). The reconnect
    /// worker reads this instead of shelling out to `tailscale status` on the
    /// GTK thread — that probe would block the main loop on a hung daemon. Main
    /// thread only.
    current_online: Rc<Cell<bool>>,
}

pub fn build(app: &libadwaita::Application, start_hidden: bool) {
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
        win: win.clone(),
        toast: toast.clone(),
        sidebar: sidebar_widgets,
        content: content_widgets,
    };

    let (refresh_tx, refresh_rx) = async_channel::unbounded::<()>();
    let (refresh_done_tx, refresh_done_rx) = async_channel::unbounded::<RefreshData>();

    let ctx = Ctx {
        ui: ui.clone(),
        cfg: cfg.clone(),
        tray: Rc::new(RefCell::new(None)),
        latency: Arc::new(Mutex::new(HashMap::new())),
        last_peer_states: Rc::new(RefCell::new(HashMap::new())),
        last_peer_notifications: Rc::new(RefCell::new(HashMap::new())),
        health_ok: Arc::new(Mutex::new(true)),
        reconnect_attempts: Rc::new(RefCell::new(0)),
        pending_reconnect: Rc::new(RefCell::new(None)),
        reconnect_was_online: Rc::new(RefCell::new(false)),
        update_available: Arc::new(Mutex::new(None)),
        device_meta: Arc::new(Mutex::new(HashMap::new())),
        expanded_peers: Rc::new(RefCell::new(HashSet::new())),
        last_peer_signature: Rc::new(RefCell::new(u64::MAX)),
        peer_rows: Rc::new(RefCell::new(HashMap::new())),
        refresh_tx,
        refresh_in_flight: Arc::new(AtomicBool::new(false)),
        refresh_pending: Arc::new(AtomicBool::new(false)),
        refresh_done_tx,
        connect_in_flight: Rc::new(RefCell::new(false)),
        tray_state: Rc::new(RefCell::new(None)),
        current_online: Rc::new(Cell::new(false)),
    };

    apply_config_to_widgets(&ui, &cfg.borrow());
    setup_menu(&ctx);
    wire_signals(&ctx);
    let tray_active = setup_tray(&ctx);
    setup_close_to_tray(&ctx, tray_active);

    // Two main-loop tasks drive the refresh cycle:
    //   • a worker asked for a refresh (`refresh_rx`) → kick a background
    //     gather (coalesced: a burst of pings triggers a single gather);
    //   • a background gather finished (`refresh_done_rx`) → apply its
    //     snapshot on the main thread.
    // Keeping the subprocess + JSON parse off the GTK thread means a hung
    // tailscaled can't freeze the UI. Both tasks sleep until a message
    // arrives — the timers that used to poll here woke the CPU 5×/s even
    // with the window hidden.
    {
        let ctx2 = ctx.clone();
        glib::spawn_future_local(async move {
            while refresh_rx.recv().await.is_ok() {
                while refresh_rx.try_recv().is_ok() {}
                refresh_state(&ctx2);
            }
        });
    }
    {
        let ctx2 = ctx.clone();
        glib::spawn_future_local(async move {
            while let Ok(mut data) = refresh_done_rx.recv().await {
                // Only the newest snapshot matters.
                while let Ok(newer) = refresh_done_rx.try_recv() {
                    data = newer;
                }
                apply_refresh(&ctx2, data);
            }
        });
    }

    // Start hidden in the tray when launched by the desktop-session autostart
    // entry (`--hidden`). The tray icon is the live presence in that case, so
    // there's nothing to show until the user clicks it. If the tray failed to
    // register we'd otherwise leave the user with no window and no way to open
    // one, so fall back to presenting normally whenever the tray is inactive.
    if !(start_hidden && tray_active) {
        win.present();
    }

    refresh_state(&ctx);

    // Periodic refresh of tailscaled status. Keep at 30s so we don't spam
    // `tailscale status --json`; transitions still get caught quickly because
    // explicit user actions also call refresh_state directly. While hidden in
    // the tray only every 4th tick runs (2 min): the tray tooltip can lag, and
    // showing the window refreshes at once. Peer notifications need the
    // regular cadence, so they keep it; auto-reconnect nudges its own refresh.
    {
        let ctx2 = ctx.clone();
        let tick = Cell::new(0_u32);
        glib::timeout_add_seconds_local(30, move || {
            tick.set(tick.get().wrapping_add(1));
            let hidden = !ctx2.ui.win.is_visible();
            let notifying = ctx2.cfg.borrow().notify_peer_changes;
            if !hidden || notifying || tick.get().is_multiple_of(4) {
                refresh_state(&ctx2);
            }
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
        if c.auto_connect && has_connect_config(&c) {
            // Probe off-thread so a hung tailscaled at login can't freeze the
            // just-shown window; fire do_connect from the main loop once the
            // probe reports the daemon is up but not yet connected.
            let ctx2 = ctx.clone();
            run_in_background(
                || tailscale::is_service_active() && !tailscale::get_status().online,
                move |should| {
                    if should {
                        let c = ctx2.cfg.borrow().clone();
                        do_connect(&ctx2, c.server_url, c.authkey, c.hostname.clone());
                    }
                },
            );
        }
    }
}

// ─── Tray (StatusNotifierItem on Linux, Shell_NotifyIcon on Windows) ─────────

/// Spawn the system-tray indicator and wire its actions to the existing UI
/// flows. Returns true when the tray was registered successfully — callers
/// use this to decide whether the close button should hide instead of quit.
fn setup_tray(ctx: &Ctx) -> bool {
    let Some((rx, handle)) = tray::spawn() else {
        return false;
    };
    *ctx.tray.borrow_mut() = Some(handle);

    let ctx2 = ctx.clone();
    glib::spawn_future_local(async move {
        // Sleeps until the tray sends something. When the tray side goes away
        // (service thread exited) the channel closes and this task ends,
        // releasing the captured `ctx2`.
        while let Ok(cmd) = rx.recv().await {
            match cmd {
                tray::Command::Show => {
                    ctx2.ui.win.present();
                    // Re-sync immediately: latency pings pause while hidden
                    // (see spawn_latency_worker), so the list could be
                    // showing stale data the moment the window reappears.
                    refresh_state(&ctx2);
                }
                tray::Command::Connect => {
                    if ctx2.ui.sidebar.btn_connect.is_sensitive()
                        && !ctx2
                            .ui
                            .sidebar
                            .btn_connect
                            .has_css_class("destructive-action")
                    {
                        ctx2.ui.sidebar.btn_connect.emit_clicked();
                    }
                }
                tray::Command::Disconnect => {
                    if ctx2
                        .ui
                        .sidebar
                        .btn_connect
                        .has_css_class("destructive-action")
                    {
                        ctx2.ui.sidebar.btn_connect.emit_clicked();
                    }
                }
                tray::Command::Quit => {
                    if let Some(app) = ctx2.ui.win.application() {
                        app.quit();
                    }
                }
            }
        }
        eprintln!("[biglace] tray: channel closed");
    });
    true
}

/// Push the current connection state into the tray indicator (tooltip + the
/// connect/disconnect menu label flip). No-op when the tray failed to spawn.
fn update_tray(ctx: &Ctx, connected: bool, peer_count: usize) {
    // Skip when nothing changed. `Handle::update` blocks the GTK loop on a
    // D-Bus round-trip with the ksni service loop; apply_state calls this on
    // every refresh, so gating on a real change keeps a congested session bus
    // (e.g. plasmashell restarting) from stalling the UI on no-op updates.
    if *ctx.tray_state.borrow() == Some((connected, peer_count)) {
        return;
    }
    if let Some(h) = ctx.tray.borrow().as_ref() {
        h.update(connected, peer_count);
    }
    *ctx.tray_state.borrow_mut() = Some((connected, peer_count));
}

/// Hijack the window's close button so it hides the window instead of
/// destroying it — the tray icon is the live presence and quitting only
/// happens via the tray's "Quit" item or the in-app menu. Without an active
/// tray we leave the default behavior alone (otherwise closing would orphan
/// the user with no way back).
fn setup_close_to_tray(ctx: &Ctx, tray_active: bool) {
    if !tray_active {
        return;
    }
    // Use the callback's own widget arg instead of capturing a strong clone of
    // the window into a handler connected to that same window — the latter is a
    // reference cycle GObject never breaks.
    ctx.ui.win.connect_close_request(move |w| {
        w.set_visible(false);
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
    let win = ctx.ui.win.clone();
    glib::timeout_add_seconds_local(20, move || {
        // Skip entirely while hidden in the tray: latency is purely the peer
        // rows' subtitle, which nobody's looking at. On a 30-peer tailnet this
        // otherwise spawns up to 6 `tailscale ping` subprocesses (2s each)
        // every 20s for a window that isn't visible — a real battery cost on
        // laptops. A refresh_state on tray Show repopulates it on reappear.
        if !win.is_visible() {
            return glib::ControlFlow::Continue;
        }
        // Snapshotting peers off the GTK thread keeps the main loop free
        // even if tailscaled is slow — we don't read the result here, the
        // background thread does the parsing + pinging + send().
        let lat = latency.clone();
        let tx_t = tx.clone();
        std::thread::spawn(move || {
            // Ping the IPv4 address when the peer has one. `ip` is just the
            // first entry of TailscaleIPs, which is IPv6 on a v6-first peer —
            // and a host with IPv6 disabled can't ping that, so the row's
            // latency stayed permanently blank.
            let online_targets: Vec<(String, String)> = tailscale::get_peers()
                .into_iter()
                .filter(|p| p.online)
                .filter_map(|p| {
                    let target = if p.ipv4.is_empty() { p.ip } else { p.ipv4 };
                    (!target.is_empty()).then_some((p.id, target))
                })
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
            let n_workers = PING_PARALLELISM.min(queue.lock().map(|q| q.len()).unwrap_or(1).max(1));
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
            let _ = tx_t.try_send(());
        });
        glib::ControlFlow::Continue
    });
}

/// Poll Headscale's `/health` every 60s and update the badge in the header.
/// Skipped while the user has no server configured.
fn spawn_health_worker(ctx: &Ctx) {
    let kick = |url: String, health_ok: Arc<Mutex<bool>>, tx: async_channel::Sender<()>| {
        std::thread::spawn(move || {
            let ok = tailscale::headscale_healthy(&url);
            if let Ok(mut g) = health_ok.lock() {
                *g = ok;
            }
            let _ = tx.try_send(());
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
    glib::timeout_add_seconds_local(15, move || {
        let cfg = ctx2.cfg.borrow().clone();
        // Cheap short-circuits first — when neither requirement to ever fire
        // a reconnect is met, we don't even need to know the online state,
        // so we can skip the tailscaled probe entirely. Most users don't
        // have auto-reconnect on, so this is the hot path most of the time.
        if !cfg.auto_reconnect || !has_connect_config(&cfg) {
            return glib::ControlFlow::Continue;
        }
        // Nudge a background gather so `current_online` stays fresh, then read
        // the last gathered value — never probe tailscaled synchronously here,
        // that would freeze the GTK loop when the daemon hangs.
        let _ = ctx2.refresh_tx.try_send(());
        let now_online = ctx2.current_online.get();
        if now_online {
            // Reset backoff on successful connect, even if user reconnected manually.
            *ctx2.reconnect_was_online.borrow_mut() = true;
            *ctx2.reconnect_attempts.borrow_mut() = 0;
            if let Some(id) = ctx2.pending_reconnect.borrow_mut().take() {
                id.remove();
            }
            return glib::ControlFlow::Continue;
        }
        // `reconnect_was_online` is an intent latch: once this session was
        // online, keep retrying until success or an explicit manual disconnect.
        if !*ctx2.reconnect_was_online.borrow()
            || ctx2.pending_reconnect.borrow().is_some()
            || *ctx2.connect_in_flight.borrow()
        {
            return glib::ControlFlow::Continue;
        }
        // Exponential backoff with cap: 15s, 30s, 1m, 2m, 4m, 5m, 5m, ...
        let n = *ctx2.reconnect_attempts.borrow();
        let delay = (15u64 << n.min(5)).min(300);
        eprintln!(
            "[biglace] auto-reconnect: drop detected, retrying in {delay}s (attempt {})",
            n + 1
        );
        *ctx2.reconnect_attempts.borrow_mut() = n + 1;

        let ctx_inner = ctx2.clone();
        let id = glib::timeout_add_seconds_local_once(delay as u32, move || {
            // Drop our own SourceId slot first so a manual disconnect arriving
            // mid-flight doesn't try to remove a source that's already firing.
            ctx_inner.pending_reconnect.borrow_mut().take();
            // Re-read config before firing: the user may have toggled
            // auto-reconnect off (or disconnected manually) during the backoff
            // wait. Without this re-check the app would reconnect itself
            // minutes after the user explicitly told it not to.
            let c = ctx_inner.cfg.borrow().clone();
            if c.auto_reconnect && has_connect_config(&c) && !ctx_inner.current_online.get() {
                do_connect(&ctx_inner, c.server_url, c.authkey, c.hostname.clone());
            }
        });
        // Stash the SourceId so the disconnect handler can cancel us. Replace
        // any previous one (shouldn't happen — the outer 15s tick already
        // detected we were offline once — but be defensive).
        if let Some(old) = ctx2.pending_reconnect.borrow_mut().replace(id) {
            old.remove();
        }
        glib::ControlFlow::Continue
    });
}

/// One-shot Github releases check. Compares `crate::BUILD_VERSION` against the
/// `tag_name` of the latest release and stashes the new version string in
/// `update_available` for the header badge to pick up. Silent on failure —
/// no internet, rate-limit, or repo rename should never spam the user.
fn spawn_update_check(ctx: &Ctx) {
    let update_available = ctx.update_available.clone();
    let tx = ctx.refresh_tx.clone();
    std::thread::spawn(move || {
        let url = "https://api.github.com/repos/big-comm/biglace/releases/latest";
        // Redirect-following agent: unauthenticated public API, and GitHub
        // redirects `/repos/<owner>/…` after a repository rename or transfer —
        // with `redirects(0)` the check would silently stop reporting updates.
        let Ok(agent) = panel::redirecting_agent() else {
            return;
        };
        let Ok(resp) = agent
            .get(url)
            .timeout(Duration::from_secs(8))
            .set("User-Agent", "biglace-update-check")
            .set("Accept", "application/vnd.github+json")
            .call()
        else {
            return;
        };
        let mut body = Vec::new();
        if resp
            .into_reader()
            .take(256 * 1024 + 1)
            .read_to_end(&mut body)
            .is_err()
            || body.len() > 256 * 1024
        {
            return;
        }
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return;
        };
        let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
            return;
        };
        let latest = tag.trim_start_matches('v').to_string();
        // Release tags are date stamps, so compare against this build's date
        // stamp, not the semantic APP_VERSION.
        let Some(current) = crate::BUILD_VERSION else {
            return;
        };
        if release_is_newer(&latest, current) {
            if let Ok(mut g) = update_available.lock() {
                *g = Some(latest);
            }
            let _ = tx.try_send(());
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
                    let _ = tx_t.try_send(());
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

/// Minutes encoded by a date-based version `YY.MM.DD[-HHMM]` (release tags
/// and packaged builds both use it). The day count is approximate (31-day
/// months), which is fine: it only has to order versions, not measure them.
fn version_minutes(version: &str) -> Option<u64> {
    let (date, time) = match version.split_once('-') {
        Some((date, time)) => (date, Some(time)),
        None => (version, None),
    };
    let mut parts = date.split('.').map(|p| p.parse::<u64>().ok());
    let (yy, mm, dd) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() || !(1..=12).contains(&mm) || !(1..=31).contains(&dd) {
        return None;
    }
    let hhmm = match time {
        Some(t) if t.len() == 4 => t
            .parse::<u64>()
            .ok()
            .filter(|v| v / 100 < 24 && v % 100 < 60)?,
        Some(_) => return None,
        None => 0,
    };
    Some(((yy * 12 + mm) * 31 + dd) * 1440 + (hhmm / 100) * 60 + hhmm % 100)
}

/// Whether the release tagged `candidate` is newer than the running build.
///
/// Both are date-based. The package is built a few minutes before (or after)
/// its release tag is created, so a release must be more than an hour newer
/// than the build to count — otherwise the release a user just installed
/// would advertise itself as an update. A build without a date version is a
/// developer build and never gets the badge.
fn release_is_newer(candidate: &str, current: &str) -> bool {
    const SAME_RELEASE_WINDOW_MIN: u64 = 60;
    match (version_minutes(candidate), version_minutes(current)) {
        (Some(new), Some(old)) => new > old + SAME_RELEASE_WINDOW_MIN,
        _ => false,
    }
}

// ─── Apply config to UI ──────────────────────────────────────────────────────

fn apply_config_to_widgets(ui: &Ui, c: &config::Config) {
    ui.sidebar.entry_server.set_text(&c.server_url);
    ui.sidebar.entry_key.set_text(&c.authkey);
    ui.sidebar.entry_host.set_text(&c.hostname);
    ui.sidebar.switch_auto.set_active(c.auto_connect);
    ui.sidebar
        .switch_start_at_login
        .set_active(autostart::is_enabled());
    ui.sidebar
        .switch_auto_reconnect
        .set_active(c.auto_reconnect);
    ui.sidebar.switch_notify.set_active(c.notify_peer_changes);
}

fn has_connect_config(c: &config::Config) -> bool {
    !c.server_url.trim().is_empty() && (c.enrolled || !c.authkey.trim().is_empty())
}

fn has_visible_identity(state: AppState, c: &config::Config) -> bool {
    state == AppState::Connected || has_connect_config(c)
}

fn should_show_manual_setup(state: AppState, c: &config::Config) -> bool {
    state != AppState::Connected && !has_connect_config(c)
}

fn mark_enrolled(mut c: config::Config) -> config::Config {
    c.enrolled = true;
    c
}

// ─── Menu (hamburger) ────────────────────────────────────────────────────────

fn setup_menu(ctx: &Ctx) {
    let menu = gtk4::gio::Menu::new();
    menu.append(
        Some(&tr!("Sign in with panel account")),
        Some("win.panel-login"),
    );
    menu.append(Some(&tr!("Sign out")), Some("win.sign-out"));
    menu.append(
        Some(&tr!("Make this user the tailscale operator")),
        Some("win.set-operator"),
    );
    menu.append(Some(&tr!("View tailscaled logs")), Some("win.view-logs"));
    menu.append(Some(&tr!("About BigLace")), Some("win.about"));
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
    // ── Header refresh icon → win.refresh action ──
    {
        let win_w = ctx.ui.win.clone();
        ctx.ui.content.btn_refresh.connect_clicked(move |_| {
            gtk4::prelude::WidgetExt::activate_action(&win_w, "win.refresh", None).ok();
        });
    }

    // ── Identity row pencil → rename-device dialog ──
    {
        let win_w = ctx.ui.win.clone();
        let toast_w = ctx.ui.toast.clone();
        let cfg2 = ctx.cfg.clone();
        ctx.ui.sidebar.btn_edit_host.connect_clicked(move |_| {
            dialogs::show_rename_device(&win_w, &toast_w, &cfg2);
        });
    }

    // ── Save manual key/server ──
    {
        let ctx2 = ctx.clone();
        ctx.ui.sidebar.btn_save_manual.connect_clicked(move |_| {
            let server = dialogs::normalize_server_url(&ctx2.ui.sidebar.entry_server.text());
            // Trim the key: a trailing space/newline from paste would otherwise
            // be persisted and rejected by the daemon as an "invalid key".
            let key = ctx2.ui.sidebar.entry_key.text().trim().to_string();
            let host = dialogs::sanitize_hostname(&ctx2.ui.sidebar.entry_host.text());
            if server.trim().is_empty() {
                ctx2.ui.toast.add_toast(
                    libadwaita::Toast::builder()
                        .title(tr!("Enter the server URL before saving."))
                        .timeout(3)
                        .build(),
                );
                return;
            }
            if key.trim().is_empty() {
                ctx2.ui.toast.add_toast(
                    libadwaita::Toast::builder()
                        .title(tr!("Enter the pre-auth key before saving."))
                        .timeout(3)
                        .build(),
                );
                return;
            }
            if let Err(message) = dialogs::validate_hostname(&host) {
                ctx2.ui.toast.add_toast(
                    libadwaita::Toast::builder()
                        .title(message)
                        .timeout(3)
                        .build(),
                );
                return;
            }
            // Reflect the canonical form back into the entry so the user sees
            // exactly what we'll persist (and what the next `tailscale up` will
            // get on `--login-server`).
            ctx2.ui.sidebar.entry_server.set_text(&server);
            ctx2.ui.sidebar.entry_host.set_text(&host);
            let saved = {
                let mut c = ctx2.cfg.borrow_mut();
                c.server_url = server;
                c.authkey = key;
                c.hostname = host;
                c.enrolled = true;
                config::save_with_authkey(&c)
            };
            ctx2.ui.sidebar.expander_manual.set_expanded(false);
            // Report what actually happened: this used to say "Setup saved"
            // even when nothing reached the disk.
            match saved {
                Ok(None) => show_toast(&ctx2.ui.toast, &tr!("Setup saved"), 2),
                Ok(Some(reason)) => show_toast(&ctx2.ui.toast, &keyring_warning(&reason), 8),
                Err(e) => show_toast(&ctx2.ui.toast, &trf!("Error: {error}", "error" => e), 6),
            }
            refresh_state(&ctx2);
        });
    }

    // ── Start at login switch ──
    {
        let toast = ctx.ui.toast.clone();
        ctx.ui
            .sidebar
            .switch_start_at_login
            .connect_state_set(move |_, active| match autostart::set_enabled(active) {
                Ok(()) => glib::Propagation::Proceed,
                Err(e) => {
                    toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(
                                glib::markup_escape_text(
                                    &trf!("Error: {error}", "error" => e.to_string()),
                                )
                                .as_str(),
                            )
                            .timeout(5)
                            .build(),
                    );
                    glib::Propagation::Stop
                }
            });
    }

    // ── Auto-connect switch → save ──
    {
        let cfg2 = ctx.cfg.clone();
        ctx.ui
            .sidebar
            .switch_auto
            .connect_state_set(move |_, active| {
                let mut c = cfg2.borrow_mut();
                c.auto_connect = active;
                config::save_or_warn(&c);
                glib::Propagation::Proceed
            });
    }

    // ── Auto-reconnect switch ──
    {
        let ctx2 = ctx.clone();
        ctx.ui
            .sidebar
            .switch_auto_reconnect
            .connect_state_set(move |_, active| {
                {
                    let mut c = ctx2.cfg.borrow_mut();
                    c.auto_reconnect = active;
                    config::save_or_warn(&c);
                }
                // Turning it off must also tear down any backoff already armed by
                // the reconnect worker, otherwise a queued reconnect still fires
                // once after the user disabled the feature.
                if !active {
                    if let Some(id) = ctx2.pending_reconnect.borrow_mut().take() {
                        id.remove();
                    }
                    *ctx2.reconnect_attempts.borrow_mut() = 0;
                }
                glib::Propagation::Proceed
            });
    }

    // ── Peer-change notifications switch ──
    {
        let cfg2 = ctx.cfg.clone();
        ctx.ui
            .sidebar
            .switch_notify
            .connect_state_set(move |_, active| {
                let mut c = cfg2.borrow_mut();
                c.notify_peer_changes = active;
                config::save_or_warn(&c);
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
                // Tear down any queued auto-reconnect backoff before issuing
                // `tailscale down` — otherwise the backoff fires a few seconds
                // later and silently re-connects, which feels like a bug to the
                // user. Also reset the attempt counter so the next genuine drop
                // starts fresh at 15s instead of mid-backoff.
                if let Some(id) = ctx2.pending_reconnect.borrow_mut().take() {
                    id.remove();
                }
                *ctx2.reconnect_attempts.borrow_mut() = 0;
                *ctx2.reconnect_was_online.borrow_mut() = false;
                // Refresh when `tailscale down` actually finished — a fixed
                // 400 ms delay raced it (and lost whenever pkexec asked for a
                // password), leaving the UI on "Connected" for up to 30 s.
                btn.set_sensitive(false);
                let ctx3 = ctx2.clone();
                run_in_background(
                    || tailscale::disconnect().map_err(|e| e.to_string()),
                    move |result| {
                        match result {
                            Ok(()) => eprintln!("[biglace] disconnect: ok"),
                            Err(e) => {
                                eprintln!("[biglace] disconnect: failed: {e}");
                                show_toast(
                                    &ctx3.ui.toast,
                                    &trf!("Error: {error}", "error" => e),
                                    6,
                                );
                            }
                        }
                        ctx3.ui.sidebar.btn_connect.set_sensitive(true);
                        refresh_state(&ctx3);
                    },
                );
            } else {
                let c = ctx2.cfg.borrow().clone();
                if c.authkey.trim().is_empty() && !c.enrolled {
                    ctx2.ui.toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Sign in or paste a pre-auth key first."))
                            .timeout(3)
                            .build(),
                    );
                    return;
                }
                if c.server_url.trim().is_empty() {
                    ctx2.ui.toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Enter the server URL before connecting."))
                            .timeout(3)
                            .build(),
                    );
                    return;
                }
                do_connect(&ctx2, c.server_url, c.authkey, c.hostname.clone());
            }
        });
    }

    // ── Start service button ──
    {
        let ctx2 = ctx.clone();
        ctx.ui
            .content
            .btn_start_service
            .connect_clicked(move |btn| {
                btn.set_sensitive(false);
                apply_state(&ctx2, AppState::Connecting, &Status::default(), &[]);
                // Hold the Connecting state against background refreshes while the
                // pkexec/sc prompt is up (see apply_refresh).
                *ctx2.connect_in_flight.borrow_mut() = true;

                let ctx3 = ctx2.clone();
                let btn3 = btn.clone();
                run_in_background(
                    move || {
                        // Linux: `pkexec systemctl enable --now tailscaled` prompts via
                        // polkit and registers the unit for auto-start. Windows: the
                        // installer already configures `Tailscale` to start at boot,
                        // so all we need is `sc start` — which silently no-ops when
                        // the service is already RUNNING. Both paths return a single
                        // bool so the GTK-side poller can be shared.
                        #[cfg(target_os = "linux")]
                        let ok = std::process::Command::new("pkexec")
                            .args(["systemctl", "enable", "--now", "tailscaled"])
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        #[cfg(target_os = "windows")]
                        let ok = std::process::Command::new("sc")
                            .args(["start", "Tailscale"])
                            .status()
                            .is_ok_and(|s| sc_start_succeeded(s.code()));
                        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                        let ok = false;
                        ok
                    },
                    move |ok: bool| {
                        btn3.set_sensitive(true);
                        *ctx3.connect_in_flight.borrow_mut() = false;
                        if !ok {
                            show_toast(
                                &ctx3.ui.toast,
                                &tr!("Could not start the Tailscale service."),
                                5,
                            );
                        }
                        // The service state just changed under us; drop the
                        // cached probe so this refresh reads the truth instead
                        // of an up-to-2s-old "stopped".
                        tailscale::invalidate_service_cache();
                        refresh_state(&ctx3);
                    },
                );
            });
    }

    // ── Copy "this device" IP ──
    {
        let row = ctx.ui.content.self_row.clone();
        let toast = ctx.ui.toast.clone();
        ctx.ui.content.btn_copy_self_ip.connect_clicked(move |btn| {
            let ip = row.subtitle().map(|s| s.to_string()).unwrap_or_default();
            if ip.is_empty() {
                return;
            }
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

/// Whether `sc start Tailscale` left the service running: exit 0, or 1056
/// (ERROR_SERVICE_ALREADY_RUNNING), which is success from the user's point of
/// view. Anything else — access denied (5), disabled (1058), missing (1060) —
/// is a real failure that used to be reported as success.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn sc_start_succeeded(code: Option<i32>) -> bool {
    matches!(code, Some(0) | Some(1056))
}

/// Run `work` on a worker thread and hand its result to `done` on the GTK
/// main loop. The main loop is woken by the channel only when the result
/// arrives — no timer polls a shared slot in the meantime.
pub(super) fn run_in_background<T, W, D>(work: W, done: D)
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(T) + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(work());
    });
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            done(result);
        }
    });
}

/// Show a toast. The text is escaped: AdwToast titles are markup, and
/// messages here can embed tailscale stderr or hostnames with `<`/`&`.
pub(super) fn show_toast(overlay: &libadwaita::ToastOverlay, text: &str, timeout: u32) {
    overlay.add_toast(
        libadwaita::Toast::builder()
            .title(glib::markup_escape_text(text).as_str())
            .timeout(timeout)
            .build(),
    );
}

/// The config was saved but the pre-auth key couldn't go into the keyring.
pub(super) fn keyring_warning(reason: &str) -> String {
    trf!(
        "Saved, but the pre-auth key could not be stored in the system keyring ({error}). This device can still reconnect; the key is only needed again to register it anew.",
        "error" => reason
    )
}

// ─── Connect flow ────────────────────────────────────────────────────────────

fn do_connect(ctx: &Ctx, server: String, authkey: String, hostname: String) {
    eprintln!("[biglace] connect: button clicked (server={server:?} hostname={hostname:?})");
    apply_state(ctx, AppState::Connecting, &Status::default(), &[]);
    // Guard the Connecting state against concurrent background refreshes until
    // this attempt resolves (see apply_refresh).
    *ctx.connect_in_flight.borrow_mut() = true;

    let ctx2 = ctx.clone();
    run_in_background(
        move || {
            let r = tailscale::connect(&server, &authkey, &hostname).map_err(|e| e.to_string());
            match &r {
                Ok(()) => eprintln!("[biglace] connect: ok"),
                Err(e) => eprintln!("[biglace] connect: failed: {e}"),
            }
            r
        },
        move |result| {
            match result {
                Ok(()) => {
                    let updated = mark_enrolled(ctx2.cfg.borrow().clone());
                    let persist_error = match config::save(&updated) {
                        Ok(()) => {
                            *ctx2.cfg.borrow_mut() = updated;
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    };
                    if let Some(error) = persist_error {
                        ctx2.ui.toast.add_toast(
                            libadwaita::Toast::builder()
                                .title(
                                    glib::markup_escape_text(
                                        &trf!("Error: {error}", "error" => error),
                                    )
                                    .as_str(),
                                )
                                .timeout(6)
                                .build(),
                        );
                    }
                    *ctx2.connect_in_flight.borrow_mut() = false;
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
                                    let _ = refresh_tx.try_send(());
                                }
                                Err(e) => {
                                    eprintln!("[biglace] panel: fetch device-meta failed: {e}")
                                }
                            }
                        });
                    }
                    ctx2.ui.toast.add_toast(
                        libadwaita::Toast::builder()
                            .title(tr!("Connected"))
                            .timeout(2)
                            .build(),
                    );
                }
                Err(e) => {
                    *ctx2.connect_in_flight.borrow_mut() = false;
                    refresh_state(&ctx2);
                    let toast = libadwaita::Toast::builder()
                        // Escape: the tailscale stderr embedded here can contain
                        // `<`/`&` (URLs, hostnames), which AdwToast's markup-aware
                        // title would otherwise swallow — hiding the very error the
                        // user needs to read.
                        .title(
                            glib::markup_escape_text(&trf!("Error: {error}", "error" => e))
                                .as_str(),
                        )
                        .timeout(6)
                        .build();
                    ctx2.ui.toast.add_toast(toast);
                }
            }
        },
    );
}

// ─── State refresh & UI binding ──────────────────────────────────────────────

/// Request a refresh: gather tailscaled state on a worker thread, then let the
/// main-loop poller apply it. Safe to call from any GTK callback — it never
/// blocks on a subprocess itself. Overlapping calls share the running gather.
fn refresh_state(ctx: &Ctx) {
    if ctx.refresh_in_flight.swap(true, Ordering::AcqRel) {
        ctx.refresh_pending.store(true, Ordering::Release);
        return;
    }
    let done = ctx.refresh_done_tx.clone();
    let device_meta = ctx.device_meta.clone();
    let in_flight = ctx.refresh_in_flight.clone();
    let pending = ctx.refresh_pending.clone();
    std::thread::spawn(move || {
        loop {
            let data = if !tailscale::is_service_active() {
                RefreshData::ServiceStopped
            } else {
                // One subprocess + JSON parse covers both status and peers.
                let (status, mut peers) = tailscale::get_status_and_peers();
                if !status.online {
                    peers.clear();
                }
                if let Ok(meta) = device_meta.lock() {
                    for p in peers.iter_mut() {
                        if let Some(u) = meta.get(&p.hostname) {
                            p.ssh_user = u.clone();
                        }
                    }
                }
                RefreshData::Live { status, peers }
            };
            let _ = done.try_send(data);

            if pending.swap(false, Ordering::AcqRel) {
                continue;
            }
            in_flight.store(false, Ordering::Release);
            // Close the tiny race between the pending check and clearing the
            // in-flight flag without starting a second worker.
            if pending.swap(false, Ordering::AcqRel) && !in_flight.swap(true, Ordering::AcqRel) {
                continue;
            }
            break;
        }
    });
}

/// Apply a gathered snapshot to the widgets. Runs on the main thread.
fn apply_refresh(ctx: &Ctx, data: RefreshData) {
    // While an explicit connect/start-service action is in flight the UI shows
    // "Connecting…". A background gather that snapshotted the pre-connect state
    // must not override it (which would re-enable Connect and allow a second
    // `tailscale up`). Once the action's own poller resolves it clears the flag
    // and the next gather paints the real state.
    let busy = *ctx.connect_in_flight.borrow();
    match data {
        RefreshData::ServiceStopped => {
            if busy {
                return;
            }
            apply_state(ctx, AppState::ServiceStopped, &Status::default(), &[]);
        }
        RefreshData::Live { status, peers } => {
            if busy && !status.online {
                return;
            }
            let state = if status.online {
                AppState::Connected
            } else if !has_connect_config(&ctx.cfg.borrow()) {
                AppState::NotSignedIn
            } else {
                AppState::Disconnected
            };
            apply_state(ctx, state, &status, &peers);
        }
    }
}

fn apply_state(ctx: &Ctx, state: AppState, status: &Status, peers: &[Peer]) {
    let ui = &ctx.ui;
    let cfg_snap = ctx.cfg.borrow().clone();

    // Record online state for the reconnect worker to read off-thread.
    ctx.current_online.set(state == AppState::Connected);

    // ── Status dot + label in content header ──
    for c in ["idle", "connected", "connecting", "error"] {
        ui.content.status_dot.remove_css_class(c);
    }
    let (dot_class, status_text) = match state {
        AppState::ServiceStopped => ("error", tr!("Service stopped")),
        AppState::NotSignedIn => ("idle", tr!("Not signed in")),
        AppState::Disconnected => ("idle", tr!("Disconnected")),
        AppState::Connecting => ("connecting", tr!("Connecting…")),
        AppState::Connected => ("connected", tr!("Connected")),
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
    let show_health_badge =
        server_set && !health_ok && matches!(state, AppState::Disconnected | AppState::NotSignedIn);
    if show_health_badge {
        ui.content.health_badge.set_visible(true);
        ui.content
            .health_badge
            .set_label(&tr!("Server unreachable"));
    } else {
        ui.content.health_badge.set_visible(false);
    }

    // ── Broken-DNS badge (header) ──
    // Independent of the Headscale badge and shown in every state: the tunnel
    // can be perfectly healthy while MagicDNS is dead, and that combination is
    // exactly what makes the SSH/SFTP buttons quietly fall back to raw IPs.
    // The label deliberately says "can't set DNS", not "DNS not working":
    // resolution often still succeeds from the configuration left behind
    // before the failure, so a badge claiming DNS is down contradicts what the
    // user can plainly see and reads as a false alarm. What's actually broken
    // is tailscaled's ability to *manage* it.
    ui.content.dns_badge.set_visible(status.dns_broken);
    if status.dns_broken {
        ui.content
            .dns_badge
            .set_label(&tr!("Tailscale can't set DNS"));
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
        AppState::NotSignedIn => "not-signed-in",
        AppState::Disconnected => "disconnected",
        AppState::Connecting => "connecting",
        AppState::Connected => "connected",
    };
    ui.content.stack.set_visible_child_name(page);

    // ── Identity card ──
    let configured = has_connect_config(&cfg_snap);
    let has_identity = has_visible_identity(state, &cfg_snap);
    let show_manual_setup = should_show_manual_setup(state, &cfg_snap);
    // Pencil button is only useful once we have an identity to rename, so it
    // stays hidden without saved config. Manual enrollment is only relevant
    // while offline; an active tunnel already has a usable network identity.
    ui.sidebar.btn_edit_host.set_visible(configured);
    ui.sidebar.expander_manual.set_visible(show_manual_setup);
    if !show_manual_setup {
        ui.sidebar.expander_manual.set_expanded(false);
    }
    if state == AppState::Connected {
        let title = match status.display_name() {
            name if !name.is_empty() => name,
            _ if !cfg_snap.hostname.is_empty() => cfg_snap.hostname.clone(),
            _ => tr!("(no device name)"),
        };
        let subtitle = status
            .ip
            .clone()
            .filter(|ip| !ip.is_empty())
            .or_else(|| (!cfg_snap.server_url.is_empty()).then(|| cfg_snap.server_url.clone()))
            .unwrap_or_else(|| tr!("Connected"));
        ui.sidebar.identity_row.set_title(&title);
        ui.sidebar.identity_row.set_subtitle(&subtitle);
    } else if !has_identity {
        ui.sidebar.identity_row.set_title(&tr!("Not signed in"));
        ui.sidebar
            .identity_row
            .set_subtitle(&tr!("Sign in or paste a pre-auth key"));
    } else {
        // Show the BigScale identifier the user picked (or a placeholder if
        // they haven't typed one yet). The OS user is intentionally not shown
        // here — it's only relevant for SSH composition, not for "who am I".
        let host = if cfg_snap.hostname.is_empty() {
            tr!("(no device name)")
        } else {
            cfg_snap.hostname.clone()
        };
        let url = if cfg_snap.server_url.is_empty() {
            tr!("Server not set")
        } else {
            cfg_snap.server_url.clone()
        };
        ui.sidebar.identity_row.set_title(&host);
        ui.sidebar.identity_row.set_subtitle(&url);
    }

    // ── Menu items: gate by sign-in state ──
    if let Some(act) = ui
        .win
        .lookup_action("panel-login")
        .and_downcast::<gtk4::gio::SimpleAction>()
    {
        act.set_enabled(!configured);
    }
    if let Some(act) = ui
        .win
        .lookup_action("sign-out")
        .and_downcast::<gtk4::gio::SimpleAction>()
    {
        act.set_enabled(configured);
    }

    // ── Connect / Disconnect button ──
    ui.sidebar.btn_connect.remove_css_class("suggested-action");
    ui.sidebar
        .btn_connect
        .remove_css_class("destructive-action");
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
        let title = if display.is_empty() {
            "—".to_string()
        } else {
            display
        };
        let ip = status.ip.as_deref().unwrap_or("—");
        // display_name is network-derived (DNS/hostname); escape before it
        // lands in AdwActionRow's markup-aware title.
        ui.content
            .self_row
            .set_title(&glib::markup_escape_text(&title));
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
            for (p, online) in peer_transitions(&prev, peers) {
                let recent = last_notif
                    .get(&p.id)
                    .map(|t| now.duration_since(*t) < cooldown)
                    .unwrap_or(false);
                if !recent {
                    notify_peer_change(&p.display_name(), online);
                    last_notif.insert(p.id.clone(), now);
                }
            }
        }
        *prev = peers.iter().map(|p| (p.id.clone(), p.online)).collect();
        drop(prev);

        // Sort: favorites → online → alpha. We keep the get_peers() order
        // (online → alpha) and just stable-partition pinned ones to the top.
        // Alphabetical on the label the row actually shows (`display_name`),
        // not on the peer's OS hostname — those differ whenever a device was
        // renamed in the panel, and sorting on the hidden one made the list
        // look arbitrarily ordered.
        let mut sorted: Vec<&Peer> = peers.iter().collect();
        sorted.sort_by_cached_key(|p| {
            (
                !cfg_snap.is_favorite(&p.hostname),
                !p.online,
                p.display_name().to_lowercase(),
            )
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
            !m.is_empty() && sorted.iter().all(|p| m.contains_key(&p.id))
        };

        if new_sig == prev_sig && row_map_has_all {
            let rows = ctx.peer_rows.borrow();
            for peer in &sorted {
                if let Some(row) = rows.get(&peer.id) {
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
                toast: ui.toast.clone(),
                cfg: ctx.cfg.clone(),
                latency: ctx.latency.clone(),
                refresh: refresh_cb,
                expanded: ctx.expanded_peers.clone(),
            };
            let mut new_rows: HashMap<String, libadwaita::ExpanderRow> =
                HashMap::with_capacity(sorted.len());
            for peer in &sorted {
                let row = peer_row::build(peer, &peer_ctx);
                ui.content.peers_list.append(&row);
                new_rows.insert(peer.id.clone(), row);
            }
            *ctx.peer_rows.borrow_mut() = new_rows;
            // Drop expanded entries for peers that no longer exist so the
            // set doesn't grow unbounded across long sessions.
            let live: HashSet<&str> = sorted.iter().map(|p| p.id.as_str()).collect();
            ctx.expanded_peers
                .borrow_mut()
                .retain(|h| live.contains(h.as_str()));
        }

        let online = peers.iter().filter(|p| p.online).count();
        let offline = peers.len().saturating_sub(online);
        let counts = if peers.is_empty() {
            tr!("no other devices")
        } else if offline == 0 {
            trf!("{online} online", "online" => online)
        } else {
            trf!("{online} online · {offline} offline",
                "online" => online, "offline" => offline)
        };
        ui.content
            .bottom_label
            .set_text(&format!("{ip}  ·  {counts}"));
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
        p.id.hash(&mut h);
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

/// Peers whose online state flipped since the previous render, keyed by node
/// id so two nodes that share a hostname never read as one flapping peer.
fn peer_transitions<'a>(prev: &HashMap<String, bool>, peers: &'a [Peer]) -> Vec<(&'a Peer, bool)> {
    peers
        .iter()
        .filter_map(|p| match prev.get(&p.id) {
            Some(&was) if was != p.online => Some((p, p.online)),
            _ => None,
        })
        .collect()
}

/// Send a desktop notification via `notify-send`. Silent on failure (no
/// notification daemon, missing binary, …) — never crash the GTK loop on
/// a missing optional integration.
///
/// Two coalescing layers, since `--replace-id` only merges notifications
/// that are still **active** in the daemon (once a card lands in the KDE
/// history each subsequent toast becomes an independent entry, which is
/// exactly the spam we want to avoid):
///
/// 1. **Buffering window (2.5 s)**: each transition is enqueued into a
///    thread-local aggregator; the first one schedules a glib timeout that
///    flushes the queue. Bursts (a reconnect bringing five peers up at
///    once) collapse into one toast like "5 devices are online" — listing
///    the names in the body — instead of five stacked banners.
/// 2. **`--replace-id`** for any subsequent flush, so a follow-up burst
///    updates the previous card in place while it's still active.
///
/// Hints used:
/// - `desktop-entry` makes KDE/Plasma attribute toasts to biglace.desktop
///   so its notification history groups them under one app entry instead
///   of expanding each as a separate card.
/// - `transient` asks the daemon to skip the persistent history — peer
///   status is volatile and doesn't deserve a permanent timeline entry.
/// - `x-canonical-private-synchronous` is the GNOME/dunst/mako equivalent
///   of an explicit tag for in-place updates.
#[cfg(target_os = "linux")]
fn notify_peer_change(name: &str, online: bool) {
    use std::cell::RefCell;

    struct Aggregator {
        online: Vec<String>,
        offline: Vec<String>,
        scheduled: bool,
    }

    thread_local! {
        static AGG: RefCell<Aggregator> = const { RefCell::new(Aggregator {
            online:    Vec::new(),
            offline:   Vec::new(),
            scheduled: false,
        }) };
    }

    // Enqueue (canceling the opposite state if present — a peer that
    // flapped offline → online inside the window has net "no change") and
    // tell the caller whether it needs to arm the flush timer.
    let need_schedule = AGG.with(|cell| {
        let mut a = cell.borrow_mut();
        if online {
            a.offline.retain(|n| n != name);
            if !a.online.iter().any(|n| n == name) {
                a.online.push(name.to_string());
            }
        } else {
            a.online.retain(|n| n != name);
            if !a.offline.iter().any(|n| n == name) {
                a.offline.push(name.to_string());
            }
        }
        if a.scheduled {
            false
        } else {
            a.scheduled = true;
            true
        }
    });

    if need_schedule {
        glib::timeout_add_local_once(Duration::from_millis(2500), || {
            let (online, offline) = AGG.with(|cell| {
                let mut a = cell.borrow_mut();
                a.scheduled = false;
                (
                    std::mem::take(&mut a.online),
                    std::mem::take(&mut a.offline),
                )
            });
            if online.is_empty() && offline.is_empty() {
                return;
            }
            send_peer_notification(online, offline);
        });
    }
}

/// Compose the aggregated summary/body and shell out to `notify-send` on a
/// worker thread (synchronous `output()` would stall the GTK loop).
#[cfg(target_os = "linux")]
fn send_peer_notification(online: Vec<String>, offline: Vec<String>) {
    use std::sync::{Mutex, OnceLock};

    static LAST_ID: OnceLock<Mutex<Option<u32>>> = OnceLock::new();
    let slot = LAST_ID.get_or_init(|| Mutex::new(None));
    let prev_id = slot.lock().ok().and_then(|g| *g);

    let on_n = online.len();
    let off_n = offline.len();
    let icon = if on_n >= off_n {
        "network-transmit-receive-symbolic"
    } else {
        "network-offline-symbolic"
    };

    let summary = match (on_n, off_n) {
        (1, 0) => format!("{} {}", online[0], tr!("is online")),
        (0, 1) => format!("{} {}", offline[0], tr!("went offline")),
        (n, 0) => trf!("{count} devices are online", "count" => n),
        (0, n) => trf!("{count} devices went offline", "count" => n),
        (a, b) => trf!(
            "{on_count} online · {off_count} offline",
            "on_count" => a, "off_count" => b,
        ),
    };
    let body = if on_n + off_n > 1 {
        let mut parts = Vec::new();
        if !online.is_empty() {
            parts.push(format!("↑ {}", online.join(", ")));
        }
        if !offline.is_empty() {
            parts.push(format!("↓ {}", offline.join(", ")));
        }
        parts.join("\n")
    } else {
        String::new()
    };

    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("notify-send");
        cmd.args(["--app-name=BigLace", "--icon", icon, "--print-id"]);
        cmd.arg("--urgency=low");
        cmd.args(["--hint", "string:desktop-entry:org.communitybig.biglace"]);
        cmd.args(["--hint", "int:transient:1"]);
        cmd.args([
            "--hint",
            "string:x-canonical-private-synchronous:biglace-peer-status",
        ]);
        let prev_id_str;
        if let Some(id) = prev_id {
            prev_id_str = id.to_string();
            cmd.args(["--replace-id", &prev_id_str]);
        }
        cmd.arg("--");
        cmd.arg(&summary);
        if !body.is_empty() {
            cmd.arg(&body);
        }

        if let Ok(out) = cmd.output() {
            if let Ok(text) = std::str::from_utf8(&out.stdout) {
                if let Ok(new_id) = text.trim().parse::<u32>() {
                    if let Ok(mut g) = LAST_ID.get().expect("initialized above").lock() {
                        *g = Some(new_id);
                    }
                }
            }
        }
    });
}

/// Windows: emit an Action Center toast through `tauri-winrt-notification`.
/// We borrow the PowerShell AUMID because Action Center silently drops
/// toasts whose AppUserModelID isn't registered with the system. A future
/// MSI tweak can register an AUMID for `org.communitybig.biglace` (Start
/// Menu shortcut + `System.AppUserModel.ID` property), at which point we
/// swap the constant for our own ID and toasts get the BigLace name + icon.
#[cfg(target_os = "windows")]
fn notify_peer_change(name: &str, online: bool) {
    use tauri_winrt_notification::Toast;

    let summary = if online {
        format!("{name} {}", tr!("is online"))
    } else {
        format!("{name} {}", tr!("went offline"))
    };

    // Show is fallible (Windows can refuse to surface toasts in Focus
    // Assist / DND mode). Drop the result — notifications are best-effort.
    let _ = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("BigLace")
        .text1(&summary)
        .show();
}

/// macOS / other targets: no toast backend wired up yet. The peer
/// transition still updates the UI; we just don't emit a system toast.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn notify_peer_change(_name: &str, _online: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_network_has_visible_identity_without_saved_config() {
        let cfg = config::Config::default();

        assert!(has_visible_identity(AppState::Connected, &cfg));
        assert!(!has_visible_identity(AppState::NotSignedIn, &cfg));
        assert!(!should_show_manual_setup(AppState::Connected, &cfg));
        assert!(should_show_manual_setup(AppState::NotSignedIn, &cfg));
    }

    #[test]
    fn update_check_compares_date_based_release_tags() {
        // A later day, or a release hours later on the same day, is an update.
        assert!(release_is_newer("26.09.02-0100", "26.09.01-0155"));
        assert!(release_is_newer("26.09.01-1800", "26.09.01-0155"));
        assert!(release_is_newer("27.01.01-0300", "26.12.31-2359"));
        assert!(!release_is_newer("27.01.01-0000", "26.12.31-2359"));
        // The release this build came from (tagged minutes apart) is not.
        assert!(!release_is_newer("26.09.01-0157", "26.09.01-0155"));
        assert!(!release_is_newer("26.09.01-0155", "26.09.01-0210"));
        assert!(!release_is_newer("26.08.31-2300", "26.09.01-0155"));
        // Development builds (crate version) and odd tags never get a badge.
        assert!(!release_is_newer("26.09.01-0155", "1.0.0"));
        assert!(!release_is_newer(
            "biglace-26.09.01-0157-x86_64",
            "26.01.01-0000"
        ));
        assert!(!release_is_newer("26.13.01-0000", "26.01.01-0000"));
    }

    fn test_peer(id: &str, hostname: &str, online: bool) -> Peer {
        Peer {
            id: id.into(),
            hostname: hostname.into(),
            ip: String::new(),
            ipv4: String::new(),
            ipv6: String::new(),
            dns_name: String::new(),
            online,
            os: String::new(),
            user: String::new(),
            ssh_user: String::new(),
            last_seen: String::new(),
            tags: Vec::new(),
            exit_node_offered: false,
            exit_node_active: false,
        }
    }

    #[test]
    fn windows_service_start_only_accepts_running_outcomes() {
        assert!(sc_start_succeeded(Some(0)));
        assert!(sc_start_succeeded(Some(1056)));
        assert!(!sc_start_succeeded(Some(5)));
        assert!(!sc_start_succeeded(Some(1060)));
        assert!(!sc_start_succeeded(None));
    }

    #[test]
    fn background_work_is_delivered_on_the_main_loop() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let got = Rc::new(Cell::new(None));
                let main_thread = std::thread::current().id();
                let sink = got.clone();
                run_in_background(
                    || (21 * 2, std::thread::current().id()),
                    move |(value, worker)| {
                        sink.set(Some((value, worker, std::thread::current().id())))
                    },
                );
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while got.get().is_none() && std::time::Instant::now() < deadline {
                    context.iteration(true);
                }
                let (value, worker, delivered_on) = got.get().expect("result delivered");
                assert_eq!(value, 42);
                assert_ne!(worker, main_thread, "work must run off the main loop");
                assert_eq!(delivered_on, main_thread, "result must come back on it");
            })
            .unwrap();
    }

    #[test]
    fn duplicate_hostnames_do_not_produce_phantom_transitions() {
        // A live node and its stale predecessor, same hostname.
        let peers = vec![
            test_peer("nodekey:new", "box", true),
            test_peer("nodekey:old", "box", false),
        ];
        let prev: HashMap<String, bool> = peers.iter().map(|p| (p.id.clone(), p.online)).collect();
        assert!(peer_transitions(&prev, &peers).is_empty());

        // A real change is still reported, for the right node.
        let changed = vec![
            test_peer("nodekey:new", "box", false),
            test_peer("nodekey:old", "box", false),
        ];
        let flips = peer_transitions(&prev, &changed);
        assert_eq!(flips.len(), 1);
        assert_eq!(flips[0].0.id, "nodekey:new");
        assert!(!flips[0].1);
    }

    #[test]
    fn successful_enrollment_preserves_connection_credentials() {
        let cfg = config::Config {
            server_url: "https://vpn.example.org".into(),
            authkey: "hskey-auth-secret".into(),
            hostname: "workstation".into(),
            ..config::Config::default()
        };

        let enrolled = mark_enrolled(cfg);

        assert!(enrolled.enrolled);
        assert_eq!(enrolled.server_url, "https://vpn.example.org");
        assert_eq!(enrolled.authkey, "hskey-auth-secret");
        assert_eq!(enrolled.hostname, "workstation");
    }
}
