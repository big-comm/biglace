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
/* Unread dot on the Chat icon while the sidebar is a rail of icons. */
.rail-dot {
    min-width: 8px;
    min-height: 8px;
    margin: -3px -4px 0 0;
    border-radius: 999px;
    background-color: @accent_bg_color;
    box-shadow: 0 0 0 2px @sidebar_bg_color;
    opacity: 0;
    transition: opacity 200ms ease;
}
.navigation-sidebar.rail .rail-dot { opacity: 1; }
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
/* Camera on our photo: says "click here to change it". */
.avatar-camera {
    padding: 3px;
    border-radius: 999px;
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    box-shadow: 0 0 0 2px @card_bg_color;
    transition: transform 150ms ease;
}
.avatar-button:hover .avatar-camera { transform: scale(1.15); }
/* Rail of icons: the icon sits in the middle of the narrow sidebar. */
.navigation-sidebar row { transition: padding 260ms cubic-bezier(0.33, 1, 0.68, 1); }
.navigation-sidebar.rail row { padding-left: 20px; }

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
.room-chat { background-color: @window_bg_color; }
/* New message in the room's conversation while it is closed. */
.room-chat-badge { margin: -6px -10px 0 0; font-size: 0.7em; min-width: 16px; padding: 0 5px; }
button.room-chat-news { animation: room-chat-news 0.7s ease-in-out 3; }
@keyframes room-chat-news {
    0%   { background-color: transparent; }
    50%  { background-color: alpha(@accent_bg_color, 0.45); }
    100% { background-color: transparent; }
}
window.media-viewer, window.media-viewer > * { background-color: black; }
.media-viewer-bar {
    padding: 10px 14px;
    color: white;
    background-image: linear-gradient(180deg, alpha(black, 0.65), alpha(black, 0));
}
.chat-list .peer-status-dot.online {
    min-width: 11px;
    min-height: 11px;
    background-color: #2ec27e;
    box-shadow: 0 0 0 2px @window_bg_color;
}
.chat-list-section { font-weight: 700; font-size: 0.85em; opacity: 0.6; margin: 12px 10px 2px 10px; }
.chat-header { padding: 8px 14px; }
/* A call ringing or running, over every page. */
.call-bar {
    padding: 8px 14px;
    background-color: alpha(@accent_bg_color, 0.12);
    border-bottom: 1px solid alpha(@borders, 0.6);
}
button.call-answer { background-color: #2ec27e; color: white; }
button.call-answer:hover { background-color: #33d17a; }
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
.bubble.mentions-me { box-shadow: inset 3px 0 0 @accent_color, 0 1px 2px alpha(black, 0.12); }
.bubble-name { font-weight: 700; font-size: 0.95em; }
.bubble .bubble-name.name-color-0 { color: #e66100; }
.bubble .bubble-name.name-color-1 { color: #2ec27e; }
.bubble .bubble-name.name-color-2 { color: #1c71d8; }
.bubble .bubble-name.name-color-3 { color: #c061cb; }
.bubble .bubble-name.name-color-4 { color: #e5a50a; }
.bubble .bubble-name.name-color-5 { color: #26a1a2; }
/* A little larger than the system's text, like Telegram's. */
.bubble-text { font-size: 1.08em; }
.bubble-meta { font-size: 0.8em; opacity: 0.7; }
.conv-subtitle { font-size: 0.92em; }
.conv-time { font-size: 0.85em; }
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
/* Replies, forwards, edits, deletions, reactions (Telegram-like). */
.bubble-forwarded { font-size: 0.9em; font-style: italic; opacity: 0.85; }
.bubble-reply {
    padding: 3px 8px;
    margin: 2px 0 3px 0;
    border-left: 3px solid @accent_color;
    border-radius: 6px;
    background-color: alpha(currentColor, 0.08);
}
.bubble-reply-name { font-weight: 700; font-size: 0.92em; }
.bubble-reply-text { font-size: 0.95em; opacity: 0.85; }
.bubble.mine .bubble-reply { border-left-color: alpha(@accent_fg_color, 0.9); }
.bubble.mine .bubble-reply .bubble-reply-name { color: @accent_fg_color; }
.bubble-reply.reply-color-0 { border-left-color: #e66100; }
.bubble-reply.reply-color-0 .bubble-reply-name { color: #e66100; }
.bubble-reply.reply-color-1 { border-left-color: #2ec27e; }
.bubble-reply.reply-color-1 .bubble-reply-name { color: #2ec27e; }
.bubble-reply.reply-color-2 { border-left-color: #1c71d8; }
.bubble-reply.reply-color-2 .bubble-reply-name { color: #1c71d8; }
.bubble-reply.reply-color-3 { border-left-color: #c061cb; }
.bubble-reply.reply-color-3 .bubble-reply-name { color: #c061cb; }
.bubble-reply.reply-color-4 { border-left-color: #e5a50a; }
.bubble-reply.reply-color-4 .bubble-reply-name { color: #e5a50a; }
.bubble-reply.reply-color-5 { border-left-color: #26a1a2; }
.bubble-reply.reply-color-5 .bubble-reply-name { color: #26a1a2; }
.bubble-deleted { font-style: italic; opacity: 0.7; }
.bubble-edited { font-style: italic; }
.reactions { margin-top: 3px; }
.reactions flowboxchild { padding: 0; }
.reaction-chip {
    min-height: 0;
    padding: 1px 8px;
    border-radius: 999px;
    background-color: alpha(currentColor, 0.10);
    font-size: 0.9em;
}
.reaction-chip.mine { background-color: alpha(@accent_bg_color, 0.85); color: @accent_fg_color; }
.bubble.mine .reaction-chip.mine { background-color: alpha(@accent_fg_color, 0.9); color: @accent_bg_color; }
popover.msg-actions > contents { padding: 6px; border-radius: 14px; min-width: 230px; }
.react-row { padding: 2px 2px 6px 2px; }
.react-pick { font-size: 20px; padding: 2px 5px; min-width: 0; min-height: 0; border-radius: 999px; background: none; box-shadow: none; }
.react-pick:hover { background-color: alpha(currentColor, 0.10); }
.react-more { min-width: 30px; min-height: 30px; border-radius: 999px; background: none; box-shadow: none; }
.msg-action { padding: 6px 10px; border-radius: 8px; }
.msg-action.destructive { color: @error_color; }
/* A message being pointed at (reply, search) or selected. */
.msg-row { border-radius: 10px; transition: background-color 300ms ease; }
.msg-row.flash { background-color: alpha(@accent_bg_color, 0.22); }
.msg-row.selected { background-color: alpha(@accent_bg_color, 0.16); }
/* Pinned message, search, "replying to". */
.pinned-bar { padding: 6px 14px; background-color: alpha(@view_bg_color, 0.6); border-bottom: 1px solid alpha(@view_fg_color, 0.08); }
.pinned-icon { color: @accent_color; }
.pinned-head { font-size: 0.88em; font-weight: 700; color: @accent_color; }
.pinned-text { font-size: 0.97em; }
.chat-search-bar { padding: 6px 12px; border-bottom: 1px solid alpha(@view_fg_color, 0.08); }
.reply-bar { padding: 6px 12px; background-color: @view_bg_color; border-top: 1px solid alpha(@view_fg_color, 0.08); }
.reply-bar-icon { color: @accent_color; }
.reply-bar-title { font-weight: 700; font-size: 0.92em; color: @accent_color; }
.reply-bar-text { font-size: 0.97em; opacity: 0.85; }
.typing { color: @accent_color; font-style: italic; }

/* Composer, flat like Telegram's: attach · text · emoji · mic/send. */
.chat-composer {
    padding: 4px 8px;
    min-height: 52px;
    background-color: @view_bg_color;
    border-top: 1px solid alpha(@view_fg_color, 0.08);
}
.chat-input-frame { background: none; box-shadow: none; }
textview.chat-input, textview.chat-input > text { background: transparent; font-size: 1.08em; }
.chat-tool { min-width: 42px; min-height: 42px; margin-bottom: 1px; color: alpha(@view_fg_color, 0.6); }
.chat-tool:hover { color: @accent_color; }
.chat-send { color: @accent_color; }
/* Recording a voice message. */
.chat-composer.recording { padding: 4px 10px 4px 8px; }
.rec-dot {
    min-width: 10px;
    min-height: 10px;
    border-radius: 999px;
    background-color: @error_color;
    animation: rec-pulse 1.1s ease-in-out infinite;
}
@keyframes rec-pulse { 0% { opacity: 1; } 50% { opacity: 0.25; } 100% { opacity: 1; } }
.rec-time { font-weight: 700; min-width: 3.2em; }
.rec-bars { color: @accent_color; }
.chat-send-voice { min-width: 40px; min-height: 40px; }
/* Files dragged over a conversation. */
.drop-zone {
    margin: 12px;
    border: 2px dashed alpha(@accent_color, 0.8);
    border-radius: 18px;
    background-color: alpha(@view_bg_color, 0.92);
    color: @accent_color;
}
/* Pictures and videos in bubbles. */
.bubble.media { padding: 4px 4px 4px 4px; }
.bubble.media .bubble-meta { margin: 2px 6px 0 6px; }
.bubble.media > label { margin: 4px 8px 0 8px; }
.bubble-media { border-radius: 14px; }
.bubble-media.no-preview { background-color: alpha(black, 0.35); }
.media-veiled { filter: blur(12px); }
.media-veil { background-color: alpha(black, 0.25); }
.media-round {
    min-width: 50px;
    min-height: 50px;
    background-color: alpha(black, 0.5);
    color: white;
    -gtk-icon-size: 22px;
}
.media-round:hover { background-color: alpha(black, 0.65); }
.media-pill {
    margin: 8px;
    padding: 2px 9px;
    border-radius: 10px;
    background-color: alpha(black, 0.55);
    color: white;
    font-size: 0.8em;
    font-weight: 600;
}
/* Documents and voice messages. */
.file-card, .voice-player { padding: 4px 2px 2px 0; }
.file-icon, .voice-play {
    min-width: 46px;
    min-height: 46px;
    -gtk-icon-size: 20px;
}
.bubble.theirs .file-icon, .bubble.theirs .voice-play { background-color: @accent_bg_color; color: @accent_fg_color; }
.bubble.mine .file-icon, .bubble.mine .voice-play { background-color: alpha(@accent_fg_color, 0.92); color: @accent_bg_color; }
.file-name { font-weight: 700; }
.file-status { font-size: 0.82em; opacity: 0.75; }
.bubble.theirs .voice-wave-bars { color: @accent_color; }
progressbar.file-progress > trough { min-height: 3px; border-radius: 3px; }
progressbar.file-progress > trough > progress { min-height: 3px; border-radius: 3px; }

/* Emoji picker (Telegram-like). */
popover.emoji-picker > contents { padding: 0; border-radius: 16px; }
.emoji-grid flowboxchild { padding: 0; border-radius: 10px; }
.emoji-grid flowboxchild:hover { background-color: alpha(@card_fg_color, 0.10); }
.emoji-cell { font-family: "Noto Color Emoji", emoji; font-size: 26px; padding: 3px; }
.emoji-section { font-weight: 700; font-size: 0.85em; opacity: 0.6; }
.emoji-tabs { border-top: 1px solid alpha(@card_fg_color, 0.08); padding: 4px; }
.emoji-tabs button { min-height: 30px; }

/* ── Voice rooms ── */
/* Video stage: grid or speaker view. */
.room-stage { min-height: 140px; }
.stage-tile {
    border-radius: 14px;
    background-color: alpha(@view_fg_color, 0.08);
    box-shadow: inset 0 0 0 1px alpha(@view_fg_color, 0.08);
    transition: box-shadow 150ms ease;
}
.stage-tile.speaking { box-shadow: inset 0 0 0 3px @accent_color, 0 0 12px alpha(@accent_color, 0.5); }
.stage-tile-back { background-color: alpha(black, 0.25); }
.stage-name {
    margin: 8px;
    padding: 2px 8px;
    border-radius: 8px;
    background-color: alpha(black, 0.55);
    color: white;
    font-size: 0.85em;
    font-weight: 600;
}
.stage-big .stage-tile { min-height: 270px; }
/* Showing your screen to the room. */
.share-banner {
    padding: 6px 16px;
    border-radius: 999px;
    background-color: alpha(@error_color, 0.14);
    color: @error_color;
    font-weight: 700;
}
.screen-tag {
    padding: 2px 10px;
    border-radius: 999px;
    background-color: alpha(@accent_bg_color, 0.16);
    color: @accent_color;
    font-size: 0.82em;
    font-weight: 700;
}
.live-pill.controlling { background-color: @success_bg_color; }
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
