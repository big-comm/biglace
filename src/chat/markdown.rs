//! Chat messages are written in Markdown and shown with GTK's own text
//! markup (Pango): **bold**, *italic*, ~~strike~~, `code`, code blocks,
//! links, lists, quotes. Everything the sender typed is escaped first — a
//! message can format text, never inject markup of its own.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Pango markup for a Markdown message.
pub fn to_pango(text: &str) -> String {
    // Underline (`<u>…</u>`, what the message box writes) unless the way it
    // was typed would make the markup invalid; then it's shown as typed.
    let with_underline = render(text, true);
    if markup_ok(&with_underline) {
        with_underline
    } else {
        render(text, false)
    }
}

/// Links (`<a href>`) are understood by GTK labels, not by Pango itself:
/// set them aside to check the rest.
fn without_links(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(i) = rest.find("<a href=\"") {
        out.push_str(&rest[..i]);
        match rest[i..].find('>') {
            Some(j) => rest = &rest[i + j + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out.replace("</a>", "")
}

/// Is `markup` valid label markup?
pub fn markup_ok(markup: &str) -> bool {
    gtk4::pango::parse_markup(&without_links(markup), '\0').is_ok()
}

/// A message as plain text (formatting dropped), for previews.
pub fn plain_text(text: &str) -> String {
    gtk4::pango::parse_markup(&without_links(&to_pango(text)), '\0')
        .map(|(_, t, _)| t.to_string())
        .unwrap_or_else(|_| text.to_string())
}

fn render(text: &str, underline: bool) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out = String::new();
    // Ordered list counters (None: bullet list).
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut pending_break = false;
    let mut quote_depth = 0usize;
    // Inside a link nothing else becomes one (no @mentions there).
    let mut in_link = 0usize;

    let paragraph_break = |out: &mut String| {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
    };

    for event in Parser::new_ext(text, opts) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if pending_break && lists.is_empty() {
                        paragraph_break(&mut out);
                    }
                    if quote_depth > 0 {
                        out.push_str("<span alpha=\"70%\">▎ ");
                    }
                }
                Tag::Heading { level, .. } => {
                    if pending_break {
                        paragraph_break(&mut out);
                    }
                    let size = match level {
                        HeadingLevel::H1 => "x-large",
                        HeadingLevel::H2 => "large",
                        _ => "medium",
                    };
                    out.push_str(&format!("<span size=\"{size}\" weight=\"bold\">"));
                }
                Tag::BlockQuote(_) => {
                    if pending_break {
                        paragraph_break(&mut out);
                        pending_break = false;
                    }
                    quote_depth += 1;
                }
                Tag::CodeBlock(kind) => {
                    if pending_break {
                        paragraph_break(&mut out);
                    }
                    let _ = kind;
                    out.push_str(CODE_OPEN);
                }
                Tag::List(start) => {
                    if pending_break && lists.is_empty() {
                        paragraph_break(&mut out);
                    }
                    lists.push(start);
                }
                Tag::Item => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    let indent = "    ".repeat(lists.len().saturating_sub(1));
                    out.push_str(&indent);
                    match lists.last_mut() {
                        Some(Some(n)) => {
                            out.push_str(&format!("{n}. "));
                            *n += 1;
                        }
                        _ => out.push_str("• "),
                    }
                }
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    in_link += 1;
                    // Only web and mail links become clickable.
                    let url = dest_url.trim();
                    if url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:") {
                        out.push_str(&format!("<a href=\"{}\">", escape(url)));
                    } else {
                        out.push_str("<u>");
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if quote_depth > 0 {
                        out.push_str("</span>");
                    }
                    pending_break = true;
                }
                TagEnd::Heading(_) => {
                    out.push_str("</span>");
                    pending_break = true;
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    pending_break = true;
                }
                TagEnd::CodeBlock => {
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str(CODE_CLOSE);
                    pending_break = true;
                }
                TagEnd::List(_) => {
                    lists.pop();
                    pending_break = true;
                }
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link => {
                    in_link = in_link.saturating_sub(1);
                    // Close what Start opened (a or u): find the last opener.
                    let a = out.rfind("<a href=");
                    let u = out.rfind("<u>");
                    if a > u {
                        out.push_str("</a>");
                    } else {
                        out.push_str("</u>");
                    }
                }
                _ => {}
            },
            Event::Text(t) if in_link == 0 => out.push_str(&with_mentions(&t)),
            Event::Text(t) => out.push_str(&escape(&t)),
            Event::Code(t) => {
                out.push_str(CODE_OPEN);
                out.push_str(&escape(&t));
                out.push_str(CODE_CLOSE);
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            // Underline is the one tag understood; any other HTML is shown
            // as typed, never interpreted.
            Event::InlineHtml(t) if underline && (&*t == "<u>" || &*t == "</u>") => out.push_str(&t),
            Event::Html(t) | Event::InlineHtml(t) => out.push_str(&escape(&t)),
            Event::Rule => {
                paragraph_break(&mut out);
                out.push_str("──────────");
                pending_break = true;
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// Code: monospace on a faint highlight that reads on light and dark
/// bubbles alike.
const CODE_OPEN: &str = "<span font_family=\"monospace\" bgcolor=\"#808080\" bgalpha=\"22%\">";
const CODE_CLOSE: &str = "</span>";

/// Emoticons typed as text, and the emoji they become (like Telegram).
const EMOTICONS: &[(&str, &str)] = &[
    (":-)", "🙂"), (":)", "🙂"), ("(:", "🙂"),
    (":-D", "😃"), (":D", "😃"), ("xD", "😆"), ("XD", "😆"),
    (";-)", "😉"), (";)", "😉"),
    (":-(", "🙁"), (":(", "🙁"),
    (":'(", "😢"), (":'-(", "😢"),
    (":-P", "😛"), (":P", "😛"), (":-p", "😛"), (":p", "😛"),
    (":-O", "😮"), (":O", "😮"), (":-o", "😮"), (":o", "😮"),
    (":-*", "😘"), (":*", "😘"),
    (":-|", "😐"), (":|", "😐"),
    (":-/", "😕"), (":/", "😕"),
    ("B-)", "😎"), ("8-)", "😎"),
    (">:(", "😠"), (">:-(", "😠"),
    ("<3", "❤️"), ("</3", "💔"),
    ("o/", "👋"), ("\\o/", "🙌"),
    (":+1:", "👍"), (":-1:", "👎"),
];

/// Turn emoticons standing alone (between spaces, or at the ends) into
/// emoji: ":-)" → 🙂. Nothing inside words, links or code (`…`, ```…```).
pub fn emoticons_to_emoji(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_block = !in_block;
            out.push_str(line);
            continue;
        }
        if in_block {
            out.push_str(line);
            continue;
        }
        let mut in_code = false;
        for (k, word) in line.split(' ').enumerate() {
            if k > 0 {
                out.push(' ');
            }
            let ticks = word.matches('`').count();
            let swap = !in_code && ticks == 0;
            if ticks % 2 == 1 {
                in_code = !in_code;
            }
            match EMOTICONS.iter().find(|(e, _)| swap && *e == word) {
                Some((_, emoji)) => out.push_str(emoji),
                None => out.push_str(word),
            }
        }
    }
    out
}

/// A character that can be part of a name after "@" (the panel's user
/// names: letters, digits, "-", "_").
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// The "@name"s in `text`: (byte range of "@name", name). Not in the
/// middle of a word (an e-mail address isn't a mention).
pub fn mentions(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if c == '@' && !prev.is_some_and(|p| p.is_alphanumeric() || p == '_' || p == '-' || p == '.') {
            let start = i + 1;
            let mut end = start;
            while let Some(&(j, d)) = iter.peek() {
                if !name_char(d) {
                    break;
                }
                end = j + d.len_utf8();
                iter.next();
            }
            // Trailing "-"/"_" are punctuation, not the name's.
            let name = text[start..end].trim_end_matches(['-', '_']);
            if !name.is_empty() && name.len() <= 64 {
                out.push((i..start + name.len(), name.to_string()));
            }
            prev = text[..end].chars().last();
            continue;
        }
        prev = Some(c);
    }
    out
}

/// Does `text` mention `user`?
pub fn mentions_user(text: &str, user: &str) -> bool {
    mentions(text).iter().any(|(_, n)| n.eq_ignore_ascii_case(user))
}

/// Escaped text with each "@name" as a link (`mention:name`): shown like
/// a link, a click opens the conversation with that person.
fn with_mentions(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut at = 0;
    for (range, name) in mentions(t) {
        out.push_str(&escape(&t[at..range.start]));
        out.push_str(&format!("<a href=\"mention:{}\"><b>@{}</b></a>", escape(&name), escape(&name)));
        at = range.end;
    }
    out.push_str(&escape(&t[at..]));
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Only emoji (at most a few, like Telegram): shown big, without a bubble.
pub fn is_jumbo_emoji(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let mut count = 0;
    for c in t.chars() {
        if c.is_whitespace() || c == '\u{200d}' || c == '\u{fe0f}' || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c) {
            continue;
        }
        if !is_emoji(c) {
            return false;
        }
        count += 1;
    }
    (1..=3).contains(&count)
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x1F000..=0x1F2FF | 0x2B00..=0x2BFF | 0x2190..=0x21FF | 0x2300..=0x23FF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_markdown() {
        assert_eq!(
            to_pango("**oi** *tudo* ~~bem~~ `x`"),
            format!("<b>oi</b> <i>tudo</i> <s>bem</s> {CODE_OPEN}x{CODE_CLOSE}")
        );
        assert_eq!(to_pango("- um\n- dois"), "• um\n• dois");
        assert_eq!(to_pango("1. a\n2. b"), "1. a\n2. b");
        assert_eq!(to_pango("```\nfn main() {}\n```"), format!("{CODE_OPEN}fn main() {{}}{CODE_CLOSE}"));
        assert_eq!(to_pango("linha 1\nlinha 2"), "linha 1\nlinha 2");
        assert_eq!(to_pango("par 1\n\npar 2"), "par 1\n\npar 2");
        assert_eq!(
            to_pango("[site](https://biglinux.com.br)"),
            "<a href=\"https://biglinux.com.br\">site</a>"
        );
    }

    /// Whatever is typed, the result is valid markup that formats only what
    /// Markdown asked for.
    #[test]
    fn nothing_typed_becomes_markup() {
        assert_eq!(to_pango("<b>x</b> & <span>"), "&lt;b&gt;x&lt;/b&gt; &amp; &lt;span&gt;");
        assert_eq!(to_pango("`<i>`"), format!("{CODE_OPEN}&lt;i&gt;{CODE_CLOSE}"));
        // Links only for the web or mail; anything else is just underlined.
        assert_eq!(to_pango("[x](javascript:alert(1))"), "<u>x</u>");
        assert_eq!(to_pango("[x](file:///etc/passwd)"), "<u>x</u>");
        // Quotes in a URL can't break out of the attribute.
        assert!(to_pango("[x](https://a.b/\"onclick)").contains("&quot;"));
        for text in [
            "*a **b* c**",
            "[a](https://x",
            "> q\n>> qq",
            "~~~\n<x>",
            "# t\n- a\n  - b\n\n1. c",
            "[**a**](https://x.y) <u>b</u>",
        ] {
            let m = to_pango(text);
            assert!(markup_ok(&m), "{text:?} → {m:?}");
        }
    }

    #[test]
    fn underline_only_when_well_formed() {
        assert_eq!(to_pango("<u>x</u>"), "<u>x</u>");
        assert_eq!(to_pango("<u>x"), "&lt;u&gt;x");
        assert_eq!(to_pango("**a<u>b** c</u>"), "<b>a&lt;u&gt;b</b> c&lt;/u&gt;");
        assert_eq!(to_pango("<U>x</U>"), "&lt;U&gt;x&lt;/U&gt;");
    }

    #[test]
    fn jumbo_emoji() {
        assert!(is_jumbo_emoji("😀"));
        assert!(is_jumbo_emoji("👍🏽 🎉"));
        assert!(is_jumbo_emoji("❤️"));
        assert!(!is_jumbo_emoji("oi 😀"));
        assert!(!is_jumbo_emoji("😀😀😀😀"));
        assert!(!is_jumbo_emoji(""));
    }

    #[test]
    fn mentions_are_found_and_linked() {
        let m = mentions("oi @bruno e @tales-2, veja (@ana). mail a@b.com @ x @-");
        let names: Vec<_> = m.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["bruno", "tales-2", "ana"]);
        assert!(mentions_user("valeu @Bruno!", "bruno"));
        assert!(!mentions_user("bruno@casa", "casa"));
        let p = to_pango("fala @bruno, **ok**");
        assert!(p.contains("<a href=\"mention:bruno\"><b>@bruno</b></a>"), "{p}");
        assert!(markup_ok(&p));
        // Not inside links or code.
        assert!(!to_pango("[@bruno](https://x.y)").contains("mention:"));
        assert!(!to_pango("`@bruno`").contains("mention:"));
    }

    #[test]
    fn emoticons_become_emoji() {
        assert_eq!(emoticons_to_emoji("oi :-) tudo bem? :D"), "oi 🙂 tudo bem? 😃");
        assert_eq!(emoticons_to_emoji(":)"), "🙂");
        assert_eq!(emoticons_to_emoji("te amo <3"), "te amo ❤️");
        // Not inside words, links or code.
        assert_eq!(emoticons_to_emoji("http://x.y/:)"), "http://x.y/:)");
        assert_eq!(emoticons_to_emoji("use `:)` aqui"), "use `:)` aqui");
        assert_eq!(emoticons_to_emoji("```\n:)\n```"), "```\n:)\n```");
        assert_eq!(emoticons_to_emoji("12:30 :Ponto"), "12:30 :Ponto");
    }
}
