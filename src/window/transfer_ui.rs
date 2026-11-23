//! Sending files to a device and receiving them (see `crate::transfer`): the
//! file picker behind a device's send button, and the notifications that
//! offer Accept, Save to… and Decline, show progress and say how it went.

use gtk4::{gio, glib, prelude::*};
use std::path::PathBuf;

use crate::transfer::{self, Action, TransferEvent};
use crate::{tr, trf};

/// App actions, from the notifications: the offer's id (Open folder: the path).
const ACCEPT: &str = "transfer-accept";
const SAVE_TO: &str = "transfer-save-to";
const DECLINE: &str = "transfer-decline";
const OPEN_FOLDER: &str = "transfer-open-folder";

fn notification_id(id: &str) -> String {
    format!("biglace-transfer-{id}")
}

/// Pick files and offer them to `user`'s device `name` at `ip`.
pub fn send_files(
    win: &libadwaita::ApplicationWindow,
    toast: &libadwaita::ToastOverlay,
    name: &str,
    send: impl FnOnce(&[PathBuf]) -> Result<usize, String> + Send + 'static,
) {
    let dialog = gtk4::FileDialog::builder()
        .title(trf!("Send files to {name}", "name" => name))
        .modal(true)
        .build();
    let (name, toast) = (name.to_string(), toast.clone());
    dialog.open_multiple(Some(win), gio::Cancellable::NONE, move |result| {
        let Ok(list) = result else { return };
        let paths: Vec<PathBuf> = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gio::File>())
            .filter_map(|f| f.path())
            .collect();
        if paths.is_empty() {
            return;
        }
        super::show_toast(&toast, &trf!("Preparing the files for {name}…", "name" => name), 3);
        let toast = toast.clone();
        let who = name.clone();
        super::run_in_background(
            move || send(&paths),
            move |r| match r {
                Ok(_) => super::show_toast(&toast, &trf!("Waiting for {name} to accept…", "name" => who), 5),
                Err(e) => super::show_toast(&toast, &e, 8),
            },
        );
    });
}

/// Register the notification actions and follow transfers (once, at startup).
pub fn wire(win: &libadwaita::ApplicationWindow) {
    let Some(app) = win.application() else { return };
    add_actions(&app, win);
    let weak = win.downgrade();
    glib::spawn_future_local(async move {
        let rx = transfer::events();
        // The last whole percent shown per transfer: progress updates the notification only when it moves.
        let mut shown: std::collections::HashMap<String, u64> = Default::default();
        while let Ok(event) = rx.recv().await {
            let Some(app) = weak.upgrade().and_then(|w| w.application()) else {
                break;
            };
            notify(&app, event, &mut shown);
        }
    });
}

/// Accept, Decline, Save to… and Open folder, for the notifications' buttons.
fn add_actions(app: &gtk4::Application, win: &libadwaita::ApplicationWindow) {
    let accept = gio::SimpleAction::new(ACCEPT, Some(glib::VariantTy::STRING));
    accept.connect_activate(|_, p| {
        if let Some(id) = p.and_then(|p| p.get::<String>()) {
            transfer::accept(&id, None);
        }
    });
    app.add_action(&accept);
    let decline = gio::SimpleAction::new(DECLINE, Some(glib::VariantTy::STRING));
    decline.connect_activate(|_, p| {
        if let Some(id) = p.and_then(|p| p.get::<String>()) {
            transfer::decline(&id);
        }
    });
    app.add_action(&decline);
    let save_to = gio::SimpleAction::new(SAVE_TO, Some(glib::VariantTy::STRING));
    let weak = win.downgrade();
    save_to.connect_activate(move |_, p| {
        let (Some(id), Some(win)) = (p.and_then(|p| p.get::<String>()), weak.upgrade()) else {
            return;
        };
        let dialog = gtk4::FileDialog::builder().title(tr!("Save the files in")).modal(true).build();
        dialog.select_folder(Some(&win), gio::Cancellable::NONE, move |result| {
            if let Some(dir) = result.ok().and_then(|f| f.path()) {
                transfer::accept(&id, Some(dir));
            }
        });
    });
    app.add_action(&save_to);
    let open = gio::SimpleAction::new(OPEN_FOLDER, Some(glib::VariantTy::STRING));
    let weak = win.downgrade();
    open.connect_activate(move |_, p| {
        let (Some(path), Some(win)) = (p.and_then(|p| p.get::<String>()), weak.upgrade()) else {
            return;
        };
        gtk4::FileLauncher::new(Some(&gio::File::for_path(path))).launch(Some(&win), gio::Cancellable::NONE, |_| {});
    });
    app.add_action(&open);
}

/// One transfer event on the notifications (one notification per transfer, updated).
fn notify(app: &gtk4::Application, event: TransferEvent, shown: &mut std::collections::HashMap<String, u64>) {
    match event {
        TransferEvent::Offered { id, from, device, items } => {
            app.send_notification(Some(&notification_id(&id)), &offer(&id, &from, &device, &items));
        }
        TransferEvent::Progress { id, from, done, total } => {
            let percent = (done * 100).checked_div(total).unwrap_or(100);
            if shown.insert(id.clone(), percent) == Some(percent) {
                return;
            }
            let n = gio::Notification::new(&trf!("Receiving files from {user}", "user" => crate::chat::label(&from)));
            n.set_body(Some(&format!(
                "{percent}% · {} / {}",
                glib::format_size(done),
                glib::format_size(total)
            )));
            app.send_notification(Some(&notification_id(&id)), &n);
        }
        TransferEvent::Received { id, from, dir, count } => {
            shown.remove(&id);
            let n = gio::Notification::new(&if count == 1 {
                trf!("File received from {user}", "user" => crate::chat::label(&from))
            } else {
                trf!("{count} files received from {user}", "user" => crate::chat::label(&from), "count" => count.to_string())
            });
            n.set_body(Some(&dir.to_string_lossy()));
            let path = dir.to_string_lossy().into_owned();
            n.add_button_with_target_value(&tr!("Open folder"), &format!("app.{OPEN_FOLDER}"), Some(&path.to_variant()));
            app.send_notification(Some(&notification_id(&id)), &n);
        }
        TransferEvent::Failed { id, from, reason } => {
            shown.remove(&id);
            let n = gio::Notification::new(&trf!("Couldn't receive the files from {user}", "user" => crate::chat::label(&from)));
            n.set_body(Some(&reason));
            app.send_notification(Some(&notification_id(&id)), &n);
        }
        TransferEvent::Gone { id } => app.withdraw_notification(&notification_id(&id)),
        TransferEvent::Answered { id, to, action, count } => {
            app.send_notification(Some(&notification_id(&id)), &gio::Notification::new(&answer(&to, action, count)));
        }
    }
}

/// `from` offers `items`: their names and size, with Accept, Save to… and Decline.
fn offer(id: &str, from: &str, device: &str, items: &[transfer::Item]) -> gio::Notification {
    let total: u64 = items.iter().map(|i| i.size).sum();
    let n = gio::Notification::new(&if items.len() == 1 {
        trf!("{user} wants to send you a file", "user" => crate::chat::label(from))
    } else {
        trf!("{user} wants to send you {count} files", "user" => crate::chat::label(from), "count" => items.len().to_string())
    });
    let names: Vec<&str> = items.iter().take(3).map(|i| i.name.as_str()).collect();
    let more = if items.len() > 3 { " …" } else { "" };
    n.set_body(Some(&format!("{}{more}\n{} · {device}", names.join(", "), glib::format_size(total))));
    n.set_priority(gio::NotificationPriority::High);
    n.add_button_with_target_value(&tr!("Accept"), &format!("app.{ACCEPT}"), Some(&id.to_variant()));
    n.add_button_with_target_value(&tr!("Save to…"), &format!("app.{SAVE_TO}"), Some(&id.to_variant()));
    n.add_button_with_target_value(&tr!("Decline"), &format!("app.{DECLINE}"), Some(&id.to_variant()));
    n
}

/// What `to` did with files we sent.
fn answer(to: &str, action: Action, count: usize) -> String {
    match action {
        Action::Accept => trf!("{user} accepted: sending…", "user" => crate::chat::label(to)),
        Action::Decline => trf!("{user} declined the files", "user" => crate::chat::label(to)),
        Action::Done if count == 1 => trf!("{user} received the file", "user" => crate::chat::label(to)),
        Action::Done => trf!("{user} received the {count} files", "user" => crate::chat::label(to), "count" => count.to_string()),
        _ => trf!("{user} couldn't receive the files", "user" => crate::chat::label(to)),
    }
}
