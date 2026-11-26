use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::tr;

/// Sections listed in the navigation sidebar, in order. The row's widget
/// name is the section id (also the content stack's child name).
pub const SECTIONS: &[&str] = &["devices", "chat", "rooms", "groups", "invites"];

#[derive(Clone)]
pub struct Sidebar {
    pub toolbar: libadwaita::ToolbarView,
    /// Section navigation (Devices, Invites…).
    pub nav: gtk4::ListBox,
    /// Behaviour switches — shown in the Preferences window.
    pub prefs_group: libadwaita::PreferencesGroup,
    /// Manual server/key setup — shown on the "sign in" page.
    pub manual_group: libadwaita::PreferencesGroup,
    pub identity_row: libadwaita::ActionRow,
    pub btn_edit_host: gtk4::Button,
    pub expander_manual: libadwaita::ExpanderRow,
    pub entry_server: libadwaita::EntryRow,
    /// Masked: a reusable pre-auth key registers devices on the user's
    /// tailnet, so it shouldn't sit readable on screen (or in screenshots).
    pub entry_key: libadwaita::PasswordEntryRow,
    pub entry_host: libadwaita::EntryRow,
    pub btn_save_manual: gtk4::Button,
    pub switch_auto: gtk4::Switch,
    pub switch_start_at_login: gtk4::Switch,
    pub switch_auto_reconnect: gtk4::Switch,
    pub switch_notify: gtk4::Switch,
    pub switch_clipboard: gtk4::Switch,
    pub gnome_clipboard_row: libadwaita::ActionRow,
    pub btn_gnome_extension: gtk4::Button,
    pub switch_lan: gtk4::Switch,
    pub btn_connect: gtk4::Button,
    /// Unread messages, on the Chat row (hidden at zero).
    pub chat_badge: gtk4::Label,
    /// Our photo on the identity card.
    pub avatar: libadwaita::Avatar,
    /// The same photo, shown alone while the sidebar is a narrow rail.
    pub rail_avatar: libadwaita::Avatar,
    /// Folds the sidebar into a rail of icons, or unfolds it.
    pub btn_rail: gtk4::Button,
    /// What changes when the sidebar folds into a rail of icons.
    rail: Rail,
}

/// The parts that fold away (or appear) in the icons-only rail.
#[derive(Clone)]
struct Rail {
    toggle: gtk4::Button,
    labels: Vec<(gtk4::ListBoxRow, gtk4::Revealer, String)>,
    title: gtk4::Stack,
    bottom: gtk4::Stack,
    nav: gtk4::ListBox,
}

/// Width of the icons-only rail.
pub const RAIL_WIDTH: f64 = 68.0;

impl Sidebar {
    /// Fold the sidebar into a rail of icons (or unfold it). Only the
    /// contents change here; the caller animates the width alongside.
    pub fn set_rail(&self, rail: bool) {
        let r = &self.rail;
        for (row, revealer, label) in &r.labels {
            revealer.set_reveal_child(!rail);
            row.set_tooltip_text(rail.then_some(label.as_str()));
        }
        r.title.set_visible_child_name(if rail { "rail" } else { "full" });
        r.toggle.set_tooltip_text(Some(&if rail { tr!("Show the full sidebar") } else { tr!("Show only icons in the sidebar") }));
        r.bottom.set_visible_child_name(if rail { "rail" } else { "full" });
        if rail {
            r.nav.add_css_class("rail");
        } else {
            r.nav.remove_css_class("rail");
        }
    }
}

/// Our photo with a little camera on its corner: it's clickable, and
/// that's where the picture is changed.
fn with_camera(avatar: &libadwaita::Avatar) -> gtk4::Overlay {
    let camera = gtk4::Image::from_icon_name("camera-photo-symbolic");
    camera.set_pixel_size(10);
    camera.add_css_class("avatar-camera");
    camera.set_halign(gtk4::Align::End);
    camera.set_valign(gtk4::Align::End);
    camera.set_can_target(false);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(avatar));
    overlay.add_overlay(&camera);
    overlay
}

pub fn build() -> Sidebar {
    let toolbar = libadwaita::ToolbarView::new();

    // ─── Header ───────────────────────────────────────────────────────────
    let header = libadwaita::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let title = gtk4::Label::new(Some("BigLace"));
    title.add_css_class("heading");
    // In the rail the header holds only the button that unfolds it.
    let title_rail = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let btn_rail = gtk4::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .css_classes(["flat"])
        .tooltip_text(tr!("Show only icons in the sidebar"))
        .build();
    header.pack_start(&btn_rail);
    let title_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(220)
        .hhomogeneous(false)
        .interpolate_size(true)
        .build();
    title_stack.add_named(&title, Some("full"));
    title_stack.add_named(&title_rail, Some("rail"));
    header.set_title_widget(Some(&title_stack));
    toolbar.add_top_bar(&header);

    // ─── Body ─────────────────────────────────────────────────────────────
    let scroll = gtk4::ScrolledWindow::builder()
        // External: the rail may be narrower than the folded-away contents
        // (they're clipped while they slide out).
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vexpand(true)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    outer.set_margin_start(6);
    outer.set_margin_end(6);
    outer.set_margin_top(6);
    outer.set_margin_bottom(12);

    // ── Sections ──
    let chat_badge = gtk4::Label::new(None);
    chat_badge.add_css_class("unread-badge");
    chat_badge.set_visible(false);
    chat_badge.set_valign(gtk4::Align::Center);
    let nav = gtk4::ListBox::new();
    nav.add_css_class("navigation-sidebar");
    nav.update_property(&[gtk4::accessible::Property::Label(&tr!("Sections"))]);
    let mut labels = Vec::new();
    for (id, icon, label) in [
        ("devices", "computer-symbolic", tr!("Devices")),
        ("chat", "chat-message-new-symbolic", tr!("Chat")),
        ("rooms", "call-start-symbolic", tr!("Rooms")),
        ("groups", "system-users-symbolic", tr!("Groups")),
        ("invites", "mail-send-symbolic", tr!("Invites")),
    ] {
        let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        line.set_margin_top(4);
        line.set_margin_bottom(4);
        let image = gtk4::Image::from_icon_name(icon);
        if id == "chat" {
            // In the rail the count can't fit: a dot on the icon says it.
            let dot = gtk4::Box::builder()
                .css_classes(["rail-dot"])
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .can_target(false)
                .build();
            chat_badge.bind_property("visible", &dot, "visible").sync_create().build();
            let overlay = gtk4::Overlay::new();
            overlay.set_child(Some(&image));
            overlay.add_overlay(&dot);
            line.append(&overlay);
        } else {
            line.append(&image);
        }
        let text_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        text_box.set_margin_start(12);
        let text = gtk4::Label::new(Some(&label));
        text.set_xalign(0.0);
        text.set_hexpand(true);
        text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        text_box.append(&text);
        if id == "chat" {
            text_box.append(&chat_badge);
        }
        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideRight)
            .transition_duration(220)
            .reveal_child(true)
            .hexpand(true)
            .child(&text_box)
            .build();
        line.append(&revealer);
        let row = gtk4::ListBoxRow::builder().child(&line).name(id).build();
        nav.append(&row);
        labels.push((row, revealer, label));
    }
    nav.select_row(nav.row_at_index(0).as_ref());
    outer.append(&nav);

    // Pushes "this device" to the bottom, like a chat app's user panel.
    let spacer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    outer.append(&spacer);

    // ── Identity card ──
    let grp_identity = libadwaita::PreferencesGroup::new();

    let identity_row = libadwaita::ActionRow::new();
    identity_row.set_title("…");
    identity_row.set_subtitle("…");
    identity_row.set_tooltip_text(Some(&tr!(
        "Your identity on this mesh — the device name shown to other peers \
         and the network you're connected to."
    )));
    // Our photo; click to change it.
    let avatar = libadwaita::Avatar::new(36, None, true);
    let photo_menu = gtk4::gio::Menu::new();
    photo_menu.append(Some(&tr!("Change photo…")), Some("win.avatar-change"));
    photo_menu.append(Some(&tr!("Remove photo")), Some("win.avatar-remove"));
    let avatar_btn = gtk4::MenuButton::builder()
        .child(&with_camera(&avatar))
        .menu_model(&photo_menu)
        .css_classes(["flat", "circular", "avatar-button"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Change your photo"))
        .build();
    identity_row.add_prefix(&avatar_btn);

    // Pencil button that opens the rename-device dialog. Hidden while
    // signed-out (`apply_state` toggles visibility) so the row stays clean on
    // the not-signed-in layout where the manual-key expander is the primary
    // affordance.
    let btn_edit_host = gtk4::Button::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .visible(false)
        .tooltip_text(tr!("Rename this device on the network"))
        .build();
    identity_row.add_suffix(&btn_edit_host);

    grp_identity.add(&identity_row);

    // Manual key expander — secondary path for users who already have a key
    // (e.g. one their admin sent by email). The primary path is the panel
    // login button above, which generates a key automatically.
    let expander_manual = libadwaita::ExpanderRow::builder()
        .title(tr!("Advanced: use a pre-auth key"))
        .subtitle(tr!(
            "Already have a key from your administrator? Paste it here."
        ))
        .expanded(false)
        .build();
    expander_manual.set_tooltip_text(Some(&tr!(
        "Manual setup — expand to paste a server URL and pre-auth key when \
         you already received them from your administrator. Most users \
         should sign in through the panel button above instead."
    )));
    let pwd_icon = gtk4::Image::from_icon_name("dialog-password-symbolic");
    pwd_icon.set_pixel_size(20);
    expander_manual.add_prefix(&pwd_icon);

    let entry_server = libadwaita::EntryRow::builder()
        .title(tr!("Server URL"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    entry_server.set_tooltip_text(Some(&tr!(
        "Address of the coordination server (e.g. vpn.example.org). \
         The https:// scheme is added automatically if you omit it."
    )));
    let server_icon = gtk4::Image::from_icon_name("network-server-symbolic");
    server_icon.set_pixel_size(18);
    entry_server.add_prefix(&server_icon);
    expander_manual.add_row(&entry_server);

    let entry_key = libadwaita::PasswordEntryRow::builder()
        .title(tr!("Pre-auth key"))
        .build();
    entry_key.set_tooltip_text(Some(&tr!(
        "One-time or reusable key created on the server (starts with \
         'hskey-auth-'). Used to register this device automatically without \
         opening a browser."
    )));
    let key_icon = gtk4::Image::from_icon_name("dialog-password-symbolic");
    key_icon.set_pixel_size(18);
    entry_key.add_prefix(&key_icon);
    expander_manual.add_row(&entry_key);

    // Device name (Tailscale hostname). Independent of the OS user and of
    // the machine's local hostname — purely how this device announces
    // itself on the mesh. Becomes the DNS leaf for this peer.
    let entry_host = libadwaita::EntryRow::builder()
        .title(tr!("Device name on the network"))
        .build();
    entry_host.set_tooltip_text(Some(&tr!(
        "The name this device shows to other peers on the VPN — this is \
         the Tailscale hostname, NOT your computer's name and NOT your \
         user. Lowercase letters, digits and hyphens. Example: \
         'tales-laptop', 'francois', 'office-pc'."
    )));
    let host_icon = gtk4::Image::from_icon_name("computer-symbolic");
    host_icon.set_pixel_size(18);
    entry_host.add_prefix(&host_icon);
    expander_manual.add_row(&entry_host);

    let save_row = libadwaita::ActionRow::new();
    save_row.set_title(&tr!("Apply manual setup"));
    let btn_save_manual = gtk4::Button::builder()
        .label(tr!("Save"))
        .css_classes(["suggested-action"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!(
            "Persist the server, key and device name above. Won't connect \
             — press Connect afterwards."
        ))
        .build();
    save_row.add_suffix(&btn_save_manual);
    save_row.set_activatable_widget(Some(&btn_save_manual));
    expander_manual.add_row(&save_row);

    let manual_group = libadwaita::PreferencesGroup::new();
    manual_group.add(&expander_manual);
    grp_identity.add_css_class("device-card");
    let bottom_full = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    bottom_full.append(&grp_identity);

    // ── Preferences card ──
    let grp_prefs = libadwaita::PreferencesGroup::new();

    let auto_row = libadwaita::ActionRow::builder()
        .title(tr!("Connect automatically"))
        .subtitle(tr!("Join the network when BigLace starts"))
        .build();
    auto_row.set_tooltip_text(Some(&tr!(
        "When on, BigLace runs 'tailscale up' as soon as it launches, so \
         you join the mesh without clicking Connect every time."
    )));
    let auto_icon = gtk4::Image::from_icon_name("emblem-synchronizing-symbolic");
    auto_icon.set_pixel_size(20);
    auto_row.add_prefix(&auto_icon);
    let switch_auto = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Toggle auto-connect on startup"))
        .build();
    auto_row.add_suffix(&switch_auto);
    auto_row.set_activatable_widget(Some(&switch_auto));
    grp_prefs.add(&auto_row);

    let start_login_row = libadwaita::ActionRow::builder()
        .title(tr!("Start BigLace with the session"))
        .subtitle(tr!("Open BigLace after you sign in to the desktop"))
        .build();
    start_login_row.set_tooltip_text(Some(&tr!(
        "Turn this on to add BigLace to your desktop session startup. Turn \
         it off to remove it."
    )));
    let start_login_icon = gtk4::Image::from_icon_name("system-run-symbolic");
    start_login_icon.set_pixel_size(20);
    start_login_row.add_prefix(&start_login_icon);
    let switch_start_at_login = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Toggle start with session"))
        .build();
    start_login_row.add_suffix(&switch_start_at_login);
    start_login_row.set_activatable_widget(Some(&switch_start_at_login));
    grp_prefs.add(&start_login_row);

    let reconnect_row = libadwaita::ActionRow::builder()
        .title(tr!("Reconnect on drop"))
        .subtitle(tr!("Retry with backoff when the connection is lost"))
        .build();
    reconnect_row.set_tooltip_text(Some(&tr!(
        "If the tunnel drops (network change, sleep, server hiccup), \
         BigLace silently retries with growing delays instead of leaving \
         you offline until you notice."
    )));
    let reconnect_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
    reconnect_icon.set_pixel_size(20);
    reconnect_row.add_prefix(&reconnect_icon);
    let switch_auto_reconnect = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Toggle auto-reconnect"))
        .build();
    reconnect_row.add_suffix(&switch_auto_reconnect);
    reconnect_row.set_activatable_widget(Some(&switch_auto_reconnect));
    grp_prefs.add(&reconnect_row);

    let notify_row = libadwaita::ActionRow::builder()
        .title(tr!("Notify on peer changes"))
        .subtitle(tr!(
            "Show a desktop notification when peers go online/offline"
        ))
        .build();
    notify_row.set_tooltip_text(Some(&tr!(
        "Send a desktop toast when another device on your network goes \
         online or offline. Rapid bursts are coalesced into a single \
         notification so you don't get spammed during reconnects."
    )));
    let notify_icon = gtk4::Image::from_icon_name("preferences-system-notifications-symbolic");
    notify_icon.set_pixel_size(20);
    notify_row.add_prefix(&notify_icon);
    let switch_notify = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Toggle peer-change notifications"))
        .build();
    notify_row.add_suffix(&switch_notify);
    notify_row.set_activatable_widget(Some(&switch_notify));
    grp_prefs.add(&notify_row);

    let clipboard_row = libadwaita::ActionRow::builder()
        .title(tr!("Share the clipboard with my phone"))
        .subtitle(tr!("Copy on one, paste on the other: text and pictures"))
        .build();
    let clipboard_icon = gtk4::Image::from_icon_name("edit-paste-symbolic");
    clipboard_icon.set_pixel_size(20);
    clipboard_row.add_prefix(&clipboard_icon);
    let switch_clipboard = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Toggle clipboard sharing"))
        .build();
    clipboard_row.add_suffix(&switch_clipboard);
    clipboard_row.set_activatable_widget(Some(&switch_clipboard));
    grp_prefs.add(&clipboard_row);

    // GNOME lets only its shell see the clipboard in the background: offered when the extension is off.
    let gnome_clipboard_row = libadwaita::ActionRow::builder()
        .title(tr!("BigLace extension for GNOME"))
        .subtitle(tr!("Needed for the phone to get what you copy while BigLace is in the background"))
        .visible(false)
        .build();
    let btn_gnome_extension = gtk4::Button::builder()
        .label(tr!("Turn on"))
        .valign(gtk4::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    gnome_clipboard_row.add_suffix(&btn_gnome_extension);
    grp_prefs.add(&gnome_clipboard_row);

    let lan_row = libadwaita::ActionRow::builder()
        .title(tr!("Local network without a server"))
        .subtitle(tr!("Pair with devices on this network and talk to them directly"))
        .build();
    let lan_icon = gtk4::Image::from_icon_name("network-wired-symbolic");
    lan_icon.set_pixel_size(20);
    lan_row.add_prefix(&lan_icon);
    let switch_lan = gtk4::Switch::builder().valign(gtk4::Align::Center).tooltip_text(tr!("Toggle the local network")).build();
    lan_row.add_suffix(&switch_lan);
    lan_row.set_activatable_widget(Some(&switch_lan));
    grp_prefs.add(&lan_row);

    // ── Connect / Disconnect button ──
    let btn_connect = gtk4::Button::builder()
        .label(tr!("Connect"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Fill)
        .margin_start(6)
        .margin_end(6)
        .tooltip_text(tr!(
            "Join or leave the mesh network. When connected, this device \
             becomes reachable by the other peers in your account."
        ))
        .build();
    // How this device connects, a click away above Connect: the two never run
    // together. Both buttons mirror the switch in Preferences, confirmation included.
    let mode = gtk4::Box::builder().css_classes(["linked"]).homogeneous(true).margin_start(6).margin_end(6).build();
    let mode_button = |label: &str, hint: &str| {
        // Wraps rather than cut: the sidebar is narrow and some languages are long.
        let text = gtk4::Label::builder().label(label).wrap(true).justify(gtk4::Justification::Center).build();
        let button = gtk4::ToggleButton::builder().child(&text).tooltip_text(hint).build();
        button.update_property(&[gtk4::accessible::Property::Label(label), gtk4::accessible::Property::Description(hint)]);
        mode.append(&button);
        button
    };
    let over_internet = mode_button(&tr!("Over the internet"), &tr!("Your devices anywhere, through your network's server"));
    let on_lan = mode_button(&tr!("Local network"), &tr!("Only devices on this same network, no server"));
    // Grouped, a click on the chosen one does nothing; the bindings keep exactly one chosen.
    on_lan.set_group(Some(&over_internet));
    switch_lan.bind_property("active", &on_lan, "active").bidirectional().sync_create().build();
    switch_lan.bind_property("active", &over_internet, "active").bidirectional().invert_boolean().sync_create().build();
    bottom_full.append(&mode);
    bottom_full.append(&btn_connect);

    // The rail keeps only our photo (same menu).
    let rail_avatar = libadwaita::Avatar::new(36, None, true);
    let rail_avatar_btn = gtk4::MenuButton::builder()
        .child(&with_camera(&rail_avatar))
        .menu_model(&photo_menu)
        .css_classes(["flat", "circular", "avatar-button"])
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::End)
        .tooltip_text(tr!("Your photo"))
        .build();
    identity_row
        .bind_property("title", &rail_avatar_btn, "tooltip-text")
        .build();
    let bottom = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(220)
        .hhomogeneous(false)
        .vhomogeneous(false)
        .interpolate_size(true)
        .build();
    bottom.add_named(&bottom_full, Some("full"));
    bottom.add_named(&rail_avatar_btn, Some("rail"));
    outer.append(&bottom);

    scroll.set_child(Some(&outer));
    toolbar.set_content(Some(&scroll));

    Sidebar {
        toolbar,
        nav: nav.clone(),
        prefs_group: grp_prefs,
        manual_group,
        identity_row,
        btn_edit_host,
        expander_manual,
        entry_server,
        entry_key,
        entry_host,
        btn_save_manual,
        switch_auto,
        switch_start_at_login,
        switch_auto_reconnect,
        switch_notify,
        switch_clipboard,
        gnome_clipboard_row,
        btn_gnome_extension,
        switch_lan,
        btn_connect,
        chat_badge,
        avatar,
        rail_avatar,
        btn_rail: btn_rail.clone(),
        rail: Rail { toggle: btn_rail, labels, title: title_stack, bottom, nav },
    }
}
