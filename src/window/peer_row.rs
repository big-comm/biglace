use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::tailscale::{self, Peer};
use crate::tr;

pub fn build(peer: &Peer, toast: &libadwaita::ToastOverlay) -> libadwaita::ActionRow {
    let row = libadwaita::ActionRow::new();
    row.set_title(if peer.hostname.is_empty() { "—" } else { &peer.hostname });

    let os_label = friendly_os(&peer.os);
    let subtitle = match (peer.online, os_label.is_empty()) {
        (true, true)   => peer.ip.clone(),
        (true, false)  => format!("{}  ·  {}", peer.ip, os_label),
        (false, true)  => format!("{}  ·  {}", peer.ip, tr!("offline")),
        (false, false) => format!("{}  ·  {}  ·  {}", peer.ip, os_label, tr!("offline")),
    };
    row.set_subtitle(&subtitle);

    let prefix = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    prefix.set_valign(gtk4::Align::Center);

    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.set_valign(gtk4::Align::Center);
    dot.set_size_request(8, 8);
    dot.add_css_class("peer-status-dot");
    if peer.online {
        dot.add_css_class("online");
    }
    prefix.append(&dot);

    let os_icon = gtk4::Image::from_icon_name(os_icon_name(&peer.os));
    os_icon.set_pixel_size(22);
    if !peer.online {
        os_icon.add_css_class("dim-label");
    }
    prefix.append(&os_icon);

    row.add_prefix(&prefix);

    let host = if peer.dns_name.is_empty() {
        peer.ip.clone()
    } else {
        peer.dns_name.clone()
    };

    if !peer.ip.is_empty() {
        let btn_copy = gtk4::Button::builder()
            .icon_name("edit-copy-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Copy IP address"))
            .build();
        let ip_for_copy = peer.ip.clone();
        let toast_clone = toast.clone();
        btn_copy.connect_clicked(move |btn| {
            btn.display().clipboard().set_text(&ip_for_copy);
            toast_clone.add_toast(
                libadwaita::Toast::builder()
                    .title(tr!("IP address copied"))
                    .timeout(2)
                    .build(),
            );
        });
        row.add_suffix(&btn_copy);
    }

    if peer.online && !host.is_empty() {
        let host_files = host.clone();
        let btn_files = gtk4::Button::builder()
            .icon_name("folder-remote-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open files (SFTP)"))
            .build();
        btn_files.connect_clicked(move |_| tailscale::open_files(&host_files));
        row.add_suffix(&btn_files);

        let host_term = host.clone();
        let btn_term = gtk4::Button::builder()
            .icon_name("utilities-terminal-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open terminal (SSH)"))
            .build();
        btn_term.connect_clicked(move |_| tailscale::open_terminal(&host_term));
        row.add_suffix(&btn_term);
    }

    row
}

fn os_icon_name(os: &str) -> &'static str {
    match os.to_lowercase().as_str() {
        "android" | "ios" | "iphone" | "ipad"  => "phone-symbolic",
        "linux" | "windows" | "macos" | "darwin" | "" => "computer-symbolic",
        _ => "network-server-symbolic",
    }
}

fn friendly_os(os: &str) -> String {
    match os.to_lowercase().as_str() {
        ""              => String::new(),
        "android"       => "Android".into(),
        "ios" | "iphone" | "ipad" => "iOS".into(),
        "linux"         => "Linux".into(),
        "windows"       => "Windows".into(),
        "macos" | "darwin" => "macOS".into(),
        _               => os.to_string(),
    }
}
