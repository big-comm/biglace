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
    /// "Invite" — shown while connected.
    pub btn_invite: gtk4::Button,
    /// Below the sign-in buttons: holds the manual key setup.
    pub signin_extra: gtk4::Box,
    /// "Sign in again as …": shown when this installation keeps the secret
    /// of an account an invite created (see `invite::rejoin`).
    pub btn_rejoin: libadwaita::ActionRow,
    pub stack: gtk4::Stack,
    pub btn_start_service: gtk4::Button,
    pub self_row: libadwaita::ActionRow,
    pub btn_copy_self_ip: gtk4::Button,
    pub peers_list: gtk4::ListBox,
    pub search: gtk4::SearchEntry,
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

    // Filter the device list as you type (name, address, system, owner).
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(tr!("Search devices"))
        .width_chars(22)
        .visible(false)
        .build();
    search.update_property(&[gtk4::accessible::Property::Label(&tr!("Search devices"))]);
    header.pack_start(&search);

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

    let btn_invite = gtk4::Button::builder()
        .child(
            &libadwaita::ButtonContent::builder()
                .icon_name("contact-new-symbolic")
                .label(tr!("Invite"))
                .build(),
        )
        .tooltip_text(tr!("Invite someone to your network"))
        .action_name("win.invite-person")
        .visible(false)
        .build();
    btn_invite.add_css_class("suggested-action");
    header.pack_end(&btn_invite);

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

    let (signin_page, signin_extra, btn_rejoin) = build_not_signed_in_page();
    stack.add_named(&signin_page, Some("not-signed-in"));

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
    {
        let query = search.clone();
        peers_list.set_filter_func(move |row| {
            let q = query.text().trim().to_lowercase();
            if q.is_empty() {
                return true;
            }
            let Some(r) = row.downcast_ref::<libadwaita::PreferencesRow>() else {
                return true;
            };
            let mut hay = r.title().to_lowercase();
            if let Some(e) = row.downcast_ref::<libadwaita::ExpanderRow>() {
                hay.push(' ');
                hay.push_str(&e.subtitle().to_lowercase());
            }
            q.split_whitespace().all(|w| hay.contains(w))
        });
        let list = peers_list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }
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
        btn_invite,
        signin_extra,
        btn_rejoin,
        stack,
        btn_start_service,
        self_row,
        btn_copy_self_ip,
        peers_list: peers_list.clone(),
        search: search.clone(),
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

/// First-run page: the invite is the main way in; the panel account and the
/// manual key (left sidebar) stay available for admins.
fn build_not_signed_in_page() -> (libadwaita::StatusPage, gtk4::Box, libadwaita::ActionRow) {
    let page = build_status_page(
        "dialog-password-symbolic",
        &tr!("Join the network"),
        &tr!("Choose how you're going to sign in."),
    );
    // One clear choice per row, each saying who it's for. Rows are
    // actionable list rows, so keyboard and screen readers get them as
    // buttons with a title and an explanation.
    let choices = gtk4::ListBox::new();
    choices.add_css_class("boxed-list");
    choices.set_selection_mode(gtk4::SelectionMode::None);
    choices.update_property(&[gtk4::accessible::Property::Label(&tr!("Ways to sign in"))]);
    let choice = |icon: &str, title: &str, subtitle: &str, action: &str| {
        let row = libadwaita::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .action_name(action)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        row
    };
    let row_rejoin = choice(
        "view-refresh-symbolic",
        "",
        &tr!("Your account was kept on this computer. No invite needed."),
        "win.rejoin",
    );
    row_rejoin.set_visible(false);
    let row_invite = choice(
        "mail-send-symbolic",
        &tr!("I have an invite"),
        &tr!("Someone on the network sent you a link or a QR code."),
        "win.join-invite",
    );
    row_invite.set_action_target_value(Some(&"".to_variant()));
    let row_key = choice(
        "dialog-password-symbolic",
        &tr!("I have an access key"),
        &tr!("A key the administrator created for you (starts with hskey-auth-)."),
        "win.use-key",
    );
    let row_admin = choice(
        "emblem-system-symbolic",
        &tr!("I'm the administrator"),
        &tr!("Sign in with the BigScale panel's admin username and password."),
        "win.panel-login",
    );
    choices.append(&row_rejoin);
    choices.append(&row_invite);
    choices.append(&row_key);
    choices.append(&row_admin);

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    body.append(&choices);
    // Kept for the manual key fields' home (not shown on this page).
    let extra = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let clamp = libadwaita::Clamp::builder().maximum_size(480).child(&body).build();
    page.set_child(Some(&clamp));
    (page, extra, row_rejoin)
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

    // On a wide (maximized) window the rows keep a readable width,
    // centred, instead of stretching names and buttons far apart.
    let clamp = libadwaita::Clamp::builder().maximum_size(960).tightening_threshold(720).child(&outer).build();
    scroll.set_child(Some(&clamp));
    (scroll, self_row, btn_copy_self_ip, peers_list)
}
