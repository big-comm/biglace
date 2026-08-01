use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::tr;

#[derive(Clone)]
pub struct Content {
    pub toolbar: libadwaita::ToolbarView,
    pub status_dot: gtk4::Box,
    pub status_label: gtk4::Label,
    pub health_badge: gtk4::Label,
    /// Shown when tailscaled couldn't install the tailnet's DNS config, which
    /// silently downgrades every peer button from names to raw IPs.
    pub dns_badge: gtk4::Label,
    pub update_badge: gtk4::Label,
    pub btn_refresh: gtk4::Button,
    pub btn_menu: gtk4::MenuButton,
    pub stack: gtk4::Stack,
    pub btn_start_service: gtk4::Button,
    pub self_row: libadwaita::ActionRow,
    pub btn_copy_self_ip: gtk4::Button,
    pub peers_list: gtk4::ListBox,
    pub bottom_label: gtk4::Label,
}

pub fn build() -> Content {
    let toolbar = libadwaita::ToolbarView::new();

    // ─── Header ───────────────────────────────────────────────────────────
    let header = libadwaita::HeaderBar::new();
    header.set_show_start_title_buttons(false);

    let title_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    title_box.set_valign(gtk4::Align::Center);

    let status_dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    status_dot.set_valign(gtk4::Align::Center);
    status_dot.set_size_request(10, 10);
    status_dot.add_css_class("status-dot");
    status_dot.add_css_class("idle");
    title_box.append(&status_dot);

    let status_label = gtk4::Label::new(Some(&tr!("Disconnected")));
    status_label.add_css_class("heading");
    title_box.append(&status_label);
    header.set_title_widget(Some(&title_box));

    let btn_menu = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text(tr!("Main menu"))
        .build();
    header.pack_end(&btn_menu);

    // Refresh used to be the first item inside the hamburger menu; bringing it
    // out as a one-click icon next to the menu cuts the most common debug
    // action from two clicks down to one. Header pack_end stacks
    // right-to-left, so adding refresh *after* the menu places it to the menu's
    // left, which is the conventional spot for action icons in Adwaita headers.
    let btn_refresh = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .css_classes(["flat"])
        .tooltip_text(tr!("Refresh"))
        .build();
    header.pack_end(&btn_refresh);

    // Header badges — visibility is toggled in apply_state(). Keep them in the
    // start area so they sit just left of the title without crowding the menu
    // button on the right.
    let health_badge = gtk4::Label::builder().label("").visible(false).build();
    health_badge.add_css_class("badge");
    health_badge.add_css_class("badge-warning");
    header.pack_start(&health_badge);

    // Unlike the Headscale badge this one stays useful while connected — the
    // tunnel is fine, it's name resolution that broke — so apply_state shows it
    // in every state. The tooltip carries the fix so the user doesn't have to
    // go digging through `tailscale status`.
    let dns_badge = gtk4::Label::builder().label("").visible(false).build();
    dns_badge.add_css_class("badge");
    dns_badge.add_css_class("badge-warning");
    dns_badge.set_tooltip_text(Some(&tr!(
        "Tailscale cannot write this device's DNS settings, so it can no longer \
         keep peer names working. They may still resolve for now, from the \
         configuration left over from before the failure, but that will break \
         the next time the network changes — and BigLace will fall back to IP \
         addresses. On most distributions `sudo resolvconf -u` repairs it."
    )));
    header.pack_start(&dns_badge);

    let update_badge = gtk4::Label::builder().label("").visible(false).build();
    update_badge.add_css_class("badge");
    update_badge.add_css_class("badge-info");
    header.pack_start(&update_badge);

    toolbar.add_top_bar(&header);

    // ─── Stack of states ──────────────────────────────────────────────────
    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.set_transition_duration(180);
    stack.set_vexpand(true);

    let (page_service, btn_start_service) = build_service_page();
    stack.add_named(&page_service, Some("service"));

    stack.add_named(
        &build_status_page(
            "dialog-password-symbolic",
            &tr!("Sign in to connect"),
            &tr!("Use the panel on the left to sign in or paste a pre-auth key."),
        ),
        Some("not-signed-in"),
    );

    stack.add_named(
        &build_status_page(
            "network-vpn-symbolic",
            &tr!("Not connected"),
            &tr!("Press Connect on the left to join the network."),
        ),
        Some("disconnected"),
    );

    stack.add_named(&build_connecting_page(), Some("connecting"));

    let (page_connected, self_row, btn_copy_self_ip, peers_list) = build_connected_page();
    stack.add_named(&page_connected, Some("connected"));

    toolbar.set_content(Some(&stack));

    // ─── Bottom info bar ──────────────────────────────────────────────────
    let bottom_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    bottom_box.add_css_class("bottom-info-bar");
    let bottom_icon = gtk4::Image::from_icon_name("network-vpn-symbolic");
    bottom_icon.set_pixel_size(14);
    bottom_icon.add_css_class("dim-label");
    bottom_box.append(&bottom_icon);
    let bottom_label = gtk4::Label::new(Some(""));
    bottom_label.add_css_class("dim-label");
    bottom_label.add_css_class("caption");
    bottom_label.set_xalign(0.0);
    bottom_label.set_hexpand(true);
    bottom_box.append(&bottom_label);
    toolbar.add_bottom_bar(&bottom_box);

    Content {
        toolbar,
        status_dot,
        status_label,
        health_badge,
        dns_badge,
        update_badge,
        btn_refresh,
        btn_menu,
        stack,
        btn_start_service,
        self_row,
        btn_copy_self_ip,
        peers_list,
        bottom_label,
    }
}

fn build_service_page() -> (libadwaita::StatusPage, gtk4::Button) {
    let page = libadwaita::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title(tr!("The tailscale service is not running"))
        .description(tr!(
            "BigLace needs the tailscaled background service to manage the network."
        ))
        .build();
    let btn = gtk4::Button::builder()
        .label(tr!("Start service"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .build();
    page.set_child(Some(&btn));
    (page, btn)
}

fn build_status_page(icon: &str, title: &str, desc: &str) -> libadwaita::StatusPage {
    libadwaita::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(desc)
        .build()
}

fn build_connecting_page() -> gtk4::Box {
    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    bx.set_valign(gtk4::Align::Center);
    bx.set_halign(gtk4::Align::Center);
    bx.set_vexpand(true);
    let spinner = gtk4::Spinner::new();
    spinner.set_size_request(48, 48);
    spinner.start();
    let lbl = gtk4::Label::new(Some(&tr!("Connecting…")));
    lbl.add_css_class("title-3");
    bx.append(&spinner);
    bx.append(&lbl);
    bx
}

fn build_connected_page() -> (
    gtk4::ScrolledWindow,
    libadwaita::ActionRow,
    gtk4::Button,
    gtk4::ListBox,
) {
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    outer.set_margin_start(18);
    outer.set_margin_end(18);
    outer.set_margin_top(18);
    outer.set_margin_bottom(18);

    // ── This device ──
    let grp_self = libadwaita::PreferencesGroup::builder()
        .title(tr!("This device"))
        .build();
    let self_row = libadwaita::ActionRow::new();
    let self_icon = gtk4::Image::from_icon_name("computer-symbolic");
    self_icon.set_pixel_size(28);
    self_row.add_prefix(&self_icon);
    let btn_copy_self_ip = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Copy IP address"))
        .build();
    self_row.add_suffix(&btn_copy_self_ip);
    grp_self.add(&self_row);
    outer.append(&grp_self);

    // ── Devices on the network ──
    let header = gtk4::Label::new(Some(&tr!("Devices on the network")));
    header.set_xalign(0.0);
    header.add_css_class("heading");
    outer.append(&header);

    let peers_list = gtk4::ListBox::new();
    peers_list.add_css_class("boxed-list");
    peers_list.set_selection_mode(gtk4::SelectionMode::None);

    let placeholder = gtk4::Label::new(Some(&tr!("No other devices on the network yet.")));
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(16);
    placeholder.set_margin_bottom(16);
    placeholder.set_margin_start(16);
    placeholder.set_margin_end(16);
    peers_list.set_placeholder(Some(&placeholder));

    outer.append(&peers_list);

    scroll.set_child(Some(&outer));
    (scroll, self_row, btn_copy_self_ip, peers_list)
}
