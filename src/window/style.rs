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
