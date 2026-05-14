use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::tr;

#[derive(Clone)]
pub struct Sidebar {
    pub toolbar:                libadwaita::ToolbarView,
    pub identity_row:           libadwaita::ActionRow,
    pub expander_manual:        libadwaita::ExpanderRow,
    pub entry_server:           libadwaita::EntryRow,
    pub entry_key:              libadwaita::EntryRow,
    pub entry_host:             libadwaita::EntryRow,
    pub btn_save_manual:        gtk4::Button,
    pub switch_auto:            gtk4::Switch,
    pub switch_auto_reconnect:  gtk4::Switch,
    pub switch_notify:          gtk4::Switch,
    pub btn_connect:            gtk4::Button,
}

pub fn build() -> Sidebar {
    let toolbar = libadwaita::ToolbarView::new();

    // ─── Header ───────────────────────────────────────────────────────────
    let header = libadwaita::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let title = gtk4::Label::new(Some("BigLace"));
    title.add_css_class("heading");
    header.set_title_widget(Some(&title));
    toolbar.add_top_bar(&header);

    // ─── Body ─────────────────────────────────────────────────────────────
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    outer.set_margin_start(12);
    outer.set_margin_end(12);
    outer.set_margin_top(6);
    outer.set_margin_bottom(12);

    // ── Identity card ──
    let grp_identity = libadwaita::PreferencesGroup::new();

    let identity_row = libadwaita::ActionRow::new();
    identity_row.set_title("…");
    identity_row.set_subtitle("…");
    identity_row.set_tooltip_text(Some(&tr!(
        "Your identity on this mesh — the device name shown to other peers \
         and the network you're connected to."
    )));
    let avatar = gtk4::Image::from_icon_name("avatar-default-symbolic");
    avatar.set_pixel_size(28);
    avatar.add_css_class("dim-label");
    identity_row.add_prefix(&avatar);

    grp_identity.add(&identity_row);

    // Manual key expander — secondary path for users who already have a key
    // (e.g. one their admin sent by email). The primary path is the panel
    // login button above, which generates a key automatically.
    let expander_manual = libadwaita::ExpanderRow::builder()
        .title(tr!("Advanced: use a pre-auth key"))
        .subtitle(tr!("Already have a key from your administrator? Paste it here."))
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

    let entry_key = libadwaita::EntryRow::builder()
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

    grp_identity.add(&expander_manual);
    outer.append(&grp_identity);

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
        .subtitle(tr!("Show a desktop notification when peers go online/offline"))
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

    outer.append(&grp_prefs);

    // ── Connect / Disconnect button ──
    let btn_connect = gtk4::Button::builder()
        .label(tr!("Connect"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_top(8)
        .tooltip_text(tr!(
            "Join or leave the mesh network. When connected, this device \
             becomes reachable by the other peers in your account."
        ))
        .build();
    outer.append(&btn_connect);

    scroll.set_child(Some(&outer));
    toolbar.set_content(Some(&scroll));

    Sidebar {
        toolbar,
        identity_row,
        expander_manual,
        entry_server,
        entry_key,
        entry_host,
        btn_save_manual,
        switch_auto,
        switch_auto_reconnect,
        switch_notify,
        btn_connect,
    }
}
