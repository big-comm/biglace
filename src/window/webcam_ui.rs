//! "Use as webcam" on one of our phones: pick the camera, the microphone or
//! both; the phone asks its person, then they show up as PipeWire devices
//! (see crate::webcam). The same button stops it.

use crate::{tr, trf};
use gtk4::prelude::*;
use libadwaita::prelude::*;
use std::cell::RefCell;

thread_local! {
    /// The webcam buttons on show, each with its phone: the one in use is marked (see [`mark`]).
    static BUTTONS: RefCell<Vec<(gtk4::glib::WeakRef<gtk4::Button>, String)>> = const { RefCell::new(Vec::new()) };
}

/// Mark `btn` as the phone's webcam in use, or not.
fn mark(btn: &gtk4::Button, on: bool) {
    if on {
        btn.add_css_class("warning");
        btn.set_tooltip_text(Some(&tr!("Stop the webcam")));
    } else {
        btn.remove_css_class("warning");
        btn.set_tooltip_text(Some(&tr!("Use as webcam")));
    }
}

/// The button for the phone at `target` (tailnet address or a paired device's LAN address).
pub fn button(target: &str, name: &str, toast: &libadwaita::ToastOverlay) -> gtk4::Button {
    let btn = gtk4::Button::builder()
        .icon_name("camera-web-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .build();
    mark(&btn, crate::webcam::active().as_deref() == Some(target));
    BUTTONS.with_borrow_mut(|b| {
        b.retain(|(w, _)| w.upgrade().is_some());
        b.push((btn.downgrade(), target.to_string()));
    });
    let (target, name, toast) = (target.to_string(), name.to_string(), toast.downgrade());
    btn.connect_clicked(move |b| {
        let Some(toast) = toast.upgrade() else { return };
        if crate::webcam::active().as_deref() == Some(target.as_str()) {
            crate::webcam::stop();
            return;
        }
        if let Some(win) = b.root().and_downcast::<libadwaita::ApplicationWindow>() {
            ask(&win, &toast, &target, &name);
        }
    });
    btn
}

/// What to take from the phone, then ask it.
fn ask(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, target: &str, name: &str) {
    let dlg = libadwaita::MessageDialog::new(
        Some(win),
        Some(&trf!("Use {name} as a webcam?", "name" => name)),
        Some(&tr!("Pick what this computer gets. The phone asks before it starts, and either side can stop it.")),
    );
    let video = gtk4::CheckButton::builder().label(tr!("Camera")).active(true).build();
    let front = gtk4::CheckButton::builder().label(tr!("Front camera")).active(true).margin_start(24).build();
    let back = gtk4::CheckButton::builder().label(tr!("Back camera")).group(&front).margin_start(24).build();
    let audio = gtk4::CheckButton::builder().label(tr!("Microphone")).active(true).build();
    video.bind_property("active", &front, "sensitive").sync_create().build();
    video.bind_property("active", &back, "sensitive").sync_create().build();
    let choices = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    choices.set_halign(gtk4::Align::Center);
    choices.append(&video);
    choices.append(&front);
    choices.append(&back);
    choices.append(&audio);
    dlg.set_extra_child(Some(&choices));
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("start", &tr!("Start"));
    dlg.set_response_appearance("start", libadwaita::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("start"));
    dlg.set_close_response("cancel");
    // Nothing to start without one of them.
    for check in [&video, &audio] {
        let (dlg, video, audio) = (dlg.downgrade(), video.downgrade(), audio.downgrade());
        check.connect_toggled(move |_| {
            if let (Some(d), Some(v), Some(a)) = (dlg.upgrade(), video.upgrade(), audio.upgrade()) {
                d.set_response_enabled("start", v.is_active() || a.is_active());
            }
        });
    }
    let (toast, target, name) = (toast.clone(), target.to_string(), name.to_string());
    dlg.connect_response(None, move |d, r| {
        d.close();
        if r != "start" {
            return;
        }
        let (v, a, b) = (video.is_active(), audio.is_active(), back.is_active());
        let (target, name, toast) = (target.clone(), name.clone(), toast.clone());
        super::show_toast(&toast, &tr!("Confirm on the phone to start."), 6);
        super::run_in_background(
            move || crate::webcam::start(&target, &name, v, a, b),
            move |r| {
                if let Err(e) = r {
                    super::show_toast(&toast, &trf!("The webcam didn't start: {error}", "error" => e), 8);
                }
            },
        );
    });
    dlg.present();
}

/// Say when a phone's webcam starts and ends.
pub fn wire(toast: &libadwaita::ToastOverlay) {
    let events = crate::webcam::events();
    let toast = toast.downgrade();
    gtk4::glib::spawn_future_local(async move {
        while let Ok(ev) = events.recv().await {
            let Some(toast) = toast.upgrade() else { break };
            let active = crate::webcam::active();
            BUTTONS.with_borrow_mut(|b| {
                b.retain(|(w, target)| {
                    let Some(btn) = w.upgrade() else { return false };
                    mark(&btn, active.as_deref() == Some(target.as_str()));
                    true
                });
            });
            let text = match (ev.on, ev.error) {
                (true, _) => tr!("Webcam connected. Pick it in the app that uses the camera or microphone."),
                (false, Some(e)) => trf!("The webcam ended: {error}", "error" => e),
                (false, None) => tr!("Webcam ended."),
            };
            super::show_toast(&toast, &text, 6);
        }
    });
}
