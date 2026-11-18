const CSS: &str = r#"
.status-dot {
    min-width: 10px;
    min-height: 10px;
    border-radius: 999px;
    background-color: alpha(@card_fg_color, 0.35);
}
.status-dot.idle       { background-color: alpha(@card_fg_color, 0.35); }
.status-dot.connected  { background-color: @success_color; }
.status-dot.connecting { background-color: @warning_color; }
.status-dot.error      { background-color: @error_color; }

.peer-status-dot {
    min-width: 8px;
    min-height: 8px;
    border-radius: 999px;
    background-color: alpha(@card_fg_color, 0.30);
}
.peer-status-dot.online { background-color: @success_color; }

.bottom-info-bar {
    padding: 6px 12px;
    color: alpha(@card_fg_color, 0.75);
}

/* Header badges — small pills sitting next to the title.
   `badge-warning` is used for "server unreachable" / health issues,
   `badge-info` for the self-update notice. */
.badge {
    padding: 2px 10px;
    border-radius: 999px;
    font-size: 0.85em;
    font-weight: 600;
}
.badge-warning {
    background-color: alpha(@warning_color, 0.18);
    color: @warning_color;
}
.badge-info {
    background-color: alpha(@accent_color, 0.18);
    color: @accent_color;
}

/* ── Navigation sidebar ── */
.navigation-sidebar row {
    border-radius: 10px;
    margin: 1px 0;
}
.navigation-sidebar row image {
    opacity: 0.85;
}
/* "This device" card at the bottom of the sidebar. */
.device-card {
    margin: 0 6px;
}

/* ── Invites ── */
.invite-hero {
    padding: 20px 22px;
    border-radius: 14px;
    background-image: linear-gradient(135deg,
        alpha(@accent_bg_color, 0.16),
        alpha(@accent_bg_color, 0.04));
}
/* The QR code keeps a white quiet zone in dark mode too: phone cameras
   need the contrast. */
/* Someone controls this screen: impossible to miss, still part of the app. */
banner.remote-banner > revealer > widget {
    background-color: alpha(@destructive_bg_color, 0.85);
    color: @destructive_fg_color;
    font-weight: bold;
}

.qr-code {
    border-radius: 12px;
    background-color: #ffffff;
    padding: 4px;
    box-shadow: 0 1px 4px alpha(black, 0.18);
}
.invite-link {
    font-size: 0.9em;
    padding: 8px 12px;
    border-radius: 8px;
    background-color: alpha(@card_fg_color, 0.06);
}
"#;

pub fn install() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
