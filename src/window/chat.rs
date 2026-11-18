//! "Chat" section: conversations with people and groups of the network,
//! Telegram-style — bubbles, Markdown, emoji, delivery ticks. The messages
//! themselves are handled by `crate::chat`.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use super::chat_media::{self, FileView};
use crate::chat::{self, files, markdown, voice, ChatEvent, ConvId, Message};
use crate::tr;
use crate::trf;

/// Messages closer than this from the same person form one block.
const BLOCK_GAP_MS: u64 = 5 * 60 * 1000;

#[derive(Clone)]
pub struct ChatPage {
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
    scroll: gtk4::ScrolledWindow,
    messages: gtk4::Box,
    composer: super::composer::Composer,
    /// Attach (direct conversations only).
    btn_attach: gtk4::Button,
    /// Microphone when there's nothing typed (direct conversations), send
    /// otherwise.
    action_stack: gtk4::Stack,
    /// Typing, or recording a voice message.
    compose_stack: gtk4::Stack,
    rec_time: gtk4::Label,
    rec_area: gtk4::DrawingArea,
    drop_hint: gtk4::Label,
    drop_reveal: gtk4::Revealer,
    state: Rc<State>,
}

/// A message on screen: what may change later.
struct Rendered {
    /// Our messages: ✓ / ✓✓.
    tick: Option<gtk4::Label>,
    /// Its file part.
    file: Option<Rc<FileView>>,
}

#[derive(Default)]
struct State {
    current: RefCell<Option<ConvId>>,
    /// Rendered messages of the current conversation.
    rendered: RefCell<HashMap<String, Rendered>>,
    /// Last rendered message (to group blocks and day separators).
    last: RefCell<Option<Message>>,
    /// The section is on screen.
    visible: Cell<bool>,
    /// A voice message being recorded, and its loudness so far.
    recorder: RefCell<Option<voice::Recorder>>,
    rec_levels: RefCell<Vec<f32>>,
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
        header.pack_start(&btn_new);
        header.pack_end(&menu_btn);
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
        conv_subtitle.add_css_class("caption");
        conv_subtitle.set_xalign(0.0);
        let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        titles.set_valign(gtk4::Align::Center);
        titles.append(&conv_title);
        titles.append(&conv_subtitle);
        let conv_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        conv_header.add_css_class("chat-header");
        conv_header.append(&conv_avatar);
        conv_header.append(&titles);

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

        // Recording a voice message: cancel · ● 0:07 · live bars · send.
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
        rec_bar.append(&btn_rec_cancel);
        rec_bar.append(&rec_dot);
        rec_bar.append(&rec_time);
        rec_bar.append(&rec_area);
        rec_bar.append(&btn_rec_send);

        let compose_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::SlideUpDown)
            .transition_duration(200)
            .vhomogeneous(false)
            .interpolate_size(true)
            .build();
        compose_stack.add_named(&bar, Some("compose"));
        compose_stack.add_named(&rec_bar, Some("record"));

        let conv_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        conv_box.append(&conv_header);
        conv_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        conv_box.append(&scroll);
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
        body.append(&list_scroll);
        body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
        body.append(&stack);
        toolbar.set_content(Some(&body));

        let page = Self {
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
            scroll,
            messages,
            composer,
            btn_attach: btn_attach.clone(),
            action_stack,
            compose_stack,
            rec_time,
            rec_area: rec_area.clone(),
            drop_hint,
            drop_reveal,
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

    /// Development builds only: open a conversation (preview).
    #[cfg(debug_assertions)]
    pub fn preview_open(&self, id: &ConvId) {
        self.state.visible.set(true);
        self.open_conv(id);
    }

    /// The section became visible (or not).
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
            let row = conv_row(c);
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
        self.conv_title.set_text(&id.name);
        if id.is_group() {
            self.conv_avatar.set_text(Some(&id.name));
            self.conv_avatar.set_icon_name(Some("system-users-symbolic"));
            self.conv_avatar.set_show_initials(false);
            crate::avatars::unbind(&self.conv_avatar);
            self.conv_subtitle.set_text(&tr!("Group"));
        } else {
            self.conv_avatar.set_text(Some(&id.name));
            self.conv_avatar.set_icon_name(None);
            self.conv_avatar.set_show_initials(true);
            crate::avatars::bind(&self.conv_avatar, &id.name);
            self.conv_subtitle.set_text(&tr!("Direct conversation"));
        }
        for m in chat::messages(id) {
            // Pictures and voice messages that didn't come down yet.
            if m.file.as_ref().is_some_and(|f| f.offer.fetch_by_itself() && f.local().is_none()) {
                files::download(id, &m.id);
            }
            self.render(&m, false);
        }
        if self.state.recorder.borrow().is_some() {
            self.stop_recording(false);
        }
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
        let text = self.composer.markdown();
        if text.trim().is_empty() {
            return;
        }
        if text.len() > chat::MAX_TEXT {
            super::show_toast(&self.toast, &tr!("That message is too long."), 5);
            return;
        }
        self.composer.clear();
        let toast = self.toast.clone();
        super::run_in_background(
            move || chat::send(&id, &text),
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
            ChatEvent::Incoming { conv, message } => {
                let looking = self.state.visible.get()
                    && self.win.is_active()
                    && self.state.current.borrow().as_ref() == Some(&conv);
                if !looking {
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
        for m in chat::messages(&id) {
            let known = self
                .state
                .rendered
                .borrow()
                .get(&m.id)
                .map(|r| (r.tick.clone(), r.file.clone()));
            match known {
                Some((tick, file)) => {
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
        let conv = self.state.current.borrow().clone().unwrap_or_else(|| ConvId::direct(&m.from));
        let (widget, tick, file) = bubble(m, &conv, group && !m.mine && !continues, continues, group && !m.mine);
        if animate {
            let rev = gtk4::Revealer::builder()
                .transition_type(gtk4::RevealerTransitionType::SlideUp)
                .transition_duration(220)
                .child(&widget)
                .build();
            self.messages.append(&rev);
            glib::idle_add_local_once(move || rev.set_reveal_child(true));
        } else {
            self.messages.append(&widget);
        }
        self.state.rendered.borrow_mut().insert(m.id.clone(), Rendered { tick, file });
        *self.state.last.borrow_mut() = Some(m.clone());
    }

    fn notify(&self, conv: &ConvId, m: &Message) {
        let Some(app) = self.win.application() else { return };
        let title = if conv.is_group() {
            trf!("{user} in {group}", "user" => m.from, "group" => conv.name)
        } else {
            m.from.clone()
        };
        let n = gtk4::gio::Notification::new(&title);
        n.set_body(Some(&message_line(m, 140)));
        app.send_notification(Some(&notification_id(conv)), &n);
    }

    fn current_is_direct(&self) -> bool {
        self.state.current.borrow().as_ref().is_some_and(|c| !c.is_group())
    }

    /// Attach and the microphone only in direct conversations; send when
    /// something is typed.
    fn update_actions(&self) {
        let direct = self.current_is_direct();
        self.btn_attach.set_visible(direct);
        let typed = self.composer.view.buffer().char_count() > 0;
        self.action_stack.set_visible_child_name(if typed || !direct { "send" } else { "mic" });
    }

    fn send_files(&self, paths: Vec<std::path::PathBuf>) {
        let Some(id) = self.state.current.borrow().clone() else { return };
        if id.is_group() {
            super::show_toast(&self.toast, &tr!("Files can only be sent in direct conversations."), 5);
            return;
        }
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
                let direct = page.current_is_direct();
                page.drop_hint.set_text(&if direct {
                    tr!("Drop to send")
                } else {
                    tr!("Files can only be sent in direct conversations")
                });
                page.drop_reveal.set_reveal_child(true);
                if direct {
                    gtk4::gdk::DragAction::COPY
                } else {
                    gtk4::gdk::DragAction::empty()
                }
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
                if paths.is_empty() || !page.current_is_direct() {
                    return false;
                }
                page.send_files(paths);
                true
            });
        }
        target_widget.add_controller(target);
    }

    fn start_recording(&self) {
        if self.state.recorder.borrow().is_some() || !self.current_is_direct() {
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
        let dlg = libadwaita::Window::builder()
            .transient_for(&self.win)
            .modal(true)
            .title(tr!("New chat"))
            .default_width(400)
            .default_height(520)
            .build();
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
        let (page, list2, status2, dlg2) = (self.clone(), list.clone(), status.clone(), dlg.clone());
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
                    let (page, dlg, id) = (page.clone(), dlg2.clone(), id.clone());
                    row.connect_activated(move |_| {
                        dlg.close();
                        page.open_conv(&id);
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
fn conv_row(c: &chat::Summary) -> gtk4::ListBoxRow {
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    line.set_margin_top(6);
    line.set_margin_bottom(6);
    let avatar = libadwaita::Avatar::new(42, Some(&c.id.name), !c.id.is_group());
    if c.id.is_group() {
        avatar.set_icon_name(Some("system-users-symbolic"));
    } else {
        crate::avatars::bind(&avatar, &c.id.name);
    }
    line.append(&avatar);
    let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    texts.set_valign(gtk4::Align::Center);
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let name = gtk4::Label::new(Some(&c.id.name));
    name.add_css_class("heading");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    top.append(&name);
    if let Some(m) = &c.last {
        let time = gtk4::Label::new(Some(&short_time(m.at)));
        time.add_css_class("dim-label");
        time.add_css_class("caption");
        top.append(&time);
    }
    texts.append(&top);
    let bottom = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let last = gtk4::Label::new(Some(&match &c.last {
        Some(m) if c.id.is_group() && !m.mine => format!("{}: {}", m.from, message_line(m, 60)),
        Some(m) if m.mine => trf!("You: {text}", "text" => message_line(m, 60)),
        Some(m) => message_line(m, 60),
        None => if c.id.is_group() { tr!("Group conversation") } else { String::new() },
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

/// A message bubble (or a big emoji); returns the widget and, for our own
/// messages, the tick label to update when it's delivered.
fn bubble(
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

    let jumbo = m.file.is_none() && markdown::is_jumbo_emoji(&m.text);
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
    let file = m.file.as_ref().map(|_| {
        let view = chat_media::build(m, conv);
        b.append(&view.slot);
        view
    });
    let text = gtk4::Label::new(None);
    text.set_visible(!m.text.trim().is_empty());
    if jumbo {
        text.set_text(m.text.trim());
        text.add_css_class("jumbo-emoji");
    } else {
        text.set_markup(&markdown::to_pango(&m.text));
    }
    text.set_wrap(true);
    text.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    text.set_max_width_chars(52);
    text.set_xalign(0.0);
    text.set_selectable(true);
    text.set_can_focus(false);
    b.append(&text);

    let meta = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    meta.set_halign(gtk4::Align::End);
    meta.add_css_class("bubble-meta");
    let time = gtk4::Label::new(Some(&clock(m.at)));
    meta.append(&time);
    let tick = m.mine.then(|| {
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
    (outer.upcast(), tick, file)
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
