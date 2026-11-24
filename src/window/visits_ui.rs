//! People in this computer: over SSH (see `crate::ssh_watch`) a notification
//! with End access when someone comes in and another when they leave, and the
//! access history; seeing the screen ([`screen_started`]); all of them on the
//! tray while they stay.

use std::cell::RefCell;
use std::collections::BTreeMap;

use gtk4::{gio, glib, prelude::*};

use crate::remote::HostSession;
use crate::ssh_watch::{self, Event, Kind, Visit};
use crate::{tr, trf};

/// App action, from the notification's button: the session id.
const END: &str = "ssh-end";

thread_local! {
    /// Who is in now, by session id (main thread).
    static VISITS: RefCell<BTreeMap<String, Visit>> = RefCell::default();
    /// Who sees and controls the screen now (main thread).
    static SCREENS: RefCell<Vec<HostSession>> = RefCell::default();
    static TRAY: RefCell<Option<super::TrayHandle>> = RefCell::default();
}

fn notification_id(id: &str) -> String {
    format!("biglace-ssh-{id}")
}

/// "tales (bigrato)", or just who when the device is unknown.
fn person(v: &Visit) -> String {
    if v.device.is_empty() || v.device == v.who {
        v.who.clone()
    } else {
        trf!("{user} ({device})", "user" => v.who, "device" => v.device)
    }
}

fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Terminal => "utilities-terminal-symbolic",
        Kind::Files => "folder-remote-symbolic",
    }
}

pub fn wire(win: &libadwaita::ApplicationWindow, tray: super::TrayHandle) {
    let Some(app) = win.application() else { return };
    let end = gio::SimpleAction::new(END, Some(glib::VariantTy::STRING));
    let weak = app.downgrade();
    end.connect_activate(move |_, p| {
        if let (Some(id), Some(app)) = (p.and_then(|p| p.get::<String>()), weak.upgrade()) {
            end_visit(&app, id);
        }
    });
    app.add_action(&end);
    TRAY.set(Some(tray));

    let (tx, rx) = async_channel::unbounded();
    ssh_watch::watch(move |event| {
        let _ = tx.send_blocking(event);
    });
    let weak = app.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            let Some(app) = weak.upgrade() else { break };
            match event {
                Event::Started(v) => started(&app, v),
                Event::Ended(id) => ended(&app, &id),
            }
            show_on_tray();
        }
    });
}

/// `session` sees and controls this screen, until [`screen_ended`].
pub fn screen_started(session: HostSession) {
    SCREENS.with_borrow_mut(|s| s.push(session));
    show_on_tray();
}

pub fn screen_ended(stop: &crate::remote::host::StopHandle) {
    SCREENS.with_borrow_mut(|s| s.retain(|session| !session.stop.same(stop)));
    show_on_tray();
}

fn started(app: &gtk4::Application, v: Visit) {
    let who = person(&v);
    let title = match v.kind {
        Kind::Terminal => trf!("{user} opened a terminal on this computer", "user" => who),
        Kind::Files => trf!("{user} is browsing the files on this computer", "user" => who),
    };
    let n = gio::Notification::new(&title);
    n.set_body(Some(&tr!("It goes on until they close it, or until you end it.")));
    n.set_icon(&gio::ThemedIcon::new(icon(v.kind)));
    n.set_priority(gio::NotificationPriority::High);
    n.add_button_with_target_value(&tr!("End access"), &format!("app.{END}"), Some(&v.id.to_variant()));
    app.send_notification(Some(&notification_id(&v.id)), &n);
    crate::access::log(&v.who, &v.device, v.kind.what(), "allowed");
    VISITS.with(|m| m.borrow_mut().insert(v.id.clone(), v));
}

/// The notification that said they came in now says they left.
fn ended(app: &gtk4::Application, id: &str) {
    let Some(v) = VISITS.with(|m| m.borrow_mut().remove(id)) else { return };
    let who = person(&v);
    let title = match v.kind {
        Kind::Terminal => trf!("{user} closed the terminal on this computer", "user" => who),
        Kind::Files => trf!("{user} stopped browsing the files on this computer", "user" => who),
    };
    let n = gio::Notification::new(&title);
    n.set_icon(&gio::ThemedIcon::new(icon(v.kind)));
    app.send_notification(Some(&notification_id(id)), &n);
    crate::access::log(&v.who, &v.device, v.kind.what(), "ended");
}

fn end_visit(app: &gtk4::Application, id: String) {
    let weak = app.downgrade();
    super::run_in_background(
        move || ssh_watch::end(&id),
        move |r| {
            if let (Err(e), Some(app)) = (r, weak.upgrade()) {
                let n = gio::Notification::new(&tr!("Couldn't end the access"));
                n.set_body(Some(&e));
                app.send_notification(Some("biglace-ssh-end-failed"), &n);
            }
        },
    );
}

/// End everyone's access (the tray's End remote access).
pub fn end_all(app: &gtk4::Application) {
    for id in VISITS.with(|m| m.borrow().keys().cloned().collect::<Vec<_>>()) {
        end_visit(app, id);
    }
    SCREENS.with_borrow(|s| s.iter().for_each(|session| session.stop.stop()));
}

fn show_on_tray() {
    let mut lines: Vec<(String, &'static str)> = VISITS.with(|m| {
        m.borrow()
            .values()
            .map(|v| {
                let line = match v.kind {
                    Kind::Terminal => trf!("{user} · terminal", "user" => person(v)),
                    Kind::Files => trf!("{user} · files", "user" => person(v)),
                };
                (line, icon(v.kind))
            })
            .collect()
    });
    SCREENS.with_borrow(|s| {
        for session in s {
            let who = trf!("{user} ({device})", "user" => session.who, "device" => session.device);
            lines.push((trf!("{user} · screen", "user" => who), "video-display-symbolic"));
        }
    });
    TRAY.with_borrow(|tray| {
        let Some(tray) = tray else { return };
        if let Some(h) = tray.borrow().as_ref() {
            h.update_visits(lines);
        }
    });
}
