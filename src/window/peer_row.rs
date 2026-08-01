use gtk4::prelude::*;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::config::{self, Config};
use crate::tailscale::{self, Peer};
use crate::tr;

/// Per-row dependencies that don't fit cleanly as plain args. The pin button
/// mutates config and the exit-node button kicks off `tailscale set` in a
/// background thread; both need a way to ask the window to re-render afterwards.
pub struct PeerCtx {
    pub toast: libadwaita::ToastOverlay,
    pub cfg: Rc<RefCell<Config>>,
    /// Latency in ms, indexed by hostname. `None` means "still measuring",
    /// missing key means "no data yet". Updated from a background poll —
    /// `Arc<Mutex<…>>` so the worker thread can write while the UI reads.
    pub latency: Arc<Mutex<std::collections::HashMap<String, Option<f64>>>>,
    /// Hook the window installs so peer-row actions can ask for a redraw
    /// (e.g. after pinning a peer, the list must re-sort).
    pub refresh: Rc<dyn Fn()>,
    /// Hostnames whose detail panel is currently expanded. Mutated by the
    /// row's `notify::expanded` listener so a periodic refresh that rebuilds
    /// the list can restore the user's open rows — otherwise tailscaled's
    /// 20-30s poll would slam every expander shut while the user is reading.
    pub expanded: Rc<RefCell<HashSet<String>>>,
}

/// Compose the row subtitle from the peer's static fields plus the latest
/// latency reading from the shared cache. Extracted so the window can update
/// just the subtitle on an existing row (soft refresh) without rebuilding
/// the whole expander — that's the common case, since the latency poll fires
/// every 20s and otherwise nothing has changed.
pub fn compose_subtitle(
    peer: &Peer,
    latency: &Arc<Mutex<std::collections::HashMap<String, Option<f64>>>>,
) -> String {
    let mut bits: Vec<String> = Vec::with_capacity(4);
    if !peer.ip.is_empty() {
        bits.push(peer.ip.clone());
    }
    let os_label = friendly_os(&peer.os);
    if !os_label.is_empty() {
        bits.push(os_label);
    }
    if !peer.online {
        bits.push(tr!("offline"));
    } else if let Some(ms) = latency
        .lock()
        .ok()
        .and_then(|g| g.get(&peer.hostname).copied().flatten())
    {
        bits.push(format!("{ms:.0} ms"));
    }
    bits.join("  ·  ")
}

pub fn build(peer: &Peer, ctx: &PeerCtx) -> libadwaita::ExpanderRow {
    let row = libadwaita::ExpanderRow::new();
    let display = peer.display_name();
    // display_name comes from the peer's DNS/hostname (network-controlled);
    // escape before it hits AdwExpanderRow's markup-aware title.
    let title = if display.is_empty() {
        "—".to_string()
    } else {
        display
    };
    row.set_title(&glib::markup_escape_text(&title));

    // ── Subtitle: IP · OS · offline · latency ──
    row.set_subtitle(&compose_subtitle(peer, &ctx.latency));

    // Restore expansion state from the previous render — without this, the
    // 20s latency poll (and any other refresh) would slam every expander
    // shut while the user was reading the detail rows.
    if ctx.expanded.borrow().contains(&peer.hostname) {
        row.set_expanded(true);
    }
    // Track the user's manual toggles so the next rebuild can restore them.
    // Programmatic `set_expanded(true)` above also fires the notify, which is
    // fine — re-inserting a value the set already contains is a no-op.
    {
        let expanded = ctx.expanded.clone();
        let host = peer.hostname.clone();
        row.connect_expanded_notify(move |r| {
            let mut s = expanded.borrow_mut();
            if r.is_expanded() {
                s.insert(host.clone());
            } else {
                s.remove(&host);
            }
        });
    }

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
        .tooltip_text(if is_fav {
            tr!("Unpin from top")
        } else {
            tr!("Pin to top")
        })
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
                config::save_or_warn(&c);
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

    // Hostname to display and to try first when launching ssh/sftp. The
    // actual target is decided at click time by `tailscale::pick_target`,
    // which falls back to the IP when the host's resolver can't resolve
    // the name (broken openresolv etc).
    let host = if peer.dns_name.is_empty() {
        peer.ip.clone()
    } else {
        peer.dns_name.clone()
    };
    let ip_fallback = if !peer.ipv4.is_empty() {
        peer.ipv4.clone()
    } else {
        peer.ip.clone()
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
        // Login precedence:
        //   1. Per-peer override set by the user — wins for multi-user
        //      servers where neither default fits.
        //   2. `os_user` propagated by the BigScale panel (Option D) —
        //      authoritative when available, but the panel needs to be on
        //      the tailnet for the worker to reach it.
        //   3. The peer's BigScale owner (`peer.user`) — works for personal
        //      devices where the OS user matches the account login (the
        //      common case), and survives panel renames since the owner
        //      doesn't change with `givenName`.
        //   4. Peer's OS hostname — last resort. Stale after a panel
        //      rename, but at least never empty.
        let ssh_user = ctx
            .cfg
            .borrow()
            .peer_overrides
            .get(&peer.hostname)
            .cloned()
            .or_else(|| {
                if !peer.ssh_user.is_empty() {
                    Some(peer.ssh_user.clone())
                } else if !peer.user.is_empty() {
                    Some(peer.user.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| peer.hostname.clone());

        let host_files = host.clone();
        let ip_files = ip_fallback.clone();
        let ssh_user_files = ssh_user.clone();
        let btn_files = gtk4::Button::builder()
            .icon_name("folder-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open files (SFTP)"))
            .build();
        let toast_files = ctx.toast.clone();
        btn_files.connect_clicked(move |_| {
            let host = host_files.clone();
            let ip = ip_files.clone();
            let user = ssh_user_files.clone();
            let toast = toast_files.clone();
            spawn_launcher(toast, move || {
                let target = tailscale::pick_target(&host, &ip);
                tailscale::open_files(&target, &user)
            });
        });
        row.add_suffix(&btn_files);

        let host_term = host.clone();
        let ip_term = ip_fallback.clone();
        let ssh_user_term = ssh_user.clone();
        let btn_term = gtk4::Button::builder()
            .icon_name("utilities-terminal-symbolic")
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Open terminal (SSH)"))
            .build();
        let toast_term = ctx.toast.clone();
        btn_term.connect_clicked(move |_| {
            let host = host_term.clone();
            let ip = ip_term.clone();
            let user = ssh_user_term.clone();
            let toast = toast_term.clone();
            spawn_launcher(toast, move || {
                let target = tailscale::pick_target(&host, &ip);
                tailscale::open_terminal(&target, &user)
            });
        });
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
        row.add_row(&detail_row(
            "avatar-default-symbolic",
            &tr!("Owner"),
            &peer.user,
        ));
    }

    // ── SSH login override row ──
    // EntryRow with libadwaita's apply button: typing + Enter saves; emptying
    // + Enter clears the override and falls back to the panel's os_user.
    {
        let host = peer.hostname.clone();
        let auto = if peer.ssh_user.is_empty() {
            peer.hostname.clone()
        } else {
            peer.ssh_user.clone()
        };
        let current = ctx
            .cfg
            .borrow()
            .peer_overrides
            .get(&host)
            .cloned()
            .unwrap_or_default();

        let entry = libadwaita::EntryRow::builder()
            .title(tr!("SSH login (override)"))
            .show_apply_button(true)
            .build();
        // Hint the user what would be used if they leave this empty.
        if !auto.is_empty() {
            entry.set_input_hints(gtk4::InputHints::NO_SPELLCHECK);
            entry.set_text(&current);
        }

        let icon = gtk4::Image::from_icon_name("avatar-default-symbolic");
        icon.set_pixel_size(18);
        icon.add_css_class("dim-label");
        entry.add_prefix(&icon);

        let cfg = ctx.cfg.clone();
        let toast = ctx.toast.clone();
        let refresh = ctx.refresh.clone();
        entry.connect_apply(move |e| {
            let value = e.text().to_string();
            {
                let mut c = cfg.borrow_mut();
                if value.trim().is_empty() {
                    c.peer_overrides.remove(&host);
                } else {
                    c.peer_overrides
                        .insert(host.clone(), value.trim().to_string());
                }
                config::save_or_warn(&c);
            }
            toast.add_toast(
                libadwaita::Toast::builder()
                    .title(tr!("Saved"))
                    .timeout(2)
                    .build(),
            );
            refresh();
        });
        row.add_row(&entry);
    }

    if !peer.tags.is_empty() {
        row.add_row(&detail_row(
            "emblem-system-symbolic",
            &tr!("Tags"),
            &peer.tags.join(", "),
        ));
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
            switch.connect_state_set(move |sw, active| {
                let target = if active { Some(host.clone()) } else { None };
                // Disable the switch until the operation resolves. Without this,
                // rapid toggles spawn concurrent `tailscale set --exit-node`
                // calls whose results land out of order, leaving the switch
                // desynced from the real routing state until the next rebuild.
                sw.set_sensitive(false);
                let sw_poll = sw.clone();
                // Run `tailscale set --exit-node=...` off the GTK thread so
                // pkexec prompts don't freeze the UI. We can't capture
                // ToastOverlay/Rc into the thread (not Send), so the worker
                // dumps its result into a Mutex<Option<...>> and a poller on
                // the main loop picks it up to fire the toast + refresh.
                // (succeeded, error message) handed back from the worker thread.
                type ExitNodeOutcome = (bool, Option<String>);
                let slot: Arc<Mutex<Option<ExitNodeOutcome>>> = Arc::new(Mutex::new(None));
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
                    sw_poll.set_sensitive(true);
                    let label = if ok {
                        if active {
                            tr!("Exit node enabled")
                        } else {
                            tr!("Exit node disabled")
                        }
                    } else {
                        format!(
                            "{}: {}",
                            tr!("Failed to update exit node"),
                            err.unwrap_or_default()
                        )
                    };
                    toast_t.add_toast(
                        libadwaita::Toast::builder()
                            // `label` can embed tailscale stderr on failure —
                            // escape so `<`/`&` don't corrupt the markup title.
                            .title(glib::markup_escape_text(&label).as_str())
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

/// Run a launcher (`open_files` / `open_terminal`) on a worker thread and toast
/// its error if it fails.
///
/// Both launchers do a DNS lookup and then probe several programs, each with a
/// settle window — up to a couple of seconds — so they can't run on the GTK
/// thread. `ToastOverlay` isn't `Send`, so the worker parks its result in a
/// Mutex and a main-loop poller picks it up, the same shape the exit-node
/// switch above uses.
fn spawn_launcher<F>(toast: libadwaita::ToastOverlay, work: F)
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let slot: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let slot_t = slot.clone();
    std::thread::spawn(move || {
        let outcome = work();
        if let Ok(mut g) = slot_t.lock() {
            *g = Some(outcome);
        }
    });
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let Some(outcome) = slot.lock().ok().and_then(|mut g| g.take()) else {
            return glib::ControlFlow::Continue;
        };
        if let Err(message) = outcome {
            toast.add_toast(
                libadwaita::Toast::builder()
                    // The message can embed a hostname or a handler name;
                    // escape before it hits AdwToast's markup-aware title.
                    .title(glib::markup_escape_text(&message).as_str())
                    .timeout(6)
                    .build(),
            );
        }
        glib::ControlFlow::Break
    });
}

fn detail_row(icon: &str, label: &str, value: &str) -> libadwaita::ActionRow {
    let r = libadwaita::ActionRow::new();
    r.set_title(label);
    // `value` is network-derived (DNS name, tags, owner); escape it so a `<`
    // or `&` can't corrupt AdwActionRow's markup-aware subtitle. `label` is
    // always one of our own literals, so it needs none.
    r.set_subtitle(&glib::markup_escape_text(value));
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
        toast_clone.add_toast(libadwaita::Toast::builder().title(&msg).timeout(2).build());
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
        let time_end = after_t
            .find('.')
            .or_else(|| after_t.find('Z'))
            .unwrap_or(after_t.len());
        let time = &after_t[..time_end];
        return format!("{date} {time} UTC");
    }
    iso.to_string()
}

fn os_icon_name(os: &str) -> &'static str {
    match os.to_lowercase().as_str() {
        "android" | "ios" | "iphone" | "ipad" => "phone-symbolic",
        "linux" | "windows" | "macos" | "darwin" | "" => "computer-symbolic",
        _ => "network-server-symbolic",
    }
}

fn friendly_os(os: &str) -> String {
    match os.to_lowercase().as_str() {
        "" => String::new(),
        "android" => "Android".into(),
        "ios" | "iphone" | "ipad" => "iOS".into(),
        "linux" => "Linux".into(),
        "windows" => "Windows".into(),
        "macos" | "darwin" => "macOS".into(),
        _ => os.to_string(),
    }
}

use gtk4::glib;
