use gtk4::prelude::*;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::config::{self, Config};
use crate::tailscale::{self, Peer};
use crate::tr;

/// Per-row dependencies that don't fit cleanly as plain args. The pin button
/// mutates config and the exit-node button kicks off `tailscale set` in a
/// background thread; both need a way to ask the window to re-render afterwards.
pub struct PeerCtx {
    pub toast:   libadwaita::ToastOverlay,
    pub cfg:     Rc<RefCell<Config>>,
    /// Latency in ms, indexed by hostname. `None` means "still measuring",
    /// missing key means "no data yet". Updated from a background poll —
    /// `Arc<Mutex<…>>` so the worker thread can write while the UI reads.
    pub latency: Arc<Mutex<std::collections::HashMap<String, Option<f64>>>>,
    /// Hook the window installs so peer-row actions can ask for a redraw
    /// (e.g. after pinning a peer, the list must re-sort).
    pub refresh: Rc<dyn Fn()>,
}

pub fn build(peer: &Peer, ctx: &PeerCtx) -> libadwaita::ExpanderRow {
    let row = libadwaita::ExpanderRow::new();
    row.set_title(if peer.hostname.is_empty() { "—" } else { &peer.hostname });

    // ── Subtitle: IP · OS · offline · latency ──
    let os_label = friendly_os(&peer.os);
    let mut bits: Vec<String> = Vec::with_capacity(4);
    if !peer.ip.is_empty() {
        bits.push(peer.ip.clone());
    }
    if !os_label.is_empty() {
        bits.push(os_label.clone());
    }
    if !peer.online {
        bits.push(tr!("offline"));
    } else if let Some(ms) = ctx.latency.lock().ok()
        .and_then(|g| g.get(&peer.hostname).copied().flatten())
    {
        bits.push(format!("{ms:.0} ms"));
    }
    row.set_subtitle(&bits.join("  ·  "));

    // ── Prefix: status dot + OS icon ──
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

    // ── Suffix buttons: pin, copy IP, files, terminal ──
    // Pin button — always visible, regardless of online state, so the user
    // can curate favorites even on quiet days. We use a fixed icon
    // (`starred-symbolic`) and toggle CSS classes for the visual state,
    // because some icon themes render `non-starred-symbolic` and
    // `starred-symbolic` nearly identically.
    let is_fav = ctx.cfg.borrow().is_favorite(&peer.hostname);
    let btn_pin = gtk4::Button::builder()
        .icon_name("starred-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(if is_fav { tr!("Unpin from top") } else { tr!("Pin to top") })
        .build();
    if is_fav {
        btn_pin.add_css_class("accent");
    } else {
        btn_pin.add_css_class("dim-label");
    }
    {
        let cfg = ctx.cfg.clone();
        let refresh = ctx.refresh.clone();
        let toast = ctx.toast.clone();
        let host = peer.hostname.clone();
        btn_pin.connect_clicked(move |_| {
            let now_pinned = {
                let mut c = cfg.borrow_mut();
                let p = c.toggle_favorite(&host);
                let _ = config::save(&c);
                p
            };
            toast.add_toast(
                libadwaita::Toast::builder()
                    .title(if now_pinned {
                        tr!("Pinned to top")
                    } else {
                        tr!("Unpinned")
                    })
                    .timeout(2)
                    .build(),
            );
            refresh();
        });
    }
    row.add_suffix(&btn_pin);

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
        let toast_clone = ctx.toast.clone();
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
        // SSH login is the OS user of the peer machine, propagated via the
        // `tag:user-<name>` ACL tag (see tailscale.rs::connect). When the
        // peer didn't advertise the tag (older biglace, denied by ACL, etc.)
        // we fall back to the peer's hostname — usually wrong, but better
        // than failing the launcher outright.
        let ssh_user = if peer.ssh_user.is_empty() {
            peer.hostname.clone()
        } else {
            peer.ssh_user.clone()
        };

        let host_files = host.clone();
        let ssh_user_files = ssh_user.clone();
        let btn_files = gtk4::Button::builder()
            .icon_name("folder-remote-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open files (SFTP)"))
            .build();
        btn_files.connect_clicked(move |_| tailscale::open_files(&host_files, &ssh_user_files));
        row.add_suffix(&btn_files);

        let host_term = host.clone();
        let ssh_user_term = ssh_user.clone();
        let btn_term = gtk4::Button::builder()
            .icon_name("utilities-terminal-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open terminal (SSH)"))
            .build();
        btn_term.connect_clicked(move |_| tailscale::open_terminal(&host_term, &ssh_user_term));
        row.add_suffix(&btn_term);
    }

    // ── Detail rows (visible when expanded) ──
    if !peer.dns_name.is_empty() {
        row.add_row(&detail_row_with_copy(
            "network-server-symbolic",
            &tr!("DNS name"),
            &peer.dns_name,
            &ctx.toast,
            &tr!("DNS name copied"),
        ));
    }
    if !peer.ipv4.is_empty() {
        row.add_row(&detail_row_with_copy(
            "network-wired-symbolic",
            "IPv4",
            &peer.ipv4,
            &ctx.toast,
            &tr!("IP address copied"),
        ));
    }
    if !peer.ipv6.is_empty() {
        row.add_row(&detail_row_with_copy(
            "network-wired-symbolic",
            "IPv6",
            &peer.ipv6,
            &ctx.toast,
            &tr!("IP address copied"),
        ));
    }
    if !peer.user.is_empty() {
        row.add_row(&detail_row("avatar-default-symbolic", &tr!("Owner"), &peer.user));
    }
    if !peer.tags.is_empty() {
        row.add_row(&detail_row("emblem-system-symbolic", &tr!("Tags"), &peer.tags.join(", ")));
    }
    if !peer.last_seen.is_empty() && !peer.online {
        row.add_row(&detail_row(
            "appointment-soon-symbolic",
            &tr!("Last seen"),
            &humanize_last_seen(&peer.last_seen),
        ));
    }

    if peer.exit_node_offered {
        let exit_row = libadwaita::ActionRow::builder()
            .title(tr!("Use as exit node"))
            .subtitle(tr!("Route all traffic through this peer"))
            .build();
        let exit_icon = gtk4::Image::from_icon_name("network-vpn-symbolic");
        exit_icon.set_pixel_size(20);
        exit_row.add_prefix(&exit_icon);
        let switch = gtk4::Switch::builder()
            .valign(gtk4::Align::Center)
            .active(peer.exit_node_active)
            .build();
        exit_row.add_suffix(&switch);
        exit_row.set_activatable_widget(Some(&switch));
        {
            let host = peer.hostname.clone();
            let refresh = ctx.refresh.clone();
            let toast = ctx.toast.clone();
            switch.connect_state_set(move |_, active| {
                let target = if active { Some(host.clone()) } else { None };
                // Run `tailscale set --exit-node=...` off the GTK thread so
                // pkexec prompts don't freeze the UI. We can't capture
                // ToastOverlay/Rc into the thread (not Send), so the worker
                // dumps its result into a Mutex<Option<...>> and a poller on
                // the main loop picks it up to fire the toast + refresh.
                let slot: Arc<Mutex<Option<(bool, Option<String>)>>> =
                    Arc::new(Mutex::new(None));
                let slot_t = slot.clone();
                std::thread::spawn(move || {
                    let r = tailscale::set_exit_node(target.as_deref());
                    let payload = match r {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e.to_string())),
                    };
                    if let Ok(mut g) = slot_t.lock() {
                        *g = Some(payload);
                    }
                });
                let toast_t = toast.clone();
                let refresh_t = refresh.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                    let Some((ok, err)) = slot.lock().ok().and_then(|mut g| g.take()) else {
                        return glib::ControlFlow::Continue;
                    };
                    let label = if ok {
                        if active { tr!("Exit node enabled") } else { tr!("Exit node disabled") }
                    } else {
                        format!("{}: {}", tr!("Failed to update exit node"), err.unwrap_or_default())
                    };
                    toast_t.add_toast(
                        libadwaita::Toast::builder()
                            .title(&label)
                            .timeout(3)
                            .build(),
                    );
                    refresh_t();
                    glib::ControlFlow::Break
                });
                glib::Propagation::Proceed
            });
        }
        row.add_row(&exit_row);
    }

    row
}

fn detail_row(icon: &str, label: &str, value: &str) -> libadwaita::ActionRow {
    let r = libadwaita::ActionRow::new();
    r.set_title(label);
    r.set_subtitle(value);
    let img = gtk4::Image::from_icon_name(icon);
    img.set_pixel_size(18);
    img.add_css_class("dim-label");
    r.add_prefix(&img);
    r
}

fn detail_row_with_copy(
    icon: &str,
    label: &str,
    value: &str,
    toast: &libadwaita::ToastOverlay,
    copied_msg: &str,
) -> libadwaita::ActionRow {
    let r = detail_row(icon, label, value);
    let btn = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Copy"))
        .build();
    let value_owned = value.to_string();
    let toast_clone = toast.clone();
    let msg = copied_msg.to_string();
    btn.connect_clicked(move |b| {
        b.display().clipboard().set_text(&value_owned);
        toast_clone.add_toast(
            libadwaita::Toast::builder()
                .title(&msg)
                .timeout(2)
                .build(),
        );
    });
    r.add_suffix(&btn);
    r
}

fn humanize_last_seen(iso: &str) -> String {
    // tailscaled emits RFC3339 like "2026-05-04T20:15:30.123Z". We don't pull
    // in chrono just for this — strip the timezone marker and trim subseconds
    // for a readable form. Falls back to the raw string when the shape is
    // unexpected.
    if let Some(t_idx) = iso.find('T') {
        let date = &iso[..t_idx];
        let after_t = &iso[t_idx + 1..];
        let time_end = after_t.find('.').or_else(|| after_t.find('Z')).unwrap_or(after_t.len());
        let time = &after_t[..time_end];
        return format!("{date} {time} UTC");
    }
    iso.to_string()
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

use gtk4::glib;
