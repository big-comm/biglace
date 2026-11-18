//! Emoji picker, Telegram-style: big colour emoji in a grid, search in the
//! user's language, recently used first, category tabs at the bottom. The
//! emoji list and its translations are the ones GTK ships.

use gtk4::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use crate::tr;

const RECENT_FILE: &str = "emoji-recent";
const RECENT_MAX: usize = 32;
const PER_LINE: u32 = 9;

#[derive(Debug, Clone)]
struct Emoji {
    text: String,
    /// English name, then the translated one and keywords (lowercase), for
    /// search.
    search: String,
    section: usize,
}

/// Sections in order: (first emoji's English name in GTK's list, icon, title).
fn sections() -> [(&'static str, &'static str, String); 8] {
    [
        ("grinning face", "emoji-people-symbolic", tr!("Smileys & people")),
        ("monkey face", "emoji-nature-symbolic", tr!("Animals & nature")),
        ("grapes", "emoji-food-symbolic", tr!("Food & drink")),
        ("globe showing Europe-Africa", "emoji-travel-symbolic", tr!("Travel & places")),
        ("jack-o-lantern", "emoji-activities-symbolic", tr!("Activities")),
        ("muted speaker", "emoji-objects-symbolic", tr!("Objects")),
        ("ATM sign", "emoji-symbols-symbolic", tr!("Symbols")),
        ("chequered flag", "emoji-flags-symbolic", tr!("Flags")),
    ]
}

type Raw = Vec<(Vec<u32>, String, String, Vec<String>)>;

fn parse(bytes: &glib::Bytes) -> Raw {
    let Some(ty) = glib::VariantTy::new("a(aussasu)").ok() else { return vec![] };
    let v = glib::Variant::from_bytes_with_type(bytes, ty);
    (0..v.n_children())
        .filter_map(|i| {
            let e = v.child_value(i);
            Some((
                e.child_value(0).get::<Vec<u32>>()?,
                e.child_value(1).get::<String>()?,
                e.child_value(2).get::<String>()?,
                e.child_value(3).get::<Vec<String>>()?,
            ))
        })
        .collect()
}

/// GTK's emoji list, with names and keywords in our language when GTK has
/// them.
fn all() -> &'static Vec<Emoji> {
    static ALL: OnceLock<Vec<Emoji>> = OnceLock::new();
    ALL.get_or_init(|| {
        let Ok(en) = gio::resources_lookup_data("/org/gtk/libgtk/emoji/en.data", gio::ResourceLookupFlags::NONE)
        else {
            return vec![];
        };
        let en = parse(&en);
        // Translations: /usr/share/gtk-4.0/emoji/<lang>.gresource.
        let lang: String = glib::language_names()
            .first()
            .map(|l| l.split(['_', '.', '@']).next().unwrap_or("").to_string())
            .unwrap_or_default();
        let mut local = std::collections::HashMap::<Vec<u32>, (String, Vec<String>)>::new();
        if !lang.is_empty() && lang != "en" && lang != "C" {
            for dir in glib::system_data_dirs() {
                let file = dir.join("gtk-4.0").join("emoji").join(format!("{lang}.gresource"));
                if let Ok(res) = gio::Resource::load(&file) {
                    if let Ok(b) = res.lookup_data(&format!("/org/gtk/libgtk/emoji/{lang}.data"), gio::ResourceLookupFlags::NONE) {
                        for (codes, _, name, keys) in parse(&b) {
                            local.insert(codes, (name, keys));
                        }
                        break;
                    }
                }
            }
        }
        let starts: Vec<&str> = sections().iter().map(|s| s.0).collect();
        let mut section = 0;
        en.into_iter()
            .filter_map(|(codes, name, _, keys)| {
                if let Some(i) = starts.iter().position(|s| *s == name) {
                    section = i;
                }
                // 0x1F3FB marks "has skin tones": show the base emoji.
                // (0 pads some entries.)
                let text: String = codes
                    .iter()
                    .filter(|c| **c != 0x1F3FB && **c != 0)
                    .filter_map(|c| char::from_u32(*c))
                    .collect();
                if text.is_empty() {
                    return None;
                }
                let mut search = format!("{name} {}", keys.join(" "));
                if let Some((lname, lkeys)) = local.get(&codes) {
                    search.push(' ');
                    search.push_str(lname);
                    search.push(' ');
                    search.push_str(&lkeys.join(" "));
                }
                Some(Emoji { text, search: fold(&search), section })
            })
            .collect()
    })
}

/// Lowercase without accents: "Coração" and "coracao" match.
fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).collect::<String>().to_lowercase()
}

fn load_recent() -> Vec<String> {
    std::fs::read_to_string(crate::config::private_file(RECENT_FILE))
        .map(|s| s.lines().filter(|l| !l.is_empty()).take(RECENT_MAX).map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_recent(list: &[String]) {
    let _ = crate::config::write_private_file(RECENT_FILE, list.join("\n").as_bytes());
}

/// A popover to attach to a button; `on_pick` gets each chosen emoji.
pub fn build(on_pick: impl Fn(&str) + 'static) -> gtk4::Popover {
    let popover = gtk4::Popover::new();
    popover.add_css_class("emoji-picker");
    popover.set_has_arrow(false);
    let built = Rc::new(std::cell::Cell::new(false));
    let on_pick: Rc<dyn Fn(&str)> = Rc::new(on_pick);
    {
        let (built, on_pick) = (built.clone(), on_pick.clone());
        // Built on first open: ~2000 emoji don't slow the app's start.
        popover.connect_show(move |p| {
            if !built.replace(true) {
                p.set_child(Some(&content(p, on_pick.clone())));
            }
        });
    }
    popover
}

fn grid() -> gtk4::FlowBox {
    let g = gtk4::FlowBox::builder()
        .homogeneous(true)
        .selection_mode(gtk4::SelectionMode::None)
        .min_children_per_line(PER_LINE)
        .max_children_per_line(PER_LINE)
        .activate_on_single_click(true)
        .row_spacing(0)
        .column_spacing(0)
        .build();
    g.add_css_class("emoji-grid");
    g
}

fn cell(text: &str) -> gtk4::FlowBoxChild {
    let l = gtk4::Label::new(Some(text));
    l.add_css_class("emoji-cell");
    let c = gtk4::FlowBoxChild::new();
    c.set_child(Some(&l));
    c.set_widget_name(text);
    c
}

fn content(popover: &gtk4::Popover, on_pick: Rc<dyn Fn(&str)>) -> gtk4::Box {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    outer.set_size_request(380, 420);
    let search = gtk4::SearchEntry::builder().placeholder_text(tr!("Search emoji")).build();
    search.set_margin_start(8);
    search.set_margin_end(8);
    search.set_margin_top(8);
    outer.append(&search);

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    body.set_margin_start(6);
    body.set_margin_end(6);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&body)
        .build();
    outer.append(&scroll);

    let recent = Rc::new(RefCell::new(load_recent()));
    let recent_title = section_title(&tr!("Recently used"));
    let recent_grid = grid();
    body.append(&recent_title);
    body.append(&recent_grid);

    let results_title = section_title(&tr!("Search results"));
    let results = grid();
    results_title.set_visible(false);
    results.set_visible(false);
    body.append(&results_title);
    body.append(&results);

    let mut headers: Vec<gtk4::Label> = Vec::new();
    let mut grids: Vec<gtk4::FlowBox> = Vec::new();
    for (i, (_, _, title)) in sections().iter().enumerate() {
        let h = section_title(title);
        let g = grid();
        for e in all().iter().filter(|e| e.section == i) {
            g.append(&cell(&e.text));
        }
        body.append(&h);
        body.append(&g);
        headers.push(h);
        grids.push(g);
    }

    let fill_recent = {
        let (recent_grid, recent_title, search) = (recent_grid.clone(), recent_title.clone(), search.clone());
        move |list: &[String]| {
            while let Some(c) = recent_grid.first_child() {
                recent_grid.remove(&c);
            }
            for e in list {
                recent_grid.append(&cell(e));
            }
            // Hidden while searching (the results stand alone).
            let show = !list.is_empty() && search.text().is_empty();
            recent_title.set_visible(show);
            recent_grid.set_visible(show);
        }
    };
    fill_recent(&recent.borrow());

    // Picking: from any grid.
    let pick = {
        let (recent, on_pick, popover) = (recent.clone(), on_pick.clone(), popover.clone());
        let fill_recent = fill_recent.clone();
        Rc::new(move |text: &str| {
            on_pick(text);
            let mut r = recent.borrow_mut();
            r.retain(|x| x != text);
            r.insert(0, text.to_string());
            r.truncate(RECENT_MAX);
            save_recent(&r);
            let snapshot = r.clone();
            drop(r);
            // Refresh "recent" after this click is handled.
            let fill_recent = fill_recent.clone();
            glib::idle_add_local_once(move || fill_recent(&snapshot));
            let _ = &popover;
        })
    };
    for g in grids.iter().chain([&recent_grid, &results]) {
        let pick = pick.clone();
        g.connect_child_activated(move |_, c| pick(&c.widget_name()));
    }

    // Search: one result grid instead of the sections.
    {
        let (body_parts, results, results_title, recent_title, recent_grid) = (
            headers.iter().cloned().zip(grids.iter().cloned()).collect::<Vec<_>>(),
            results.clone(),
            results_title.clone(),
            recent_title.clone(),
            recent_grid.clone(),
        );
        let recent = recent.clone();
        search.connect_search_changed(move |s| {
            let q = fold(s.text().trim());
            let searching = !q.is_empty();
            for (h, g) in &body_parts {
                h.set_visible(!searching);
                g.set_visible(!searching);
            }
            let has_recent = !recent.borrow().is_empty();
            recent_title.set_visible(!searching && has_recent);
            recent_grid.set_visible(!searching && has_recent);
            results_title.set_visible(searching);
            results.set_visible(searching);
            while let Some(c) = results.first_child() {
                results.remove(&c);
            }
            if searching {
                let words: Vec<&str> = q.split_whitespace().collect();
                for e in all().iter().filter(|e| words.iter().all(|w| e.search.contains(w))).take(180) {
                    results.append(&cell(&e.text));
                }
            }
        });
    }
    {
        let (results, pick) = (results.clone(), pick.clone());
        search.connect_activate(move |_| {
            if let Some(first) = results.child_at_index(0) {
                pick(&first.widget_name());
            }
        });
    }

    // Category tabs.
    let tabs = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    tabs.add_css_class("emoji-tabs");
    tabs.set_homogeneous(true);
    let recent_tab = gtk4::Button::builder()
        .icon_name("emoji-recent-symbolic")
        .css_classes(["flat"])
        .tooltip_text(tr!("Recently used"))
        .build();
    tabs.append(&recent_tab);
    {
        let scroll = scroll.clone();
        recent_tab.connect_clicked(move |_| scroll.vadjustment().set_value(0.0));
    }
    for (i, (_, icon, title)) in sections().iter().enumerate() {
        let b = gtk4::Button::builder().icon_name(*icon).css_classes(["flat"]).tooltip_text(title.as_str()).build();
        let (scroll, header, body, search) = (scroll.clone(), headers[i].clone(), body.clone(), search.clone());
        b.connect_clicked(move |_| {
            if !search.text().is_empty() {
                search.set_text("");
            }
            let (scroll, header, body) = (scroll.clone(), header.clone(), body.clone());
            glib::idle_add_local_once(move || {
                if let Some(p) = header.compute_point(&body, &gtk4::graphene::Point::new(0.0, 0.0)) {
                    scroll.vadjustment().set_value(f64::from(p.y()));
                }
            });
        });
        tabs.append(&b);
    }
    outer.append(&tabs);
    {
        let search = search.clone();
        popover.connect_show(move |_| {
            search.set_text("");
            search.grab_focus();
        });
    }
    search.grab_focus();
    outer
}

fn section_title(text: &str) -> gtk4::Label {
    let l = gtk4::Label::new(Some(text));
    l.add_css_class("emoji-section");
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_margin_start(4);
    l
}
