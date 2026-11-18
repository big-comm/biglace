//! What a chat bubble shows for a file: a picture, a video with its
//! preview, a voice message with its sound bars, or a document card —
//! with download progress, and a click to open.

use gtk4::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

use crate::chat::{files, voice, ConvId, FileKind, Message};
use crate::tr;
use crate::trf;

/// Largest size a picture or video preview is shown at.
const MEDIA_W: i32 = 300;
const MEDIA_H: i32 = 320;

/// A bubble's file part, rebuilt when the file arrives.
pub struct FileView {
    pub slot: gtk4::Box,
    key: RefCell<String>,
    /// Shown while downloading: bar and "1.2 MB of 5 MB".
    progress: RefCell<Option<(gtk4::ProgressBar, gtk4::Label)>>,
}

/// What the view looks like for `m` now (rebuild when it changes).
fn state_key(m: &Message) -> String {
    let Some(f) = &m.file else { return String::new() };
    if f.local().is_some() {
        "done".into()
    } else if files::downloading(&m.id) {
        "busy".into()
    } else {
        "offer".into()
    }
}

pub fn build(m: &Message, conv: &ConvId) -> Rc<FileView> {
    let view = Rc::new(FileView {
        slot: gtk4::Box::new(gtk4::Orientation::Vertical, 0),
        key: RefCell::default(),
        progress: RefCell::default(),
    });
    fill(&view, m, conv);
    view
}

/// The message changed (downloaded…): redraw if it looks different now.
pub fn refresh(view: &FileView, m: &Message, conv: &ConvId) {
    if *view.key.borrow() != state_key(m) {
        fill(view, m, conv);
    }
}

pub fn progress(view: &FileView, done: u64, total: u64) {
    if let Some((bar, label)) = view.progress.borrow().as_ref() {
        bar.set_visible(true);
        if total > 0 {
            bar.set_fraction(done as f64 / total as f64);
            label.set_text(&trf!(
                "{done} of {total}",
                "done" => glib::format_size(done),
                "total" => glib::format_size(total)
            ));
        } else {
            bar.pulse();
        }
    }
}

fn fill(view: &FileView, m: &Message, conv: &ConvId) {
    while let Some(c) = view.slot.first_child() {
        view.slot.remove(&c);
    }
    *view.key.borrow_mut() = state_key(m);
    *view.progress.borrow_mut() = None;
    let Some(f) = &m.file else { return };
    let busy = files::downloading(&m.id);
    let local = f.local();
    let bar = gtk4::ProgressBar::new();
    bar.add_css_class("file-progress");
    bar.set_visible(busy);
    let status = gtk4::Label::new(None);
    let content: gtk4::Widget = match f.offer.kind {
        FileKind::Image | FileKind::Video => media(m, conv, local.as_deref(), busy, &bar, &status),
        FileKind::Voice | FileKind::Audio => player(m, conv, local.as_deref(), busy, &status),
        FileKind::File => card(m, conv, local.as_deref(), busy, &bar, &status),
    };
    view.slot.append(&content);
    *view.progress.borrow_mut() = Some((bar, status));
}

/// Open a file with the desktop's app for it.
fn open(widget: &impl IsA<gtk4::Widget>, path: &std::path::Path) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let win = widget.root().and_downcast::<gtk4::Window>();
    launcher.launch(win.as_ref(), gio::Cancellable::NONE, |_| {});
}

fn open_folder(widget: &impl IsA<gtk4::Widget>, path: &std::path::Path) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let win = widget.root().and_downcast::<gtk4::Window>();
    launcher.open_containing_folder(win.as_ref(), gio::Cancellable::NONE, |_| {});
}

fn on_click(widget: &impl IsA<gtk4::Widget>, f: impl Fn() + 'static) {
    let click = gtk4::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk4::EventSequenceState::Claimed);
        f();
    });
    widget.add_controller(click);
    widget.set_cursor_from_name(Some("pointer"));
}

/// Display size keeping the picture's shape.
fn fit(w: u32, h: u32) -> (i32, i32) {
    if w == 0 || h == 0 {
        return (MEDIA_W, MEDIA_W * 9 / 16);
    }
    let scale = (f64::from(MEDIA_W) / f64::from(w)).min(f64::from(MEDIA_H) / f64::from(h));
    let (dw, dh) = ((f64::from(w) * scale) as i32, (f64::from(h) * scale) as i32);
    (dw.max(120), dh.max(80))
}

/// A picture (full, once here) or a video (its preview, with a play button).
fn media(
    m: &Message,
    conv: &ConvId,
    local: Option<&std::path::Path>,
    busy: bool,
    bar: &gtk4::ProgressBar,
    status: &gtk4::Label,
) -> gtk4::Widget {
    let f = m.file.as_ref().expect("file");
    let video = f.offer.kind == FileKind::Video;
    let (w, h) = fit(f.offer.width, f.offer.height);
    let texture = match local {
        Some(p) if !video => gtk4::gdk_pixbuf::Pixbuf::from_file_at_scale(p, w * 2, h * 2, true)
            .ok()
            .map(|pb| gdk::Texture::for_pixbuf(&pb)),
        _ => None,
    }
    .or_else(|| f.offer.thumb_bytes().and_then(|b| gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).ok()));
    let picture = gtk4::Picture::new();
    picture.set_paintable(texture.as_ref());
    picture.set_content_fit(gtk4::ContentFit::Cover);
    picture.set_size_request(w, h);
    picture.set_can_shrink(true);
    let frame = gtk4::Overlay::new();
    frame.add_css_class("bubble-media");
    frame.set_overflow(gtk4::Overflow::Hidden);
    frame.set_child(Some(&picture));
    frame.set_halign(gtk4::Align::Start);
    if texture.is_none() {
        frame.add_css_class("no-preview");
    }

    // Middle: play / download / progress.
    let center = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    center.set_halign(gtk4::Align::Center);
    center.set_valign(gtk4::Align::Center);
    let round = gtk4::Button::builder().css_classes(["media-round", "circular"]).build();
    if busy {
        let spinner = gtk4::Spinner::builder().spinning(true).build();
        round.set_child(Some(&spinner));
        round.set_can_target(false);
    } else if local.is_some() {
        round.set_icon_name("media-playback-start-symbolic");
        round.set_tooltip_text(Some(&tr!("Play")));
    } else {
        round.set_icon_name("folder-download-symbolic");
        round.set_tooltip_text(Some(&tr!("Download")));
    }
    if video || local.is_none() {
        center.append(&round);
        frame.add_overlay(&center);
    }
    // Bottom left: size (or progress).
    let pill = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
    pill.add_css_class("media-pill");
    pill.set_halign(gtk4::Align::Start);
    pill.set_valign(gtk4::Align::End);
    status.set_text(&glib::format_size(f.offer.size));
    pill.append(status);
    bar.set_size_request(90, -1);
    pill.append(bar);
    if local.is_none() || video {
        frame.add_overlay(&pill);
    }

    let (conv, id, file) = (conv.clone(), m.id.clone(), local.map(std::path::Path::to_path_buf));
    let act = {
        let frame = frame.clone();
        move || match &file {
            Some(p) => open(&frame, p),
            None => files::download(&conv, &id),
        }
    };
    let act = Rc::new(act);
    {
        let act = act.clone();
        round.connect_clicked(move |_| act());
    }
    if !busy {
        let act = act.clone();
        on_click(&picture, move || act());
    }
    if local.is_some() {
        picture.set_tooltip_text(Some(&if video { tr!("Play") } else { tr!("Open") }));
    }
    frame.upcast()
}

/// A document: icon, name, size; click to download, then to open.
fn card(
    m: &Message,
    conv: &ConvId,
    local: Option<&std::path::Path>,
    busy: bool,
    bar: &gtk4::ProgressBar,
    status: &gtk4::Label,
) -> gtk4::Widget {
    let f = m.file.as_ref().expect("file");
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.add_css_class("file-card");
    let icon = gtk4::Button::builder().css_classes(["file-icon", "circular"]).valign(gtk4::Align::Center).build();
    if busy {
        icon.set_child(Some(&gtk4::Spinner::builder().spinning(true).build()));
        icon.set_can_target(false);
    } else if local.is_some() {
        icon.set_icon_name("text-x-generic-symbolic");
        icon.set_tooltip_text(Some(&tr!("Open")));
    } else {
        icon.set_icon_name("folder-download-symbolic");
        icon.set_tooltip_text(Some(&tr!("Download")));
    }
    row.append(&icon);
    let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
    texts.set_valign(gtk4::Align::Center);
    texts.set_hexpand(true);
    let name = gtk4::Label::new(Some(&f.offer.name));
    name.add_css_class("file-name");
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(28);
    texts.append(&name);
    status.add_css_class("file-status");
    status.set_xalign(0.0);
    status.set_text(&glib::format_size(f.offer.size));
    texts.append(status);
    bar.set_size_request(160, -1);
    texts.append(bar);
    row.append(&texts);
    if let Some(p) = local {
        let folder = gtk4::Button::builder()
            .icon_name("folder-open-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk4::Align::Center)
            .tooltip_text(tr!("Show in folder"))
            .build();
        let p = p.to_path_buf();
        folder.connect_clicked(move |b| open_folder(b, &p));
        row.append(&folder);
    }
    let (conv, id, file) = (conv.clone(), m.id.clone(), local.map(std::path::Path::to_path_buf));
    let r = row.clone();
    icon.connect_clicked(move |_| match &file {
        Some(p) => open(&r, p),
        None => files::download(&conv, &id),
    });
    row.upcast()
}

thread_local! {
    /// The voice message playing now (one at a time).
    static PLAYING: RefCell<Option<gtk4::MediaFile>> = const { RefCell::new(None) };
}

/// A voice message (or a song): play button, sound bars, length.
fn player(m: &Message, conv: &ConvId, local: Option<&std::path::Path>, busy: bool, status: &gtk4::Label) -> gtk4::Widget {
    let f = m.file.as_ref().expect("file");
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    row.add_css_class("voice-player");
    let play = gtk4::Button::builder().css_classes(["voice-play", "circular"]).valign(gtk4::Align::Center).build();
    if busy {
        play.set_child(Some(&gtk4::Spinner::builder().spinning(true).build()));
        play.set_can_target(false);
    } else if local.is_some() {
        play.set_icon_name("media-playback-start-symbolic");
        play.set_tooltip_text(Some(&tr!("Play")));
    } else {
        play.set_icon_name("folder-download-symbolic");
        play.set_tooltip_text(Some(&tr!("Download")));
    }
    row.append(&play);
    let right = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    right.set_valign(gtk4::Align::Center);
    if f.offer.kind == FileKind::Audio {
        let name = gtk4::Label::new(Some(&f.offer.name));
        name.add_css_class("file-name");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        name.set_max_width_chars(26);
        right.append(&name);
    }
    let wave: Vec<u8> = if f.offer.wave.is_empty() {
        // A song: a gentle made-up shape.
        (0..voice::WAVE_BARS).map(|i| 8 + ((i * 7 + 3) % 11) as u8).collect()
    } else {
        f.offer.wave.clone()
    };
    let played = Rc::new(std::cell::Cell::new(0.0f64));
    let area = gtk4::DrawingArea::builder()
        .content_width((voice::WAVE_BARS * 4) as i32)
        .content_height(26)
        .css_classes(["voice-wave-bars"])
        .build();
    {
        let played = played.clone();
        area.set_draw_func(move |a, cr, w, h| {
            let color = a.color();
            let n = wave.len().max(1);
            let step = f64::from(w) / n as f64;
            for (i, v) in wave.iter().enumerate() {
                let frac = (i as f64 + 0.5) / n as f64;
                let alpha = if frac <= played.get() { 1.0 } else { 0.4 };
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    f64::from(color.alpha()) * alpha,
                );
                let bh = (f64::from(*v) / 31.0 * f64::from(h)).max(3.0);
                let x = i as f64 * step + 0.5;
                rounded_bar(cr, x, (f64::from(h) - bh) / 2.0, (step - 1.5).max(1.5), bh);
                let _ = cr.fill();
            }
        });
    }
    right.append(&area);
    status.add_css_class("file-status");
    status.set_xalign(0.0);
    let length = if f.offer.duration_ms > 0 { voice::clock(f.offer.duration_ms) } else { glib::format_size(f.offer.size).to_string() };
    status.set_text(&length);
    right.append(status);
    row.append(&right);

    let (conv, id, file) = (conv.clone(), m.id.clone(), local.map(std::path::Path::to_path_buf));
    let media: Rc<RefCell<Option<gtk4::MediaFile>>> = Rc::default();
    let status = status.clone();
    play.connect_clicked(move |btn| {
        let Some(p) = &file else {
            files::download(&conv, &id);
            return;
        };
        let existing = media.borrow().clone();
        let stream = match existing {
            Some(s) => s,
            None => {
                let s = gtk4::MediaFile::for_filename(p);
                let (btn2, area2, played2, status2, length2) =
                    (btn.clone(), area.clone(), played.clone(), status.clone(), length.clone());
                s.connect_timestamp_notify(move |s| {
                    let d = s.duration();
                    if d > 0 && s.is_playing() {
                        played2.set(s.timestamp() as f64 / d as f64);
                        status2.set_text(&voice::clock((s.timestamp() / 1000) as u64));
                        area2.queue_draw();
                    }
                });
                let (area3, played3, status3) = (area.clone(), played.clone(), status.clone());
                s.connect_ended_notify(move |s| {
                    if s.is_ended() {
                        btn2.set_icon_name("media-playback-start-symbolic");
                        played3.set(0.0);
                        status3.set_text(&length2);
                        area3.queue_draw();
                    }
                });
                {
                    let btn3 = btn.clone();
                    s.connect_playing_notify(move |s| {
                        let playing = s.is_playing();
                        btn3.set_icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
                        btn3.set_tooltip_text(Some(&if playing { tr!("Pause") } else { tr!("Play") }));
                    });
                }
                *media.borrow_mut() = Some(s.clone());
                s
            }
        };
        if stream.is_playing() {
            stream.pause();
        } else {
            PLAYING.with(|cur| {
                if let Some(other) = cur.borrow().as_ref() {
                    if other != &stream {
                        other.pause();
                    }
                }
                *cur.borrow_mut() = Some(stream.clone());
            });
            if stream.is_ended() {
                stream.seek(0);
            }
            stream.play();
        }
    });
    row.upcast()
}

fn rounded_bar(cr: &gtk4::cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    let r = (w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.close_path();
}

/// One line about a message for lists and notifications.
pub fn summary(m: &Message) -> Option<String> {
    let f = m.file.as_ref()?;
    Some(match f.offer.kind {
        FileKind::Image => format!("🖼 {}", tr!("Photo")),
        FileKind::Video => format!("🎬 {}", tr!("Video")),
        FileKind::Voice => format!("🎤 {} ({})", tr!("Voice message"), voice::clock(f.offer.duration_ms)),
        FileKind::Audio => format!("🎵 {}", f.offer.name),
        FileKind::File => format!("📎 {}", f.offer.name),
    })
}
