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
}

/// Set later (the picker belongs to the page around the composer).
type OpenEmoji = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

impl Composer {
    pub fn new(win: &impl IsA<gtk4::Window>) -> Self {
        let view = gtk4::TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(10)
            .bottom_margin(10)
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
        placeholder.set_valign(gtk4::Align::Start);
        // Same spot as the text cursor: the view's margins.
        placeholder.set_margin_start(14);
        placeholder.set_margin_top(10);
        placeholder.set_can_target(false);
        {
            let placeholder = placeholder.clone();
            buf.connect_changed(move |b| placeholder.set_visible(b.char_count() == 0));
        }

        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(180)
            .hexpand(true)
            .child(&view)
            .build();
        let widget = gtk4::Overlay::new();
        widget.set_child(Some(&scroll));
        widget.add_overlay(&placeholder);
        widget.add_css_class("chat-input-frame");
        widget.set_hexpand(true);

        let c = Self { widget, view, emoji: Default::default() };
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

    /// What Ctrl+; and "Emoji" in the context menu do.
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
