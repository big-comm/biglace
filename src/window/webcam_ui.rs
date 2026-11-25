//! "Use as webcam" on one of our phones: pick the camera, the microphone or
//! both; the phone asks its person, then they show up as PipeWire devices
//! (see crate::webcam). The same button stops it.

use crate::{tr, trf};
use gtk4::prelude::*;
use libadwaita::prelude::*;

/// The button for the phone at `target` (tailnet address or a paired device's LAN address).
pub fn button(target: &str, name: &str, toast: &libadwaita::ToastOverlay) -> gtk4::Button {
    let btn = gtk4::Button::builder()
        .icon_name("camera-web-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Use as webcam"))
        .build();
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
    let audio = gtk4::CheckButton::builder().label(tr!("Microphone")).active(true).build();
    let choices = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    choices.set_halign(gtk4::Align::Center);
    choices.append(&video);
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
        let (v, a) = (video.is_active(), audio.is_active());
        let (target, name, toast) = (target.clone(), name.clone(), toast.clone());
        super::show_toast(&toast, &tr!("Confirm on the phone to start."), 6);
        super::run_in_background(
            move || crate::webcam::start(&target, &name, v, a),
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
            let text = match (ev.on, ev.error) {
                (true, _) => tr!("Webcam connected. Pick it in the app that uses the camera or microphone."),
                (false, Some(e)) => trf!("The webcam ended: {error}", "error" => e),
                (false, None) => tr!("Webcam ended."),
            };
            super::show_toast(&toast, &text, 6);
        }
    });
}
