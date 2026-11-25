//! The banner saying who reaches our devices for support, until when, with
//! "End now" (see crate::support).

use crate::{tr, trf};
use gtk4::glib;
use gtk4::prelude::ObjectExt;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
pub struct SupportBanner {
    pub banner: libadwaita::Banner,
    /// The access the banner speaks of.
    shown: Rc<RefCell<Option<String>>>,
}

/// When `ends_at` (RFC 3339) is, as people read it here: the time today, else the date too.
pub fn until(ends_at: &str) -> String {
    let Ok(ends) = glib::DateTime::from_iso8601(ends_at, None).and_then(|d| d.to_local()) else {
        return ends_at.to_string();
    };
    let today = glib::DateTime::now_local().is_ok_and(|now| now.ymd() == ends.ymd());
    let pattern = if today { "%H:%M" } else { "%x %H:%M" };
    ends.format(pattern).map(|s| s.to_string()).unwrap_or_default()
}

/// Show on `banner` the access that ends first, or nothing.
fn show(banner: &libadwaita::Banner, shown: &RefCell<Option<String>>) {
    let state = crate::support::state();
    let first = state.helped_by.iter().filter(|g| crate::support::open(g)).min_by(|a, b| a.ends_at.cmp(&b.ends_at));
    match first {
        Some(g) => {
            // The title is markup.
            banner.set_title(&trf!(
                "{user} can reach your devices until {time}, to help you",
                "user" => glib::markup_escape_text(&g.helper),
                "time" => until(&g.ends_at)
            ));
            banner.set_revealed(true);
            *shown.borrow_mut() = Some(g.id.clone());
        }
        None => {
            banner.set_revealed(false);
            *shown.borrow_mut() = None;
        }
    }
}

impl SupportBanner {
    pub fn build(toast: &libadwaita::ToastOverlay) -> Self {
        let banner = libadwaita::Banner::builder().revealed(false).button_label(tr!("End now")).build();
        let shown: Rc<RefCell<Option<String>>> = Rc::default();
        let (weak, ids, toast) = (banner.downgrade(), shown.clone(), toast.downgrade());
        banner.connect_button_clicked(move |_| {
            let Some(id) = ids.borrow().clone() else { return };
            let (weak, ids, toast) = (weak.clone(), ids.clone(), toast.clone());
            super::run_in_background(
                move || crate::support::end(&id),
                move |r| {
                    if let Some(toast) = toast.upgrade() {
                        match r {
                            Ok(()) => super::show_toast(&toast, &tr!("Access ended."), 4),
                            Err(e) => super::show_toast(&toast, &super::invites::explain(&e), 8),
                        }
                    }
                    if let Some(banner) = weak.upgrade() {
                        show(&banner, &ids);
                    }
                },
            );
        });
        Self { banner, shown }
    }

    pub fn update(&self) {
        show(&self.banner, &self.shown);
    }
}
