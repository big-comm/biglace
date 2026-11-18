//! "Rooms" section: a voice room per group — see who's there, join, talk,
//! mute, leave. The sound itself is handled by `crate::room`.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::invite::{self, RoomInfo};
use crate::room::{self, MemberView, RoomEvent};
use crate::tr;
use crate::trf;

#[derive(Clone)]
pub struct RoomsPage {
    pub toolbar: libadwaita::ToolbarView,
    pub menu_btn: gtk4::MenuButton,
    stack: gtk4::Stack,
    /// Rooms of my groups (not in a room).
    list: gtk4::ListBox,
    /// Inside a room.
    in_title: libadwaita::WindowTitle,
    members: gtk4::ListBox,
    btn_mute: gtk4::ToggleButton,
    error_page: libadwaita::StatusPage,
    toast: libadwaita::ToastOverlay,
    /// Reflects the room state while updating the mute toggle ourselves.
    syncing: Rc<std::cell::Cell<bool>>,
    rooms: Rc<RefCell<Vec<RoomInfo>>>,
}

impl RoomsPage {
    pub fn build(toast: &libadwaita::ToastOverlay) -> Self {
        let toolbar = libadwaita::ToolbarView::new();
        let header = libadwaita::HeaderBar::new();
        header.set_title_widget(Some(&libadwaita::WindowTitle::new(&tr!("Rooms"), "")));
        let btn_refresh = gtk4::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(tr!("Refresh"))
            .build();
        let menu_btn = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr!("Main menu"))
            .build();
        header.pack_end(&menu_btn);
        header.pack_end(&btn_refresh);
        toolbar.add_top_bar(&header);

        // ── Not in a room: one row per group ──
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        body.set_margin_top(24);
        body.set_margin_bottom(24);
        body.set_margin_start(12);
        body.set_margin_end(12);
        let intro = gtk4::Label::new(Some(&tr!(
            "Each of your groups has a voice room. Join it and talk with whoever is there — the sound goes straight between you, through the network."
        )));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        intro.add_css_class("dim-label");
        body.append(&intro);
        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.update_property(&[gtk4::accessible::Property::Label(&tr!("Rooms"))]);
        let empty = libadwaita::ActionRow::builder()
            .title(tr!("You're not in any group yet. Create one in Groups."))
            .build();
        list.set_placeholder(Some(&empty));
        body.append(&list);
        let lobby = libadwaita::Clamp::builder().maximum_size(720).child(&body).build();
        let lobby = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&lobby)
            .build();

        // ── In a room ──
        let in_box = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        in_box.set_margin_top(24);
        in_box.set_margin_bottom(24);
        in_box.set_margin_start(12);
        in_box.set_margin_end(12);
        let in_title = libadwaita::WindowTitle::new("", "");
        in_title.add_css_class("title-2");
        in_box.append(&in_title);
        let members = gtk4::ListBox::new();
        members.add_css_class("boxed-list");
        members.set_selection_mode(gtk4::SelectionMode::None);
        members.update_property(&[gtk4::accessible::Property::Label(&tr!("People in the room"))]);
        in_box.append(&members);
        let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        controls.set_halign(gtk4::Align::Center);
        let btn_mute = gtk4::ToggleButton::builder()
            .child(
                &libadwaita::ButtonContent::builder()
                    .icon_name("microphone-sensitivity-high-symbolic")
                    .label(tr!("Mute"))
                    .build(),
            )
            .css_classes(["pill"])
            .tooltip_text(tr!("Mute your microphone for everyone"))
            .build();
        let btn_leave = gtk4::Button::builder()
            .child(
                &libadwaita::ButtonContent::builder()
                    .icon_name("call-stop-symbolic")
                    .label(tr!("Leave"))
                    .build(),
            )
            .css_classes(["pill", "destructive-action"])
            .build();
        controls.append(&btn_mute);
        controls.append(&btn_leave);
        in_box.append(&controls);
        let in_room = libadwaita::Clamp::builder().maximum_size(720).child(&in_box).build();
        let in_room = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&in_room)
            .build();

        let loading = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        let error_page = libadwaita::StatusPage::builder()
            .icon_name("audio-input-microphone-symbolic")
            .title(tr!("Rooms unavailable"))
            .build();
        let stack = gtk4::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&lobby, Some("lobby"));
        stack.add_named(&in_room, Some("room"));
        stack.add_named(&error_page, Some("error"));
        toolbar.set_content(Some(&stack));

        let page = Self {
            toolbar,
            menu_btn,
            stack,
            list,
            in_title,
            members,
            btn_mute,
            error_page,
            toast: toast.clone(),
            syncing: Rc::new(std::cell::Cell::new(false)),
            rooms: Rc::default(),
        };
        {
            let page = page.clone();
            btn_refresh.connect_clicked(move |_| page.refresh());
        }
        {
            let syncing = page.syncing.clone();
            page.btn_mute.connect_toggled(move |b| {
                set_mute_look(b);
                if !syncing.get() {
                    let muted = b.is_active();
                    std::thread::spawn(move || room::set_muted(muted));
                }
            });
        }
        {
            let page = page.clone();
            btn_leave.connect_clicked(move |_| {
                let page = page.clone();
                super::run_in_background(room::leave, move |()| page.refresh());
            });
        }
        // Room state from the sound engine.
        {
            let page = page.clone();
            let rx = room::events();
            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    page.on_event(ev);
                }
            });
        }
        page
    }

    /// Reload the rooms of my groups (when not in one).
    pub fn refresh(&self) {
        if room::active_group().is_some() {
            return;
        }
        if self.rooms.borrow().is_empty() {
            self.stack.set_visible_child_name("loading");
        }
        let page = self.clone();
        super::run_in_background(invite::rooms, move |r| match r {
            Ok(rooms) => {
                page.fill(&rooms);
                *page.rooms.borrow_mut() = rooms;
                if room::active_group().is_none() {
                    page.stack.set_visible_child_name("lobby");
                }
            }
            Err(e) => {
                page.error_page.set_description(Some(
                    glib::markup_escape_text(&trf!("Could not reach the panel: {error}", "error" => format!("{e:?}"))).as_str(),
                ));
                page.stack.set_visible_child_name("error");
            }
        });
    }

    fn fill(&self, rooms: &[RoomInfo]) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for r in rooms {
            let who: Vec<&str> = r.participants.iter().map(|p| p.user.as_str()).collect();
            let subtitle = if who.is_empty() {
                tr!("Nobody here right now")
            } else {
                trf!("In the room: {people}", "people" => who.join(", "))
            };
            let row = libadwaita::ActionRow::builder()
                .title(glib::markup_escape_text(&r.group).as_str())
                .subtitle(glib::markup_escape_text(&subtitle).as_str())
                .build();
            let icon = if who.is_empty() { "audio-input-microphone-symbolic" } else { "call-start-symbolic" };
            row.add_prefix(&gtk4::Image::from_icon_name(icon));
            let btn = gtk4::Button::builder()
                .label(tr!("Join"))
                .valign(gtk4::Align::Center)
                .css_classes(["suggested-action"])
                .build();
            btn.update_property(&[gtk4::accessible::Property::Label(&trf!("Join the room {group}", "group" => r.group))]);
            let (page, group) = (self.clone(), r.group.clone());
            btn.connect_clicked(move |b| {
                b.set_sensitive(false);
                let (page, group, b) = (page.clone(), group.clone(), b.clone());
                let g = group.clone();
                super::run_in_background(move || room::join(&g), move |r| {
                    b.set_sensitive(true);
                    if let Err(e) = r {
                        super::show_toast(&page.toast, &trf!("Could not join {group}: {error}", "group" => group, "error" => e), 6);
                    }
                });
            });
            row.add_suffix(&btn);
            self.list.append(&row);
        }
    }

    fn on_event(&self, ev: RoomEvent) {
        match ev {
            RoomEvent::State { group: Some(group), muted, members } => {
                self.in_title.set_title(&trf!("Room {group}", "group" => group));
                let talking = members.iter().filter(|m| m.talking).count();
                let people = if members.len() == 1 {
                    tr!("1 person")
                } else {
                    trf!("{count} people", "count" => members.len().to_string())
                };
                self.in_title.set_subtitle(&trf!(
                    "{people} · {talking} talking",
                    "people" => people,
                    "talking" => talking.to_string()
                ));
                self.syncing.set(true);
                self.btn_mute.set_active(muted);
                self.syncing.set(false);
                set_mute_look(&self.btn_mute);
                self.fill_members(&members);
                self.stack.set_visible_child_name("room");
            }
            RoomEvent::State { group: None, .. } => {
                self.stack.set_visible_child_name("lobby");
                self.refresh();
            }
            RoomEvent::Problem(p) => super::show_toast(&self.toast, &p, 6),
        }
    }

    fn fill_members(&self, members: &[MemberView]) {
        while let Some(child) = self.members.first_child() {
            self.members.remove(&child);
        }
        for m in members {
            let mut subtitle = vec![m.device.clone()];
            if m.me {
                subtitle.push(tr!("you"));
            }
            if !m.connected {
                subtitle.push(tr!("connecting…"));
            }
            let row = libadwaita::ActionRow::builder()
                .title(glib::markup_escape_text(&m.user).as_str())
                .subtitle(glib::markup_escape_text(&subtitle.join(" · ")).as_str())
                .build();
            row.add_prefix(&gtk4::Image::from_icon_name("avatar-default-symbolic"));
            let (icon, label) = if m.muted {
                ("microphone-disabled-symbolic", tr!("muted"))
            } else if m.talking {
                ("audio-volume-high-symbolic", tr!("talking"))
            } else {
                ("microphone-sensitivity-low-symbolic", tr!("quiet"))
            };
            let state = gtk4::Image::from_icon_name(icon);
            state.set_tooltip_text(Some(&label));
            state.update_property(&[gtk4::accessible::Property::Label(&label)]);
            if m.talking {
                state.add_css_class("success");
                row.add_css_class("room-talking");
            }
            row.add_suffix(&state);
            self.members.append(&row);
        }
    }
}

fn set_mute_look(b: &gtk4::ToggleButton) {
    let (icon, label) = if b.is_active() {
        ("microphone-disabled-symbolic", tr!("Unmute"))
    } else {
        ("microphone-sensitivity-high-symbolic", tr!("Mute"))
    };
    b.set_child(Some(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).build()));
    if b.is_active() {
        b.add_css_class("destructive-action");
    } else {
        b.remove_css_class("destructive-action");
    }
}
