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
    pub btn_save_manual:        gtk4::Button,
    pub entry_host:             libadwaita::EntryRow,
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
    let pwd_icon = gtk4::Image::from_icon_name("dialog-password-symbolic");
    pwd_icon.set_pixel_size(20);
    expander_manual.add_prefix(&pwd_icon);

    let entry_server = libadwaita::EntryRow::builder()
        .title(tr!("Server URL"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    let server_icon = gtk4::Image::from_icon_name("network-server-symbolic");
    server_icon.set_pixel_size(18);
    entry_server.add_prefix(&server_icon);
    expander_manual.add_row(&entry_server);

    let entry_key = libadwaita::EntryRow::builder()
        .title(tr!("Pre-auth key"))
        .build();
    let key_icon = gtk4::Image::from_icon_name("dialog-password-symbolic");
    key_icon.set_pixel_size(18);
    entry_key.add_prefix(&key_icon);
    expander_manual.add_row(&entry_key);

    let save_row = libadwaita::ActionRow::new();
    save_row.set_title(&tr!("Apply manual setup"));
    let btn_save_manual = gtk4::Button::builder()
        .label(tr!("Save"))
        .css_classes(["suggested-action"])
        .valign(gtk4::Align::Center)
        .tooltip_text(tr!("Save the server and key above"))
        .build();
    save_row.add_suffix(&btn_save_manual);
    save_row.set_activatable_widget(Some(&btn_save_manual));
    expander_manual.add_row(&save_row);

    grp_identity.add(&expander_manual);
    outer.append(&grp_identity);

    // ── Preferences card ──
    let grp_prefs = libadwaita::PreferencesGroup::new();

    let entry_host = libadwaita::EntryRow::builder()
        .title(tr!("Device name"))
        .build();
    let host_icon = gtk4::Image::from_icon_name("computer-symbolic");
    host_icon.set_pixel_size(20);
    entry_host.add_prefix(&host_icon);
    grp_prefs.add(&entry_host);

    let auto_row = libadwaita::ActionRow::builder()
        .title(tr!("Connect automatically"))
        .subtitle(tr!("Join the network when BigLace starts"))
        .build();
    let auto_icon = gtk4::Image::from_icon_name("emblem-synchronizing-symbolic");
    auto_icon.set_pixel_size(20);
    auto_row.add_prefix(&auto_icon);
    let switch_auto = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .build();
    auto_row.add_suffix(&switch_auto);
    auto_row.set_activatable_widget(Some(&switch_auto));
    grp_prefs.add(&auto_row);

    let reconnect_row = libadwaita::ActionRow::builder()
        .title(tr!("Reconnect on drop"))
        .subtitle(tr!("Retry with backoff when the connection is lost"))
        .build();
    let reconnect_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
    reconnect_icon.set_pixel_size(20);
    reconnect_row.add_prefix(&reconnect_icon);
    let switch_auto_reconnect = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .build();
    reconnect_row.add_suffix(&switch_auto_reconnect);
    reconnect_row.set_activatable_widget(Some(&switch_auto_reconnect));
    grp_prefs.add(&reconnect_row);

    let notify_row = libadwaita::ActionRow::builder()
        .title(tr!("Notify on peer changes"))
        .subtitle(tr!("Show a desktop notification when peers go online/offline"))
        .build();
    let notify_icon = gtk4::Image::from_icon_name("preferences-system-notifications-symbolic");
    notify_icon.set_pixel_size(20);
    notify_row.add_prefix(&notify_icon);
    let switch_notify = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
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
        btn_save_manual,
        entry_host,
        switch_auto,
        switch_auto_reconnect,
        switch_notify,
        btn_connect,
    }
}
