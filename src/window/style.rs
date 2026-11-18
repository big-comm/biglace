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
/* Identity card: our photo is a button (change / remove). */
.avatar-button { padding: 0; min-width: 0; min-height: 0; border-radius: 999px; }

/* ── Chat ── */
.unread-badge {
    min-width: 20px;
    padding: 1px 7px;
    border-radius: 999px;
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    font-weight: 800;
    font-size: 0.8em;
}
.chat-list row { border-radius: 12px; margin: 2px 4px; }
.chat-header { padding: 8px 14px; }
.chat-backdrop {
    background-image: linear-gradient(180deg,
        alpha(@accent_bg_color, 0.06),
        alpha(@accent_bg_color, 0.015));
}
.chat-messages { padding: 6px 0 14px 0; }
.bubble {
    padding: 7px 12px 5px 12px;
    border-radius: 18px;
    box-shadow: 0 1px 2px alpha(black, 0.12);
}
.bubble.theirs {
    background-color: @card_bg_color;
    color: @card_fg_color;
    border-bottom-left-radius: 6px;
}
.bubble.mine {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    border-bottom-right-radius: 6px;
}
.bubble.theirs.continues { border-top-left-radius: 6px; }
.bubble.mine.continues { border-top-right-radius: 6px; }
.bubble label { color: inherit; }
.bubble.mine label link, .bubble.mine label > link { color: @accent_fg_color; text-decoration: underline; }
.bubble-name { font-weight: 700; font-size: 0.9em; }
.bubble .bubble-name.name-color-0 { color: #e66100; }
.bubble .bubble-name.name-color-1 { color: #2ec27e; }
.bubble .bubble-name.name-color-2 { color: #1c71d8; }
.bubble .bubble-name.name-color-3 { color: #c061cb; }
.bubble .bubble-name.name-color-4 { color: #e5a50a; }
.bubble .bubble-name.name-color-5 { color: #26a1a2; }
.bubble-meta { font-size: 0.72em; opacity: 0.7; }
.bubble-meta .tick { font-weight: 800; letter-spacing: -2px; }
.bubble.mine .bubble-meta .tick.delivered { opacity: 1; }
.bubble.theirs .bubble-meta .tick.delivered { color: @accent_color; }
.bubble-jumbo { padding: 0 4px; }
.bubble-jumbo .bubble-meta { opacity: 0.55; }
.jumbo-emoji { font-size: 44px; }
.day-pill {
    padding: 2px 12px;
    border-radius: 999px;
    background-color: alpha(@card_fg_color, 0.08);
    font-size: 0.8em;
    font-weight: 600;
}
.chat-composer { padding: 8px 12px 12px 12px; }
.chat-input-frame {
    border-radius: 20px;
    background-color: @card_bg_color;
    box-shadow: inset 0 0 0 1px alpha(@card_fg_color, 0.10);
}
.chat-input-frame:focus-within { box-shadow: inset 0 0 0 2px alpha(@accent_color, 0.6); }
textview.chat-input, textview.chat-input > text { background: transparent; }
.chat-send { min-width: 40px; min-height: 40px; }

/* Emoji picker (Telegram-like). */
popover.emoji-picker > contents { padding: 0; border-radius: 16px; }
.emoji-grid flowboxchild { padding: 0; border-radius: 10px; }
.emoji-grid flowboxchild:hover { background-color: alpha(@card_fg_color, 0.10); }
.emoji-cell { font-family: "Noto Color Emoji", emoji; font-size: 26px; padding: 3px; }
.emoji-section { font-weight: 700; font-size: 0.85em; opacity: 0.6; }
.emoji-tabs { border-top: 1px solid alpha(@card_fg_color, 0.08); padding: 4px; }
.emoji-tabs button { min-height: 30px; }
.chat-emoji { min-width: 40px; min-height: 40px; }

/* ── Voice rooms ── */
/* Ring around the avatar: lights up and pulses while the person talks. */
.speaking-ring {
    padding: 3px;
    border-radius: 999px;
    box-shadow: 0 0 0 2px transparent;
    transition: box-shadow 180ms ease-out, background-color 180ms ease-out;
}
.speaking-ring.speaking {
    background-color: alpha(@success_color, 0.18);
    box-shadow: 0 0 0 2px @success_color, 0 0 12px 2px alpha(@success_color, 0.45);
    animation: speaking-pulse 1.1s ease-in-out infinite;
}
@keyframes speaking-pulse {
    0%   { box-shadow: 0 0 0 2px @success_color, 0 0 6px 1px alpha(@success_color, 0.35); }
    50%  { box-shadow: 0 0 0 2px @success_color, 0 0 16px 4px alpha(@success_color, 0.55); }
    100% { box-shadow: 0 0 0 2px @success_color, 0 0 6px 1px alpha(@success_color, 0.35); }
}
/* The wave meter takes its colour from here (drawn with the CSS color). */
.voice-wave { color: @card_fg_color; }
.voice-wave.speaking { color: @success_color; }
.room-muted { color: @error_color; }

/* "Allow remote access?" */
.consent-badge {
    padding: 16px;
    border-radius: 999px;
    color: @accent_color;
    background-color: alpha(@accent_bg_color, 0.18);
}
.consent-warning {
    padding: 10px 12px;
    border-radius: 10px;
    color: @warning_color;
    background-color: alpha(@warning_color, 0.14);
}
.consent-warning label {
    color: @window_fg_color;
}
progressbar.consent-countdown > trough > progress {
    background-color: @warning_color;
}
banner.remote-banner > revealer > widget {
    background-color: alpha(@destructive_bg_color, 0.85);
    color: @destructive_fg_color;
    font-weight: bold;
    animation: remote-pulse 2s ease-in-out infinite;
}
@keyframes remote-pulse {
    0%   { background-color: alpha(@destructive_bg_color, 0.95); }
    50%  { background-color: alpha(@destructive_bg_color, 0.65); }
    100% { background-color: alpha(@destructive_bg_color, 0.95); }
}
/* Viewer window: "● LIVE". */
.live-pill {
    padding: 2px 10px;
    border-radius: 999px;
    background-color: alpha(@destructive_bg_color, 0.18);
    color: @destructive_color;
    font-weight: 800;
    font-size: 0.8em;
    letter-spacing: 1px;
}
.live-dot {
    min-width: 8px;
    min-height: 8px;
    border-radius: 999px;
    background-color: @destructive_color;
    animation: live-blink 1.2s ease-in-out infinite;
}
@keyframes live-blink {
    0%   { opacity: 1; }
    50%  { opacity: 0.25; }
    100% { opacity: 1; }
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
