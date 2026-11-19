//! The message box, Telegram-style: formatting shows as you type (no
//! Markdown marks on screen). Select text and press Ctrl+B / Ctrl+I /
//! Ctrl+U / Ctrl+Shift+X / Ctrl+Shift+M / Ctrl+K, or right-click →
//! Formatting. On send the text becomes Markdown (see `chat::markdown`).

use gtk4::{gio, prelude::*};
use libadwaita::prelude::*;

use crate::tr;

/// Formatting a piece of text can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
}

impl Style {
    const ALL: [Style; 5] = [Style::Bold, Style::Italic, Style::Underline, Style::Strike, Style::Code];

    fn tag(self) -> &'static str {
        match self {
            Style::Bold => "bold",
            Style::Italic => "italic",
            Style::Underline => "underline",
            Style::Strike => "strike",
            Style::Code => "code",
        }
    }
}

/// Link tags are named `link:<url>`.
const LINK_PREFIX: &str = "link:";

#[derive(Clone)]
pub struct Composer {
    /// The framed text box (with its placeholder).
    pub widget: gtk4::Overlay,
    pub view: gtk4::TextView,
    /// Opens the app's emoji picker (Ctrl+; and the context menu).
    emoji: OpenEmoji,
    /// Gets pictures and files pasted with Ctrl+V.
    paste_files: PasteFiles,
    /// Who can be "@mentioned" here (set by the page, per conversation).
    mention_source: MentionSource,
}

type MentionSource = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn() -> Vec<String>>>>>;

type PasteFiles = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(Vec<std::path::PathBuf>)>>>>;

/// Tallest the message box grows before it scrolls.
const MAX_INPUT_HEIGHT: i32 = 180;

/// Set later (the picker belongs to the page around the composer).
type OpenEmoji = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

impl Composer {
    pub fn new(win: &impl IsA<gtk4::Window>) -> Self {
        let view = gtk4::TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .accepts_tab(false)
            .left_margin(14)
            .right_margin(14)
            .build();
        view.add_css_class("chat-input");
        // GTK's own emoji chooser is replaced by the app's (see on_emoji).
        view.set_input_hints(gtk4::InputHints::NO_EMOJI);
        view.update_property(&[gtk4::accessible::Property::Label(&tr!("Message"))]);
        let buf = view.buffer();
        let table = buf.tag_table();
        for style in Style::ALL {
            let tag = gtk4::TextTag::new(Some(style.tag()));
            match style {
                Style::Bold => tag.set_weight(700),
                Style::Italic => tag.set_style(gtk4::pango::Style::Italic),
                Style::Underline => tag.set_underline(gtk4::pango::Underline::Single),
                Style::Strike => tag.set_strikethrough(true),
                Style::Code => {
                    tag.set_family(Some("monospace"));
                    tag.set_background_rgba(Some(&gtk4::gdk::RGBA::new(0.5, 0.5, 0.5, 0.22)));
                }
            }
            table.add(&tag);
        }

        let placeholder = gtk4::Label::new(Some(&tr!("Message")));
        placeholder.add_css_class("dim-label");
        placeholder.set_halign(gtk4::Align::Start);
        // Same spot as the text cursor: the view's left margin, and both
        // centred vertically in the frame.
        placeholder.set_valign(gtk4::Align::Center);
        placeholder.set_margin_start(14);
        placeholder.set_can_target(false);
        {
            let placeholder = placeholder.clone();
            buf.connect_changed(move |b| placeholder.set_visible(b.char_count() == 0));
        }

        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(MAX_INPUT_HEIGHT)
            // The breathing room goes around the scroller: a text view's own
            // top/bottom margins get counted twice by it, which left the
            // single line sitting high in a too-tall box.
            .margin_top(10)
            .margin_bottom(10)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .child(&view)
            .build();
        let widget = gtk4::Overlay::new();
        widget.set_child(Some(&scroll));
        widget.add_overlay(&placeholder);
        widget.add_css_class("chat-input-frame");
        widget.set_hexpand(true);

        // A scrollbar needs ~58 px of height even when there's nothing to
        // scroll, which made a one-line box too tall: it's only turned on
        // once the text outgrows the box.
        {
            let (sc, view) = (scroll.clone(), view.clone());
            let fit = move || {
                let scroll = &sc;
                let (_, natural, _, _) = view.measure(gtk4::Orientation::Vertical, view.width().max(1));
                let policy = if natural > MAX_INPUT_HEIGHT { gtk4::PolicyType::Automatic } else { gtk4::PolicyType::Never };
                if scroll.vscrollbar_policy() != policy {
                    scroll.set_vscrollbar_policy(policy);
                }
            };
            buf.connect_changed(move |_| {
                let fit = fit.clone();
                gtk4::glib::idle_add_local_once(fit);
            });
            // While it all fits, nothing is scrolled away (the view would
            // otherwise keep the offset it had before the box grew).
            let sc = scroll.clone();
            scroll.vadjustment().connect_value_changed(move |adj| {
                if sc.vscrollbar_policy() == gtk4::PolicyType::Never && adj.value() != 0.0 {
                    adj.set_value(0.0);
                }
            });
        }
        let c = Self { widget, view, emoji: Default::default(), paste_files: Default::default(), mention_source: Default::default() };
        c.setup_paste();
        c.setup_mentions();
        c.setup_actions(win);
        c
    }

    /// Formatting actions (`fmt.*`), their shortcuts and the right-click
    /// "Formatting" submenu.
    fn setup_actions(&self, win: &impl IsA<gtk4::Window>) {
        let group = gio::SimpleActionGroup::new();
        for style in Style::ALL {
            let action = gio::SimpleAction::new(style.tag(), None);
            let c = self.clone();
            action.connect_activate(move |_, _| c.toggle(style));
            group.add_action(&action);
        }
        {
            let action = gio::SimpleAction::new("link", None);
            let (c, win) = (self.clone(), win.as_ref().clone());
            action.connect_activate(move |_, _| c.ask_link(&win));
            group.add_action(&action);
        }
        {
            let action = gio::SimpleAction::new("emoji", None);
            let emoji = self.emoji.clone();
            action.connect_activate(move |_, _| {
                if let Some(open) = emoji.borrow().as_ref() {
                    open();
                }
            });
            group.add_action(&action);
        }
        {
            let action = gio::SimpleAction::new("plain", None);
            let c = self.clone();
            action.connect_activate(move |_, _| c.clear_selection_styles());
            group.add_action(&action);
        }
        self.view.insert_action_group("fmt", Some(&group));

        let shortcuts = gtk4::ShortcutController::new();
        shortcuts.set_scope(gtk4::ShortcutScope::Local);
        for (accel, action) in [
            ("<Control>b", "fmt.bold"),
            ("<Control>i", "fmt.italic"),
            ("<Control>u", "fmt.underline"),
            ("<Control><Shift>x", "fmt.strike"),
            ("<Control><Shift>m", "fmt.code"),
            ("<Control>k", "fmt.link"),
            ("<Control><Shift>n", "fmt.plain"),
            ("<Control>semicolon", "fmt.emoji"),
            ("<Control>period", "fmt.emoji"),
        ] {
            shortcuts.add_shortcut(gtk4::Shortcut::new(
                gtk4::ShortcutTrigger::parse_string(accel),
                Some(gtk4::NamedAction::new(action)),
            ));
        }
        self.view.add_controller(shortcuts);

        let fmt = gio::Menu::new();
        for (label, action, accel) in [
            (tr!("Bold"), "fmt.bold", "<Control>b"),
            (tr!("Italic"), "fmt.italic", "<Control>i"),
            (tr!("Underline"), "fmt.underline", "<Control>u"),
            (tr!("Strikethrough"), "fmt.strike", "<Control><Shift>x"),
            (tr!("Monospace"), "fmt.code", "<Control><Shift>m"),
            (tr!("Create link"), "fmt.link", "<Control>k"),
            (tr!("Plain text"), "fmt.plain", "<Control><Shift>n"),
        ] {
            let item = gio::MenuItem::new(Some(&label), Some(action));
            item.set_attribute_value("accel", Some(&accel.to_variant()));
            fmt.append_item(&item);
        }
        let extra = gio::Menu::new();
        let emoji = gio::MenuItem::new(Some(&tr!("Emoji")), Some("fmt.emoji"));
        emoji.set_attribute_value("accel", Some(&"<Control>semicolon".to_variant()));
        extra.append_item(&emoji);
        extra.append_submenu(Some(&tr!("Formatting")), &fmt);
        self.view.set_extra_menu(Some(&extra));
    }

    fn selection(&self) -> Option<(gtk4::TextIter, gtk4::TextIter)> {
        self.view.buffer().selection_bounds()
    }

    /// Apply `style` to the selection, or remove it when the whole
    /// selection already has it (like Telegram).
    fn toggle(&self, style: Style) {
        let Some((start, end)) = self.selection() else { return };
        let buf = self.view.buffer();
        let Some(tag) = buf.tag_table().lookup(style.tag()) else { return };
        if covers(&start, &end, &tag) {
            buf.remove_tag(&tag, &start, &end);
        } else {
            if style == Style::Code {
                // Code is shown as typed: no other styles inside.
                self.clear_selection_styles();
            }
            buf.apply_tag(&tag, &start, &end);
        }
    }

    fn clear_selection_styles(&self) {
        let Some((start, end)) = self.selection() else { return };
        self.view.buffer().remove_all_tags(&start, &end);
    }

    /// Ctrl+K: turn the selection into a link.
    fn ask_link(&self, win: &gtk4::Window) {
        let Some((start, end)) = self.selection() else { return };
        let (s_off, e_off) = (start.offset(), end.offset());
        let dlg = libadwaita::MessageDialog::new(Some(win), Some(&tr!("Create link")), None);
        let entry = gtk4::Entry::builder().placeholder_text("https://").activates_default(true).build();
        dlg.set_extra_child(Some(&entry));
        dlg.add_response("cancel", &tr!("Cancel"));
        dlg.add_response("ok", &tr!("Create"));
        dlg.set_response_appearance("ok", libadwaita::ResponseAppearance::Suggested);
        dlg.set_default_response(Some("ok"));
        dlg.set_close_response("cancel");
        let (view, field) = (self.view.clone(), entry.clone());
        dlg.connect_response(None, move |d, r| {
            d.close();
            if r != "ok" {
                return;
            }
            let mut url = field.text().trim().to_string();
            if url.is_empty() {
                return;
            }
            if !url.contains("://") && !url.starts_with("mailto:") {
                url = format!("https://{url}");
            }
            let buf = view.buffer();
            let table = buf.tag_table();
            let name = format!("{LINK_PREFIX}{url}");
            let tag = table.lookup(&name).unwrap_or_else(|| {
                let t = gtk4::TextTag::new(Some(&name));
                t.set_underline(gtk4::pango::Underline::Single);
                t.set_foreground_rgba(Some(&gtk4::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0)));
                table.add(&t);
                t
            });
            let (s, e) = (buf.iter_at_offset(s_off), buf.iter_at_offset(e_off));
            buf.apply_tag(&tag, &s, &e);
            view.grab_focus();
        });
        dlg.present();
        entry.grab_focus();
    }

    /// Where pictures and files pasted with Ctrl+V go (sent).
    pub fn on_paste_files(&self, send: impl Fn(Vec<std::path::PathBuf>) + 'static) {
        *self.paste_files.borrow_mut() = Some(Box::new(send));
    }

    /// Ctrl+V with a picture (a screenshot…) or copied files in the
    /// clipboard sends them; text pastes as usual.
    fn setup_paste(&self) {
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let (view, hook) = (self.view.clone(), self.paste_files.clone());
        keys.connect_key_pressed(move |_, key, _, mods| {
            let ctrl_v = mods.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gtk4::gdk::Key::v | gtk4::gdk::Key::V);
            if !ctrl_v || hook.borrow().is_none() {
                return gtk4::glib::Propagation::Proceed;
            }
            let clipboard = view.clipboard();
            let formats = clipboard.formats();
            if formats.contains_type(gtk4::gdk::FileList::static_type()) {
                let hook = hook.clone();
                clipboard.read_value_async(
                    gtk4::gdk::FileList::static_type(),
                    gtk4::glib::Priority::DEFAULT,
                    gtk4::gio::Cancellable::NONE,
                    move |r| {
                        let paths: Vec<_> = r
                            .ok()
                            .and_then(|v| v.get::<gtk4::gdk::FileList>().ok())
                            .map(|l| l.files().iter().filter_map(|f| f.path()).collect())
                            .unwrap_or_default();
                        if !paths.is_empty() {
                            if let Some(send) = hook.borrow().as_ref() {
                                send(paths);
                            }
                        }
                    },
                );
                return gtk4::glib::Propagation::Stop;
            }
            if formats.contains_type(gtk4::gdk::Texture::static_type()) && !formats.contain_mime_type("text/plain") {
                let hook = hook.clone();
                clipboard.read_texture_async(gtk4::gio::Cancellable::NONE, move |r| {
                    let Ok(Some(texture)) = r else { return };
                    let stamp = gtk4::glib::DateTime::now_local()
                        .ok()
                        .and_then(|d| d.format("%Y-%m-%d_%H-%M-%S").ok())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let path = crate::chat::media_dir().join(format!("image-{stamp}.png"));
                    if texture.save_to_png(&path).is_ok() {
                        if let Some(send) = hook.borrow().as_ref() {
                            send(vec![path]);
                        }
                    }
                });
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });
        self.view.add_controller(keys);
    }

    /// What Ctrl+; and "Emoji" in the context menu do.
    /// Who may be mentioned with "@" (asked each time the list opens).
    pub fn on_mention_candidates(&self, source: impl Fn() -> Vec<String> + 'static) {
        *self.mention_source.borrow_mut() = Some(Box::new(source));
    }

    /// Typing "@" opens a list of people (filtered as you type); Enter, Tab
    /// or a click puts "@name " in, arrows move, Esc closes — like Telegram.
    fn setup_mentions(&self) {
        use gtk4::glib;
        use std::rc::Rc;
        let list = gtk4::ListBox::new();
        list.add_css_class("mention-list");
        list.set_selection_mode(gtk4::SelectionMode::Single);
        let pop = gtk4::Popover::builder()
            .child(&list)
            .has_arrow(false)
            .autohide(false)
            .position(gtk4::PositionType::Top)
            .build();
        pop.set_parent(&self.view);
        pop.add_css_class("menu");
        {
            let pop = pop.clone();
            self.view.connect_destroy(move |_| pop.unparent());
        }
        // The "@word" the cursor is in: (offset of "@", what follows it).
        let token = |view: &gtk4::TextView| -> Option<(i32, String)> {
            let buf = view.buffer();
            let cursor = buf.iter_at_mark(&buf.get_insert());
            let mut start = cursor;
            start.set_line_offset(0);
            let before = buf.text(&start, &cursor, false).to_string();
            let at = before.rfind('@')?;
            let typed = &before[at + 1..];
            if !typed.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return None;
            }
            if before[..at].chars().last().is_some_and(|p| !p.is_whitespace() && !"([{\"'".contains(p)) {
                return None;
            }
            let at_offset = cursor.offset() - typed.chars().count() as i32 - 1;
            Some((at_offset, typed.to_string()))
        };
        let token = Rc::new(token);
        let choose = {
            let (view, pop) = (self.view.clone(), pop.clone());
            let token = token.clone();
            Rc::new(move |name: &str| {
                pop.popdown();
                let Some((at, _)) = token(&view) else { return };
                let buf = view.buffer();
                let mut start = buf.iter_at_offset(at);
                let mut end = buf.iter_at_mark(&buf.get_insert());
                buf.delete(&mut start, &mut end);
                buf.insert_at_cursor(&format!("@{name} "));
                view.grab_focus();
            })
        };
        {
            let choose = choose.clone();
            list.connect_row_activated(move |_, row| choose(&row.widget_name()));
        }
        let update = {
            let (view, pop, list, source) = (self.view.clone(), pop.clone(), list.clone(), self.mention_source.clone());
            let token = token.clone();
            move || {
                let found = token(&view).and_then(|(_, typed)| {
                    let people = source.borrow().as_ref().map(|f| f()).unwrap_or_default();
                    let typed = typed.to_lowercase();
                    let mut hits: Vec<String> = people.into_iter().filter(|p| p.to_lowercase().starts_with(&typed)).collect();
                    hits.sort();
                    hits.truncate(6);
                    (!hits.is_empty()).then_some(hits)
                });
                let Some(hits) = found else {
                    pop.popdown();
                    return;
                };
                while let Some(c) = list.first_child() {
                    list.remove(&c);
                }
                for name in &hits {
                    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                    let avatar = libadwaita::Avatar::new(24, Some(name), true);
                    crate::avatars::bind(&avatar, name);
                    line.append(&avatar);
                    line.append(&gtk4::Label::new(Some(&format!("@{name}"))));
                    let row = gtk4::ListBoxRow::builder().child(&line).build();
                    row.set_widget_name(name);
                    list.append(&row);
                }
                list.select_row(list.row_at_index(0).as_ref());
                // Point at the cursor.
                let buf = view.buffer();
                let rect = view.iter_location(&buf.iter_at_mark(&buf.get_insert()));
                let (x, y) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, rect.x(), rect.y());
                pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x, y, 1, rect.height())));
                if !pop.is_visible() {
                    pop.popup();
                }
            }
        };
        let update = Rc::new(update);
        {
            let update = update.clone();
            self.view.buffer().connect_changed(move |_| {
                let update = update.clone();
                glib::idle_add_local_once(move || update());
            });
        }
        {
            let pop = pop.clone();
            self.view.connect_has_focus_notify(move |v| {
                if !v.has_focus() {
                    pop.popdown();
                }
            });
        }
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let (pop, list) = (pop.clone(), list.clone());
            keys.connect_key_pressed(move |_, key, _, _| {
                use gtk4::gdk::Key;
                if !pop.is_visible() {
                    return glib::Propagation::Proceed;
                }
                let index = list.selected_row().map(|r| r.index()).unwrap_or(0);
                match key {
                    Key::Up | Key::Down => {
                        let next = if key == Key::Up { index - 1 } else { index + 1 };
                        if let Some(r) = list.row_at_index(next) {
                            list.select_row(Some(&r));
                        }
                        glib::Propagation::Stop
                    }
                    Key::Return | Key::KP_Enter | Key::Tab => {
                        if let Some(r) = list.selected_row() {
                            choose(&r.widget_name());
                        }
                        glib::Propagation::Stop
                    }
                    Key::Escape => {
                        pop.popdown();
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
        }
        self.view.add_controller(keys);
    }

    pub fn on_emoji(&self, open: impl Fn() + 'static) {
        *self.emoji.borrow_mut() = Some(Box::new(open));
    }

    pub fn insert(&self, text: &str) {
        let buf = self.view.buffer();
        buf.delete_selection(true, true);
        buf.insert_at_cursor(text);
        self.view.grab_focus();
    }

    pub fn clear(&self) {
        self.view.buffer().set_text("");
    }

    /// Put a message's Markdown in the box, formatted (to edit it).
    pub fn set_markdown(&self, md: &str) {
        let buf = self.view.buffer();
        buf.set_text("");
        let table = buf.tag_table();
        for run in from_markdown(md) {
            let start = buf.end_iter().offset();
            let mut end_iter = buf.end_iter();
            buf.insert(&mut end_iter, &run.text);
            let (s, e) = (buf.iter_at_offset(start), buf.end_iter());
            let mut names: Vec<String> = Vec::new();
            for (on, style) in [
                (run.bold, Style::Bold),
                (run.italic, Style::Italic),
                (run.underline, Style::Underline),
                (run.strike, Style::Strike),
                (run.code, Style::Code),
            ] {
                if on {
                    names.push(style.tag().to_string());
                }
            }
            for name in names {
                if let Some(tag) = table.lookup(&name) {
                    buf.apply_tag(&tag, &s, &e);
                }
            }
            if let Some(url) = &run.link {
                let name = format!("{LINK_PREFIX}{url}");
                let tag = table.lookup(&name).unwrap_or_else(|| {
                    let t = gtk4::TextTag::new(Some(&name));
                    t.set_underline(gtk4::pango::Underline::Single);
                    t.set_foreground_rgba(Some(&gtk4::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0)));
                    table.add(&t);
                    t
                });
                buf.apply_tag(&tag, &s, &e);
            }
        }
        buf.place_cursor(&buf.end_iter());
        self.view.grab_focus();
    }

    /// The message as Markdown.
    pub fn markdown(&self) -> String {
        let buf = self.view.buffer();
        let mut runs: Vec<Run> = Vec::new();
        let mut it = buf.start_iter();
        let end = buf.end_iter();
        while it < end {
            let mut next = it;
            if !next.forward_to_tag_toggle(None::<&gtk4::TextTag>) || next > end {
                next = end;
            }
            let text = buf.text(&it, &next, false).to_string();
            let tags = it.tags();
            let names: Vec<String> = tags.iter().filter_map(|t| t.name().map(|n| n.to_string())).collect();
            runs.push(Run::from_names(text, &names));
            it = next;
        }
        to_markdown(&runs)
    }
}

/// Does every character between `start` and `end` have `tag`?
fn covers(start: &gtk4::TextIter, end: &gtk4::TextIter, tag: &gtk4::TextTag) -> bool {
    let mut it = *start;
    while it < *end {
        if !it.has_tag(tag) {
            return false;
        }
        if !it.forward_char() {
            break;
        }
    }
    true
}

/// A stretch of text with the same formatting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Run {
    text: String,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    code: bool,
    link: Option<String>,
}

impl Run {
    fn from_names(text: String, names: &[String]) -> Self {
        let has = |n: &str| names.iter().any(|x| x == n);
        Run {
            text,
            bold: has("bold"),
            italic: has("italic"),
            underline: has("underline"),
            strike: has("strike"),
            code: has("code"),
            link: names.iter().find_map(|n| n.strip_prefix(LINK_PREFIX).map(str::to_string)),
        }
    }
}

/// Formatted runs for a message's Markdown (what `to_markdown` writes).
fn from_markdown(md: &str) -> Vec<Run> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let (mut bold, mut italic, mut underline, mut strike, mut code_block) = (0u32, 0u32, false, 0u32, false);
    let mut link: Option<String> = None;
    let mut runs: Vec<Run> = Vec::new();
    let push = |runs: &mut Vec<Run>, text: &str, r: Run| {
        if text.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some(last) if Run { text: String::new(), ..last.clone() } == Run { text: String::new(), ..r.clone() } => {
                last.text.push_str(text)
            }
            _ => runs.push(Run { text: text.to_string(), ..r }),
        }
    };
    let mut paragraphs = 0;
    for event in Parser::new_ext(md, opts) {
        let style = Run {
            text: String::new(),
            bold: bold > 0,
            italic: italic > 0,
            underline,
            strike: strike > 0,
            code: code_block,
            link: link.clone(),
        };
        match event {
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => strike += 1,
            Event::End(TagEnd::Strikethrough) => strike = strike.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => link = None,
            Event::Start(Tag::CodeBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
                push(&mut runs, "\n", Run::default());
            }
            Event::Start(Tag::Paragraph) => {
                if paragraphs > 0 {
                    push(&mut runs, "\n\n", Run::default());
                }
                paragraphs += 1;
            }
            Event::Start(Tag::Item) => push(&mut runs, "- ", Run::default()),
            Event::End(TagEnd::Item) => push(&mut runs, "\n", Run::default()),
            Event::Text(t) => push(&mut runs, &t, style),
            Event::Code(t) => push(&mut runs, &t, Run { code: true, ..style }),
            Event::SoftBreak | Event::HardBreak => push(&mut runs, "\n", Run::default()),
            Event::InlineHtml(h) | Event::Html(h) => match h.trim() {
                "<u>" => underline = true,
                "</u>" => underline = false,
                other => push(&mut runs, other, style),
            },
            _ => {}
        }
    }
    // No trailing blank lines.
    while let Some(last) = runs.last_mut() {
        let trimmed = last.text.trim_end_matches('\n').len();
        if trimmed == 0 {
            runs.pop();
            continue;
        }
        last.text.truncate(trimmed);
        break;
    }
    runs
}

/// Markdown for the formatted runs. Marks go around the words, never
/// around the spaces next to them (Markdown wouldn't see them then).
fn to_markdown(runs: &[Run]) -> String {
    let mut out = String::new();
    for r in runs {
        if r.text.is_empty() {
            continue;
        }
        let plain = !(r.bold || r.italic || r.underline || r.strike || r.code) && r.link.is_none();
        if plain {
            out.push_str(&r.text);
            continue;
        }
        // Format line by line (a mark can't span lines in Markdown), keeping
        // leading/trailing spaces outside the marks.
        let lines: Vec<&str> = r.text.split('\n').collect();
        if r.code && lines.len() > 1 {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
            out.push_str(r.text.trim_end_matches('\n'));
            out.push_str("\n```\n");
            continue;
        }
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let core = line.trim();
            if core.is_empty() {
                out.push_str(line);
                continue;
            }
            let lead = &line[..line.len() - line.trim_start().len()];
            let trail = &line[line.trim_end().len()..];
            let mut s = if r.code {
                let ticks = if core.contains('`') { "``" } else { "`" };
                format!("{ticks}{core}{ticks}")
            } else {
                core.to_string()
            };
            if !r.code {
                if r.strike {
                    s = format!("~~{s}~~");
                }
                if r.underline {
                    s = format!("<u>{s}</u>");
                }
                if r.italic {
                    s = format!("_{s}_");
                }
                if r.bold {
                    s = format!("**{s}**");
                }
            }
            if let Some(url) = &r.link {
                s = format!("[{s}]({url})");
            }
            out.push_str(lead);
            out.push_str(&s);
            out.push_str(trail);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::markdown::to_pango;

    /// What's loaded for editing comes back out as the same Markdown.
    #[test]
    fn markdown_round_trips_through_the_box() {
        for md in [
            "plain text",
            "**bold** and _italic_ and ~~gone~~",
            "a `code` word",
            "see [the site](https://biglinux.com.br) now",
            "<u>under</u> line",
            "two\nlines",
        ] {
            assert_eq!(to_markdown(&from_markdown(md)), md, "{md}");
        }
    }

    fn run(text: &str) -> Run {
        Run { text: text.into(), ..Default::default() }
    }

    #[test]
    fn formatting_becomes_markdown_that_reads_back() {
        let runs = vec![
            run("Oi "),
            Run { bold: true, ..run("todo mundo ") },
            run("e "),
            Run { italic: true, underline: true, ..run("você") },
            run(", veja "),
            Run { code: true, ..run("sudo pacman -Syu") },
            run(" e "),
            Run { link: Some("https://biglinux.com.br".into()), ..run("o site") },
            Run { strike: true, ..run(" nada") },
        ];
        let md = to_markdown(&runs);
        assert_eq!(
            md,
            "Oi **todo mundo** e _<u>você</u>_, veja `sudo pacman -Syu` e [o site](https://biglinux.com.br) ~~nada~~"
        );
        let pango = to_pango(&md);
        assert!(pango.contains("<b>todo mundo</b>"), "{pango}");
        assert!(pango.contains("<i><u>você</u></i>"), "{pango}");
        assert!(pango.contains("<s>nada</s>"), "{pango}");
        assert!(pango.contains("<a href=\"https://biglinux.com.br\">o site</a>"), "{pango}");
        assert!(crate::chat::markdown::markup_ok(&pango));
    }

    #[test]
    fn multi_line_code_is_a_block() {
        let md = to_markdown(&[run("veja:\n"), Run { code: true, ..run("a = 1\nb = 2") }]);
        assert_eq!(md, "veja:\n```\na = 1\nb = 2\n```\n");
        let md = to_markdown(&[Run { bold: true, ..run("linha 1\nlinha 2") }]);
        assert_eq!(md, "**linha 1**\n**linha 2**");
    }
}
