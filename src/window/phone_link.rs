//! What our phones send (see `crate::link`), on the desktop: links open in
//! the browser, notifications show here with a Reply button, battery and
//! storage on the phone's device row, what it copies on the clipboard. Replying from a notification works the
//! same for BigLace's own chat messages.

use gtk4::{gio, glib, prelude::*};
use libadwaita::prelude::*;

use crate::link::{self, LinkEvent};
use crate::{tr, trf};

/// App action answering one of a phone's notifications: `[ip, key, title]`.
const PHONE_REPLY: &str = "phone-reply";
/// App action answering a chat conversation: its key (`dm-bruna`).
pub const CHAT_REPLY: &str = "chat-reply";

fn phone_notification_id(ip: &str, key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("biglace-phone-{ip}-{:x}", h.finish())
}

/// Listen to our phones and register the reply actions (once, at startup).
pub fn wire(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, refresh: async_channel::Sender<()>) {
    let Some(app) = win.application() else { return };
    app.add_action(&phone_reply_action(win, toast));
    app.add_action(&chat_reply_action(win, toast));
    let (win, toast) = (win.downgrade(), toast.clone());
    glib::spawn_future_local(async move {
        let rx = link::events();
        while let Ok(ev) = rx.recv().await {
            let Some(win) = win.upgrade() else { break };
            on_event(&win, &toast, &refresh, ev);
        }
    });
}

/// [`PHONE_REPLY`]: ask for the text, send it back through the phone.
fn phone_reply_action(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(PHONE_REPLY, Some(glib::VariantTy::STRING_ARRAY));
    let (win, toast) = (win.downgrade(), toast.clone());
    action.connect_activate(move |_, param| {
        let Some([ip, key, title]) = param.and_then(|p| p.get::<Vec<String>>()).and_then(|v| <[String; 3]>::try_from(v).ok())
        else {
            return;
        };
        let Some(win) = win.upgrade() else { return };
        let toast = toast.clone();
        ask_reply(&win, &title, move |text| {
            let (ip, key, toast) = (ip.clone(), key.clone(), toast.clone());
            super::run_in_background(
                move || link::reply(&ip, &key, &text),
                move |r| {
                    if let Err(e) = r {
                        super::show_toast(&toast, &trf!("Could not send: {error}", "error" => e), 6);
                    }
                },
            );
        });
    });
    action
}

/// [`CHAT_REPLY`]: ask for the text, send it to the conversation.
fn chat_reply_action(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(CHAT_REPLY, Some(glib::VariantTy::STRING));
    let (win, toast) = (win.downgrade(), toast.clone());
    action.connect_activate(move |_, param| {
        let Some(conv) = param.and_then(|p| p.get::<String>()).and_then(|k| crate::chat::ConvId::from_key(&k)) else { return };
        let Some(win) = win.upgrade() else { return };
        let toast = toast.clone();
        let title = conv.name.clone();
        ask_reply(&win, &title, move |text| {
            let (conv, toast) = (conv.clone(), toast.clone());
            super::run_in_background(
                move || crate::chat::send_reply(&conv, &text, None),
                move |r| {
                    if let Err(e) = r {
                        super::show_toast(&toast, &trf!("Could not send: {error}", "error" => e), 6);
                    }
                },
            );
        });
    });
    action
}

fn on_event(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, refresh: &async_channel::Sender<()>, ev: LinkEvent) {
    match ev {
        LinkEvent::Status => {
            let _ = refresh.try_send(());
        }
        LinkEvent::Open { device, url } => {
            gtk4::UriLauncher::new(&url).launch(Some(win), gio::Cancellable::NONE, |_| {});
            super::show_toast(toast, &trf!("Opened a link from {device}", "device" => device), 4);
        }
        LinkEvent::Notify { ip, device, key, app: from_app, title, text, can_reply } => {
            let Some(app) = win.application() else { return };
            let heading = if title.is_empty() { from_app.clone() } else { format!("{from_app} · {title}") };
            let n = gio::Notification::new(&heading);
            n.set_body(Some(&trf!("{text} (on {device})", "text" => text, "device" => device)));
            if can_reply {
                let target = vec![ip.clone(), key.clone(), heading].to_variant();
                n.add_button_with_target_value(&tr!("Reply"), &format!("app.{PHONE_REPLY}"), Some(&target));
            }
            app.send_notification(Some(&phone_notification_id(&ip, &key)), &n);
        }
        LinkEvent::Removed { ip, key } => {
            if let Some(app) = win.application() {
                app.withdraw_notification(&phone_notification_id(&ip, &key));
            }
        }
        LinkEvent::Clip(c) => crate::clipboard::apply(&gtk4::prelude::WidgetExt::display(win), c),
    }
}

/// A small window to type a reply to `title`; `send` gets the text.
fn ask_reply(win: &libadwaita::ApplicationWindow, title: &str, send: impl Fn(String) + 'static) {
    let dlg = libadwaita::MessageDialog::new(Some(win), Some(&trf!("Reply to {who}", "who" => title)), None);
    let entry = gtk4::Entry::builder().activates_default(true).placeholder_text(tr!("Message")).build();
    dlg.set_extra_child(Some(&entry));
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("send", &tr!("Send"));
    dlg.set_response_appearance("send", libadwaita::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("send"));
    dlg.set_close_response("cancel");
    let e = entry.clone();
    dlg.connect_response(None, move |_, response| {
        let text = e.text().trim().to_string();
        if response == "send" && !text.is_empty() {
            send(text);
        }
    });
    win.present();
    dlg.present();
    entry.grab_focus();
}
