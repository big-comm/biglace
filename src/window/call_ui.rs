//! Calls in the window (see `crate::call`): the call button's menu, the bar
//! over every page while a call rings or runs, the ringtone, and the
//! notification with Answer and Decline when someone calls.

use gtk4::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

use crate::call::{self, CallView};
use crate::{tr, trf};

/// Window actions behind the call button's menu: the person to call.
const CALL_VOICE: &str = "call-voice";
const CALL_VIDEO: &str = "call-video";
/// App actions (the notification's buttons, the bar's).
const CALL_ANSWER: &str = "call-answer";
const CALL_DECLINE: &str = "call-decline";
const CALL_HANG_UP: &str = "call-hang-up";
const NOTIFICATION_ID: &str = "biglace-call";

const RINGTONE: &str = "/usr/share/sounds/freedesktop/stereo/phone-incoming-call.oga";
const RINGBACK: &str = "/usr/share/sounds/freedesktop/stereo/phone-outgoing-calling.oga";

/// "Voice call" and "Video call" for `user` (the chat header's call button).
pub fn call_menu(user: &str) -> gio::Menu {
    let menu = gio::Menu::new();
    for (label, action) in [(tr!("Voice call"), CALL_VOICE), (tr!("Video call"), CALL_VIDEO)] {
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some(&format!("win.{action}")), Some(&user.to_variant()));
        menu.append_item(&item);
    }
    menu
}

/// A call in the history: how it went, and a button to call back.
pub fn call_entry(call: &crate::chat::CallRecord, outgoing: bool, user: &str) -> gtk4::Box {
    let missed = matches!(call.outcome, crate::chat::CallOutcome::Missed);
    let icon = gtk4::Image::from_icon_name(if call.video { "camera-web-symbolic" } else { "call-start-symbolic" });
    if missed {
        icon.add_css_class("error");
    }
    let line = gtk4::Label::builder()
        .label(crate::chat::call_line(call, outgoing))
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["bubble-text"])
        .build();
    let again = gtk4::Button::builder()
        .icon_name(if call.video { "camera-web-symbolic" } else { "call-start-symbolic" })
        .css_classes(["flat", "circular"])
        .tooltip_text(trf!("Call {user}", "user" => user))
        .action_name(format!("win.{}", if call.video { CALL_VIDEO } else { CALL_VOICE }))
        .action_target(&user.to_variant())
        .build();
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&icon);
    row.append(&line);
    row.append(&again);
    row
}

/// Register the call actions and follow the call. Returns the bar to put
/// over the pages; `show_room` brings the room page forward once a call is
/// answered.
pub fn wire(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, show_room: impl Fn() + 'static) -> gtk4::Revealer {
    add_actions(win, toast);
    follow(win, show_room)
}

/// The call button's actions (window) and Answer, Decline, Hang up (app,
/// for the notification too).
fn add_actions(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay) {
    for (name, video) in [(CALL_VOICE, false), (CALL_VIDEO, true)] {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let toast = toast.clone();
        action.connect_activate(move |_, param| {
            let Some(user) = param.and_then(|p| p.get::<String>()) else { return };
            let toast = toast.clone();
            super::run_in_background(
                move || call::start(&user, video),
                move |r| {
                    if let Err(e) = r {
                        super::show_toast(&toast, &e, 6);
                    }
                },
            );
        });
        win.add_action(&action);
    }
    if let Some(app) = win.application() {
        let toast_for_answer = toast.clone();
        let answer = gio::SimpleAction::new(CALL_ANSWER, None);
        answer.connect_activate(move |_, _| {
            let toast = toast_for_answer.clone();
            super::run_in_background(call::accept, move |r| {
                if let Err(e) = r {
                    super::show_toast(&toast, &e, 6);
                }
            });
        });
        app.add_action(&answer);
        let decline = gio::SimpleAction::new(CALL_DECLINE, None);
        decline.connect_activate(|_, _| call::decline());
        app.add_action(&decline);
        let hang_up = gio::SimpleAction::new(CALL_HANG_UP, None);
        hang_up.connect_activate(|_, _| {
            std::thread::spawn(call::hang_up);
        });
        app.add_action(&hang_up);
    }
}

/// The bar, the ringtone and the notification follow the call.
fn follow(win: &libadwaita::ApplicationWindow, show_room: impl Fn() + 'static) -> gtk4::Revealer {
    let bar = Bar::build();
    let revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .child(&bar.root)
        .build();
    let tone: Rc<RefCell<Option<Tone>>> = Rc::default();
    let ticker: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let (win, revealer_weak) = (win.downgrade(), revealer.downgrade());
    glib::spawn_future_local(async move {
        let rx = call::events();
        let mut was = CallView::Idle;
        while let Ok(view) = rx.recv().await {
            let (Some(win), Some(revealer)) = (win.upgrade(), revealer_weak.upgrade()) else { break };
            if let Some(id) = ticker.borrow_mut().take() {
                id.remove();
            }
            *tone.borrow_mut() = match &view {
                CallView::Incoming { .. } => Tone::play(RINGTONE),
                CallView::Outgoing { .. } => Tone::play(RINGBACK),
                _ => None,
            };
            notify(&win, &view);
            bar.show(&view);
            revealer.set_reveal_child(view != CallView::Idle);
            if let CallView::Active { since, .. } = view {
                if !matches!(was, CallView::Active { .. }) {
                    show_room();
                }
                let bar = bar.clone();
                *ticker.borrow_mut() = Some(glib::timeout_add_seconds_local(1, move || {
                    bar.tick(since);
                    glib::ControlFlow::Continue
                }));
            }
            was = view;
        }
    });
    revealer
}

/// Someone calling: a notification with Answer and Decline (withdrawn once
/// it stops ringing).
fn notify(win: &libadwaita::ApplicationWindow, view: &CallView) {
    let Some(app) = win.application() else { return };
    let CallView::Incoming { user, video, .. } = view else {
        app.withdraw_notification(NOTIFICATION_ID);
        return;
    };
    let n = gio::Notification::new(&if *video {
        trf!("{user} is video calling you", "user" => user)
    } else {
        trf!("{user} is calling you", "user" => user)
    });
    n.set_priority(gio::NotificationPriority::Urgent);
    n.add_button(&tr!("Answer"), &format!("app.{CALL_ANSWER}"));
    n.add_button(&tr!("Decline"), &format!("app.{CALL_DECLINE}"));
    app.send_notification(Some(NOTIFICATION_ID), &n);
}

/// The strip over the pages: who, what's happening, and its buttons.
#[derive(Clone)]
struct Bar {
    root: gtk4::Box,
    avatar: libadwaita::Avatar,
    title: gtk4::Label,
    status: gtk4::Label,
    answer: gtk4::Button,
    decline: gtk4::Button,
}

impl Bar {
    fn build() -> Self {
        let avatar = libadwaita::Avatar::new(32, None, true);
        let title = gtk4::Label::builder().xalign(0.0).css_classes(["heading"]).build();
        let status = gtk4::Label::builder().xalign(0.0).css_classes(["dim-label", "numeric"]).build();
        let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        texts.set_hexpand(true);
        texts.set_valign(gtk4::Align::Center);
        texts.append(&title);
        texts.append(&status);
        let answer = gtk4::Button::builder()
            .icon_name("call-start-symbolic")
            .css_classes(["circular", "call-answer"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Answer"))
            .action_name(format!("app.{CALL_ANSWER}"))
            .build();
        let decline = gtk4::Button::builder()
            .icon_name("call-stop-symbolic")
            .css_classes(["circular", "destructive-action"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Hang up"))
            .action_name(format!("app.{CALL_HANG_UP}"))
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        root.add_css_class("call-bar");
        root.append(&avatar);
        root.append(&texts);
        root.append(&answer);
        root.append(&decline);
        Self { root, avatar, title, status, answer, decline }
    }

    fn show(&self, view: &CallView) {
        let (user, status) = match view {
            CallView::Idle => return,
            CallView::Outgoing { user, video } => {
                (user, if *video { tr!("Video calling…") } else { tr!("Calling…") })
            }
            CallView::Incoming { user, device, video } => {
                let what = if *video { tr!("Video call") } else { tr!("Voice call") };
                (user, if device.is_empty() { what } else { format!("{what} · {device}") })
            }
            CallView::Active { user, since, .. } => (user, elapsed(*since)),
        };
        let ringing = matches!(view, CallView::Incoming { .. });
        self.avatar.set_text(Some(user));
        crate::avatars::bind(&self.avatar, user);
        self.title.set_text(user);
        self.status.set_text(&status);
        self.answer.set_visible(ringing);
        self.decline.set_tooltip_text(Some(&if ringing { tr!("Decline") } else { tr!("Hang up") }));
        self.decline.set_action_name(Some(&format!("app.{}", if ringing { CALL_DECLINE } else { CALL_HANG_UP })));
        self.root.update_property(&[gtk4::accessible::Property::Label(&format!("{user}: {status}"))]);
    }

    fn tick(&self, since: u64) {
        self.status.set_text(&elapsed(since));
    }
}

/// "In a call · 3:07" since `since` (ms since the epoch).
fn elapsed(since: u64) -> String {
    let secs = crate::chat::now_ms().saturating_sub(since) / 1000;
    trf!("In a call · {time}", "time" => format!("{}:{:02}", secs / 60, secs % 60))
}

/// A sound played over and over until dropped.
struct Tone {
    player: gstreamer::Element,
    _watch: gstreamer::bus::BusWatchGuard,
}

impl Tone {
    /// `None` when the sound can't play (missing file, no GStreamer).
    fn play(path: &str) -> Option<Self> {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        if !std::path::Path::new(path).exists() || gst::init().is_err() {
            return None;
        }
        let player = gst::ElementFactory::make("playbin").property("uri", format!("file://{path}")).build().ok()?;
        let weak = player.downgrade();
        let watch = player
            .bus()?
            .add_watch_local(move |_, msg| {
                if msg.type_() == gst::MessageType::Eos {
                    if let Some(p) = weak.upgrade() {
                        let _ = p.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::ZERO);
                    }
                }
                glib::ControlFlow::Continue
            })
            .ok()?;
        player.set_state(gst::State::Playing).ok()?;
        Some(Self { player, _watch: watch })
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        use gstreamer::prelude::*;
        let _ = self.player.set_state(gstreamer::State::Null);
    }
}
