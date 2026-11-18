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
/// session start/end.
pub fn start_host_listener(win: &libadwaita::ApplicationWindow) {
    let rx = remote::host_events();
    let win = win.clone();
    glib::spawn_future_local(async move {
        let notices: Rc<RefCell<Vec<(remote::host::StopHandle, libadwaita::Window)>>> = Rc::default();
        while let Ok(event) = rx.recv().await {
            match event {
                HostEvent::Consent(req) => ask_consent(&win, req),
                HostEvent::Started(session) => {
                    let notice = show_notice(&session);
                    notices.borrow_mut().push((session.stop.clone(), notice));
                }
                HostEvent::Ended(stop) => {
                    notices.borrow_mut().retain(|(s, w)| {
                        if s.same(&stop) {
                            w.destroy();
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
    });
}

fn ask_consent(win: &libadwaita::ApplicationWindow, req: remote::ConsentRequest) {
    // Bring BigLace forward: the question must not wait behind other windows.
    win.present();
    let dlg = libadwaita::MessageDialog::new(
        Some(win),
        Some(&tr!("Allow remote access?")),
        Some(&trf!(
            "{user} (device {device}) wants to see and control this screen.\n\nOnly allow it if you know this person and expect it. You can end the access at any time.",
            "user" => req.who,
            "device" => req.device
        )),
    );
    dlg.add_response("deny", &tr!("Deny"));
    dlg.add_response("allow", &tr!("Allow"));
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
    // The host gives up after a minute; don't leave a dead question up.
    {
        let dlg = dlg.clone();
        glib::timeout_add_seconds_local_once(60, move || {
            if !answered.get() {
                let _ = reply.send(false);
                dlg.close();
            }
        });
    }
    dlg.present();
}

/// Small window that stays while someone controls this screen.
fn show_notice(session: &remote::HostSession) -> libadwaita::Window {
    let w = libadwaita::Window::builder()
        .title(tr!("Remote access"))
        .default_width(360)
        .resizable(false)
        .deletable(false)
        .build();
    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    bx.set_margin_top(18);
    bx.set_margin_bottom(18);
    bx.set_margin_start(18);
    bx.set_margin_end(18);
    let icon = gtk4::Image::from_icon_name("video-display-symbolic");
    icon.set_pixel_size(32);
    let label = gtk4::Label::new(Some(&trf!(
        "{user} ({device}) is seeing and controlling this screen.",
        "user" => session.who,
        "device" => session.device
    )));
    label.add_css_class("title-4");
    label.set_wrap(true);
    label.set_justify(gtk4::Justification::Center);
    let btn = gtk4::Button::builder()
        .label(tr!("End access"))
        .css_classes(["destructive-action", "pill"])
        .halign(gtk4::Align::Center)
        .build();
    {
        let stop = session.stop.clone();
        btn.connect_clicked(move |_| stop.stop());
    }
    bx.append(&icon);
    bx.append(&label);
    bx.append(&btn);
    w.set_content(Some(&bx));
    w.present();
    w
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
    let btn_full = gtk4::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text(tr!("Full screen (F11)"))
        .build();
    header.pack_end(&btn_full);
    toolbar.add_top_bar(&header);

    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Contain)
        .can_shrink(true)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.update_property(&[gtk4::accessible::Property::Label(&trf!("Screen of {name}", "name" => name))]);
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
        let (win, session, stack, picture, video) =
            (win.clone(), session.clone(), stack.clone(), picture.clone(), video.clone());
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
                            "{name}'s computer can't share its screen yet (it needs an X11 session).",
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
                    client::Event::Info(info) => video.set((info.width, info.height)),
                    client::Event::Frame(f) => {
                        video.set((f.width, f.height));
                        let bytes = glib::Bytes::from_owned(f.data);
                        let tex = gdk::MemoryTexture::new(
                            f.width as i32,
                            f.height as i32,
                            gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
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
        win.connect_close_request(move |_| {
            if let Some(s) = session.borrow_mut().take() {
                s.close();
            }
            glib::Propagation::Proceed
        });
    }
    win.present();
}

/// Pointer position over the picture → 0..1 of the remote screen, taking
/// the letterboxing of "contain" into account. None outside the image.
fn normalize(picture: &gtk4::Picture, video: (u32, u32), x: f64, y: f64) -> Option<(f64, f64)> {
    let (vw, vh) = (f64::from(video.0), f64::from(video.1));
    let (ww, wh) = (f64::from(picture.width()), f64::from(picture.height()));
    if vw <= 0.0 || vh <= 0.0 || ww <= 0.0 || wh <= 0.0 {
        return None;
    }
    let scale = (ww / vw).min(wh / vh);
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
