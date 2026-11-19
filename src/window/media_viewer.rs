//! Pictures and videos from the chat, full screen inside BigLace (like
//! Telegram) instead of in another program: zoom on a picture (wheel,
//! pinch, double-click), play a video with its controls. Esc or ✕ closes;
//! "Open with…" and "Show in folder" are still a click away.

use gtk4::{gdk, gio, glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::tr;

const MAX_ZOOM: f64 = 8.0;

pub fn show(parent: &impl IsA<gtk4::Widget>, path: &std::path::Path, video: bool) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let win = libadwaita::Window::builder().title(&name).default_width(1000).default_height(700).build();
    if let Some(root) = parent.root().and_downcast::<gtk4::Window>() {
        win.set_transient_for(Some(&root));
    }
    win.add_css_class("media-viewer");

    let overlay = gtk4::Overlay::new();
    let stage: gtk4::Widget = if video {
        let v = gtk4::Video::for_filename(Some(path));
        v.set_autoplay(true);
        v.set_hexpand(true);
        v.set_vexpand(true);
        v.upcast()
    } else {
        picture_stage(path)
    };
    overlay.set_child(Some(&stage));

    // Top bar over the picture: name, open with, show in folder, close.
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    bar.add_css_class("media-viewer-bar");
    bar.set_valign(gtk4::Align::Start);
    let title = gtk4::Label::new(Some(&name));
    title.set_hexpand(true);
    title.set_xalign(0.0);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    let open_with = gtk4::Button::builder().icon_name("adw-external-link-symbolic").tooltip_text(tr!("Open with another app")).build();
    let folder = gtk4::Button::builder().icon_name("folder-open-symbolic").tooltip_text(tr!("Show in folder")).build();
    let close = gtk4::Button::builder().icon_name("window-close-symbolic").tooltip_text(tr!("Close")).build();
    for b in [&open_with, &folder, &close] {
        b.add_css_class("circular");
        b.add_css_class("osd");
    }
    bar.append(&title);
    bar.append(&open_with);
    bar.append(&folder);
    bar.append(&close);
    overlay.add_overlay(&bar);
    win.set_content(Some(&overlay));

    {
        let (p, w) = (path.to_path_buf(), win.clone());
        open_with.connect_clicked(move |_| {
            let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(&p)));
            launcher.set_always_ask(true);
            launcher.launch(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    {
        let (p, w) = (path.to_path_buf(), win.clone());
        folder.connect_clicked(move |_| {
            gtk4::FileLauncher::new(Some(&gio::File::for_path(&p))).open_containing_folder(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    {
        let w = win.clone();
        close.connect_clicked(move |_| w.close());
    }
    let keys = gtk4::EventControllerKey::new();
    {
        let w = win.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                w.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    win.add_controller(keys);
    // Stop the video with the window.
    {
        let stage = stage.clone();
        win.connect_close_request(move |_| {
            if let Some(s) = stage.downcast_ref::<gtk4::Video>().and_then(|v| v.media_stream()) {
                s.set_playing(false);
            }
            glib::Propagation::Proceed
        });
    }
    win.fullscreen();
    win.present();
}

/// The picture, fitted to the screen; zoom follows the wheel (and pinch)
/// around the pointer, a double-click toggles 1× / 2.5×.
fn picture_stage(path: &std::path::Path) -> gtk4::Widget {
    let texture = gdk::Texture::from_filename(path).ok();
    let picture = gtk4::Picture::builder().can_shrink(true).content_fit(gtk4::ContentFit::Contain).build();
    picture.set_paintable(texture.as_ref());
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::External)
        .child(&picture)
        .hexpand(true)
        .vexpand(true)
        .build();
    let zoom = Rc::new(Cell::new(1.0f64));
    let (tw, th) = texture.as_ref().map(|t| (f64::from(t.width()), f64::from(t.height()))).unwrap_or((1.0, 1.0));

    // Zoom 1 = fit the view; above it, the picture grows and scrolls.
    let apply = {
        let (scroll, picture, zoom) = (scroll.clone(), picture.clone(), zoom.clone());
        Rc::new(move |z: f64, at: Option<(f64, f64)>| {
            let z = z.clamp(1.0, MAX_ZOOM);
            let old = zoom.replace(z);
            let (vw, vh) = (f64::from(scroll.width().max(1)), f64::from(scroll.height().max(1)));
            let fitted = (vw / tw).min(vh / th);
            if z <= 1.0 {
                picture.set_size_request(-1, -1);
                return;
            }
            picture.set_size_request((tw * fitted * z) as i32, (th * fitted * z) as i32);
            // Keep the point under the pointer where it is.
            let (px, py) = at.unwrap_or((vw / 2.0, vh / 2.0));
            let (h, v) = (scroll.hadjustment(), scroll.vadjustment());
            let ratio = z / old;
            let (nx, ny) = ((h.value() + px) * ratio - px, (v.value() + py) * ratio - py);
            glib::idle_add_local_once(move || {
                h.set_value(nx);
                v.set_value(ny);
            });
        })
    };
    let pointer = Rc::new(Cell::new((0.0, 0.0)));
    {
        let motion = gtk4::EventControllerMotion::new();
        let pointer = pointer.clone();
        motion.connect_motion(move |_, x, y| pointer.set((x, y)));
        scroll.add_controller(motion);
    }
    {
        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let (apply, zoom, pointer) = (apply.clone(), zoom.clone(), pointer.clone());
        wheel.connect_scroll(move |c, _, dy| {
            // Plain wheel zooms while the picture fits; once zoomed, the
            // wheel scrolls and Ctrl+wheel zooms (like image viewers).
            let ctrl = c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            if zoom.get() > 1.0 && !ctrl {
                return glib::Propagation::Proceed;
            }
            let factor = if dy < 0.0 { 1.25 } else { 0.8 };
            apply(zoom.get() * factor, Some(pointer.get()));
            glib::Propagation::Stop
        });
        scroll.add_controller(wheel);
    }
    {
        let pinch = gtk4::GestureZoom::new();
        let start = Rc::new(Cell::new(1.0));
        let (s, z) = (start.clone(), zoom.clone());
        pinch.connect_begin(move |_, _| s.set(z.get()));
        let apply = apply.clone();
        pinch.connect_scale_changed(move |g, scale| {
            let at = g.bounding_box_center();
            apply(start.get() * scale, at);
        });
        scroll.add_controller(pinch);
    }
    {
        let click = gtk4::GestureClick::new();
        let (apply, zoom) = (apply.clone(), zoom.clone());
        click.connect_pressed(move |_, n, x, y| {
            if n == 2 {
                apply(if zoom.get() > 1.0 { 1.0 } else { 2.5 }, Some((x, y)));
            }
        });
        scroll.add_controller(click);
    }
    scroll.upcast()
}
