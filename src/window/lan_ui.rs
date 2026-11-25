//! The local network without a server (see `crate::lan`) in the window: its
//! switch, the devices nearby to pair with, and the code both people compare.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;

use crate::lan::discovery::Nearby;
use crate::lan::pairing::{self, LanEvent};
use crate::{config, tr, trf};

/// Wire the switch and the devices nearby on the main page, and follow the network (once, at startup).
pub fn wire(
    win: &libadwaita::ApplicationWindow,
    toast: &libadwaita::ToastOverlay,
    sidebar: &super::sidebar::Sidebar,
    content: &super::content::Content,
    cfg: &Rc<RefCell<config::Config>>,
    refresh: async_channel::Sender<()>,
) {
    for paired in crate::lan::peers::paired() {
        crate::chat::open(&crate::chat::ConvId::lan(&paired.key));
    }
    // Before the network starts: its first list of devices nearby would be lost.
    let events = pairing::events();
    let on = cfg.borrow().lan_enabled;
    crate::chat::set_local_only(on);
    sidebar.switch_lan.set_active(on);
    if on {
        start(toast, &content.lan_qr);
    }
    let nearby: Rc<RefCell<Vec<Nearby>>> = Rc::default();
    fill(&content.lan_list, &nearby, toast);
    wire_switch(win, toast, sidebar, &content.lan_qr, cfg, refresh);
    ask_if_both_on(win, sidebar, cfg);
    // A linked phone's battery, in its row.
    let linked = crate::link::events();
    let (list, shared, weak_toast) = (content.lan_list.downgrade(), nearby.clone(), toast.downgrade());
    glib::spawn_future_local(async move {
        while let Ok(event) = linked.recv().await {
            let (Some(list), Some(toast)) = (list.upgrade(), weak_toast.upgrade()) else { break };
            if matches!(event, crate::link::LinkEvent::Status) {
                fill(&list, &shared, &toast);
            }
        }
    });
    let (win, toast, list) = (win.downgrade(), toast.clone(), content.lan_list.clone());
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            let Some(win) = win.upgrade() else { break };
            match event {
                LanEvent::Nearby(found) => *nearby.borrow_mut() = found,
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
            fill(&list, &nearby, &toast);
        }
    });
}

/// The local network's switch: on while on BigScale leaves it first, once
/// the person agrees. Either way the window shows the network now in use.
fn wire_switch(
    win: &libadwaita::ApplicationWindow,
    toast: &libadwaita::ToastOverlay,
    sidebar: &super::sidebar::Sidebar,
    qr: &gtk4::Box,
    cfg: &Rc<RefCell<config::Config>>,
    refresh: async_channel::Sender<()>,
) {
    let (qr, toast, cfg, connect, win) = (qr.clone(), toast.clone(), cfg.clone(), sidebar.btn_connect.clone(), win.clone());
    sidebar.switch_lan.connect_state_set(move |switch, active| {
        if active && connect.has_css_class("destructive-action") {
            let (switch, qr, toast, cfg, connect, refresh) = (switch.clone(), qr.clone(), toast.clone(), cfg.clone(), connect.clone(), refresh.clone());
            confirm_local_only(&win, move |yes| {
                if !yes {
                    switch.set_active(false);
                    return;
                }
                connect.emit_clicked();
                switch.set_state(true);
                turn(&cfg, &qr, &toast, true, &connect);
                let _ = refresh.try_send(());
            });
            return glib::Propagation::Stop;
        }
        turn(&cfg, &qr, &toast, active, &connect);
        let _ = refresh.try_send(());
        glib::Propagation::Proceed
    });
}

/// Both on, as versions before local-only mode left them: once BigScale
/// shows as connected, ask once which one to keep.
fn ask_if_both_on(win: &libadwaita::ApplicationWindow, sidebar: &super::sidebar::Sidebar, cfg: &Rc<RefCell<config::Config>>) {
    if !cfg.borrow().lan_enabled || cfg.borrow().left_for_lan {
        return;
    }
    let asked = std::cell::Cell::new(false);
    let (win, switch, cfg) = (win.clone(), sidebar.switch_lan.clone(), cfg.clone());
    sidebar.btn_connect.connect_notify_local(Some("css-classes"), move |connect, _| {
        if asked.get() || !connect.has_css_class("destructive-action") || !switch.is_active() {
            return;
        }
        asked.set(true);
        let (switch, cfg, connect) = (switch.clone(), cfg.clone(), connect.clone());
        confirm_local_only(&win, move |yes| {
            if yes {
                let mut c = cfg.borrow_mut();
                c.left_for_lan = true;
                config::save_or_warn(&c);
                drop(c);
                connect.emit_clicked();
            } else {
                switch.set_active(false);
            }
        });
    });
}

/// Turn the local network on or off: on leaves BigScale, off connects it
/// again (when this device has a way in).
fn turn(cfg: &Rc<RefCell<config::Config>>, qr: &gtk4::Box, toast: &libadwaita::ToastOverlay, on: bool, connect: &gtk4::Button) {
    {
        let mut c = cfg.borrow_mut();
        c.lan_enabled = on;
        c.left_for_lan = on;
        config::save_or_warn(&c);
    }
    crate::chat::set_local_only(on);
    if on {
        start(toast, qr);
    } else {
        pairing::stop();
        if connect.is_sensitive() && !connect.has_css_class("destructive-action") {
            connect.emit_clicked();
        }
    }
}

/// "Use only the local network?": BigScale stays off while it is on.
/// `answer` gets whether the person agreed.
fn confirm_local_only(win: &libadwaita::ApplicationWindow, answer: impl Fn(bool) + 'static) {
    let dlg = libadwaita::MessageDialog::new(
        Some(win),
        Some(&tr!("Use only the local network?")),
        Some(&tr!("You leave BigScale while the local network is on, and connect again when you turn it off.")),
    );
    dlg.add_response("cancel", &tr!("Cancel"));
    dlg.add_response("local", &tr!("Use only the local network"));
    dlg.set_response_appearance("local", libadwaita::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("local"));
    dlg.set_close_response("cancel");
    dlg.connect_response(None, move |_, response| answer(response == "local"));
    dlg.present();
}

/// Announce us and look for the others; the code to scan follows our address.
fn start(toast: &libadwaita::ToastOverlay, qr: &gtk4::Box) {
    // The person and the machine: one person often has several.
    let (real, host) = (glib::real_name().to_string_lossy().into_owned(), glib::host_name().to_string());
    let name = if real.trim().is_empty() || real == "Unknown" { host } else { format!("{real} ({host})") };
    if let Err(e) = pairing::start(&name) {
        super::show_toast(toast, &trf!("Could not turn on the local network: {error}", "error" => e), 8);
    }
    while let Some(child) = qr.first_child() {
        qr.remove(&child);
    }
    // For a phone, or a network where devices don't see each other: it scans a code.
    if let Some(link) = pairing::link() {
        let button = gtk4::Button::builder().label(tr!("Pair a phone")).halign(gtk4::Align::Center).css_classes(["pill"]).build();
        button.connect_clicked(move |b| show_pairing_code(b, &link));
        qr.append(&button);
    }
}

/// The code a phone scans to pair with us, in a window of its own.
fn show_pairing_code(from: &gtk4::Button, link: &str) {
    let Some(texture) = super::invites::qr_texture(link) else { return };
    let code = gtk4::Image::from_paintable(Some(&texture));
    code.set_pixel_size(240);
    let how = gtk4::Label::builder()
        .label(tr!("On the phone, turn on Use only the local network, then tap Scan a computer's code."))
        .wrap(true)
        .max_width_chars(36)
        .justify(gtk4::Justification::Center)
        .build();
    let page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(18).margin_top(12).margin_bottom(24).margin_start(24).margin_end(24).build();
    page.append(&how);
    page.append(&code);
    let view = libadwaita::ToolbarView::new();
    view.add_top_bar(&libadwaita::HeaderBar::new());
    view.set_content(Some(&page));
    let window = libadwaita::Window::builder().title(tr!("Pair a phone")).modal(true).resizable(false).content(&view).build();
    if let Some(parent) = from.root().and_downcast::<gtk4::Window>() {
        window.set_transient_for(Some(&parent));
    }
    super::dialogs::close_on_escape(&window);
    window.present();
}

/// The devices nearby, each with Pair, and the paired ones, each with Forget.
fn fill(list: &gtk4::ListBox, shared: &Rc<RefCell<Vec<Nearby>>>, toast: &libadwaita::ToastOverlay) {
    let nearby = &*shared.borrow();
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let paired = crate::lan::peers::paired();
    let away: Vec<_> = paired.iter().filter(|p| !nearby.iter().any(|n| n.key == p.key)).collect();
    if nearby.is_empty() && away.is_empty() {
        list.append(&nobody_nearby());
        return;
    }
    let forget = |row: &libadwaita::ActionRow, key: &str| {
        let button = gtk4::Button::builder().label(tr!("Forget")).valign(gtk4::Align::Center).build();
        let (key, list, nearby, toast) = (key.to_string(), list.downgrade(), shared.clone(), toast.clone());
        button.connect_clicked(move |_| {
            crate::lan::peers::remove(&key);
            crate::chat::forget_lan(&key);
            crate::link::disconnect(&crate::chat::lan_address(&key));
            if let Some(list) = list.upgrade() {
                fill(&list, &nearby, &toast);
            }
        });
        row.add_suffix(&button);
    };
    for device in nearby {
        let mut subtitle = vec![device.address.clone()];
        subtitle.extend(super::peer_row::phone_status(&crate::chat::lan_address(&device.key)));
        let row = libadwaita::ActionRow::builder().title(&device.name).subtitle(subtitle.join("  ·  ")).build();
        if paired.iter().any(|p| p.key == device.key) {
            add_paired_actions(&row, device, toast);
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

/// What a paired device nearby offers: a computer its terminal, files and
/// screen; a phone whether it is ours; both taking files.
fn add_paired_actions(row: &libadwaita::ActionRow, device: &Nearby, toast: &libadwaita::ToastOverlay) {
    if let Some(login) = &device.login {
        add_ssh_buttons(row, &device.address, login, toast);
    }
    if device.takes_files {
        add_send_button(row, device, toast);
    }
    if let Some(screen) = device.screen {
        add_screen_button(row, device, screen, toast);
    }
    if device.login.is_none() {
        add_mine_check(row, &device.key);
    }
}

/// Marks a paired phone as ours: only then does it link to this computer.
fn add_mine_check(row: &libadwaita::ActionRow, key: &str) {
    let check = gtk4::CheckButton::builder()
        .label(tr!("My phone"))
        .tooltip_text(tr!("Share notifications, battery and clipboard with this phone"))
        .active(crate::lan::peers::is_mine(key))
        .valign(gtk4::Align::Center)
        .build();
    let key = key.to_string();
    check.connect_toggled(move |c| {
        crate::lan::peers::set_mine(&key, c.is_active());
        if !c.is_active() {
            crate::link::disconnect(&crate::chat::lan_address(&key));
        }
    });
    row.add_suffix(&check);
}

/// What the list says while no BigLace is in sight, and why that may be.
fn nobody_nearby() -> gtk4::Label {
    gtk4::Label::builder()
        .label(tr!("No BigLace nearby. The other device needs the local network turned on, on this same network. Guest Wi-Fi and some routers keep devices apart: use the main network."))
        .wrap(true)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build()
}

/// Terminal and files of a paired computer, over SSH to its address on this network.
fn add_ssh_buttons(row: &libadwaita::ActionRow, address: &str, login: &str, toast: &libadwaita::ToastOverlay) {
    type Open = fn(&str, &str) -> Result<(), String>;
    let buttons: [(&str, String, Open); 2] = [
        ("utilities-terminal-symbolic", tr!("Open terminal (SSH)"), crate::launchers::open_terminal),
        ("folder-symbolic", tr!("Open files (SFTP)"), crate::launchers::open_files),
    ];
    for (icon, tooltip, open) in buttons {
        let button = gtk4::Button::builder().icon_name(icon).css_classes(["flat"]).valign(gtk4::Align::Center).tooltip_text(tooltip).build();
        let (address, login, toast) = (address.to_string(), login.to_string(), toast.clone());
        button.connect_clicked(move |_| {
            let (address, login) = (address.clone(), login.clone());
            let toast = toast.clone();
            super::run_in_background(move || open(&address, &login), move |outcome| {
                if let Err(message) = outcome {
                    super::show_toast(&toast, &message, 6);
                }
            });
        });
        row.add_suffix(&button);
    }
}

/// See and control a paired computer's screen; the person there is asked each time.
fn add_screen_button(row: &libadwaita::ActionRow, device: &Nearby, screen: crate::lan::discovery::Screen, toast: &libadwaita::ToastOverlay) {
    let label = tr!("See and control the screen");
    let button = gtk4::Button::builder()
        .icon_name("video-display-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(&label)
        .build();
    button.update_property(&[gtk4::accessible::Property::Label(&label)]);
    let caps = super::remote_ui::HostCaps { power: false, resize: screen == crate::lan::discovery::Screen::Fits };
    let (key, name, toast) = (device.key.clone(), device.name.clone(), toast.clone());
    button.connect_clicked(move |_| super::remote_ui::open_lan_viewer(&key, &name, &toast, caps));
    row.add_suffix(&button);
}

/// Send files to a paired device: it accepts or declines them, as on the tailnet.
fn add_send_button(row: &libadwaita::ActionRow, device: &Nearby, toast: &libadwaita::ToastOverlay) {
    let button = gtk4::Button::builder()
        .icon_name("document-send-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Send files"))
        .build();
    let (key, name, toast) = (device.key.clone(), device.name.clone(), toast.clone());
    button.connect_clicked(move |b| {
        if let Some(win) = b.root().and_downcast::<libadwaita::ApplicationWindow>() {
            let key = key.clone();
            super::transfer_ui::send_files(&win, &toast, &name, move |paths| crate::transfer::send_lan(&key, paths));
        }
    });
    row.add_suffix(&button);
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
