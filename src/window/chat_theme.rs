//! Chat themes, Telegram-style: light or dark, the colour of our bubbles,
//! and the conversation's background (gradients, a dotted pattern, or a
//! picture of our own). Kept on this computer (`chat-theme.json`).

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::rc::Rc;

use crate::tr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    /// `system`, `light` or `dark`.
    #[serde(default = "system")]
    pub scheme: String,
    /// One of [`BUBBLES`].
    #[serde(default = "accent")]
    pub bubble: String,
    /// One of [`BACKGROUNDS`], or `image`.
    #[serde(default = "default_bg")]
    pub background: String,
}

fn system() -> String {
    "system".into()
}
fn accent() -> String {
    "accent".into()
}
fn default_bg() -> String {
    "default".into()
}

impl Default for Theme {
    fn default() -> Self {
        Self { scheme: system(), bubble: accent(), background: default_bg() }
    }
}

/// Colours for our own bubbles: (id, name, colour shown in the picker).
pub fn bubbles() -> Vec<(&'static str, String, &'static str)> {
    vec![
        ("accent", tr!("System accent"), "@accent_bg_color"),
        ("blue", tr!("Blue"), "#3584e4"),
        ("teal", tr!("Teal"), "#1f8f8f"),
        ("green", tr!("Green"), "#26a269"),
        ("purple", tr!("Purple"), "#8a4fbf"),
        ("pink", tr!("Pink"), "#d4497d"),
        ("orange", tr!("Orange"), "#d9640f"),
        ("red", tr!("Red"), "#c01c28"),
        ("graphite", tr!("Graphite"), "#4a4a52"),
    ]
}

/// Backgrounds: (id, name).
pub fn backgrounds() -> Vec<(&'static str, String)> {
    vec![
        ("default", tr!("Default")),
        ("plain", tr!("Plain")),
        ("dots", tr!("Dots")),
        ("ocean", tr!("Ocean")),
        ("sunset", tr!("Sunset")),
        ("forest", tr!("Forest")),
        ("lavender", tr!("Lavender")),
        ("night", tr!("Night")),
    ]
}

/// Classes for the presets (the picture is added by `apply`).
pub fn css() -> String {
    let mut css = String::new();
    for (id, _, color) in bubbles() {
        if id == "accent" {
            continue;
        }
        css.push_str(&format!(
            ".bubble-color-{id} .bubble.mine {{ background-color: {color}; color: white; }}\n\
             .bubble-color-{id} .bubble.mine .file-icon, .bubble-color-{id} .bubble.mine .voice-play {{ color: {color}; }}\n\
             .bubble-color-{id} .bubble.mine label link {{ color: white; }}\n\
             .swatch-{id} {{ background-color: {color}; }}\n"
        ));
    }
    css.push_str(
        r#"
.swatch-accent { background-color: @accent_bg_color; }
.chat-backdrop.chat-bg-plain { background-image: none; background-color: @view_bg_color; }
.chat-backdrop.chat-bg-dots {
    background-image: radial-gradient(circle, alpha(@view_fg_color, 0.13) 1.2px, transparent 1.6px);
    background-size: 18px 18px;
}
.chat-backdrop.chat-bg-ocean { background-image: linear-gradient(160deg, alpha(#1c71d8, 0.32), alpha(#26a1a2, 0.22)); }
.chat-backdrop.chat-bg-sunset { background-image: linear-gradient(160deg, alpha(#e66100, 0.30), alpha(#c061cb, 0.26)); }
.chat-backdrop.chat-bg-forest { background-image: linear-gradient(160deg, alpha(#26a269, 0.32), alpha(#986a44, 0.20)); }
.chat-backdrop.chat-bg-lavender { background-image: linear-gradient(160deg, alpha(#9141ac, 0.28), alpha(#3584e4, 0.22)); }
.chat-backdrop.chat-bg-night { background-image: linear-gradient(180deg, #1e2233, #12131a); }
.chat-backdrop.chat-bg-night .day-pill { color: white; background-color: alpha(white, 0.12); }
/* Picker */
.theme-swatch {
    min-width: 34px;
    min-height: 34px;
    border-radius: 999px;
    padding: 0;
    box-shadow: inset 0 0 0 1px alpha(black, 0.15);
}
.theme-swatch:checked { box-shadow: 0 0 0 3px @window_bg_color, 0 0 0 5px @accent_color; }
.theme-swatch image { color: white; -gtk-icon-size: 16px; }
.theme-tile {
    min-width: 92px;
    min-height: 62px;
    border-radius: 12px;
    padding: 0;
    box-shadow: inset 0 0 0 1px alpha(@view_fg_color, 0.12);
}
.theme-tile:checked { box-shadow: 0 0 0 3px @window_bg_color, 0 0 0 5px @accent_color; }
.theme-tile label { font-size: 0.8em; font-weight: 700; padding: 2px 6px; border-radius: 8px; background-color: alpha(@window_bg_color, 0.75); }
.theme-preview { border-radius: 14px; padding: 14px; }
"#,
    );
    css
}

fn dir() -> PathBuf {
    // Development previews keep their own theme (BIGLACE_CHAT_DIR).
    #[cfg(debug_assertions)]
    if let Ok(d) = std::env::var("BIGLACE_CHAT_DIR") {
        return PathBuf::from(d);
    }
    glib::user_config_dir().join("biglace")
}

fn path() -> PathBuf {
    dir().join("chat-theme.json")
}

/// Our background picture (copied here when chosen).
fn image_path() -> PathBuf {
    dir().join("chat-background")
}

pub fn load() -> Theme {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(t: &Theme) {
    if let Ok(json) = serde_json::to_vec_pretty(t) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, json);
    }
}

thread_local! {
    static PROVIDERS: std::cell::OnceCell<(gtk4::CssProvider, gtk4::CssProvider)> = const { std::cell::OnceCell::new() };
}

/// (presets, picture) style providers, installed once.
fn providers() -> (gtk4::CssProvider, gtk4::CssProvider) {
    PROVIDERS.with(|p| {
        p.get_or_init(|| {
            let presets = gtk4::CssProvider::new();
            presets.load_from_string(&css());
            let picture = gtk4::CssProvider::new();
            if let Some(display) = gtk4::gdk::Display::default() {
                for provider in [&presets, &picture] {
                    gtk4::style_context_add_provider_for_display(
                        &display,
                        provider,
                        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
                    );
                }
            }
            (presets, picture)
        })
        .clone()
    })
}

/// Light, dark or the system's choice — for the whole app.
pub fn apply_scheme(t: &Theme) {
    let scheme = match t.scheme.as_str() {
        "light" => libadwaita::ColorScheme::ForceLight,
        "dark" => libadwaita::ColorScheme::ForceDark,
        _ => libadwaita::ColorScheme::Default,
    };
    libadwaita::StyleManager::default().set_color_scheme(scheme);
}

fn picture_css() -> String {
    let Ok(uri) = glib::filename_to_uri(image_path(), None) else { return String::new() };
    // A veil keeps the bubbles readable on any picture.
    format!(
        ".chat-backdrop.chat-bg-image {{ background-image: linear-gradient(alpha(@view_bg_color, 0.25), alpha(@view_bg_color, 0.25)), url(\"{uri}\"); background-size: cover; background-position: center; }}\n\
         .theme-tile.chat-bg-image {{ background-image: url(\"{uri}\"); background-size: cover; background-position: center; }}"
    )
}

/// Put the theme on a chat page (`root`: holds the bubbles; `backdrop`:
/// behind them).
pub fn apply(root: &impl IsA<gtk4::Widget>, backdrop: &impl IsA<gtk4::Widget>, t: &Theme) {
    let (_, picture) = providers();
    picture.load_from_string(&picture_css());
    for (id, _, _) in bubbles() {
        root.remove_css_class(&format!("bubble-color-{id}"));
    }
    root.add_css_class(&format!("bubble-color-{}", t.bubble));
    backdrop.remove_css_class("chat-bg-image");
    for (id, _) in backgrounds() {
        backdrop.remove_css_class(&format!("chat-bg-{id}"));
    }
    let bg = if t.background == "image" && !image_path().is_file() { "default" } else { t.background.as_str() };
    backdrop.add_css_class(&format!("chat-bg-{bg}"));
}

/// A little conversation showing the theme.
fn preview() -> (gtk4::Box, gtk4::Box) {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let backdrop = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    backdrop.add_css_class("chat-backdrop");
    backdrop.add_css_class("theme-preview");
    backdrop.set_overflow(gtk4::Overflow::Hidden);
    let bubble = |text: &str, mine: bool| {
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        b.add_css_class("bubble");
        b.add_css_class(if mine { "mine" } else { "theirs" });
        b.set_halign(if mine { gtk4::Align::End } else { gtk4::Align::Start });
        let l = gtk4::Label::new(Some(text));
        l.set_xalign(0.0);
        b.append(&l);
        let meta = gtk4::Label::new(Some(if mine { "12:01 ✓✓" } else { "12:00" }));
        meta.add_css_class("bubble-meta");
        meta.set_halign(gtk4::Align::End);
        b.append(&meta);
        b
    };
    backdrop.append(&bubble(&tr!("Hi! How does this look? 😊"), false));
    backdrop.append(&bubble(&tr!("Looks great!"), true));
    root.append(&backdrop);
    (root, backdrop)
}

/// The "Chat theme" window; `on_change` gets each change as it's made.
pub fn show_dialog(parent: &impl IsA<gtk4::Window>, on_change: impl Fn(&Theme) + 'static) {
    let on_change = Rc::new(on_change);
    let theme = Rc::new(std::cell::RefCell::new(load()));
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Chat theme"))
        .default_width(540)
        .default_height(700)
        .build();
    super::dialogs::close_on_escape(&dlg);
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&libadwaita::HeaderBar::new());
    let page = libadwaita::PreferencesPage::new();
    let (preview_root, preview_backdrop) = preview();
    let preview_group = libadwaita::PreferencesGroup::new();
    preview_group.add(&preview_root);
    page.add(&preview_group);

    // Everything goes through here: save, show, tell the chat.
    let changed: Rc<dyn Fn()> = {
        let (theme, on_change, pr, pb) = (theme.clone(), on_change.clone(), preview_root.clone(), preview_backdrop.clone());
        Rc::new(move || {
            let t = theme.borrow().clone();
            save(&t);
            apply_scheme(&t);
            apply(&pr, &pb, &t);
            on_change(&t);
        })
    };
    apply(&preview_root, &preview_backdrop, &theme.borrow());

    // ── Light / dark ──
    let scheme_group = libadwaita::PreferencesGroup::builder().title(tr!("Appearance")).build();
    let scheme_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    scheme_box.add_css_class("linked");
    scheme_box.set_homogeneous(true);
    let mut first: Option<gtk4::ToggleButton> = None;
    for (id, label, icon) in [
        ("system", tr!("System"), "emblem-system-symbolic"),
        ("light", tr!("Light"), "weather-clear-symbolic"),
        ("dark", tr!("Dark"), "weather-clear-night-symbolic"),
    ] {
        let b = gtk4::ToggleButton::builder()
            .child(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).build())
            .active(theme.borrow().scheme == id)
            .build();
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        let (theme, changed) = (theme.clone(), changed.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                theme.borrow_mut().scheme = id.to_string();
                changed();
            }
        });
        scheme_box.append(&b);
    }
    scheme_group.add(&scheme_box);
    page.add(&scheme_group);

    // ── Bubble colour ──
    let bubble_group = libadwaita::PreferencesGroup::builder().title(tr!("Your messages")).build();
    let swatches = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .max_children_per_line(9)
        .min_children_per_line(5)
        .column_spacing(10)
        .row_spacing(10)
        .homogeneous(true)
        .build();
    let mut first: Option<gtk4::ToggleButton> = None;
    for (id, name, _) in bubbles() {
        let b = gtk4::ToggleButton::builder()
            .css_classes(["theme-swatch", &format!("swatch-{id}")])
            .tooltip_text(name)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .active(theme.borrow().bubble == id)
            .build();
        let check = gtk4::Image::from_icon_name("object-select-symbolic");
        b.set_child(Some(&check));
        check.set_visible(b.is_active());
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        let (theme, changed) = (theme.clone(), changed.clone());
        b.connect_toggled(move |b| {
            check.set_visible(b.is_active());
            if b.is_active() {
                theme.borrow_mut().bubble = id.to_string();
                changed();
            }
        });
        swatches.insert(&b, -1);
    }
    bubble_group.add(&swatches);
    page.add(&bubble_group);

    // ── Background ──
    let bg_group = libadwaita::PreferencesGroup::builder().title(tr!("Background")).build();
    let tiles = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .max_children_per_line(4)
        .min_children_per_line(3)
        .column_spacing(10)
        .row_spacing(10)
        .homogeneous(true)
        .build();
    let mut first: Option<gtk4::ToggleButton> = None;
    let mut list = backgrounds();
    list.push(("image", tr!("Picture…")));
    for (id, name) in list {
        let b = gtk4::ToggleButton::builder()
            .css_classes(["theme-tile", "chat-backdrop", &format!("chat-bg-{id}")])
            .active(theme.borrow().background == id && (id != "image" || image_path().is_file()))
            .build();
        let l = gtk4::Label::new(Some(&name));
        l.set_valign(gtk4::Align::End);
        l.set_halign(gtk4::Align::Center);
        l.set_margin_bottom(4);
        b.set_child(Some(&l));
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        if id == "image" {
            // Choose a picture each time this tile is pressed.
            let (theme, changed, dlg2, tile) = (theme.clone(), changed.clone(), dlg.clone(), b.clone());
            let click = gtk4::GestureClick::new();
            click.connect_pressed(move |g, _, _, _| {
                g.set_state(gtk4::EventSequenceState::Claimed);
                pick_picture(&dlg2, theme.clone(), changed.clone(), tile.clone());
            });
            b.add_controller(click);
        } else {
            let (theme, changed) = (theme.clone(), changed.clone());
            b.connect_toggled(move |b| {
                if b.is_active() {
                    theme.borrow_mut().background = id.to_string();
                    changed();
                }
            });
        }
        tiles.insert(&b, -1);
    }
    bg_group.add(&tiles);
    page.add(&bg_group);
    outer.append(&page);
    dlg.set_content(Some(&outer));
    dlg.present();
}

fn pick_picture(
    dlg: &libadwaita::Window,
    theme: Rc<std::cell::RefCell<Theme>>,
    changed: Rc<dyn Fn()>,
    tile: gtk4::ToggleButton,
) {
    let filter = gtk4::FileFilter::new();
    filter.set_name(Some(&tr!("Pictures")));
    filter.add_pixbuf_formats();
    let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
    filters.append(&filter);
    let chooser = gtk4::FileDialog::builder()
        .title(tr!("Choose a background picture"))
        .modal(true)
        .filters(&filters)
        .default_filter(&filter)
        .build();
    chooser.open(Some(dlg), gtk4::gio::Cancellable::NONE, move |r| {
        let Some(src) = r.ok().and_then(|f| f.path()) else { return };
        // Kept as a reasonably sized JPEG of our own (the original may move).
        let Ok(pb) = gtk4::gdk_pixbuf::Pixbuf::from_file_at_scale(&src, 2560, 2560, true) else { return };
        let pb = pb.apply_embedded_orientation().unwrap_or(pb);
        let dest = image_path();
        if let Some(dir) = dest.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if pb.savev(&dest, "jpeg", &[("quality", "88")]).is_err() {
            return;
        }
        theme.borrow_mut().background = "image".into();
        changed();
        tile.set_active(true);
    });
}
