//! "Chat" section: conversations with people and groups of the network,
//! Telegram-style — bubbles, Markdown, emoji, delivery ticks. The messages
//! themselves are handled by `crate::chat`.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::chat::{self, markdown, ChatEvent, ConvId, Message};
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
    state: Rc<State>,
}

#[derive(Default)]
struct State {
    current: RefCell<Option<ConvId>>,
    /// Rendered messages of the current conversation: id → its tick label.
    rendered: RefCell<HashMap<String, Option<gtk4::Label>>>,
    /// Last rendered message (to group blocks and day separators).
    last: RefCell<Option<Message>>,
    /// The section is on screen.
    visible: Cell<bool>,
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

        // Composer: emoji · text · send.
        let composer = super::composer::Composer::new(win);
        let picker = {
            let composer = composer.clone();
            super::emoji_picker::build(move |e| composer.insert(e))
        };
        let btn_emoji = gtk4::MenuButton::builder()
            .icon_name("emoji-people-symbolic")
            .popover(&picker)
            .valign(gtk4::Align::End)
            .css_classes(["flat", "circular", "chat-emoji"])
            .tooltip_text(tr!("Emoji"))
            .build();
        // Ctrl+; and "Insert emoji" open this picker, not GTK's own.
        {
            let btn = btn_emoji.clone();
            composer.on_emoji(move || btn.popup());
        }
        let btn_send = gtk4::Button::builder()
            .icon_name("mail-send-symbolic")
            .valign(gtk4::Align::End)
            .css_classes(["suggested-action", "circular", "chat-send"])
            .tooltip_text(tr!("Send (Enter) — Shift+Enter for a new line"))
            .build();
        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        bar.add_css_class("chat-composer");
        bar.append(&btn_emoji);
        bar.append(&composer.widget);
        bar.append(&btn_send);

        let conv_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        conv_box.append(&conv_header);
        conv_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        conv_box.append(&scroll);
        conv_box.append(&bar);

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
        stack.add_named(&conv_box, Some("conv"));
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
            state: Rc::default(),
        };

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
            self.render(&m, false);
        }
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
            let known = self.state.rendered.borrow().get(&m.id).cloned();
            match known {
                Some(tick) => {
                    if let Some(t) = tick {
                        set_tick(&t, &m, id.is_group());
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
        let (widget, tick) = bubble(m, group && !m.mine && !continues, continues, group && !m.mine);
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
        self.state.rendered.borrow_mut().insert(m.id.clone(), tick);
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
        n.set_body(Some(&preview_text(&m.text, 140)));
        app.send_notification(Some(&notification_id(conv)), &n);
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
        Some(m) if c.id.is_group() && !m.mine => format!("{}: {}", m.from, preview_text(&m.text, 60)),
        Some(m) if m.mine => trf!("You: {text}", "text" => preview_text(&m.text, 60)),
        Some(m) => preview_text(&m.text, 60),
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
fn bubble(m: &Message, show_name: bool, continues: bool, avatar_slot: bool) -> (gtk4::Widget, Option<gtk4::Label>) {
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

    let jumbo = markdown::is_jumbo_emoji(&m.text);
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    if jumbo {
        b.add_css_class("bubble-jumbo");
    } else {
        b.add_css_class("bubble");
        b.add_css_class(if m.mine { "mine" } else { "theirs" });
        if continues {
            b.add_css_class("continues");
        }
    }
    if show_name {
        let name = gtk4::Label::new(Some(&m.from));
        name.add_css_class("bubble-name");
        name.add_css_class(&format!("name-color-{}", color_index(&m.from)));
        name.set_xalign(0.0);
        b.append(&name);
    }
    let text = gtk4::Label::new(None);
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
    (outer.upcast(), tick)
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
