//! Preferences → "This computer": who may use it remotely (ask / always /
//! never, per person) and the history of accesses.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;

use crate::access::{self, Rule};
use crate::tr;
use crate::trf;

pub struct AccessPage {
    pub page: libadwaita::PreferencesPage,
    people: libadwaita::PreferencesGroup,
    history: libadwaita::PreferencesGroup,
    /// Rows added by the last refresh (removed by the next).
    rows: std::cell::RefCell<Vec<(libadwaita::PreferencesGroup, gtk4::Widget)>>,
}

impl AccessPage {
    pub fn build() -> std::rc::Rc<Self> {
        let page = libadwaita::PreferencesPage::builder()
            .title(tr!("This computer"))
            .icon_name("preferences-desktop-remote-desktop-symbolic")
            .build();
        let people = libadwaita::PreferencesGroup::builder()
            .title(tr!("Who may use this computer remotely"))
            .description(tr!(
                "Anyone you're connected to can ask to see and control this screen. Choose who is let in without asking, and who is never let in."
            ))
            .build();
        let history = libadwaita::PreferencesGroup::builder()
            .title(tr!("History of accesses"))
            .description(tr!("Kept on this computer only."))
            .build();
        let clear = gtk4::Button::builder()
            .label(tr!("Clear"))
            .css_classes(["flat"])
            .valign(gtk4::Align::Center)
            .build();
        history.set_header_suffix(Some(&clear));
        page.add(&people);
        page.add(&history);
        let this = std::rc::Rc::new(Self { page, people, history, rows: Default::default() });
        {
            let weak = std::rc::Rc::downgrade(&this);
            clear.connect_clicked(move |_| {
                access::clear_history();
                if let Some(p) = weak.upgrade() {
                    p.refresh();
                }
            });
        }
        this.refresh();
        this
    }

    fn add(&self, group: &libadwaita::PreferencesGroup, row: &impl IsA<gtk4::Widget>) {
        group.add(row);
        self.rows.borrow_mut().push((group.clone(), row.clone().upcast()));
    }

    /// Read the rules and the history again.
    pub fn refresh(&self) {
        for (g, w) in self.rows.borrow_mut().drain(..) {
            g.remove(&w);
        }
        let history = access::history();
        // Everyone with a rule, and everyone who ever asked.
        let mut people: Vec<String> = access::rules().into_keys().collect();
        for e in &history {
            if !people.contains(&e.who) {
                people.push(e.who.clone());
            }
        }
        people.sort();
        if people.is_empty() {
            let row = libadwaita::ActionRow::builder()
                .title(tr!("Nobody has asked yet."))
                .subtitle(tr!("When someone asks, you can choose to always let them in."))
                .build();
            self.add(&self.people, &row);
        }
        let choices = gtk4::StringList::new(&[&tr!("Ask every time"), &tr!("Always allow"), &tr!("Never allow")]);
        for who in people {
            let row = libadwaita::ComboRow::builder().title(glib::markup_escape_text(&who).as_str()).model(&choices).build();
            let avatar = libadwaita::Avatar::new(32, Some(&who), true);
            crate::avatars::bind(&avatar, &who);
            row.add_prefix(&avatar);
            row.set_selected(match access::rule(&who) {
                Rule::Ask => 0,
                Rule::Always => 1,
                Rule::Never => 2,
            });
            row.connect_selected_notify(move |r| {
                let rule = match r.selected() {
                    1 => Rule::Always,
                    2 => Rule::Never,
                    _ => Rule::Ask,
                };
                access::set_rule(&who, rule);
            });
            self.add(&self.people, &row);
        }

        if history.is_empty() {
            let row = libadwaita::ActionRow::builder().title(tr!("No accesses yet.")).build();
            self.add(&self.history, &row);
        }
        for e in history.iter().take(100) {
            let what = match e.what.as_str() {
                "screen" => tr!("Remote access"),
                "watch" => tr!("Watched the screen shared with the room"),
                "control" => tr!("Got control of the shared screen"),
                "terminal" => tr!("Opened a terminal"),
                "files" => tr!("Browsed the files"),
                "restart" => tr!("Restarted the computer"),
                "relogin" => tr!("Restarted the session"),
                "shutdown" => tr!("Shut down the computer"),
                other => other.to_string(),
            };
            let (outcome, icon, class) = match e.outcome.as_str() {
                "allowed" => (tr!("allowed"), "emblem-ok-symbolic", "success"),
                "remembered" => (tr!("allowed (always)"), "emblem-ok-symbolic", "success"),
                "returning" => (tr!("came back after a restart"), "view-refresh-symbolic", "accent"),
                "denied" => (tr!("denied"), "action-unavailable-symbolic", "warning"),
                "blocked" => (tr!("blocked (never allowed)"), "action-unavailable-symbolic", "error"),
                "ended" => (tr!("ended"), "media-playback-stop-symbolic", "dim-label"),
                other => (other.to_string(), "dialog-information-symbolic", "dim-label"),
            };
            let when = glib::DateTime::from_unix_local(e.at as i64)
                .ok()
                .and_then(|d| d.format("%d/%m/%Y %H:%M").ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let who = if e.device.is_empty() || e.device == e.who {
                e.who.clone()
            } else {
                trf!("{user} ({device})", "user" => e.who, "device" => e.device)
            };
            let row = libadwaita::ActionRow::builder()
                .title(glib::markup_escape_text(&format!("{who} · {what}")).as_str())
                .subtitle(glib::markup_escape_text(&format!("{when} · {outcome}")).as_str())
                .build();
            let img = gtk4::Image::from_icon_name(icon);
            img.add_css_class(class);
            row.add_prefix(&img);
            self.add(&self.history, &row);
        }
    }
}
