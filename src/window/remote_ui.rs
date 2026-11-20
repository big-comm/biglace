//! Remote desktop windows: the viewer (someone else's screen in a BigLace
//! window) and, on the host, the "Allow / Deny" question and the always
//! visible "End access" notice.

use gtk4::{gdk, glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::remote::{self, client, HostEvent, Input};
use crate::tr;
use crate::trf;

// ─── Host side ───────────────────────────────────────────────────────────────

/// Listen for host events for the life of the app: consent questions and
/// session start/end. Running sessions show in `banner`, inside the main
/// window (not a loose window that hides behind others), plus a desktop
/// notification when one starts.
pub fn start_host_listener(win: &libadwaita::ApplicationWindow, banner: &libadwaita::Banner) {
    // Development builds only: BIGLACE_PREVIEW=consent shows the question
    // with a sample request, to look at it without a second computer.
    #[cfg(debug_assertions)]
    if std::env::var("BIGLACE_PREVIEW").as_deref() == Ok("consent") {
        let (tx, _rx) = std::sync::mpsc::channel();
        std::mem::forget(_rx);
        let w = win.clone();
        glib::timeout_add_seconds_local_once(1, move || {
            ask_consent(&w, remote::ConsentRequest { who: "bruna".into(), device: "notebook-bruna".into(), reply: tx });
        });
    }
    let rx = remote::host_events();
    let (win, banner) = (win.clone(), banner.clone());
    let sessions: Rc<RefCell<Vec<remote::HostSession>>> = Rc::default();
    banner.set_button_label(Some(&tr!("End access")));
    banner.add_css_class("remote-banner");
    {
        let sessions = sessions.clone();
        banner.connect_button_clicked(move |_| {
            for s in sessions.borrow().iter() {
                s.stop.stop();
            }
        });
    }
    let refresh = {
        let (banner, sessions) = (banner.clone(), sessions.clone());
        move || {
            let list = sessions.borrow();
            let names: Vec<String> = list.iter().map(|s| format!("{} ({})", s.who, s.device)).collect();
            if names.is_empty() {
                banner.set_revealed(false);
            } else {
                banner.set_title(&trf!(
                    "{who} is seeing and controlling this screen",
                    "who" => names.join(", ")
                ));
                banner.set_revealed(true);
            }
        }
    };
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                HostEvent::Consent(req) => ask_consent(&win, req),
                HostEvent::Started(session) => {
                    notify_started(&win, &session);
                    sessions.borrow_mut().push(session);
                    refresh();
                }
                HostEvent::Ended(stop) => {
                    sessions.borrow_mut().retain(|s| !s.stop.same(&stop));
                    refresh();
                    if let Some(app) = win.application() {
                        app.withdraw_notification("biglace-remote-access");
                    }
                }
            }
        }
    });
}

/// Desktop notification: visible even with BigLace minimized or in the tray.
fn notify_started(win: &libadwaita::ApplicationWindow, session: &remote::HostSession) {
    let Some(app) = win.application() else { return };
    let n = gtk4::gio::Notification::new(&tr!("Remote access started"));
    n.set_body(Some(&trf!(
        "{user} ({device}) is seeing and controlling this screen. Open BigLace to end the access.",
        "user" => session.who,
        "device" => session.device
    )));
    n.set_priority(gtk4::gio::NotificationPriority::High);
    app.send_notification(Some("biglace-remote-access"), &n);
}

fn ask_consent(win: &libadwaita::ApplicationWindow, req: remote::ConsentRequest) {
    // Bring BigLace forward: the question must not wait behind other windows.
    win.present();
    let dlg = libadwaita::MessageDialog::new(Some(win), None::<&str>, None::<&str>);
    dlg.add_css_class("consent-dialog");
    let body = consent_body(&req.who, &req.device);
    let remember = gtk4::CheckButton::builder()
        .label(trf!("Always allow {user} (no more asking)", "user" => req.who))
        .halign(gtk4::Align::Center)
        .build();
    body.append(&remember);
    dlg.set_extra_child(Some(&body));
    dlg.add_response("deny", &tr!("Deny"));
    dlg.add_response("allow", &tr!("Allow access"));
    dlg.set_response_appearance("allow", libadwaita::ResponseAppearance::Suggested);
    // Safe default: Enter / Escape / closing the dialog all deny.
    dlg.set_default_response(Some("deny"));
    dlg.set_close_response("deny");
    let reply = req.reply;
    let answered = Rc::new(Cell::new(false));
    {
        let answered = answered.clone();
        let reply = reply.clone();
        dlg.connect_response(None, move |d, r| {
            answered.set(true);
            let allow = r == "allow";
            let _ = reply.send((allow, allow && remember.is_active()));
            d.close();
        });
    }
    // The host gives up after a minute; don't leave a dead question up. The
    // bar shows how long is left.
    {
        let dlg = dlg.clone();
        let left = Rc::new(Cell::new(CONSENT_SECONDS));
        glib::timeout_add_seconds_local(1, move || {
            if answered.get() {
                return glib::ControlFlow::Break;
            }
            let now = left.get().saturating_sub(1);
            left.set(now);
            if now == 0 {
                let _ = reply.send((false, false));
                dlg.close();
                return glib::ControlFlow::Break;
            }
            if let Some(extra) = dlg.extra_child() {
                update_countdown(&extra, now);
            }
            glib::ControlFlow::Continue
        });
    }
    dlg.present();
}

const CONSENT_SECONDS: u32 = 60;

/// The question itself: who, from which device, what they'd be able to do,
/// and a clear warning — readable at a glance.
fn consent_body(who: &str, device: &str) -> gtk4::Box {
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    body.set_width_request(380);

    let badge = gtk4::Image::from_icon_name("preferences-desktop-remote-desktop-symbolic");
    badge.set_pixel_size(40);
    badge.add_css_class("consent-badge");
    badge.set_halign(gtk4::Align::Center);
    body.append(&badge);

    let title = gtk4::Label::new(Some(&tr!("Allow remote access?")));
    title.add_css_class("title-2");
    body.append(&title);

    let lead = gtk4::Label::new(None);
    lead.set_markup(&trf!(
        "<b>{user}</b> wants to <b>see and control</b> this screen.",
        "user" => glib::markup_escape_text(who)
    ));
    lead.set_wrap(true);
    lead.set_justify(gtk4::Justification::Center);
    body.append(&lead);

    // Who is asking, as a small card.
    let who_box = gtk4::ListBox::new();
    who_box.add_css_class("boxed-list");
    who_box.set_selection_mode(gtk4::SelectionMode::None);
    for (icon, label, value) in [
        ("avatar-default-symbolic", tr!("Person"), who),
        ("computer-symbolic", tr!("Device"), device),
    ] {
        let row = libadwaita::ActionRow::builder()
            .title(label)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        let v = gtk4::Label::new(Some(value));
        v.add_css_class("heading");
        v.set_selectable(true);
        row.add_suffix(&v);
        who_box.append(&row);
    }
    body.append(&who_box);

    // What allowing means.
    let can = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    for (icon, text) in [
        ("view-reveal-symbolic", tr!("See everything on your screen")),
        ("input-mouse-symbolic", tr!("Move the mouse and type on this computer")),
        ("audio-speakers-symbolic", tr!("Hear this computer's sound")),
    ] {
        let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        line.append(&gtk4::Image::from_icon_name(icon));
        let l = gtk4::Label::new(Some(&text));
        l.set_xalign(0.0);
        l.set_wrap(true);
        line.append(&l);
        can.append(&line);
    }
    body.append(&can);

    let warn = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    warn.add_css_class("consent-warning");
    let warn_icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    warn_icon.set_valign(gtk4::Align::Start);
    warn.append(&warn_icon);
    let warn_text = gtk4::Label::new(None);
    warn_text.set_markup(&tr!(
        "<b>Only allow it if you know this person and are expecting it.</b> You can end the access at any time from BigLace."
    ));
    warn_text.set_wrap(true);
    warn_text.set_xalign(0.0);
    warn_text.set_hexpand(true);
    warn.append(&warn_text);
    body.append(&warn);

    let bar = gtk4::ProgressBar::new();
    bar.set_fraction(1.0);
    bar.set_widget_name("consent-countdown");
    bar.add_css_class("consent-countdown");
    let left = gtk4::Label::new(None);
    left.set_widget_name("consent-left");
    left.add_css_class("dim-label");
    left.add_css_class("caption");
    body.append(&bar);
    body.append(&left);
    update_countdown(body.upcast_ref(), CONSENT_SECONDS);
    body
}

/// "Denied automatically in N s", and the bar shrinking with it.
fn update_countdown(body: &gtk4::Widget, left: u32) {
    let mut child = body.first_child();
    while let Some(w) = child {
        if let Some(bar) = w.downcast_ref::<gtk4::ProgressBar>() {
            bar.set_fraction(f64::from(left) / f64::from(CONSENT_SECONDS));
        } else if w.widget_name() == "consent-left" {
            if let Some(l) = w.downcast_ref::<gtk4::Label>() {
                l.set_text(&trf!("Denied automatically in {seconds} s", "seconds" => left.to_string()));
            }
        }
        child = w.next_sibling();
    }
}

// ─── Viewer side ─────────────────────────────────────────────────────────────

/// Open `name`'s screen (peer at `ip`) in a new window.
/// What the host can do besides showing its screen.
#[derive(Clone, Copy, Default)]
pub struct HostCaps {
    /// Restart / shut down.
    pub power: bool,
    /// Fit its resolution to this window.
    pub resize: bool,
}

pub fn open_viewer(ip: &str, name: &str, toast: &libadwaita::ToastOverlay, caps: HostCaps) {
    open_viewer_as(ip, name, toast, None, caps);
}

/// How long a restarted computer has to come back.
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// "Restart and reconnect" / "Shut down": ask first.
fn confirm_power(win: &libadwaita::Window, action: &'static str, name: &str, session: Rc<RefCell<Option<client::Session>>>) {
    let (heading, body, label) = if action == "restart" {
        (
            trf!("Restart {name}'s computer?", "name" => name),
            tr!("It restarts, signs in by itself and locks the screen right away; this window connects again when it's back. Unsaved work there is lost."),
            tr!("Restart"),
        )
    } else if action == "relogin" {
        (
            trf!("Restart {name}'s session?", "name" => name),
            tr!("The open programs there close, the computer signs out and back in by itself (without restarting) and locks the screen; this window connects again. Unsaved work there is lost."),
            tr!("Restart the session"),
        )
    } else {
        (
            trf!("Shut down {name}'s computer?", "name" => name),
            tr!("You won't be able to reach it again until someone turns it on. Unsaved work there is lost."),
            tr!("Shut Down"),
        )
    };
    let dlg = libadwaita::MessageDialog::builder().transient_for(win).modal(true).heading(heading).body(body).build();
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("go", &label);
    dlg.set_response_appearance("go", libadwaita::ResponseAppearance::Destructive);
    dlg.set_default_response(Some("cancel"));
    dlg.set_close_response("cancel");
    dlg.connect_response(None, move |_, r| {
        if r == "go" {
            if let Some(s) = session.borrow().as_ref() {
                s.power(action);
            }
        }
    });
    dlg.present();
}

/// Watch the screen `name` shows to the voice room of `group` (view only,
/// with "Request control").
pub fn open_stage_viewer(ip: &str, name: &str, group: &str, toast: &libadwaita::ToastOverlay) {
    open_viewer_as(ip, name, toast, Some(group.to_string()), HostCaps::default());
}

fn open_viewer_as(ip: &str, name: &str, toast: &libadwaita::ToastOverlay, stage: Option<String>, caps: HostCaps) {
    let power = caps.power;
    let watching = stage.is_some();
    let win = libadwaita::Window::builder()
        .title(trf!("Screen of {name}", "name" => name))
        .default_width(1100)
        .default_height(700)
        .build();
    let toolbar = libadwaita::ToolbarView::new();
    let header = libadwaita::HeaderBar::new();
    // Title + live numbers (latency, fps, bitrate), like a game streamer's
    // overlay: what this session really does, not a guess.
    let title = libadwaita::WindowTitle::new(&trf!("Screen of {name}", "name" => name), "");
    header.set_title_widget(Some(&title));
    let btn_full = gtk4::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text(tr!("Full screen (F11)"))
        .build();
    header.pack_end(&btn_full);
    // The host's sound plays here; one click to silence it.
    let btn_mute = gtk4::ToggleButton::builder()
        .icon_name("audio-volume-high-symbolic")
        .tooltip_text(tr!("Mute the remote sound"))
        .build();
    header.pack_end(&btn_mute);
    // Fit the host's resolution to this window (when it can).
    let btn_fit = gtk4::ToggleButton::builder()
        .icon_name("zoom-fit-best-symbolic")
        .tooltip_text(trf!("Fit {name}'s resolution to this window", "name" => name))
        .visible(caps.resize && !watching)
        .build();
    header.pack_end(&btn_fit);
    // "● LIVE": you're looking at (and controlling) someone's real screen.
    let live = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    live.add_css_class("live-pill");
    live.set_valign(gtk4::Align::Center);
    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("live-dot");
    dot.set_valign(gtk4::Align::Center);
    live.append(&dot);
    live.append(&gtk4::Label::new(Some(&tr!("LIVE"))));
    live.set_tooltip_text(Some(&if watching {
        tr!("You are watching this screen right now")
    } else {
        tr!("You are seeing and controlling this screen right now")
    }));
    header.pack_start(&live);
    // Room share: view only until the presenter hands over control.
    let btn_control = gtk4::Button::builder()
        .child(
            &libadwaita::ButtonContent::builder()
                .icon_name("input-mouse-symbolic")
                .label(tr!("Request control"))
                .build(),
        )
        .tooltip_text(trf!("Ask {name} to let you use the mouse and keyboard", "name" => name))
        .visible(watching)
        .build();
    header.pack_start(&btn_control);
    // 0 idle, 1 asked, 2 in control.
    let control_state = Rc::new(Cell::new(0u8));
    // The other side draws its pointer into the video (Wayland, rooms).
    let pointer_in_video = Rc::new(Cell::new(false));
    toolbar.add_top_bar(&header);

    // Pixel for pixel whenever the window is big enough (scaling is what
    // makes text blurry); only a screen larger than the window is reduced.
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::ScaleDown)
        .can_shrink(true)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.update_property(&[gtk4::accessible::Property::Label(&trf!("Screen of {name}", "name" => name))]);
    // The host doesn't draw its pointer into the video: ours moves with the
    // hand, without the video's delay (as RDP clients do).
    let waiting = libadwaita::StatusPage::builder()
        .icon_name("video-display-symbolic")
        .title(trf!("Waiting for {name}…", "name" => name))
        .description(if watching {
            tr!("Connecting to the screen shared with the room.")
        } else {
            tr!("The person at that computer needs to allow the access.")
        })
        .build();
    let stack = gtk4::Stack::new();
    stack.add_named(&waiting, Some("waiting"));
    stack.add_named(&picture, Some("screen"));
    toolbar.set_content(Some(&stack));
    win.set_content(Some(&toolbar));

    {
        let win = win.clone();
        btn_full.connect_clicked(move |_| {
            if win.is_fullscreen() {
                win.unfullscreen();
            } else {
                win.fullscreen();
            }
        });
    }
    {
        // Fullscreen hides the header bar; keep a way back.
        let toolbar = toolbar.clone();
        win.connect_fullscreened_notify(move |w| toolbar.set_reveal_top_bars(!w.is_fullscreen()));
    }

    let session: Rc<RefCell<Option<client::Session>>> = Rc::default();
    let video = Rc::new(Cell::new((0u32, 0u32)));
    wire_input(&win, &picture, &session, &video);

    // Opens a connection on a thread: (its outcome, its events). Called
    // again to come back after a restart.
    let connect = {
        let (ip, stage) = (ip.to_string(), stage.clone());
        move || {
            let (tx, rx) = async_channel::bounded::<client::Event>(4);
            let (open_tx, open_rx) = async_channel::bounded::<Result<client::Session, client::OpenError>>(1);
            let (ip, stage) = (ip.clone(), stage.clone());
            std::thread::spawn(move || {
                let opened = match &stage {
                    Some(group) => client::open_stage(&ip, group, tx),
                    None => client::open(&ip, tx),
                };
                let _ = open_tx.send_blocking(opened);
            });
            (open_rx, rx)
        }
    };
    // The window went away: stop trying to come back.
    let closed = Rc::new(Cell::new(false));
    {
        let closed = closed.clone();
        win.connect_destroy(move |_| closed.set(true));
    }
    // Set when the host said it's restarting / shutting down; how long to
    // wait before knocking again.
    let restarting = Rc::new(Cell::new(false));
    let first_wait = Rc::new(Cell::new(20u32));
    let shutting_down = Rc::new(Cell::new(false));

    let name_s = name.to_string();
    let toast = toast.clone();
    {
        let (session, btn, state, name_s, toast) =
            (session.clone(), btn_control.clone(), control_state.clone(), name_s.clone(), toast.clone());
        btn_control.connect_clicked(move |b| {
            let guard = session.borrow();
            let Some(s) = guard.as_ref() else { return };
            match state.get() {
                0 => {
                    s.ask_control();
                    state.set(1);
                    set_control_look(&btn, 1, &name_s);
                    super::show_toast(&toast, &trf!("Asked {name} for control…", "name" => name_s), 4);
                }
                2 => {
                    s.release_control();
                    state.set(0);
                    set_control_look(b, 0, &name_s);
                }
                _ => {}
            }
        });
    }
    // Power: restart (and come back) or shut down the computer being helped.
    if power && !watching {
        let pop_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        pop_box.set_margin_top(6);
        pop_box.set_margin_bottom(6);
        pop_box.set_margin_start(6);
        pop_box.set_margin_end(6);
        let popover = gtk4::Popover::builder().child(&pop_box).build();
        let btn_power = gtk4::MenuButton::builder()
            .icon_name("system-shutdown-symbolic")
            .tooltip_text(trf!("Restart or shut down {name}'s computer", "name" => name))
            .popover(&popover)
            .build();
        header.pack_end(&btn_power);
        for (action, icon, label) in [
            ("restart", "system-reboot-symbolic", tr!("Restart and reconnect")),
            ("relogin", "system-log-out-symbolic", tr!("Restart the session")),
            ("shutdown", "system-shutdown-symbolic", tr!("Shut down")),
        ] {
            let item = gtk4::Button::builder()
                .child(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).build())
                .css_classes(["flat"])
                .build();
            let (session, win, popover, name_s) = (session.clone(), win.clone(), popover.clone(), name.to_string());
            item.connect_clicked(move |_| {
                popover.popdown();
                confirm_power(&win, action, &name_s, session.clone());
            });
            pop_box.append(&item);
        }
    }
    {
        let (win, session, stack, picture, video, title) =
            (win.clone(), session.clone(), stack.clone(), picture.clone(), video.clone(), title.clone());
        let (waiting, connect, closed, restarting, shutting_down, first_wait) = (
            waiting.clone(),
            connect.clone(),
            closed.clone(),
            restarting.clone(),
            shutting_down.clone(),
            first_wait.clone(),
        );
        let (btn_control, control_state, live) = (btn_control.clone(), control_state.clone(), live.clone());
        let sized = Cell::new(false);
        // Decoded → drawn by GTK, added to the latency shown: the last frame
        // set and not drawn yet, and the (sum in ms, count) since the stats.
        let undrawn: Rc<Cell<Option<std::time::Instant>>> = Rc::default();
        let drawing: Rc<Cell<(f64, u32)>> = Rc::default();
        let measuring = Cell::new(false);
        glib::spawn_future_local(async move {
            // Set while waiting for a restarted computer to come back.
            let mut back_since: Option<std::time::Instant> = None;
            loop {
                if closed.get() {
                    return;
                }
                let (open_rx, rx) = connect();
                match open_rx.recv().await {
                    Ok(Ok(s)) => {
                        *session.borrow_mut() = Some(s);
                        if back_since.take().is_some() {
                            super::show_toast(&toast, &trf!("{name} is back.", "name" => name_s), 5);
                        }
                    }
                    Ok(Err(e)) => {
                        // Still restarting: try again in a moment.
                        if back_since.is_some_and(|t| t.elapsed() < RESTART_WAIT) {
                            glib::timeout_future_seconds(5).await;
                            continue;
                        }
                        let msg = if back_since.is_some() {
                            trf!("{name}'s computer didn't come back after the restart.", "name" => name_s)
                        } else {
                            match e {
                                client::OpenError::Denied => trf!("{name} didn't allow the access.", "name" => name_s),
                                client::OpenError::Unverified => tr!(
                                    "That computer couldn't confirm this device. Make sure both run an up-to-date BigLace."
                                ),
                                client::OpenError::Unavailable => trf!(
                                    "{name}'s computer can't share its screen (screen sharing isn't available there).",
                                    "name" => name_s
                                ),
                                client::OpenError::Other(d) => trf!("Could not connect: {error}", "error" => d),
                            }
                        };
                        super::show_toast(&toast, &msg, 8);
                        win.close();
                        return;
                    }
                    Err(_) => return,
                }
                stack.set_visible_child_name("screen");
                picture.grab_focus();
                while let Ok(event) = rx.recv().await {
                    match event {
                        client::Event::Power { action, ok, error } => {
                            if !ok {
                                // It didn't happen after all: back to the screen.
                                restarting.set(false);
                                shutting_down.set(false);
                                stack.set_visible_child_name("screen");
                                super::show_toast(&toast, &trf!("{name}'s computer said: {error}", "name" => name_s, "error" => error), 8);
                            } else if action == "restart" || action == "relogin" {
                                restarting.set(true);
                                // A session restart is quick (no boot).
                                first_wait.set(if action == "relogin" { 6 } else { 20 });
                                // Don't wait for the old connection to time
                                // out: the tunnel goes down with the machine.
                                let (s, still) = (session.clone(), restarting.clone());
                                glib::timeout_add_seconds_local_once(3, move || {
                                    // Unless it turned out it couldn't restart.
                                    if still.get() {
                                        if let Some(s) = s.borrow().as_ref() {
                                            s.close();
                                        }
                                    }
                                });
                                if action == "relogin" {
                                    waiting.set_icon_name(Some("system-log-out-symbolic"));
                                    waiting.set_title(&trf!("Restarting {name}'s session…", "name" => name_s));
                                    waiting.set_description(Some(&tr!(
                                        "This window connects again by itself when the session is back. It takes a few seconds."
                                    )));
                                } else {
                                    waiting.set_icon_name(Some("system-reboot-symbolic"));
                                    waiting.set_title(&trf!("Restarting {name}…", "name" => name_s));
                                    waiting.set_description(Some(&tr!(
                                        "This window connects again by itself when the computer is back. It takes a few minutes."
                                    )));
                                }
                                stack.set_visible_child_name("waiting");
                            } else if action == "shutdown" {
                                shutting_down.set(true);
                            }
                        }
                        client::Event::Cursor(shape) => match shape {
                            // Drawn in the video: no second pointer on top —
                            // unless we're the one in control (a room's
                            // shared screen): then our own, which follows the
                            // hand at once, and not the one coming back late
                            // in the video.
                            None => {
                                pointer_in_video.set(true);
                                if control_state.get() != 2 {
                                    picture.set_cursor_from_name(Some("none"));
                                }
                            }
                            Some(c) => {
                                crate::tailscale::dbg(&format!(
                                    "viewer: pointer shape {}x{} (hotspot {},{})",
                                    c.width, c.height, c.hot_x, c.hot_y
                                ));
                                let tex = gdk::MemoryTexture::new(
                                    c.width as i32,
                                    c.height as i32,
                                    gdk::MemoryFormat::R8g8b8a8,
                                    &c.data,
                                    c.width as usize * 4,
                                );
                                let cursor = gdk::Cursor::from_texture(&tex, c.hot_x as i32, c.hot_y as i32, None);
                                picture.set_cursor(Some(&cursor));
                            }
                        },
                        client::Event::Info(info) => {
                            video.set((info.width, info.height));
                            if !sized.replace(true) {
                                fit_window_to(&win, info.width, info.height);
                            }
                        }
                        client::Event::Stats(st) => {
                            let (drawn_ms, drawn) = drawing.replace((0.0, 0));
                            let draw_ms = if drawn > 0 { drawn_ms / f64::from(drawn) } else { 0.0 };
                            let mut line = trf!(
                                "{latency} ms · {fps} fps · {mbps} Mbit/s",
                                "latency" => format!("{:.0}", (st.latency_ms + draw_ms).max(0.0)),
                                "fps" => format!("{:.0}", st.fps),
                                "mbps" => format!("{:.1}", st.mbps)
                            );
                            line.push_str(if st.gpu { " · GPU" } else { " · CPU" });
                            if st.audio {
                                line.push_str(" · ♪");
                            }
                            title.set_subtitle(&line);
                        }
                        client::Event::Frame(f) => {
                            video.set((f.width, f.height));
                            let tex = gdk::MemoryTexture::new(
                                f.width as i32,
                                f.height as i32,
                                gdk::MemoryFormat::R8g8b8a8,
                                &f.data,
                                f.stride,
                            );
                            picture.set_paintable(Some(&tex));
                            undrawn.set(Some(f.decoded_at));
                            if !measuring.get() {
                                if let Some(clock) = picture.frame_clock() {
                                    measuring.set(true);
                                    let (undrawn, drawing) = (undrawn.clone(), drawing.clone());
                                    clock.connect_after_paint(move |_| {
                                        if let Some(at) = undrawn.take() {
                                            let (sum, n) = drawing.get();
                                            drawing.set((sum + at.elapsed().as_secs_f64() * 1000.0, n + 1));
                                        }
                                    });
                                }
                            }
                        }
                        client::Event::Control(granted) => {
                            if !watching {
                                continue;
                            }
                            if let Some(s) = session.borrow().as_ref() {
                                s.set_input(granted);
                            }
                            let before = control_state.replace(if granted { 2 } else { 0 });
                            set_control_look(&btn_control, control_state.get(), &name_s);
                            if pointer_in_video.get() {
                                picture.set_cursor_from_name(Some(if granted { "default" } else { "none" }));
                            }
                            if granted {
                                live.add_css_class("controlling");
                                picture.grab_focus();
                                super::show_toast(&toast, &trf!("You're controlling {name}'s screen now.", "name" => name_s), 4);
                            } else {
                                live.remove_css_class("controlling");
                                let msg = match before {
                                    1 => trf!("{name} didn't give you control.", "name" => name_s),
                                    2 => trf!("{name} took control back.", "name" => name_s),
                                    _ => String::new(),
                                };
                                if !msg.is_empty() {
                                    super::show_toast(&toast, &msg, 4);
                                }
                            }
                        }
                        client::Event::Ended(why) => {
                            session.borrow_mut().take();
                            if restarting.replace(false) {
                                // Give it time to go down, then knock until it's back.
                                stack.set_visible_child_name("waiting");
                                back_since = Some(std::time::Instant::now());
                                glib::timeout_future_seconds(first_wait.get()).await;
                                break;
                            }
                            let msg = if shutting_down.get() {
                                trf!("{name}'s computer is shutting down.", "name" => name_s)
                            } else if why.is_empty() {
                                trf!("The access to {name}'s screen ended.", "name" => name_s)
                            } else {
                                trf!("The access to {name}'s screen ended: {error}", "name" => name_s, "error" => why)
                            };
                            super::show_toast(&toast, &msg, 6);
                            win.close();
                            return;
                        }
                    }
                }
                if back_since.is_none() {
                    return;
                }
            }
        });
    }
    {
        // While on: follow this window's size (once it stops changing).
        let session2 = session.clone();
        btn_fit.connect_toggled(move |b| {
            if !b.is_active() {
                if let Some(s) = session2.borrow().as_ref() {
                    s.restore_size();
                }
            }
        });
        let (session, picture, btn, closed) = (session.clone(), picture.clone(), btn_fit.clone(), closed.clone());
        let (sent, seen) = (Cell::new((0u32, 0u32)), Cell::new((0u32, 0u32)));
        glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            if !btn.is_active() {
                sent.set((0, 0));
                return glib::ControlFlow::Continue;
            }
            let scale = picture.scale_factor().max(1) as u32;
            let size = (picture.width().max(0) as u32 * scale, picture.height().max(0) as u32 * scale);
            let steady = seen.replace(size) == size;
            if steady && size.0 >= 320 && size.1 >= 200 && sent.get() != size {
                if let Some(s) = session.borrow().as_ref() {
                    s.fit(size.0, size.1);
                    sent.set(size);
                }
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let session = session.clone();
        btn_mute.connect_toggled(move |b| {
            b.set_icon_name(if b.is_active() { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
            if let Some(s) = session.borrow().as_ref() {
                s.set_muted(b.is_active());
            }
        });
    }
    {
        // However the window goes away (even destroyed without a close
        // request), the session ends with it.
        let session = session.clone();
        win.connect_destroy(move |_| {
            if let Some(s) = session.borrow_mut().take() {
                s.close();
            }
        });
    }
    {
        let session = session.clone();
        win.connect_close_request(move |_| {
            if let Some(s) = session.borrow_mut().take() {
                s.close();
            }
            glib::Propagation::Proceed
        });
    }
    win.present();
}

fn set_control_look(b: &gtk4::Button, state: u8, name: &str) {
    let (icon, label) = match state {
        1 => ("content-loading-symbolic", trf!("Waiting for {name}…", "name" => name)),
        2 => ("input-mouse-symbolic", tr!("Give back control")),
        _ => ("input-mouse-symbolic", tr!("Request control")),
    };
    b.set_child(Some(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).build()));
    b.set_sensitive(state != 1);
    if state == 2 {
        b.add_css_class("suggested-action");
    } else {
        b.remove_css_class("suggested-action");
    }
}

/// Open the viewer at the remote screen's size (1:1 pixels) when it fits on
/// this monitor; otherwise maximized, reduced as little as possible.
fn fit_window_to(win: &libadwaita::Window, width: u32, height: u32) {
    let monitor = win
        .surface()
        .and_then(|surface| gtk4::prelude::WidgetExt::display(win).monitor_at_surface(&surface))
        .map(|m| m.geometry());
    // Header bar and borders.
    let (w, h) = (width as i32, height as i32 + 48);
    match monitor {
        Some(g) if w <= g.width() && h <= g.height() - 32 => {
            win.unmaximize();
            win.set_default_size(w, h);
        }
        _ => win.maximize(),
    }
}

/// Pointer position over the picture → 0..1 of the remote screen, taking
/// "scale down" (1:1 when it fits, reduced otherwise, centered) into account.
/// None outside the image.
fn normalize(picture: &gtk4::Picture, video: (u32, u32), x: f64, y: f64) -> Option<(f64, f64)> {
    let (vw, vh) = (f64::from(video.0), f64::from(video.1));
    let (ww, wh) = (f64::from(picture.width()), f64::from(picture.height()));
    if vw <= 0.0 || vh <= 0.0 || ww <= 0.0 || wh <= 0.0 {
        return None;
    }
    let scale = (ww / vw).min(wh / vh).min(1.0);
    let (dw, dh) = (vw * scale, vh * scale);
    let (ox, oy) = ((ww - dw) / 2.0, (wh - dh) / 2.0);
    let (nx, ny) = ((x - ox) / dw, (y - oy) / dh);
    ((0.0..=1.0).contains(&nx) && (0.0..=1.0).contains(&ny)).then_some((nx, ny))
}

/// The symbol of the key itself (first group, no modifiers): "a" for A,
/// "1" for "!". Falls back to what GTK reported, lower-cased.
fn base_keysym(c: &gtk4::EventControllerKey, keyval: gdk::Key, keycode: u32) -> u32 {
    use glib::translate::IntoGlib;
    let base = c
        .widget()
        .and_then(|w| w.display().map_keycode(keycode))
        .and_then(|entries| {
            entries.into_iter().find(|(k, _)| k.group() == 0 && k.level() == 0).map(|(_, key)| key)
        })
        .filter(|k| k.into_glib() != 0)
        .unwrap_or_else(|| keyval.to_lower());
    base.into_glib()
}

fn wire_input(
    win: &libadwaita::Window,
    picture: &gtk4::Picture,
    session: &Rc<RefCell<Option<client::Session>>>,
    video: &Rc<Cell<(u32, u32)>>,
) {
    let send = {
        let session = session.clone();
        move |e: Input| {
            if let Some(s) = session.borrow().as_ref() {
                s.send(e);
            }
        }
    };

    let motion = gtk4::EventControllerMotion::new();
    {
        let (send, picture, video) = (send.clone(), picture.clone(), video.clone());
        motion.connect_motion(move |_, x, y| {
            if let Some((x, y)) = normalize(&picture, video.get(), x, y) {
                send(Input::Move { x, y });
            }
        });
    }
    picture.add_controller(motion);

    let click = gtk4::GestureClick::new();
    click.set_button(0); // every button
    {
        let (send, picture, video) = (send.clone(), picture.clone(), video.clone());
        click.connect_pressed(move |g, _, x, y| {
            picture.grab_focus();
            if let Some((x, y)) = normalize(&picture, video.get(), x, y) {
                send(Input::Move { x, y });
            }
            send(Input::Button { button: g.current_button() as u8, pressed: true });
        });
    }
    {
        let send = send.clone();
        click.connect_released(move |g, _, _, _| {
            send(Input::Button { button: g.current_button() as u8, pressed: false });
        });
    }
    picture.add_controller(click);

    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    {
        let send = send.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            let (dx, dy) = (dx.round() as i32, dy.round() as i32);
            if dx != 0 || dy != 0 {
                send(Input::Scroll { dx, dy });
            }
            glib::Propagation::Stop
        });
    }
    picture.add_controller(scroll);

    // Keys go to the remote screen, except F11 (full screen here). What goes
    // is the key's own symbol, without Shift & co. applied ("a", not "A"):
    // the remote side applies its modifiers itself, and a release always
    // names the same key as its press — otherwise a Shift let go first left
    // "A" (and the Shift some compositors add to type it) held down there.
    let held: Rc<RefCell<std::collections::HashMap<u32, u32>>> = Rc::default();
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let (send, win, held) = (send.clone(), win.clone(), held.clone());
        keys.connect_key_pressed(move |c, keyval, keycode, _| {
            if keyval == gdk::Key::F11 {
                if win.is_fullscreen() {
                    win.unfullscreen();
                } else {
                    win.fullscreen();
                }
                return glib::Propagation::Stop;
            }
            let keysym = base_keysym(c, keyval, keycode);
            held.borrow_mut().insert(keycode, keysym);
            send(Input::Key { keysym, pressed: true });
            glib::Propagation::Stop
        });
    }
    {
        let (send, held) = (send.clone(), held.clone());
        keys.connect_key_released(move |c, keyval, keycode, _| {
            if keyval == gdk::Key::F11 {
                return;
            }
            let keysym = held.borrow_mut().remove(&keycode).unwrap_or_else(|| base_keysym(c, keyval, keycode));
            send(Input::Key { keysym, pressed: false });
        });
    }
    // Leaving the window (Alt+Tab, a dialog) with keys down: let them go
    // there too, or they stay pressed on the remote screen.
    {
        let (send, held) = (send.clone(), held.clone());
        win.connect_is_active_notify(move |w| {
            if !w.is_active() {
                for (_, keysym) in held.borrow_mut().drain() {
                    send(Input::Key { keysym, pressed: false });
                }
            }
        });
    }
    picture.add_controller(keys);
}
