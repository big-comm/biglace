//! The local network without a server (see `crate::lan`) in the window: its
//! switch, the devices nearby to pair with, and the code both people compare.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;

use crate::lan::discovery::Nearby;
use crate::lan::pairing::{self, LanEvent};
use crate::{config, tr, trf};

/// The devices nearby, and the list showing them while its window is open.
#[derive(Default)]
struct State {
    nearby: Vec<Nearby>,
    shown: Option<glib::WeakRef<gtk4::ListBox>>,
}

/// Wire the switch and the devices row, and follow the network (once, at startup).
pub fn wire(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, sidebar: &super::sidebar::Sidebar, cfg: &Rc<RefCell<config::Config>>) {
    let state: Rc<RefCell<State>> = Rc::default();
    for paired in crate::lan::peers::paired() {
        crate::chat::open(&crate::chat::ConvId::lan(&paired.key));
    }
    let on = cfg.borrow().lan_enabled;
    sidebar.switch_lan.set_active(on);
    sidebar.lan_devices_row.set_visible(on);
    if on {
        start(toast);
    }
    let (row, toast2, cfg) = (sidebar.lan_devices_row.clone(), toast.clone(), cfg.clone());
    sidebar.switch_lan.connect_state_set(move |_, active| {
        {
            let mut c = cfg.borrow_mut();
            c.lan_enabled = active;
            config::save_or_warn(&c);
        }
        row.set_visible(active);
        if active {
            start(&toast2);
        } else {
            pairing::stop();
        }
        glib::Propagation::Proceed
    });
    let (win2, state2) = (win.clone(), state.clone());
    sidebar.lan_devices_row.connect_activated(move |_| show_nearby(&win2, &state2));
    let (win, toast) = (win.downgrade(), toast.clone());
    glib::spawn_future_local(async move {
        let events = pairing::events();
        while let Ok(event) = events.recv().await {
            let Some(win) = win.upgrade() else { break };
            match event {
                LanEvent::Nearby(list) => state.borrow_mut().nearby = list,
                LanEvent::Code { name, code, answer } => {
                    ask_code(&win, &name, &code, answer);
                    continue;
                }
                LanEvent::Paired { key, name } => {
                    // Its conversation shows in Chat right away.
                    crate::chat::open(&crate::chat::ConvId::lan(&key));
                    super::show_toast(&toast, &trf!("Paired with {name}", "name" => name), 4);
                }
                LanEvent::PairFailed { name } => super::show_toast(&toast, &trf!("Not paired with {name}", "name" => name), 5),
            }
            // Who's nearby and where each pairing stands.
            let shown = state.borrow().shown.as_ref().and_then(glib::WeakRef::upgrade);
            if let Some(list) = shown {
                fill(&list, &state.borrow().nearby);
            }
        }
    });
}

fn start(toast: &libadwaita::ToastOverlay) {
    // The person and the machine: one person often has several.
    let (real, host) = (glib::real_name().to_string_lossy().into_owned(), glib::host_name().to_string());
    let name = if real.trim().is_empty() || real == "Unknown" { host } else { format!("{real} ({host})") };
    if let Err(e) = pairing::start(&name) {
        super::show_toast(toast, &trf!("Could not turn on the local network: {error}", "error" => e), 8);
    }
}

/// The devices nearby, each with Pair, and the paired ones, each with Forget.
fn fill(list: &gtk4::ListBox, nearby: &[Nearby]) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let paired = crate::lan::peers::paired();
    let away: Vec<_> = paired.iter().filter(|p| !nearby.iter().any(|n| n.key == p.key)).collect();
    if nearby.is_empty() && away.is_empty() {
        let empty = gtk4::Label::builder()
            .label(tr!("No BigLace nearby. The other device needs the local network turned on, on this same network."))
            .wrap(true)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .css_classes(["dim-label"])
            .build();
        list.append(&empty);
        return;
    }
    let forget = |row: &libadwaita::ActionRow, key: &str| {
        let button = gtk4::Button::builder().label(tr!("Forget")).valign(gtk4::Align::Center).build();
        let (key, list, nearby) = (key.to_string(), list.downgrade(), nearby.to_vec());
        button.connect_clicked(move |_| {
            crate::lan::peers::remove(&key);
            crate::chat::forget_lan(&key);
            if let Some(list) = list.upgrade() {
                fill(&list, &nearby);
            }
        });
        row.add_suffix(&button);
    };
    for device in nearby {
        let row = libadwaita::ActionRow::builder().title(&device.name).subtitle(&device.address).build();
        if paired.iter().any(|p| p.key == device.key) {
            forget(&row, &device.key);
            list.append(&row);
            continue;
        }
        let pair = gtk4::Button::builder().label(tr!("Pair")).valign(gtk4::Align::Center).css_classes(["suggested-action"]).build();
        let device = device.clone();
        pair.connect_clicked(move |b| {
            b.set_sensitive(false);
            b.set_label(&tr!("Waiting…"));
            pairing::pair_with(device.clone());
        });
        row.add_suffix(&pair);
        list.append(&row);
    }
    for device in away {
        let row = libadwaita::ActionRow::builder().title(&device.name).subtitle(tr!("Paired, not nearby")).build();
        forget(&row, &device.key);
        list.append(&row);
    }
}

fn show_nearby(win: &libadwaita::ApplicationWindow, state: &Rc<RefCell<State>>) {
    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    fill(&list, &state.borrow().nearby);
    state.borrow_mut().shown = Some(list.downgrade());
    let page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    page.append(&list);
    let view = libadwaita::ToolbarView::new();
    view.add_top_bar(&libadwaita::HeaderBar::new());
    view.set_content(Some(&gtk4::ScrolledWindow::builder().child(&page).propagate_natural_height(true).build()));
    let dialog = libadwaita::Window::builder()
        .title(tr!("Devices nearby"))
        .transient_for(win)
        .modal(true)
        .default_width(420)
        .default_height(360)
        .content(&view)
        .build();
    super::dialogs::close_on_escape(&dialog);
    dialog.present();
}

/// The code both people compare; our person's answer goes back to the pairing.
fn ask_code(win: &libadwaita::ApplicationWindow, name: &str, code: &str, answer: std::sync::mpsc::SyncSender<bool>) {
    let spaced = format!("{} {}", code.get(..3).unwrap_or(code), code.get(3..).unwrap_or(""));
    let dialog = libadwaita::MessageDialog::new(
        Some(win),
        Some(&trf!("Pair with {name}?", "name" => name)),
        Some(&trf!("Check that {name} shows the same code:", "name" => name)),
    );
    let digits = gtk4::Label::builder().label(spaced).css_classes(["title-1"]).selectable(true).build();
    dialog.set_extra_child(Some(&digits));
    dialog.add_response("cancel", &tr!("Cancel"));
    dialog.add_response("pair", &tr!("Pair"));
    dialog.set_response_appearance("pair", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.connect_response(None, move |_, response| {
        let _ = answer.try_send(response == "pair");
    });
    dialog.present();
}
