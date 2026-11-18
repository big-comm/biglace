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
    dlg.set_extra_child(Some(&consent_body(&req.who, &req.device)));
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
            let _ = reply.send(r == "allow");
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
                let _ = reply.send(false);
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
pub fn open_viewer(ip: &str, name: &str, toast: &libadwaita::ToastOverlay) {
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
        .description(tr!("The person at that computer needs to allow the access."))
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

    let (tx, rx) = async_channel::bounded::<client::Event>(4);
    let (open_tx, open_rx) = async_channel::bounded::<Result<client::Session, client::OpenError>>(1);
    let ip_w = ip.to_string();
    std::thread::spawn(move || {
        let _ = open_tx.send_blocking(client::open(&ip_w, tx));
    });

    let name_s = name.to_string();
    let toast = toast.clone();
    {
        let (win, session, stack, picture, video, title) =
            (win.clone(), session.clone(), stack.clone(), picture.clone(), video.clone(), title.clone());
        let sized = Cell::new(false);
        glib::spawn_future_local(async move {
            match open_rx.recv().await {
                Ok(Ok(s)) => *session.borrow_mut() = Some(s),
                Ok(Err(e)) => {
                    let msg = match e {
                        client::OpenError::Denied => trf!("{name} didn't allow the access.", "name" => name_s),
                        client::OpenError::Unverified => tr!(
                            "That computer couldn't confirm this device. Make sure both run an up-to-date BigLace."
                        ),
                        client::OpenError::Unavailable => trf!(
                            "{name}'s computer can't share its screen (screen sharing isn't available there).",
                            "name" => name_s
                        ),
                        client::OpenError::Other(d) => trf!("Could not connect: {error}", "error" => d),
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
                    client::Event::Info(info) => {
                        video.set((info.width, info.height));
                        if !sized.replace(true) {
                            fit_window_to(&win, info.width, info.height);
                        }
                    }
                    client::Event::Stats(st) => {
                        let mut line = trf!(
                            "{latency} ms · {fps} fps · {mbps} Mbit/s",
                            "latency" => format!("{:.0}", st.latency_ms.max(0.0)),
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
                    }
                    client::Event::Ended(why) => {
                        let msg = if why.is_empty() {
                            trf!("The access to {name}'s screen ended.", "name" => name_s)
                        } else {
                            trf!("The access to {name}'s screen ended: {error}", "name" => name_s, "error" => why)
                        };
                        super::show_toast(&toast, &msg, 6);
                        win.close();
                        break;
                    }
                }
            }
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

    // Keys go to the remote screen, except F11 (full screen here).
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let (send, win) = (send.clone(), win.clone());
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::F11 {
                if win.is_fullscreen() {
                    win.unfullscreen();
                } else {
                    win.fullscreen();
                }
                return glib::Propagation::Stop;
            }
            send(Input::Key { keysym: glib::translate::IntoGlib::into_glib(keyval), pressed: true });
            glib::Propagation::Stop
        });
    }
    {
        let send = send.clone();
        keys.connect_key_released(move |_, keyval, _, _| {
            if keyval != gdk::Key::F11 {
                send(Input::Key { keysym: glib::translate::IntoGlib::into_glib(keyval), pressed: false });
            }
        });
    }
    picture.add_controller(keys);
}
