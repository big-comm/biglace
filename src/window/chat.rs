//! "Chat" section: conversations with people and groups of the network,
//! Telegram-style — bubbles, Markdown, emoji, delivery ticks. The messages
//! themselves are handled by `crate::chat`.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::chat_media::{self, FileView};
use crate::chat::{self, files, markdown, voice, ChatEvent, ConvId, Message};
use crate::tr;
use crate::trf;

/// Messages closer than this from the same person form one block.
const BLOCK_GAP_MS: u64 = 5 * 60 * 1000;

thread_local! {
    /// The conversation a room's chat shows right now (no notification for it).
    static ON_SCREEN: RefCell<Option<ConvId>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub struct ChatPage {
    /// The conversation list and its separator (hidden when embedded).
    side: (gtk4::Widget, gtk4::Widget),
    /// Showing one conversation inside something else (a room).
    embedded: Rc<Cell<bool>>,
    pub toolbar: libadwaita::ToolbarView,
    pub menu_btn: gtk4::MenuButton,
    win: libadwaita::ApplicationWindow,
    toast: libadwaita::ToastOverlay,
    badge: gtk4::Label,
    list: gtk4::ListBox,
    stack: gtk4::Stack,
    conv_avatar: libadwaita::Avatar,
    conv_title: gtk4::Label,
    conv_subtitle: gtk4::Label,
    /// Voice or video call (direct conversations).
    btn_call: gtk4::MenuButton,
    scroll: gtk4::ScrolledWindow,
    messages: gtk4::Box,
    composer: super::composer::Composer,
    /// Microphone when there's nothing typed (direct conversations), send
    /// otherwise.
    action_stack: gtk4::Stack,
    /// Typing, or recording a voice message.
    compose_stack: gtk4::Stack,
    rec_time: gtk4::Label,
    rec_area: gtk4::DrawingArea,
    drop_hint: gtk4::Label,
    drop_reveal: gtk4::Revealer,
    /// "Replying to …" / "Editing" above the box.
    reply_reveal: gtk4::Revealer,
    reply_icon: gtk4::Image,
    reply_title: gtk4::Label,
    reply_text: gtk4::Label,
    /// The pinned message, under the header.
    pinned_reveal: gtk4::Revealer,
    pinned_text: gtk4::Label,
    /// Search in the conversation.
    search_reveal: gtk4::Revealer,
    search_entry: gtk4::SearchEntry,
    search_count: gtk4::Label,
    /// "3 selected" in selection mode.
    select_count: gtk4::Label,
    state: Rc<State>,
}

/// A message on screen: what may change later.
struct Rendered {
    /// Holds the bubble (rebuilt in place when the message changes).
    holder: gtk4::Box,
    /// What the bubble shows, to tell when it changed.
    sig: String,
    /// How it sits in its block (kept when rebuilt).
    look: (bool, bool, bool),
    /// Our messages: ✓ / ✓✓.
    tick: Option<gtk4::Label>,
    /// Its file part.
    file: Option<Rc<FileView>>,
}

/// What a bubble shows (besides delivery and downloads).
fn signature(m: &Message) -> String {
    format!("{}|{}|{}|{:?}|{}", m.text, m.edited_at, m.deleted, m.reactions, m.file.is_some())
}

/// Quick reactions offered first (like Telegram's).
const QUICK_REACTIONS: [&str; 6] = ["❤️", "👍", "👀", "🙏", "🤓", "😢"];

type UnreadHook = RefCell<Option<Box<dyn Fn(u32)>>>;

#[derive(Default)]
struct State {
    current: RefCell<Option<ConvId>>,
    /// Rendered messages of the current conversation.
    rendered: RefCell<HashMap<String, Rendered>>,
    /// Last rendered message (to group blocks and day separators).
    last: RefCell<Option<Message>>,
    /// The section is on screen.
    visible: Cell<bool>,
    /// Who's on the network, as last listed (to redraw only on a change).
    people: RefCell<String>,
    /// A voice message being recorded, and its loudness so far.
    recorder: RefCell<Option<voice::Recorder>>,
    rec_levels: RefCell<Vec<f32>>,
    /// Answering this message / editing this one of ours.
    reply_to: RefCell<Option<String>>,
    editing: RefCell<Option<String>>,
    /// Selection mode and what's selected.
    selecting: Cell<bool>,
    selected: RefCell<HashSet<String>>,
    /// Bumped by each "typing…" (an older timeout doesn't clear a newer one).
    typing_gen: Cell<u64>,
    /// Search hits (message ids, oldest first) and where we are.
    search_hits: RefCell<Vec<String>>,
    search_pos: Cell<usize>,
    /// Told the unread total each time the sidebar badge is set.
    unread_hook: UnreadHook,
}

impl ChatPage {
    pub fn build(win: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay, badge: &gtk4::Label) -> Self {
        let toolbar = libadwaita::ToolbarView::new();
        let header = libadwaita::HeaderBar::new();
        header.set_title_widget(Some(&libadwaita::WindowTitle::new(&tr!("Chat"), "")));
        let btn_new = gtk4::Button::builder()
            .child(
                &libadwaita::ButtonContent::builder()
                    .icon_name("chat-message-new-symbolic")
                    .label(tr!("New chat"))
                    .build(),
            )
            .css_classes(["suggested-action"])
            .tooltip_text(tr!("Start a conversation with someone on the network"))
            .build();
        let menu_btn = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr!("Main menu"))
            .build();
        let btn_theme = gtk4::Button::builder()
            .icon_name("applications-graphics-symbolic")
            .tooltip_text(tr!("Chat theme"))
            .build();
        header.pack_start(&btn_new);
        header.pack_end(&menu_btn);
        header.pack_end(&btn_theme);
        toolbar.add_top_bar(&header);

        // ── Left: conversations ──
        let list = gtk4::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.add_css_class("chat-list");
        list.update_property(&[gtk4::accessible::Property::Label(&tr!("Conversations"))]);
        let empty_list = gtk4::Label::new(Some(&tr!("No conversations yet.")));
        empty_list.add_css_class("dim-label");
        empty_list.set_margin_top(24);
        empty_list.set_wrap(true);
        list.set_placeholder(Some(&empty_list));
        let list_scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .width_request(270)
            .child(&list)
            .build();
        list_scroll.add_css_class("chat-list-pane");

        // ── Right: the open conversation ──
        let conv_avatar = libadwaita::Avatar::new(36, None, true);
        let conv_title = gtk4::Label::new(None);
        conv_title.add_css_class("heading");
        conv_title.set_xalign(0.0);
        let conv_subtitle = gtk4::Label::new(None);
        conv_subtitle.add_css_class("dim-label");
        conv_subtitle.add_css_class("conv-subtitle");
        conv_subtitle.set_xalign(0.0);
        let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        titles.set_valign(gtk4::Align::Center);
        titles.append(&conv_title);
        titles.append(&conv_subtitle);
        titles.set_hexpand(true);
        let conv_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        conv_header.add_css_class("chat-header");
        conv_header.append(&conv_avatar);
        conv_header.append(&titles);
        let btn_search = gtk4::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Search in this conversation (Ctrl+F)"))
            .build();
        let btn_call = gtk4::MenuButton::builder()
            .icon_name("call-start-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Call"))
            .build();
        conv_header.append(&btn_call);
        conv_header.append(&btn_search);

        // The pinned message (click: go to it; ✕: unpin).
        let pinned_text = gtk4::Label::new(None);
        pinned_text.set_xalign(0.0);
        pinned_text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        pinned_text.add_css_class("pinned-text");
        let pinned_head = gtk4::Label::new(Some(&tr!("Pinned message")));
        pinned_head.set_xalign(0.0);
        pinned_head.add_css_class("pinned-head");
        let pinned_texts = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        pinned_texts.set_hexpand(true);
        pinned_texts.append(&pinned_head);
        pinned_texts.append(&pinned_text);
        let btn_unpin = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Unpin"))
            .build();
        let pinned_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        pinned_bar.add_css_class("pinned-bar");
        let pin_icon = gtk4::Image::from_icon_name("view-pin-symbolic");
        pin_icon.add_css_class("pinned-icon");
        pinned_bar.append(&pin_icon);
        pinned_bar.append(&pinned_texts);
        pinned_bar.append(&btn_unpin);
        let pinned_reveal = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .child(&pinned_bar)
            .build();

        // Search: the words, "2 of 5", previous / next.
        let search_entry = gtk4::SearchEntry::builder()
            .placeholder_text(tr!("Search messages"))
            .hexpand(true)
            .build();
        let search_count = gtk4::Label::new(None);
        search_count.add_css_class("dim-label");
        search_count.add_css_class("numeric");
        let btn_prev = gtk4::Button::builder()
            .icon_name("go-up-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text(tr!("Older"))
            .build();
        let btn_next = gtk4::Button::builder()
            .icon_name("go-down-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text(tr!("Newer"))
            .build();
        let search_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        search_bar.add_css_class("chat-search-bar");
        search_bar.append(&search_entry);
        search_bar.append(&search_count);
        search_bar.append(&btn_prev);
        search_bar.append(&btn_next);
        let search_reveal = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .child(&search_bar)
            .build();

        let messages = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        messages.add_css_class("chat-messages");
        messages.set_valign(gtk4::Align::End);
        let clamp = libadwaita::Clamp::builder().maximum_size(820).child(&messages).build();
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        scroll.add_css_class("chat-backdrop");

        // Composer, like Telegram's: attach · text · emoji · mic/send.
        let composer = super::composer::Composer::new(win);
        let picker = {
            let composer = composer.clone();
            super::emoji_picker::build(move |e| composer.insert(e))
        };
        let btn_emoji = gtk4::MenuButton::builder()
            .icon_name("emoji-people-symbolic")
            .popover(&picker)
            .valign(gtk4::Align::End)
            .css_classes(["flat", "circular", "chat-tool"])
            .tooltip_text(tr!("Emoji"))
            .build();
        // Ctrl+; and "Insert emoji" open this picker, not GTK's own.
        {
            let btn = btn_emoji.clone();
            composer.on_emoji(move || btn.popup());
        }
        let btn_attach = gtk4::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .valign(gtk4::Align::End)
            .css_classes(["flat", "circular", "chat-tool"])
            .tooltip_text(tr!("Send files, pictures or videos"))
            .build();
        let btn_send = gtk4::Button::builder()
            .icon_name("mail-send-symbolic")
            .css_classes(["flat", "circular", "chat-tool", "chat-send"])
            .tooltip_text(tr!("Send (Enter) — Shift+Enter for a new line"))
            .build();
        let btn_mic = gtk4::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .css_classes(["flat", "circular", "chat-tool"])
            .tooltip_text(tr!("Record a voice message"))
            .build();
        let action_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(150)
            .valign(gtk4::Align::End)
            .build();
        action_stack.add_named(&btn_mic, Some("mic"));
        action_stack.add_named(&btn_send, Some("send"));
        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        bar.add_css_class("chat-composer");
        bar.append(&btn_attach);
        bar.append(&composer.widget);
        bar.append(&btn_emoji);
        bar.append(&action_stack);

        // Recording a voice message: ● 0:07 · live bars · discard · send —
        // the controls together, send where the microphone was (click to
        // record, click the same spot to send).
        let btn_rec_cancel = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "circular", "chat-tool"])
            .tooltip_text(tr!("Discard"))
            .valign(gtk4::Align::Center)
            .build();
        let rec_dot = gtk4::Box::builder().css_classes(["rec-dot"]).valign(gtk4::Align::Center).build();
        let rec_time = gtk4::Label::new(Some("0:00"));
        rec_time.add_css_class("rec-time");
        rec_time.add_css_class("numeric");
        let rec_area = gtk4::DrawingArea::builder().hexpand(true).content_height(28).css_classes(["rec-bars"]).build();
        let btn_rec_send = gtk4::Button::builder()
            .icon_name("mail-send-symbolic")
            .css_classes(["suggested-action", "circular", "chat-send-voice"])
            .tooltip_text(tr!("Send voice message"))
            .valign(gtk4::Align::Center)
            .build();
        let rec_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        rec_bar.add_css_class("chat-composer");
        rec_bar.add_css_class("recording");
        rec_dot.set_margin_start(10);
        rec_bar.append(&rec_dot);
        rec_bar.append(&rec_time);
        rec_bar.append(&rec_area);
        rec_bar.append(&btn_rec_cancel);
        rec_bar.append(&btn_rec_send);

        let compose_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::SlideUpDown)
            .transition_duration(200)
            .vhomogeneous(false)
            .interpolate_size(true)
            .build();
        compose_stack.add_named(&bar, Some("compose"));
        compose_stack.add_named(&rec_bar, Some("record"));

        // Selection mode: "3 selected" · copy · forward · delete · cancel.
        let select_count = gtk4::Label::new(None);
        select_count.add_css_class("heading");
        select_count.set_hexpand(true);
        select_count.set_xalign(0.0);
        let sel_button = |icon: &str, tip: String| {
            gtk4::Button::builder()
                .icon_name(icon)
                .css_classes(["flat", "circular", "chat-tool"])
                .tooltip_text(tip)
                .valign(gtk4::Align::Center)
                .build()
        };
        let btn_sel_cancel = sel_button("window-close-symbolic", tr!("Cancel"));
        let btn_sel_copy = sel_button("edit-copy-symbolic", tr!("Copy text"));
        let btn_sel_forward = sel_button("mail-forward-symbolic", tr!("Forward"));
        let btn_sel_delete = sel_button("user-trash-symbolic", tr!("Delete"));
        btn_sel_delete.add_css_class("destructive-action");
        let select_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        select_bar.add_css_class("chat-composer");
        select_bar.append(&btn_sel_cancel);
        select_bar.append(&select_count);
        select_bar.append(&btn_sel_copy);
        select_bar.append(&btn_sel_forward);
        select_bar.append(&btn_sel_delete);
        compose_stack.add_named(&select_bar, Some("select"));

        // "Replying to …" / "Editing" over the box.
        let reply_title = gtk4::Label::new(None);
        reply_title.set_xalign(0.0);
        reply_title.add_css_class("reply-bar-title");
        let reply_text = gtk4::Label::new(None);
        reply_text.set_xalign(0.0);
        reply_text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        reply_text.add_css_class("reply-bar-text");
        let reply_texts = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        reply_texts.set_hexpand(true);
        reply_texts.append(&reply_title);
        reply_texts.append(&reply_text);
        let btn_reply_close = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Cancel"))
            .build();
        let reply_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        reply_bar.add_css_class("reply-bar");
        let reply_icon = gtk4::Image::from_icon_name("mail-reply-sender-symbolic");
        reply_icon.add_css_class("reply-bar-icon");
        reply_bar.append(&reply_icon.clone());
        reply_bar.append(&reply_texts);
        reply_bar.append(&btn_reply_close);
        let reply_reveal = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .child(&reply_bar)
            .build();

        let conv_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        conv_box.append(&conv_header);
        conv_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        conv_box.append(&pinned_reveal);
        conv_box.append(&search_reveal);
        conv_box.append(&scroll);
        conv_box.append(&reply_reveal);
        conv_box.append(&compose_stack);

        // Dragging files over the conversation.
        let drop_icon = gtk4::Image::from_icon_name("document-send-symbolic");
        drop_icon.set_pixel_size(48);
        let drop_hint = gtk4::Label::new(None);
        drop_hint.add_css_class("title-3");
        drop_hint.set_wrap(true);
        drop_hint.set_justify(gtk4::Justification::Center);
        let drop_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        drop_box.add_css_class("drop-zone");
        drop_box.set_valign(gtk4::Align::Fill);
        drop_box.set_halign(gtk4::Align::Fill);
        let drop_inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        drop_inner.set_valign(gtk4::Align::Center);
        drop_inner.set_vexpand(true);
        drop_inner.append(&drop_icon);
        drop_inner.append(&drop_hint);
        drop_box.append(&drop_inner);
        let drop_reveal = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .child(&drop_box)
            .can_target(false)
            .build();
        let conv_overlay = gtk4::Overlay::new();
        conv_overlay.set_child(Some(&conv_box));
        conv_overlay.add_overlay(&drop_reveal);

        let none = libadwaita::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title(tr!("Talk with your network"))
            .description(tr!(
                "Pick a conversation, or start one. Messages go straight between your computers — the server never sees them."
            ))
            .build();
        let stack = gtk4::Stack::new();
        stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        stack.add_named(&none, Some("none"));
        stack.add_named(&conv_overlay, Some("conv"));
        stack.set_hexpand(true);

        let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let side_sep = gtk4::Separator::new(gtk4::Orientation::Vertical);
        body.append(&list_scroll);
        body.append(&side_sep);
        body.append(&stack);
        toolbar.set_content(Some(&body));

        let page = Self {
            side: (list_scroll.clone().upcast(), side_sep.upcast()),
            embedded: Rc::default(),
            toolbar,
            menu_btn,
            win: win.clone(),
            toast: toast.clone(),
            badge: badge.clone(),
            list,
            stack,
            conv_avatar,
            conv_title,
            conv_subtitle,
            btn_call,
            scroll,
            messages,
            composer,
            action_stack,
            compose_stack,
            rec_time,
            rec_area: rec_area.clone(),
            drop_hint,
            drop_reveal,
            reply_reveal,
            reply_icon,
            reply_title,
            reply_text,
            pinned_reveal,
            pinned_text,
            search_reveal,
            search_entry: search_entry.clone(),
            search_count,
            select_count,
            state: Rc::default(),
        };
        // Mic or send, as the box fills.
        {
            let p = page.clone();
            page.composer.view.buffer().connect_changed(move |_| p.update_actions());
        }
        {
            let page = page.clone();
            btn_attach.connect_clicked(move |_| page.pick_files());
        }
        {
            let page = page.clone();
            btn_mic.connect_clicked(move |_| page.start_recording());
        }
        {
            let page = page.clone();
            btn_rec_cancel.connect_clicked(move |_| page.stop_recording(false));
        }
        {
            let page = page.clone();
            btn_rec_send.connect_clicked(move |_| page.stop_recording(true));
        }
        {
            let state = page.state.clone();
            rec_area.set_draw_func(move |a, cr, w, h| {
                let levels = state.rec_levels.borrow();
                let color = a.color();
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    f64::from(color.alpha()) * 0.8,
                );
                // The newest at the right, scrolling left.
                let step = 4.0;
                let n = (f64::from(w) / step) as usize;
                let start = levels.len().saturating_sub(n);
                let offset = f64::from(w) - (levels.len() - start) as f64 * step;
                for (i, v) in levels[start..].iter().enumerate() {
                    let bh = (f64::from(*v) * f64::from(h)).max(3.0);
                    cr.rectangle(offset + i as f64 * step, (f64::from(h) - bh) / 2.0, 2.5, bh);
                }
                let _ = cr.fill();
            });
        }
        page.setup_drop(&conv_overlay);
        {
            // Ctrl+V with a picture or copied files: sent.
            let page2 = page.clone();
            page.composer.on_paste_files(move |paths| page2.send_files(paths));
        }
        // ── Replies, edits, pins, selection, search ──
        {
            let page = page.clone();
            btn_reply_close.connect_clicked(move |_| page.cancel_reply());
        }
        {
            let page = page.clone();
            btn_unpin.connect_clicked(move |_| {
                if let Some(id) = page.state.current.borrow().clone() {
                    let toast = page.toast.clone();
                    super::run_in_background(move || chat::pin(&id, None), move |r| {
                        if let Err(e) = r {
                            super::show_toast(&toast, &e, 5);
                        }
                    });
                }
            });
        }
        {
            let page = page.clone();
            let click = gtk4::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                if let Some(m) = page.state.current.borrow().as_ref().and_then(chat::pinned) {
                    page.scroll_to_message(&m.id);
                }
            });
            pinned_bar.add_controller(click);
            pinned_bar.set_cursor_from_name(Some("pointer"));
        }
        {
            let page = page.clone();
            btn_sel_cancel.connect_clicked(move |_| page.end_selection());
        }
        {
            let page = page.clone();
            btn_sel_copy.connect_clicked(move |_| {
                let ids = page.selected_in_order();
                page.copy_messages(&ids);
                page.end_selection();
            });
        }
        {
            let page = page.clone();
            btn_sel_forward.connect_clicked(move |_| {
                let ids = page.selected_in_order();
                page.forward_dialog(ids);
            });
        }
        {
            let page = page.clone();
            btn_sel_delete.connect_clicked(move |_| {
                let ids = page.selected_in_order();
                page.confirm_delete(ids);
            });
        }
        {
            let (page, entry) = (page.clone(), search_entry.clone());
            btn_search.connect_toggled(move |b| {
                page.search_reveal.set_reveal_child(b.is_active());
                if b.is_active() {
                    entry.grab_focus();
                } else {
                    entry.set_text("");
                    page.clear_search();
                    page.composer.view.grab_focus();
                }
            });
        }
        {
            let page = page.clone();
            search_entry.connect_search_changed(move |e| page.search(&e.text()));
        }
        {
            let page = page.clone();
            search_entry.connect_activate(move |_| page.search_step(-1));
        }
        {
            let btn = btn_search.clone();
            search_entry.connect_stop_search(move |_| btn.set_active(false));
        }
        {
            let page = page.clone();
            btn_prev.connect_clicked(move |_| page.search_step(-1));
        }
        {
            let page = page.clone();
            btn_next.connect_clicked(move |_| page.search_step(1));
        }
        {
            // Ctrl+F searches; Esc leaves a reply, an edit or a selection.
            let (page, btn, toolbar) = (page.clone(), btn_search.clone(), page.toolbar.clone());
            let keys = gtk4::EventControllerKey::new();
            keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, key, _, mods| {
                if page.state.current.borrow().is_none() {
                    return glib::Propagation::Proceed;
                }
                if key == gtk4::gdk::Key::f && mods.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    btn.set_active(true);
                    page.search_entry.grab_focus();
                    return glib::Propagation::Stop;
                }
                if key == gtk4::gdk::Key::Escape {
                    if page.state.selecting.get() {
                        page.end_selection();
                        return glib::Propagation::Stop;
                    }
                    if page.state.reply_to.borrow().is_some() || page.state.editing.borrow().is_some() {
                        page.cancel_reply();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
            toolbar.add_controller(keys);
        }
        // Typing… tells the others (now and then).
        {
            let page = page.clone();
            page.composer.view.buffer().connect_changed(move |b| {
                if b.char_count() > 0 && page.state.editing.borrow().is_none() {
                    if let Some(id) = page.state.current.borrow().as_ref() {
                        chat::typing(id);
                    }
                }
            });
        }
        super::chat_theme::apply(&page.toolbar, &page.scroll, &super::chat_theme::load());
        {
            let page = page.clone();
            btn_theme.connect_clicked(move |_| {
                let p = page.clone();
                super::chat_theme::show_dialog(&page.win, move |t| super::chat_theme::apply(&p.toolbar, &p.scroll, t));
            });
        }

        // "@" lists who's in the conversation (not us).
        {
            let state = page.state.clone();
            page.composer.on_mention_candidates(move || {
                let me = chat::my_user();
                let Some(conv) = state.current.borrow().clone() else { return vec![] };
                let people = if conv.is_group() { chat::group_members(&conv.name) } else { vec![conv.name.clone()] };
                people.into_iter().filter(|p| Some(p) != me.as_ref()).collect()
            });
        }

        // Enter sends; Shift+Enter is a new line.
        {
            let p = page.clone();
            let keys = gtk4::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, mods| {
                let enter = matches!(key, gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter);
                if enter && !mods.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
                    p.send_typed();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            page.composer.view.add_controller(keys);
        }
        {
            let page = page.clone();
            btn_send.connect_clicked(move |_| page.send_typed());
        }
        {
            let page = page.clone();
            btn_new.connect_clicked(move |_| page.show_new_chat());
        }
        {
            let page = page.clone();
            page.list.clone().connect_row_activated(move |_, row| {
                if let Some(id) = row_conv(row) {
                    page.open_conv(&id);
                }
            });
        }
        // People coming online (or leaving): the list follows.
        {
            let page = page.clone();
            glib::timeout_add_seconds_local(8, move || {
                if page.state.visible.get() {
                    let people = people_to_list(&chat::conversations());
                    if people_signature(&people) != *page.state.people.borrow() {
                        page.refresh_list();
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        // Messages arriving, delivered…
        {
            let page = page.clone();
            let rx = chat::events();
            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    page.on_event(ev);
                }
            });
        }
        // Read when the window comes back to the front.
        {
            let page = page.clone();
            win.connect_is_active_notify(move |w| {
                if w.is_active() {
                    page.mark_current_read();
                }
            });
        }
        page.update_badge();
        page
    }

    /// Open `id` in the visible section (a notification's click, the preview).
    pub fn open(&self, id: &ConvId) {
        self.state.visible.set(true);
        self.open_conv(id);
    }

    /// The section became visible (or not).
    /// Show only one conversation (no list, no header): a room's chat.
    pub fn embed(&self) {
        self.embedded.set(true);
        self.side.0.set_visible(false);
        self.side.1.set_visible(false);
        self.toolbar.set_reveal_top_bars(false);
    }

    /// Embedded: open `id` and keep it on screen (or let it go).
    pub fn show_only(&self, id: Option<&ConvId>) {
        match id {
            Some(id) => {
                if self.state.current.borrow().as_ref() != Some(id) {
                    self.open_conv(id);
                }
                self.set_visible(true);
                ON_SCREEN.with(|o| *o.borrow_mut() = Some(id.clone()));
            }
            None => {
                self.set_visible(false);
                ON_SCREEN.with(|o| *o.borrow_mut() = None);
            }
        }
    }

    /// Put the cursor where the next message is typed.
    pub fn focus_composer(&self) {
        self.composer.view.grab_focus();
    }

    pub fn set_visible(&self, visible: bool) {
        self.state.visible.set(visible);
        if visible {
            self.refresh_list();
            self.mark_current_read();
            // Learn our groups (each has a conversation) in the background.
            let page = self.clone();
            super::run_in_background(
                || {
                    let _ = crate::invite::my_groups();
                    chat::my_groups_refresh();
                },
                move |()| page.refresh_list(),
            );
        }
    }

    fn update_badge(&self) {
        let n = chat::unread_total();
        self.badge.set_text(&if n > 99 { "99+".to_string() } else { n.to_string() });
        self.badge.set_visible(n > 0);
        if let Some(hook) = self.state.unread_hook.borrow().as_ref() {
            hook(n);
        }
    }

    /// Run `hook` with the unread total now and whenever it's shown again.
    pub fn on_unread_change(&self, hook: impl Fn(u32) + 'static) {
        hook(chat::unread_total());
        *self.state.unread_hook.borrow_mut() = Some(Box::new(hook));
    }

    /// Rebuild the conversation list (groups always listed).
    fn refresh_list(&self) {
        let mut convs = chat::conversations();
        for g in chat::my_groups() {
            let id = ConvId::group(&g);
            if !convs.iter().any(|c| c.id == id) {
                convs.push(chat::Summary { id, last: None, unread: 0 });
            }
        }
        let current = self.state.current.borrow().clone();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for c in &convs {
            let row = conv_row(c, None);
            self.list.append(&row);
            if Some(&c.id) == current.as_ref() {
                self.list.select_row(Some(&row));
            }
        }
        // Everyone else on the network, right there: one click to talk.
        let people = people_to_list(&convs);
        *self.state.people.borrow_mut() = people_signature(&people);
        if !people.is_empty() {
            let title = gtk4::Label::new(Some(&tr!("People on the network")));
            title.add_css_class("chat-list-section");
            title.set_xalign(0.0);
            let header = gtk4::ListBoxRow::builder().child(&title).activatable(false).selectable(false).build();
            self.list.append(&header);
        }
        for (user, online) in people {
            let c = chat::Summary { id: ConvId::direct(&user), last: None, unread: 0 };
            let row = conv_row(&c, Some(online));
            self.list.append(&row);
            if Some(&c.id) == current.as_ref() {
                self.list.select_row(Some(&row));
            }
        }
        self.update_badge();
    }

    fn open_conv(&self, id: &ConvId) {
        chat::open(id);
        *self.state.current.borrow_mut() = Some(id.clone());
        self.state.rendered.borrow_mut().clear();
        *self.state.last.borrow_mut() = None;
        while let Some(child) = self.messages.first_child() {
            self.messages.remove(&child);
        }
        self.conv_title.set_text(&chat::label(&id.name));
        if id.is_lan() {
            // No account behind it: no photo to fetch, no calls (they go over the tailnet).
            // "Maria (notebook)": initials from the words, not the parenthesis.
            self.conv_avatar.set_text(Some(&chat::label(&id.name).replace(['(', ')'], "")));
            self.conv_avatar.set_icon_name(None);
            self.conv_avatar.set_show_initials(true);
            self.conv_avatar.set_custom_image(gtk4::gdk::Paintable::NONE);
            self.conv_subtitle.set_text(&tr!("On this local network"));
            self.btn_call.set_visible(false);
        } else if id.is_group() {
            self.conv_avatar.set_text(Some(&id.name));
            self.conv_avatar.set_icon_name(Some("system-users-symbolic"));
            self.conv_avatar.set_show_initials(false);
            crate::avatars::bind_group(&self.conv_avatar, &id.name);
            self.conv_subtitle.set_text(&tr!("Group"));
            self.btn_call.set_visible(false);
        } else {
            self.btn_call.set_menu_model(Some(&super::call_ui::call_menu(&id.name)));
            self.btn_call.set_visible(true);
            self.conv_avatar.set_text(Some(&id.name));
            self.conv_avatar.set_icon_name(None);
            self.conv_avatar.set_show_initials(true);
            crate::avatars::bind(&self.conv_avatar, &id.name);
            self.conv_subtitle.set_text(&tr!("Direct conversation"));
        }
        for m in chat::messages(id) {
            // Pictures and voice messages that didn't come down yet.
            // (Not our own: what we sent and is gone here was moved or
            // deleted — there's nowhere to fetch it from.)
            if !m.mine && m.file.as_ref().is_some_and(|f| f.offer.fetch_by_itself() && f.local().is_none()) {
                files::download(id, &m.id);
            }
            self.render(&m, false);
        }
        if self.state.recorder.borrow().is_some() {
            self.stop_recording(false);
        }
        self.cancel_reply();
        self.end_selection();
        self.clear_search();
        self.update_pinned();
        self.update_actions();
        self.stack.set_visible_child_name("conv");
        self.scroll_to_end();
        self.mark_current_read();
        self.refresh_list();
        self.composer.view.grab_focus();
    }

    fn mark_current_read(&self) {
        if !self.state.visible.get() || !self.win.is_active() {
            return;
        }
        if let Some(id) = self.state.current.borrow().clone() {
            chat::mark_read(&id);
            if let Some(app) = self.win.application() {
                app.withdraw_notification(&notification_id(&id));
            }
        }
        self.update_badge();
    }

    fn send_typed(&self) {
        let Some(id) = self.state.current.borrow().clone() else { return };
        let text = markdown::emoticons_to_emoji(&self.composer.markdown());
        if text.trim().is_empty() {
            return;
        }
        if text.len() > chat::MAX_TEXT {
            super::show_toast(&self.toast, &tr!("That message is too long."), 5);
            return;
        }
        self.composer.clear();
        let editing = self.state.editing.borrow_mut().take();
        let reply_to = self.state.reply_to.borrow_mut().take();
        self.reply_reveal.set_reveal_child(false);
        let toast = self.toast.clone();
        super::run_in_background(
            move || match editing {
                Some(target) => chat::edit(&id, &target, &text),
                None => chat::send_reply(&id, &text, reply_to.as_deref()),
            },
            move |r| {
                if let Err(e) = r {
                    super::show_toast(&toast, &trf!("Could not send: {error}", "error" => e), 6);
                }
            },
        );
    }

    fn on_event(&self, ev: ChatEvent) {
        match ev {
            ChatEvent::Changed(id) => {
                if self.state.current.borrow().as_ref() == Some(&id) {
                    self.sync_current();
                    self.mark_current_read();
                }
                if self.state.visible.get() {
                    self.refresh_list();
                } else {
                    self.update_badge();
                }
            }
            ChatEvent::Progress { conv, id, done, total } => {
                if self.state.current.borrow().as_ref() != Some(&conv) {
                    return;
                }
                let msg = chat::messages(&conv).into_iter().find(|m| m.id == id);
                let view = self.state.rendered.borrow().get(&id).and_then(|r| r.file.clone());
                if let (Some(view), Some(m)) = (view, msg) {
                    chat_media::refresh(&view, &m, &conv);
                    chat_media::progress(&view, done, total);
                }
            }
            ChatEvent::TransferFailed { conv, id, reason } => {
                if self.state.current.borrow().as_ref() == Some(&conv) {
                    let msg = chat::messages(&conv).into_iter().find(|m| m.id == id);
                    let view = self.state.rendered.borrow().get(&id).and_then(|r| r.file.clone());
                    if let (Some(view), Some(m)) = (view, msg) {
                        chat_media::refresh(&view, &m, &conv);
                    }
                    super::show_toast(&self.toast, &trf!("Could not download: {error}", "error" => reason), 6);
                }
            }
            ChatEvent::Typing { conv, user } => self.show_typing(&conv, &user),
            ChatEvent::ReadElsewhere(conv) => {
                if let Some(app) = self.win.application() {
                    app.withdraw_notification(&notification_id(&conv));
                }
                if self.state.visible.get() {
                    self.refresh_list();
                } else {
                    self.update_badge();
                }
            }
            ChatEvent::Incoming { conv, message } => {
                let looking = self.state.visible.get()
                    && self.win.is_active()
                    && self.state.current.borrow().as_ref() == Some(&conv);
                // One notification per message: the Chat page sends it (the
                // room's chat doesn't), unless the room's chat shows it now.
                let elsewhere = self.win.is_active() && ON_SCREEN.with(|o| o.borrow().as_ref() == Some(&conv));
                if !looking && !elsewhere && !self.embedded.get() {
                    self.notify(&conv, &message);
                }
                self.update_badge();
            }
        }
    }

    /// New messages of the open conversation, and delivery ticks.
    fn sync_current(&self) {
        let Some(id) = self.state.current.borrow().clone() else { return };
        let near_end = {
            let adj = self.scroll.vadjustment();
            adj.upper() - adj.page_size() - adj.value() < 80.0
        };
        let mut added = false;
        let all = chat::messages(&id);
        // Deleted here ("for me"): their bubbles go.
        {
            let present: HashSet<&str> = all.iter().map(|m| m.id.as_str()).collect();
            let mut rendered = self.state.rendered.borrow_mut();
            let gone: Vec<String> = rendered.keys().filter(|k| !present.contains(k.as_str())).cloned().collect();
            for k in gone {
                if let Some(r) = rendered.remove(&k) {
                    let widget: gtk4::Widget = match r.holder.parent().and_downcast::<gtk4::Revealer>() {
                        Some(rev) => rev.upcast(),
                        None => r.holder.upcast(),
                    };
                    if widget.parent().as_ref() == Some(self.messages.upcast_ref()) {
                        self.messages.remove(&widget);
                    }
                }
            }
        }
        for m in all {
            let known = self
                .state
                .rendered
                .borrow()
                .get(&m.id)
                .map(|r| (r.tick.clone(), r.file.clone(), r.sig.clone(), r.holder.clone(), r.look));
            match known {
                Some((_, _, sig, holder, look)) if sig != signature(&m) => {
                    // Edited, reacted to, deleted: draw it again in place.
                    let (tick, file) = self.fill_bubble(&holder, &m, look);
                    if let Some(r) = self.state.rendered.borrow_mut().get_mut(&m.id) {
                        r.sig = signature(&m);
                        r.tick = tick;
                        r.file = file;
                    }
                }
                Some((tick, file, ..)) => {
                    if let Some(t) = tick {
                        set_tick(&t, &m, id.is_group());
                    }
                    if let Some(f) = file {
                        chat_media::refresh(&f, &m, &id);
                    }
                }
                None => {
                    self.render(&m, true);
                    added = true;
                    if m.mine {
                        // Our own message: always show it.
                        self.scroll_to_end();
                    }
                }
            }
        }
        if added && near_end {
            self.scroll_to_end();
        }
        self.update_pinned();
    }

    fn scroll_to_end(&self) {
        // Now, and again once a new bubble finished sliding in (it grows
        // the list while it animates).
        for delay in [0, 260] {
            let scroll = self.scroll.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
                let adj = scroll.vadjustment();
                adj.set_value(adj.upper());
            });
        }
    }

    /// Add one message at the end, with a day separator and sender name
    /// when needed; `animate`: slide it in.
    fn render(&self, m: &Message, animate: bool) {
        let prev = self.state.last.borrow().clone();
        let group = self.state.current.borrow().as_ref().is_some_and(ConvId::is_group);
        if prev.as_ref().is_none_or(|p| day_of(p.at) != day_of(m.at)) {
            self.messages.append(&day_separator(m.at));
        }
        let continues = prev
            .as_ref()
            .is_some_and(|p| p.from == m.from && p.mine == m.mine && m.at.saturating_sub(p.at) < BLOCK_GAP_MS && day_of(p.at) == day_of(m.at));
        let look = (group && !m.mine && !continues, continues, group && !m.mine);
        let holder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        holder.add_css_class("msg-row");
        let (tick, file) = self.fill_bubble(&holder, m, look);
        if animate {
            let rev = gtk4::Revealer::builder()
                .transition_type(gtk4::RevealerTransitionType::SlideUp)
                .transition_duration(220)
                .child(&holder)
                .build();
            self.messages.append(&rev);
            glib::idle_add_local_once(move || rev.set_reveal_child(true));
        } else {
            self.messages.append(&holder);
        }
        self.state
            .rendered
            .borrow_mut()
            .insert(m.id.clone(), Rendered { holder, sig: signature(m), look, tick, file });
        *self.state.last.borrow_mut() = Some(m.clone());
    }

    fn notify(&self, conv: &ConvId, m: &Message) {
        let Some(app) = self.win.application() else { return };
        let title = if conv.is_group() {
            trf!("{user} in {group}", "user" => m.from, "group" => conv.name)
        } else {
            chat::label(&m.from)
        };
        let n = gtk4::gio::Notification::new(&title);
        n.set_body(Some(&message_line(m, 140)));
        n.set_default_action_and_target_value(&format!("app.{}", super::CHAT_OPEN), Some(&conv.key().to_variant()));
        n.add_button_with_target_value(
            &tr!("Reply"),
            &format!("app.{}", super::phone_link::CHAT_REPLY),
            Some(&conv.key().to_variant()),
        );
        app.send_notification(Some(&notification_id(conv)), &n);
    }

    /// Microphone when nothing is typed, send otherwise.
    fn update_actions(&self) {
        let typed = self.composer.view.buffer().char_count() > 0;
        self.action_stack.set_visible_child_name(if typed { "send" } else { "mic" });
    }

    fn send_files(&self, paths: Vec<std::path::PathBuf>) {
        let Some(id) = self.state.current.borrow().clone() else { return };
        for path in paths {
            let (id, toast) = (id.clone(), self.toast.clone());
            super::run_in_background(
                move || files::send_file(&id, &path),
                move |r| {
                    if let Err(e) = r {
                        super::show_toast(&toast, &trf!("Could not send: {error}", "error" => e), 6);
                    }
                },
            );
        }
    }

    fn pick_files(&self) {
        let dialog = gtk4::FileDialog::builder().title(tr!("Send files")).modal(true).build();
        let page = self.clone();
        dialog.open_multiple(Some(&self.win), gtk4::gio::Cancellable::NONE, move |r| {
            let Ok(list) = r else { return };
            let paths: Vec<_> = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gtk4::gio::File>())
                .filter_map(|f| f.path())
                .collect();
            page.send_files(paths);
        });
    }

    /// Files dropped on the conversation are sent.
    fn setup_drop(&self, target_widget: &gtk4::Overlay) {
        let target = gtk4::DropTarget::new(gtk4::gdk::FileList::static_type(), gtk4::gdk::DragAction::COPY);
        {
            let page = self.clone();
            target.connect_enter(move |_, _, _| {
                page.drop_hint.set_text(&tr!("Drop to send"));
                page.drop_reveal.set_reveal_child(true);
                gtk4::gdk::DragAction::COPY
            });
        }
        {
            let page = self.clone();
            target.connect_leave(move |_| page.drop_reveal.set_reveal_child(false));
        }
        {
            let page = self.clone();
            target.connect_drop(move |_, value, _, _| {
                page.drop_reveal.set_reveal_child(false);
                let Ok(list) = value.get::<gtk4::gdk::FileList>() else { return false };
                let paths: Vec<_> = list.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() || page.state.current.borrow().is_none() {
                    return false;
                }
                page.send_files(paths);
                true
            });
        }
        target_widget.add_controller(target);
    }

    fn start_recording(&self) {
        if self.state.recorder.borrow().is_some() || self.state.current.borrow().is_none() {
            return;
        }
        let rec = match voice::Recorder::start() {
            Ok(r) => r,
            Err(e) => {
                super::show_toast(&self.toast, &trf!("Could not record: {error}", "error" => e), 6);
                return;
            }
        };
        *self.state.recorder.borrow_mut() = Some(rec);
        self.state.rec_levels.borrow_mut().clear();
        self.rec_time.set_text("0:00");
        self.compose_stack.set_visible_child_name("record");
        let page = self.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let (polled, elapsed) = {
                let mut r = page.state.recorder.borrow_mut();
                let Some(rec) = r.as_mut() else { return glib::ControlFlow::Break };
                (rec.poll(), rec.elapsed_ms())
            };
            match polled {
                Ok(Some(level)) => page.state.rec_levels.borrow_mut().push(level),
                Ok(None) => {}
                Err(e) => {
                    page.stop_recording(false);
                    super::show_toast(&page.toast, &trf!("Could not record: {error}", "error" => e), 6);
                    return glib::ControlFlow::Break;
                }
            }
            page.rec_time.set_text(&voice::clock(elapsed));
            page.rec_area.queue_draw();
            if elapsed >= voice::MAX_MS {
                page.stop_recording(true);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    /// End the recording: send it, or throw it away.
    fn stop_recording(&self, send: bool) {
        let Some(rec) = self.state.recorder.borrow_mut().take() else { return };
        self.compose_stack.set_visible_child_name("compose");
        self.composer.view.grab_focus();
        if !send {
            rec.cancel();
            return;
        }
        if rec.elapsed_ms() < 700 {
            rec.cancel();
            super::show_toast(&self.toast, &tr!("Too short — record for at least a second."), 4);
            return;
        }
        let Some(id) = self.state.current.borrow().clone() else {
            rec.cancel();
            return;
        };
        let toast = self.toast.clone();
        super::run_in_background(
            move || {
                let (path, duration, wave) = rec.finish()?;
                files::send_voice(&id, &path, duration, wave)
            },
            move |r| {
                if let Err(e) = r {
                    super::show_toast(&toast, &trf!("Could not send: {error}", "error" => e), 6);
                }
            },
        );
    }

    /// "New chat": pick a person (or one of our groups).
    fn show_new_chat(&self) {
        let page = self.clone();
        self.pick_conversation(tr!("New chat"), move |id| page.open_conv(&id));
    }

    /// Pick a person or a group; `on_pick` gets the conversation.
    fn pick_conversation(&self, title: String, on_pick: impl Fn(ConvId) + 'static) {
        let on_pick = Rc::new(on_pick);
        let dlg = libadwaita::Window::builder()
            .transient_for(&self.win)
            .modal(true)
            .title(title)
            .default_width(400)
            .default_height(520)
            .build();
        super::dialogs::close_on_escape(&dlg);
        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        outer.append(&libadwaita::HeaderBar::new());
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        let search = gtk4::SearchEntry::builder().placeholder_text(tr!("Search people and groups")).build();
        body.append(&search);
        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        let status = libadwaita::ActionRow::builder().title(tr!("Loading…")).build();
        list.set_placeholder(Some(&status));
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        body.append(&scroll);
        outer.append(&body);
        dlg.set_content(Some(&outer));
        {
            let search = search.clone();
            list.set_filter_func(move |row| {
                let q = search.text().to_lowercase();
                q.is_empty() || row.widget_name().to_lowercase().contains(&q)
            });
        }
        {
            let list = list.clone();
            search.connect_search_changed(move |_| list.invalidate_filter());
        }
        let (on_pick2, list2, status2, dlg2) = (on_pick.clone(), list.clone(), status.clone(), dlg.clone());
        super::run_in_background(
            || {
                let me = chat::my_groups_refresh();
                let mut people: Vec<(String, bool)> = Vec::new();
                for p in crate::tailscale::get_peers() {
                    if p.user.is_empty() || Some(&p.user) == me.as_ref() || p.user.starts_with('_') || p.user == "tagged-devices" {
                        continue;
                    }
                    match people.iter_mut().find(|(u, _)| *u == p.user) {
                        Some(e) => e.1 |= p.online,
                        None => people.push((p.user.clone(), p.online)),
                    }
                }
                people.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                (people, chat::my_groups())
            },
            move |(people, groups)| {
                status2.set_title(&tr!("Nobody you can reach yet."));
                let add = |id: ConvId, subtitle: String, online: bool| {
                    let row = libadwaita::ActionRow::builder()
                        .title(glib::markup_escape_text(&id.name).as_str())
                        .subtitle(subtitle)
                        .activatable(true)
                        .build();
                    row.set_widget_name(&id.name);
                    let avatar = libadwaita::Avatar::new(32, Some(&id.name), !id.is_group());
                    if id.is_group() {
                        avatar.set_icon_name(Some("system-users-symbolic"));
                        crate::avatars::bind_group(&avatar, &id.name);
                    } else {
                        crate::avatars::bind(&avatar, &id.name);
                    }
                    row.add_prefix(&avatar);
                    if online {
                        let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
                        dot.add_css_class("peer-status-dot");
                        dot.add_css_class("online");
                        dot.set_valign(gtk4::Align::Center);
                        row.add_suffix(&dot);
                    }
                    let (on_pick, dlg, id) = (on_pick2.clone(), dlg2.clone(), id.clone());
                    row.connect_activated(move |_| {
                        dlg.close();
                        on_pick(id.clone());
                    });
                    list2.append(&row);
                };
                for (user, online) in people {
                    add(ConvId::direct(&user), if online { tr!("online") } else { tr!("offline") }, online);
                }
                for g in groups {
                    add(ConvId::group(&g), tr!("Group"), false);
                }
            },
        );
        dlg.present();
        search.grab_focus();
    }
}

fn notification_id(conv: &ConvId) -> String {
    format!("biglace-chat-{}", conv.key())
}

/// The conversation a list row stands for (kept in its widget name).
fn row_conv(row: &gtk4::ListBoxRow) -> Option<ConvId> {
    let name = row.widget_name();
    let (kind, n) = name.split_once(':')?;
    Some(ConvId { kind: kind.to_string(), name: n.to_string() })
}

/// One line of the conversation list: avatar, name, last message, time,
/// unread count.
/// People on the network we have no conversation with (online first).
fn people_to_list(convs: &[chat::Summary]) -> Vec<(String, bool)> {
    let me = chat::my_user();
    let mut people: Vec<(String, bool)> = Vec::new();
    for p in crate::tailscale::get_peers() {
        if p.user.is_empty() || Some(&p.user) == me.as_ref() || p.user.starts_with('_') || p.user == "tagged-devices" {
            continue;
        }
        if convs.iter().any(|c| !c.id.is_group() && c.id.name == p.user) {
            continue;
        }
        match people.iter_mut().find(|(u, _)| *u == p.user) {
            Some(e) => e.1 |= p.online,
            None => people.push((p.user.clone(), p.online)),
        }
    }
    people.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    people
}

fn people_signature(people: &[(String, bool)]) -> String {
    people.iter().map(|(u, on)| format!("{u}{}", if *on { "+" } else { "-" })).collect::<Vec<_>>().join(",")
}

/// A conversation in the list; `online` for a person we have no
/// conversation with yet (shown with whether they're on).
fn conv_row(c: &chat::Summary, online: Option<bool>) -> gtk4::ListBoxRow {
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    line.set_margin_top(6);
    line.set_margin_bottom(6);
    let avatar = libadwaita::Avatar::new(42, Some(&chat::label(&c.id.name).replace(['(', ')'], "")), !c.id.is_group());
    if c.id.is_group() {
        avatar.set_icon_name(Some("system-users-symbolic"));
        crate::avatars::bind_group(&avatar, &c.id.name);
    } else if !c.id.is_lan() {
        crate::avatars::bind(&avatar, &c.id.name);
    }
    if online == Some(true) {
        let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        dot.add_css_class("peer-status-dot");
        dot.add_css_class("online");
        dot.set_halign(gtk4::Align::End);
        dot.set_valign(gtk4::Align::End);
        let o = gtk4::Overlay::new();
        o.set_child(Some(&avatar));
        o.add_overlay(&dot);
        line.append(&o);
    } else {
        line.append(&avatar);
    }
    let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    texts.set_valign(gtk4::Align::Center);
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let name = gtk4::Label::new(Some(&chat::label(&c.id.name)));
    name.add_css_class("heading");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    top.append(&name);
    if let Some(m) = &c.last {
        let time = gtk4::Label::new(Some(&short_time(m.at)));
        time.add_css_class("dim-label");
        time.add_css_class("conv-time");
        top.append(&time);
    }
    texts.append(&top);
    let bottom = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let last = gtk4::Label::new(Some(&match &c.last {
        Some(m) if c.id.is_group() && !m.mine => format!("{}: {}", m.from, message_line(m, 60)),
        Some(m) if m.mine => trf!("You: {text}", "text" => message_line(m, 60)),
        Some(m) => message_line(m, 60),
        None if c.id.is_group() => tr!("Group conversation"),
        None => match online {
            Some(true) => tr!("online"),
            Some(false) => tr!("offline"),
            None => String::new(),
        },
    }));
    last.add_css_class("dim-label");
    last.set_xalign(0.0);
    last.set_hexpand(true);
    last.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    bottom.append(&last);
    if c.unread > 0 {
        let badge = gtk4::Label::new(Some(&if c.unread > 99 { "99+".into() } else { c.unread.to_string() }));
        badge.add_css_class("unread-badge");
        badge.set_valign(gtk4::Align::Center);
        bottom.append(&badge);
    }
    texts.append(&bottom);
    line.append(&texts);
    let row = gtk4::ListBoxRow::builder().child(&line).build();
    row.set_widget_name(&format!("{}:{}", c.id.kind, c.id.name));
    row
}

/// ✓ written, ✓✓ delivered.
fn set_tick(t: &gtk4::Label, m: &Message, _group: bool) {
    let delivered = !m.delivered.is_empty();
    t.set_text(if delivered { "✓✓" } else { "✓" });
    t.set_tooltip_text(Some(&if delivered {
        trf!("Delivered to {people}", "people" => m.delivered.join(", "))
    } else {
        tr!("Waiting to be delivered")
    }));
    if delivered {
        t.add_css_class("delivered");
    } else {
        t.remove_css_class("delivered");
    }
}

fn day_separator(at_ms: u64) -> gtk4::Widget {
    let label = gtk4::Label::new(Some(&day_label(at_ms)));
    label.add_css_class("day-pill");
    label.set_halign(gtk4::Align::Center);
    label.set_margin_top(12);
    label.set_margin_bottom(4);
    label.upcast()
}

fn local(at_ms: u64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local((at_ms / 1000) as i64).ok()
}

fn day_of(at_ms: u64) -> (i32, i32) {
    local(at_ms).map_or((0, 0), |d| (d.year(), d.day_of_year()))
}

fn clock(at_ms: u64) -> String {
    local(at_ms).and_then(|d| d.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

fn day_label(at_ms: u64) -> String {
    let now = glib::DateTime::now_local().ok();
    let today = now.as_ref().map(|d| (d.year(), d.day_of_year()));
    let yesterday = now.as_ref().and_then(|d| d.add_days(-1).ok()).map(|d| (d.year(), d.day_of_year()));
    let day = Some(day_of(at_ms));
    if day == today {
        tr!("Today")
    } else if day == yesterday {
        tr!("Yesterday")
    } else {
        local(at_ms).and_then(|d| d.format("%x").ok()).map(|s| s.to_string()).unwrap_or_default()
    }
}

/// Time for the list: the clock today, the date before.
fn short_time(at_ms: u64) -> String {
    let today = glib::DateTime::now_local().ok().map(|d| (d.year(), d.day_of_year()));
    if Some(day_of(at_ms)) == today {
        clock(at_ms)
    } else {
        local(at_ms).and_then(|d| d.format("%d/%m").ok()).map(|s| s.to_string()).unwrap_or_default()
    }
}

/// One line about a message: its file, or its text.
fn message_line(m: &Message, max_chars: usize) -> String {
    if m.deleted {
        return tr!("🗑 Message deleted");
    }
    if let Some(call) = &m.call {
        return chat::call_line(call, m.mine);
    }
    match chat_media::summary(m) {
        Some(file) if m.text.trim().is_empty() => file,
        Some(file) => format!("{file} · {}", preview_text(&m.text, max_chars)),
        None => preview_text(&m.text, max_chars),
    }
}

/// Plain one-line preview of a message (Markdown marks dropped).
fn preview_text(text: &str, max_chars: usize) -> String {
    let plain = markdown::plain_text(text);
    let one_line: String = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max_chars {
        format!("{}…", one_line.chars().take(max_chars).collect::<String>())
    } else {
        one_line
    }
}

/// Stable colour per person for their name in group bubbles.
fn color_index(user: &str) -> u32 {
    user.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 6
}

// ─── Telegram-style message actions ─────────────────────────────────────────

impl ChatPage {
    fn current(&self) -> Option<ConvId> {
        self.state.current.borrow().clone()
    }

    /// (Re)draw message `m` inside `holder`.
    fn fill_bubble(&self, holder: &gtk4::Box, m: &Message, look: (bool, bool, bool)) -> (Option<gtk4::Label>, Option<Rc<FileView>>) {
        while let Some(c) = holder.first_child() {
            holder.remove(&c);
        }
        let conv = self.current().unwrap_or_else(|| ConvId::direct(&m.from));
        let (widget, tick, file) = self.bubble(m, &conv, look.0, look.1, look.2);
        holder.append(&widget);
        if self.state.selected.borrow().contains(&m.id) {
            holder.add_css_class("selected");
        }
        (tick, file)
    }

    /// A message bubble (or a big emoji); the widget and, for our own
    /// messages, the tick label to update when it's delivered.
    fn bubble(
        &self,
        m: &Message,
        conv: &ConvId,
        show_name: bool,
        continues: bool,
        avatar_slot: bool,
    ) -> (gtk4::Widget, Option<gtk4::Label>, Option<Rc<FileView>>) {
        let outer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        outer.set_halign(if m.mine { gtk4::Align::End } else { gtk4::Align::Start });
        outer.set_margin_top(if continues { 1 } else { 8 });
        outer.set_margin_start(if m.mine { 64 } else { 10 });
        outer.set_margin_end(if m.mine { 10 } else { 64 });
        // Groups: who's talking, by face (first message of each block).
        if avatar_slot {
            let a = libadwaita::Avatar::new(32, Some(&m.from), true);
            a.set_valign(gtk4::Align::Start);
            if continues {
                a.set_opacity(0.0);
            } else {
                crate::avatars::bind(&a, &m.from);
            }
            outer.append(&a);
        }

        let jumbo = !m.deleted && m.file.is_none() && m.reply_to.is_none() && markdown::is_jumbo_emoji(&m.text);
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        if jumbo {
            b.add_css_class("bubble-jumbo");
        } else {
            b.add_css_class("bubble");
            b.add_css_class(if m.mine { "mine" } else { "theirs" });
            if continues {
                b.add_css_class("continues");
            }
            if m.file.as_ref().is_some_and(|f| matches!(f.offer.kind, chat::FileKind::Image | chat::FileKind::Video)) {
                b.add_css_class("media");
            }
        }
        if show_name {
            let name = gtk4::Label::new(Some(&m.from));
            name.add_css_class("bubble-name");
            name.add_css_class(&format!("name-color-{}", color_index(&m.from)));
            name.set_xalign(0.0);
            b.append(&name);
        }
        if let Some(author) = m.forwarded_from.as_ref().filter(|_| !m.deleted) {
            let fwd = gtk4::Label::new(Some(&trf!("Forwarded from {name}", "name" => author)));
            fwd.add_css_class("bubble-forwarded");
            fwd.set_xalign(0.0);
            b.append(&fwd);
        }
        if let Some(r) = m.reply_to.as_ref().filter(|_| !m.deleted) {
            let quote = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            quote.add_css_class("bubble-reply");
            quote.add_css_class(&format!("reply-color-{}", color_index(&r.from)));
            let who = gtk4::Label::new(Some(&chat::label(&r.from)));
            who.add_css_class("bubble-reply-name");
            who.set_xalign(0.0);
            let what = gtk4::Label::new(Some(&r.preview));
            what.set_xalign(0.0);
            what.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            what.set_max_width_chars(40);
            what.add_css_class("bubble-reply-text");
            quote.append(&who);
            quote.append(&what);
            let (page, target) = (self.clone(), r.id.clone());
            let click = gtk4::GestureClick::new();
            click.connect_released(move |g, _, _, _| {
                g.set_state(gtk4::EventSequenceState::Claimed);
                page.scroll_to_message(&target);
            });
            quote.add_controller(click);
            quote.set_cursor_from_name(Some("pointer"));
            b.append(&quote);
        }
        let file = m.file.as_ref().filter(|_| !m.deleted).map(|_| {
            let view = chat_media::build(m, conv);
            b.append(&view.slot);
            view
        });
        if let Some(call) = &m.call {
            b.append(&super::call_ui::call_entry(call, m.mine, &conv.name));
        }
        let text = gtk4::Label::new(None);
        text.add_css_class("bubble-text");
        if m.deleted {
            text.set_text(&tr!("🗑 Message deleted"));
            text.add_css_class("bubble-deleted");
        } else if jumbo {
            text.set_text(m.text.trim());
            text.add_css_class("jumbo-emoji");
        } else {
            text.set_markup(&markdown::to_pango(&m.text));
            text.set_visible(!m.text.trim().is_empty());
            // "@name": a click opens the conversation with that person.
            let page = self.clone();
            text.connect_activate_link(move |_, uri| match uri.strip_prefix("mention:") {
                Some(name) => {
                    if chat::my_user().as_deref() != Some(name) && crate::chat::name_ok(name) {
                        page.open_conv(&ConvId::direct(name));
                    }
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            });
            // Someone mentioned us: the bubble says so (like Telegram).
            if !m.mine && chat::my_user().is_some_and(|me| markdown::mentions_user(&m.text, &me)) {
                b.add_css_class("mentions-me");
            }
        }
        text.set_wrap(true);
        text.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        text.set_max_width_chars(52);
        text.set_xalign(0.0);
        text.set_selectable(!m.deleted);
        text.set_can_focus(false);
        b.append(&text);

        // Reactions: one chip per emoji, ours highlighted; click to toggle.
        if !m.reactions.is_empty() {
            let me = chat::my_user().unwrap_or_default();
            let chips = gtk4::FlowBox::builder()
                .selection_mode(gtk4::SelectionMode::None)
                .max_children_per_line(8)
                .column_spacing(4)
                .row_spacing(4)
                .halign(gtk4::Align::Start)
                .build();
            chips.add_css_class("reactions");
            for (emoji, who) in &m.reactions {
                let chip = gtk4::Button::builder().label(format!("{emoji} {}", who.len())).build();
                chip.add_css_class("reaction-chip");
                if who.contains(&me) {
                    chip.add_css_class("mine");
                }
                chip.set_tooltip_text(Some(&who.join(", ")));
                let (page, id, emoji) = (self.clone(), m.id.clone(), emoji.clone());
                chip.connect_clicked(move |_| page.react(&id, &emoji));
                chips.insert(&chip, -1);
            }
            b.append(&chips);
        }

        let meta = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        meta.set_halign(gtk4::Align::End);
        meta.add_css_class("bubble-meta");
        if m.edited_at > 0 && !m.deleted {
            let edited = gtk4::Label::new(Some(&tr!("edited")));
            edited.add_css_class("bubble-edited");
            meta.append(&edited);
        }
        let time = gtk4::Label::new(Some(&clock(m.at)));
        meta.append(&time);
        let tick = (m.mine && !m.deleted && m.call.is_none()).then(|| {
            let t = gtk4::Label::new(None);
            t.add_css_class("tick");
            meta.append(&t);
            t
        });
        if let Some(t) = &tick {
            set_tick(t, m, false);
        }
        b.append(&meta);
        outer.append(&b);

        // Right click / long press: the actions; in selection mode a click
        // selects. Captured first, so a selectable label doesn't get it.
        let menu_click = gtk4::GestureClick::new();
        menu_click.set_button(0);
        menu_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let (page, msg, anchor) = (self.clone(), m.clone(), b.clone());
            menu_click.connect_pressed(move |g, _, x, y| {
                let button = g.current_button();
                if page.state.selecting.get() && button == gtk4::gdk::BUTTON_PRIMARY {
                    g.set_state(gtk4::EventSequenceState::Claimed);
                    page.toggle_select(&msg.id);
                } else if button == gtk4::gdk::BUTTON_SECONDARY {
                    g.set_state(gtk4::EventSequenceState::Claimed);
                    page.show_actions(&anchor, &msg, x, y);
                }
            });
        }
        b.add_controller(menu_click);
        let press = gtk4::GestureLongPress::new();
        {
            let (page, msg, anchor) = (self.clone(), m.clone(), b.clone());
            press.connect_pressed(move |g, x, y| {
                g.set_state(gtk4::EventSequenceState::Claimed);
                page.show_actions(&anchor, &msg, x, y);
            });
        }
        b.add_controller(press);
        (outer.upcast(), tick, file)
    }

    /// The actions popover for message `m`, Telegram-style: quick
    /// reactions on top, then reply, edit, pin, copy, forward, delete, select.
    fn show_actions(&self, anchor: &gtk4::Box, m: &Message, x: f64, y: f64) {
        let Some(conv) = self.current() else { return };
        let pop = gtk4::Popover::builder().has_arrow(false).build();
        pop.add_css_class("msg-actions");
        pop.set_parent(anchor);
        pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let col = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        if !m.deleted {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
            row.add_css_class("react-row");
            for emoji in QUICK_REACTIONS {
                let b = gtk4::Button::builder().label(emoji).build();
                b.add_css_class("react-pick");
                let (page, id, pop) = (self.clone(), m.id.clone(), pop.clone());
                b.connect_clicked(move |_| {
                    pop.popdown();
                    page.react(&id, emoji);
                });
                row.append(&b);
            }
            let more = gtk4::Button::builder()
                .icon_name("pan-down-symbolic")
                .tooltip_text(tr!("More reactions"))
                .build();
            more.add_css_class("react-more");
            {
                let (page, id, pop, anchor) = (self.clone(), m.id.clone(), pop.clone(), anchor.clone());
                more.connect_clicked(move |_| {
                    pop.popdown();
                    let (page2, id2) = (page.clone(), id.clone());
                    let picker = super::emoji_picker::build(move |e| page2.react(&id2, e));
                    picker.set_parent(&anchor);
                    picker.connect_closed(|p| {
                        let p = p.clone();
                        glib::idle_add_local_once(move || p.unparent());
                    });
                    picker.popup();
                });
            }
            row.append(&more);
            col.append(&row);
            col.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        }
        let item = |icon: &str, label: String, destructive: bool, act: Box<dyn Fn()>| {
            let b = gtk4::Button::builder()
                .child(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).halign(gtk4::Align::Start).build())
                .css_classes(["flat", "msg-action"])
                .build();
            if destructive {
                b.add_css_class("destructive");
            }
            let pop = pop.clone();
            b.connect_clicked(move |_| {
                pop.popdown();
                act();
            });
            col.append(&b);
        };
        if !m.deleted {
            let (page, msg) = (self.clone(), m.clone());
            item("mail-reply-sender-symbolic", tr!("Reply"), false, Box::new(move || page.start_reply(&msg)));
            if m.mine && m.file.is_none() && m.forwarded_from.is_none() && !m.text.trim().is_empty() {
                let (page, msg) = (self.clone(), m.clone());
                item("document-edit-symbolic", tr!("Edit"), false, Box::new(move || page.start_edit(&msg)));
            }
            let pinned_now = chat::pinned(&conv).is_some_and(|p| p.id == m.id);
            let (page, id) = (self.clone(), m.id.clone());
            item(
                "view-pin-symbolic",
                if pinned_now { tr!("Unpin") } else { tr!("Pin") },
                false,
                Box::new(move || page.toggle_pin(&id, pinned_now)),
            );
            if !m.text.trim().is_empty() {
                let (page, id) = (self.clone(), m.id.clone());
                item("edit-copy-symbolic", tr!("Copy text"), false, Box::new(move || page.copy_messages(std::slice::from_ref(&id))));
            }
            let (page, id) = (self.clone(), m.id.clone());
            item("mail-forward-symbolic", tr!("Forward"), false, Box::new(move || page.forward_dialog(vec![id.clone()])));
        }
        let (page, id) = (self.clone(), m.id.clone());
        item("user-trash-symbolic", tr!("Delete"), true, Box::new(move || page.confirm_delete(vec![id.clone()])));
        let (page, id) = (self.clone(), m.id.clone());
        item("object-select-symbolic", tr!("Select"), false, Box::new(move || page.start_selection(&id)));
        pop.set_child(Some(&col));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    fn react(&self, id: &str, emoji: &str) {
        let Some(conv) = self.current() else { return };
        let (id, emoji, toast) = (id.to_string(), emoji.to_string(), self.toast.clone());
        super::run_in_background(move || chat::react(&conv, &id, &emoji), move |r| {
            if let Err(e) = r {
                super::show_toast(&toast, &trf!("Could not react: {error}", "error" => e), 5);
            }
        });
    }

    fn show_reply_bar(&self, icon: &str, title: &str, text: &str) {
        self.reply_icon.set_icon_name(Some(icon));
        self.reply_title.set_text(title);
        self.reply_text.set_text(text);
        self.reply_reveal.set_reveal_child(true);
        self.composer.view.grab_focus();
    }

    fn start_reply(&self, m: &Message) {
        *self.state.editing.borrow_mut() = None;
        *self.state.reply_to.borrow_mut() = Some(m.id.clone());
        self.show_reply_bar("mail-reply-sender-symbolic", &trf!("Reply to {name}", "name" => chat::label(&m.from)), &chat::preview_line(m));
    }

    fn start_edit(&self, m: &Message) {
        *self.state.reply_to.borrow_mut() = None;
        *self.state.editing.borrow_mut() = Some(m.id.clone());
        self.show_reply_bar("document-edit-symbolic", &tr!("Editing message"), &chat::preview_line(m));
        self.composer.set_markdown(&m.text);
    }

    fn cancel_reply(&self) {
        let was_editing = self.state.editing.borrow_mut().take().is_some();
        *self.state.reply_to.borrow_mut() = None;
        self.reply_reveal.set_reveal_child(false);
        if was_editing {
            self.composer.clear();
        }
    }

    fn toggle_pin(&self, id: &str, pinned_now: bool) {
        let Some(conv) = self.current() else { return };
        let (id, toast) = (id.to_string(), self.toast.clone());
        super::run_in_background(
            move || chat::pin(&conv, if pinned_now { None } else { Some(&id) }),
            move |r| {
                if let Err(e) = r {
                    super::show_toast(&toast, &e, 5);
                }
            },
        );
    }

    fn update_pinned(&self) {
        match self.current().as_ref().and_then(chat::pinned) {
            Some(m) => {
                self.pinned_text.set_text(&chat::preview_line(&m));
                self.pinned_reveal.set_reveal_child(true);
            }
            None => self.pinned_reveal.set_reveal_child(false),
        }
    }

    /// The text of messages `ids` to the clipboard (several: "who: text").
    fn copy_messages(&self, ids: &[String]) {
        let Some(conv) = self.current() else { return };
        let all = chat::messages(&conv);
        let picked: Vec<&Message> = ids.iter().filter_map(|id| all.iter().find(|m| &m.id == id && !m.deleted)).collect();
        let text = if picked.len() == 1 {
            markdown::plain_text(&picked[0].text)
        } else {
            picked
                .iter()
                .map(|m| format!("{}: {}", m.from, markdown::plain_text(&m.text)))
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        if text.trim().is_empty() {
            return;
        }
        self.win.clipboard().set_text(&text);
        super::show_toast(&self.toast, &tr!("Copied."), 2);
    }

    /// Pick where to forward messages `ids`, then send them there.
    fn forward_dialog(&self, ids: Vec<String>) {
        let Some(from) = self.current() else { return };
        if ids.is_empty() {
            return;
        }
        let page = self.clone();
        self.pick_conversation(tr!("Forward to…"), move |to| {
            let (page2, from, ids, to2) = (page.clone(), from.clone(), ids.clone(), to.clone());
            super::run_in_background(
                move || chat::forward(&to2, &from, &ids),
                move |r| match r {
                    Ok(()) => {
                        page2.end_selection();
                        page2.open_conv(&to);
                        super::show_toast(&page2.toast, &tr!("Forwarded."), 3);
                    }
                    Err(e) => super::show_toast(&page2.toast, &trf!("Could not forward: {error}", "error" => e), 6),
                },
            );
        });
    }

    /// "Delete N messages?": for me, or (all ours) for everyone.
    fn confirm_delete(&self, ids: Vec<String>) {
        let Some(conv) = self.current() else { return };
        if ids.is_empty() {
            return;
        }
        let all = chat::messages(&conv);
        let ours = ids.iter().all(|id| all.iter().any(|m| &m.id == id && m.mine && !m.deleted));
        let heading = if ids.len() == 1 {
            tr!("Delete the message?")
        } else {
            trf!("Delete {count} messages?", "count" => ids.len().to_string())
        };
        let dlg = libadwaita::MessageDialog::builder()
            .transient_for(&self.win)
            .modal(true)
            .heading(heading)
            .body(if ours {
                tr!("“For everyone” removes it from the other computers too.")
            } else {
                tr!("It's removed from this computer only.")
            })
            .build();
        dlg.add_response("cancel", &tr!("Cancel"));
        dlg.add_response("me", &tr!("Delete for me"));
        if ours {
            dlg.add_response("all", &tr!("Delete for everyone"));
            dlg.set_response_appearance("all", libadwaita::ResponseAppearance::Destructive);
        } else {
            dlg.set_response_appearance("me", libadwaita::ResponseAppearance::Destructive);
        }
        dlg.set_default_response(Some("cancel"));
        dlg.set_close_response("cancel");
        let page = self.clone();
        dlg.connect_response(None, move |_, r| {
            let (conv, ids) = (conv.clone(), ids.clone());
            match r {
                "me" => {
                    chat::delete_for_me(&conv, &ids);
                    page.end_selection();
                }
                "all" => {
                    let toast = page.toast.clone();
                    page.end_selection();
                    super::run_in_background(move || chat::delete_for_all(&conv, &ids), move |r| {
                        if let Err(e) = r {
                            super::show_toast(&toast, &trf!("Could not delete: {error}", "error" => e), 6);
                        }
                    });
                }
                _ => {}
            }
        });
        dlg.present();
    }

    fn start_selection(&self, id: &str) {
        self.cancel_reply();
        self.state.selecting.set(true);
        self.state.selected.borrow_mut().clear();
        self.toggle_select(id);
        self.compose_stack.set_visible_child_name("select");
    }

    fn toggle_select(&self, id: &str) {
        {
            let mut sel = self.state.selected.borrow_mut();
            if !sel.remove(id) {
                sel.insert(id.to_string());
            }
        }
        if self.state.selected.borrow().is_empty() {
            self.end_selection();
            return;
        }
        self.update_selection();
    }

    fn update_selection(&self) {
        let sel = self.state.selected.borrow();
        for (id, r) in self.state.rendered.borrow().iter() {
            if sel.contains(id) {
                r.holder.add_css_class("selected");
            } else {
                r.holder.remove_css_class("selected");
            }
        }
        self.select_count.set_text(&if sel.len() == 1 {
            tr!("1 selected")
        } else {
            trf!("{count} selected", "count" => sel.len().to_string())
        });
    }

    fn end_selection(&self) {
        if !self.state.selecting.replace(false) {
            return;
        }
        self.state.selected.borrow_mut().clear();
        self.update_selection();
        self.compose_stack.set_visible_child_name("compose");
    }

    /// The selected messages, oldest first.
    fn selected_in_order(&self) -> Vec<String> {
        let Some(conv) = self.current() else { return vec![] };
        let sel = self.state.selected.borrow();
        chat::messages(&conv).into_iter().filter(|m| sel.contains(&m.id)).map(|m| m.id).collect()
    }

    /// Bring message `id` into view and make it flash.
    fn scroll_to_message(&self, id: &str) {
        let Some(holder) = self.state.rendered.borrow().get(id).map(|r| r.holder.clone()) else {
            super::show_toast(&self.toast, &tr!("That message isn't here anymore."), 3);
            return;
        };
        if let Some(p) = holder.compute_point(&self.messages, &gtk4::graphene::Point::new(0.0, 0.0)) {
            let adj = self.scroll.vadjustment();
            adj.set_value((f64::from(p.y()) - adj.page_size() / 3.0).max(0.0));
        }
        holder.add_css_class("flash");
        glib::timeout_add_local_once(std::time::Duration::from_millis(1600), move || holder.remove_css_class("flash"));
    }

    fn clear_search(&self) {
        self.state.search_hits.borrow_mut().clear();
        self.search_count.set_text("");
    }

    /// Find `query` in this conversation (accents and case ignored); the
    /// newest hit first.
    fn search(&self, query: &str) {
        let Some(conv) = self.current() else { return };
        let q = super::emoji_picker::fold(query.trim());
        if q.is_empty() {
            self.clear_search();
            return;
        }
        let hits: Vec<String> = chat::messages(&conv)
            .into_iter()
            .filter(|m| !m.deleted && super::emoji_picker::fold(&markdown::plain_text(&m.text)).contains(&q))
            .map(|m| m.id)
            .collect();
        let n = hits.len();
        *self.state.search_hits.borrow_mut() = hits;
        if n == 0 {
            self.search_count.set_text(&tr!("No results"));
            return;
        }
        self.state.search_pos.set(n - 1);
        self.show_hit();
    }

    /// Older (-1) or newer (+1) hit.
    fn search_step(&self, delta: i32) {
        let n = self.state.search_hits.borrow().len();
        if n == 0 {
            return;
        }
        let pos = (self.state.search_pos.get() as i32 + delta).clamp(0, n as i32 - 1) as usize;
        self.state.search_pos.set(pos);
        self.show_hit();
    }

    fn show_hit(&self) {
        let hits = self.state.search_hits.borrow().clone();
        let pos = self.state.search_pos.get();
        if let Some(id) = hits.get(pos) {
            self.search_count.set_text(&trf!("{n} of {total}", "n" => (pos + 1).to_string(), "total" => hits.len().to_string()));
            self.scroll_to_message(id);
        }
    }

    /// "typing…" under the conversation's name, for a few seconds.
    fn show_typing(&self, conv: &ConvId, user: &str) {
        if self.current().as_ref() != Some(conv) {
            return;
        }
        let gen = self.state.typing_gen.get() + 1;
        self.state.typing_gen.set(gen);
        self.conv_subtitle.set_text(&if conv.is_group() {
            trf!("{user} is typing…", "user" => user)
        } else {
            tr!("typing…")
        });
        self.conv_subtitle.add_css_class("typing");
        let (page, conv) = (self.clone(), conv.clone());
        glib::timeout_add_local_once(std::time::Duration::from_secs(5), move || {
            if page.state.typing_gen.get() == gen {
                page.conv_subtitle.remove_css_class("typing");
                page.conv_subtitle.set_text(&if conv.is_group() { tr!("Group") } else { tr!("Direct conversation") });
            }
        });
    }
}
