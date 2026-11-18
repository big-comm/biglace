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
    /// Show my screen to the room (or stop).
    btn_share: gtk4::Button,
    /// "You're showing your screen · 2 watching".
    share_banner: gtk4::Label,
    error_page: libadwaita::StatusPage,
    toast: libadwaita::ToastOverlay,
    /// Reflects the room state while updating the mute toggle ourselves.
    syncing: Rc<std::cell::Cell<bool>>,
    rooms: Rc<RefCell<Vec<RoomInfo>>>,
    /// The people rows, kept (and updated in place) so their animations
    /// run smoothly across updates. Keyed by address.
    member_rows: Rc<RefCell<Vec<(String, MemberRow)>>>,
    /// The group whose room we're in, and who was sharing (to announce a
    /// new share once).
    group: Rc<RefCell<Option<String>>>,
    sharing: Rc<RefCell<std::collections::HashSet<String>>>,
    presenting: Rc<std::cell::Cell<bool>>,
}

/// One person in the room: avatar with a ring that lights up while they
/// talk, name, and live sound waves (or the muted icon).
#[derive(Clone)]
struct MemberRow {
    row: libadwaita::ActionRow,
    muted_icon: gtk4::Image,
    wave: gtk4::DrawingArea,
    muted: Rc<std::cell::Cell<bool>>,
    /// "Screen" / "watching" / "in control" tag.
    screen_tag: gtk4::Label,
    /// Watch their screen.
    btn_watch: gtk4::Button,
    /// (We're presenting) give or take back control.
    btn_control: gtk4::Button,
    /// The latest state of this person.
    view: Rc<RefCell<MemberView>>,
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
        let btn_share = gtk4::Button::builder().css_classes(["pill"]).build();
        set_share_look(&btn_share, false);
        controls.append(&btn_mute);
        controls.append(&btn_share);
        controls.append(&btn_leave);
        let share_banner = gtk4::Label::new(None);
        share_banner.add_css_class("share-banner");
        share_banner.set_halign(gtk4::Align::Center);
        share_banner.set_visible(false);
        in_box.insert_child_after(&share_banner, Some(&in_title));
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
            btn_share,
            share_banner,
            error_page,
            toast: toast.clone(),
            syncing: Rc::new(std::cell::Cell::new(false)),
            rooms: Rc::default(),
            member_rows: Rc::default(),
            group: Rc::default(),
            sharing: Rc::default(),
            presenting: Rc::default(),
        };
        {
            let page = page.clone();
            page.btn_share.clone().connect_clicked(move |b| {
                if page.presenting.get() {
                    std::thread::spawn(room::stage::stop);
                    return;
                }
                b.set_sensitive(false);
                let (page, b) = (page.clone(), b.clone());
                super::run_in_background(room::stage::start, move |r| {
                    b.set_sensitive(true);
                    if let Err(e) = r {
                        super::show_toast(&page.toast, &trf!("Could not share the screen: {error}", "error" => e), 6);
                    }
                });
            });
        }
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

    /// Development builds only (BIGLACE_PREVIEW=room): a sample room.
    #[cfg(debug_assertions)]
    pub fn show_preview(&self) {
        let m = |user: &str, device: &str, ip: &str, me: bool, muted: bool| MemberView {
            user: user.into(),
            device: device.into(),
            ip: ip.into(),
            me,
            muted,
            connected: true,
            talking: false,
            sharing: user == "bruna",
            watching: None,
            controlling: false,
        };
        self.on_event(RoomEvent::State {
            group: Some("familia".into()),
            muted: false,
            presenting: false,
            members: vec![
                m("tales", "notebook", "100.64.0.100", true, false),
                m("bruna", "notebook-bruna", "100.64.0.101", false, false),
                m("bruno", "desktop", "100.64.0.102", false, false),
                m("maria", "celular", "100.64.0.103", false, false),
                m("joao", "pc-sala", "100.64.0.104", false, true),
            ],
        });
        // A few seconds later: we show our screen; Bruno watches, Maria
        // has control.
        let page = self.clone();
        glib::timeout_add_seconds_local_once(6, move || {
            let mut members = vec![
                m("tales", "notebook", "100.64.0.100", true, false),
                m("bruna", "notebook-bruna", "100.64.0.101", false, false),
                m("bruno", "desktop", "100.64.0.102", false, false),
                m("maria", "celular", "100.64.0.103", false, false),
            ];
            members[0].sharing = true;
            members[1].sharing = false;
            members[2].watching = Some(1);
            members[3].watching = Some(2);
            members[3].controlling = true;
            page.on_event(RoomEvent::State { group: Some("familia".into()), muted: false, presenting: true, members });
        });
    }

    fn on_event(&self, ev: RoomEvent) {
        match ev {
            RoomEvent::State { group: Some(group), muted, presenting, members } => {
                *self.group.borrow_mut() = Some(group.clone());
                self.presenting.set(presenting);
                set_share_look(&self.btn_share, presenting);
                let watchers = members.iter().filter(|m| m.watching.is_some()).count();
                self.share_banner.set_visible(presenting);
                self.share_banner.set_text(&if watchers == 0 {
                    tr!("📺 You're showing your screen to the room")
                } else if watchers == 1 {
                    tr!("📺 You're showing your screen · 1 person watching")
                } else {
                    trf!("📺 You're showing your screen · {count} people watching", "count" => watchers.to_string())
                });
                self.announce_shares(&group, &members);
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
            RoomEvent::ControlRequest { viewer, user } => self.ask_control(viewer, &user),
            RoomEvent::State { group: None, .. } => {
                *self.group.borrow_mut() = None;
                self.sharing.borrow_mut().clear();
                self.presenting.set(false);
                self.stack.set_visible_child_name("lobby");
                self.refresh();
            }
            RoomEvent::Problem(p) => super::show_toast(&self.toast, &p, 6),
        }
    }

    fn fill_members(&self, members: &[MemberView]) {
        let mut rows = self.member_rows.borrow_mut();
        // Gone: drop their rows.
        rows.retain(|(ip, r)| {
            let keep = members.iter().any(|m| &m.ip == ip);
            if !keep {
                self.members.remove(&r.row);
            }
            keep
        });
        for m in members {
            let existing = rows.iter().find(|(ip, _)| ip == &m.ip).map(|(_, r)| r.clone());
            let r = match existing {
                Some(r) => r,
                None => {
                    let r = member_row(m, self);
                    self.members.append(&r.row);
                    rows.push((m.ip.clone(), r.clone()));
                    r
                }
            };
            let mut subtitle = vec![m.device.clone()];
            if m.me {
                subtitle.push(tr!("you"));
            }
            if !m.connected {
                subtitle.push(tr!("connecting…"));
            }
            r.row.set_title(glib::markup_escape_text(&m.user).as_str());
            r.row.set_subtitle(glib::markup_escape_text(&subtitle.join(" · ")).as_str());
            r.muted.set(m.muted);
            r.muted_icon.set_visible(m.muted);
            r.wave.set_visible(!m.muted);
            let label = if m.muted {
                tr!("muted")
            } else if m.talking {
                tr!("talking")
            } else {
                tr!("quiet")
            };
            r.row.update_property(&[gtk4::accessible::Property::Description(&label)]);
            *r.view.borrow_mut() = m.clone();
            // Screen share: watch them, or (when we present) their status
            // and control.
            let (tag, tip) = if m.sharing && !m.me {
                (tr!("Sharing screen"), String::new())
            } else if m.controlling {
                (tr!("In control"), tr!("Controls your screen now"))
            } else if m.watching.is_some() {
                (tr!("Watching"), tr!("Watching your screen"))
            } else if m.me && m.sharing {
                (tr!("Sharing screen"), String::new())
            } else {
                (String::new(), String::new())
            };
            r.screen_tag.set_text(&tag);
            r.screen_tag.set_tooltip_text((!tip.is_empty()).then_some(tip.as_str()));
            r.screen_tag.set_visible(!tag.is_empty());
            r.btn_watch.set_visible(m.sharing && !m.me);
            r.btn_control.set_visible(m.watching.is_some());
            set_control_look(&r.btn_control, m.controlling);
        }
    }

    /// Someone starts showing their screen: say so, with "Watch".
    fn announce_shares(&self, group: &str, members: &[MemberView]) {
        let now: std::collections::HashSet<String> =
            members.iter().filter(|m| m.sharing && !m.me).map(|m| m.ip.clone()).collect();
        let before = self.sharing.replace(now.clone());
        for m in members.iter().filter(|m| now.contains(&m.ip) && !before.contains(&m.ip)) {
            let t = libadwaita::Toast::builder()
                .title(glib::markup_escape_text(&trf!("{user} is showing their screen", "user" => m.user)).as_str())
                .button_label(tr!("Watch"))
                .timeout(8)
                .build();
            let (ip, user, group, toast) = (m.ip.clone(), m.user.clone(), group.to_string(), self.toast.clone());
            t.connect_button_clicked(move |_| super::remote_ui::open_stage_viewer(&ip, &user, &group, &toast));
            self.toast.add_toast(t);
        }
    }

    /// A viewer asks to control our screen.
    fn ask_control(&self, viewer: u64, user: &str) {
        let Some(win) = self.toolbar.root().and_downcast::<gtk4::Window>() else { return };
        let dlg = libadwaita::MessageDialog::builder()
            .transient_for(&win)
            .modal(true)
            .heading(trf!("{user} asks to control your screen", "user" => user))
            .body(tr!("They'll be able to use your mouse and keyboard. You can take control back at any time."))
            .build();
        dlg.add_response("deny", &tr!("Deny"));
        dlg.add_response("allow", &tr!("Allow"));
        dlg.set_response_appearance("allow", libadwaita::ResponseAppearance::Suggested);
        dlg.set_default_response(Some("deny"));
        dlg.set_close_response("deny");
        dlg.connect_response(None, move |_, r| {
            if r == "allow" {
                std::thread::spawn(move || room::stage::grant(viewer));
            } else {
                std::thread::spawn(move || room::stage::deny(viewer));
            }
        });
        // Unanswered for a while: no.
        let d = dlg.clone();
        glib::timeout_add_seconds_local_once(60, move || d.close());
        dlg.present();
    }
}

fn set_share_look(b: &gtk4::Button, presenting: bool) {
    let (icon, label) = if presenting {
        ("media-playback-stop-symbolic", tr!("Stop sharing"))
    } else {
        ("video-display-symbolic", tr!("Share screen"))
    };
    b.set_child(Some(&libadwaita::ButtonContent::builder().icon_name(icon).label(label).build()));
    b.set_tooltip_text(Some(&if presenting {
        tr!("Stop showing your screen to the room")
    } else {
        tr!("Show your screen to everyone in the room")
    }));
    if presenting {
        b.add_css_class("destructive-action");
        b.remove_css_class("suggested-action");
    } else {
        b.remove_css_class("destructive-action");
        b.add_css_class("suggested-action");
    }
}

fn set_control_look(b: &gtk4::Button, controlling: bool) {
    b.set_label(&if controlling { tr!("Take back control") } else { tr!("Give control") });
    b.set_tooltip_text(Some(&if controlling {
        tr!("Stop them from using your mouse and keyboard")
    } else {
        tr!("Let them use your mouse and keyboard")
    }));
}

/// Build one person's row: avatar + speaking ring, and the wave meter that
/// animates from their live sound level (`room::level`).
fn member_row(m: &MemberView, page: &RoomsPage) -> MemberRow {
    let row = libadwaita::ActionRow::new();
    row.add_css_class("room-member");
    let view = Rc::new(RefCell::new(m.clone()));
    let screen_tag = gtk4::Label::new(None);
    screen_tag.add_css_class("screen-tag");
    screen_tag.set_valign(gtk4::Align::Center);
    screen_tag.set_visible(false);
    let btn_watch = gtk4::Button::builder()
        .child(&libadwaita::ButtonContent::builder().icon_name("video-display-symbolic").label(tr!("Watch")).build())
        .css_classes(["suggested-action"])
        .valign(gtk4::Align::Center)
        .visible(false)
        .build();
    {
        let (view, page) = (view.clone(), page.clone());
        btn_watch.connect_clicked(move |_| {
            let v = view.borrow().clone();
            if let Some(group) = page.group.borrow().clone() {
                super::remote_ui::open_stage_viewer(&v.ip, &v.user, &group, &page.toast);
            }
        });
    }
    let btn_control = gtk4::Button::builder().valign(gtk4::Align::Center).visible(false).build();
    {
        let view = view.clone();
        btn_control.connect_clicked(move |_| {
            let v = view.borrow().clone();
            match (v.controlling, v.watching) {
                (true, _) => {
                    std::thread::spawn(room::stage::revoke);
                }
                (false, Some(id)) => {
                    std::thread::spawn(move || room::stage::grant(id));
                }
                _ => {}
            }
        });
    }

    let avatar = libadwaita::Avatar::new(40, Some(&m.user), true);
    crate::avatars::bind(&avatar, &m.user);
    let ring = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    ring.add_css_class("speaking-ring");
    ring.set_valign(gtk4::Align::Center);
    ring.append(&avatar);
    row.add_prefix(&ring);

    let muted_icon = gtk4::Image::from_icon_name("microphone-disabled-symbolic");
    muted_icon.add_css_class("room-muted");
    muted_icon.set_tooltip_text(Some(&tr!("muted")));
    muted_icon.set_visible(false);
    row.add_suffix(&muted_icon);

    let wave = gtk4::DrawingArea::builder()
        .content_width(44)
        .content_height(28)
        .valign(gtk4::Align::Center)
        .build();
    wave.add_css_class("voice-wave");
    wave.set_tooltip_text(Some(&tr!("Sound level")));
    row.add_suffix(&wave);
    row.add_suffix(&screen_tag);
    row.add_suffix(&btn_watch);
    row.add_suffix(&btn_control);

    let muted = Rc::new(std::cell::Cell::new(m.muted));
    // Smoothed level: rises fast with the voice, falls gently.
    let shown = Rc::new(std::cell::Cell::new(0.0_f64));
    {
        let shown = shown.clone();
        wave.set_draw_func(move |w, cr, width, height| {
            let level = shown.get();
            let t = w.frame_clock().map_or(0, |c| c.frame_time()) as f64 / 1_000_000.0;
            let (bars, gap) = (5, 4.0);
            let bar_w = (f64::from(width) - gap * f64::from(bars - 1)) / f64::from(bars);
            let h = f64::from(height);
            let accent = w.color();
            let speaking = level > 0.06;
            for i in 0..bars {
                // Each bar has its own phase, so the waves ripple.
                let phase = t * 9.0 + f64::from(i) * 1.3;
                let wiggle = 0.55 + 0.45 * phase.sin().abs();
                let shape = [0.55, 0.8, 1.0, 0.8, 0.55][i as usize];
                let frac = if speaking { (0.2 + 0.8 * level * shape * wiggle).min(1.0) } else { 0.14 };
                let bh = (h * frac).max(4.0);
                let x = f64::from(i) * (bar_w + gap);
                let y = (h - bh) / 2.0;
                let r = (bar_w / 2.0).min(bh / 2.0);
                cr.new_sub_path();
                cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
                cr.arc(x + bar_w - r, y + r, r, 1.5 * std::f64::consts::PI, 0.0);
                cr.arc(x + bar_w - r, y + bh - r, r, 0.0, 0.5 * std::f64::consts::PI);
                cr.arc(x + r, y + bh - r, r, 0.5 * std::f64::consts::PI, std::f64::consts::PI);
                cr.close_path();
                let alpha = if speaking { 0.95 } else { 0.28 };
                cr.set_source_rgba(
                    f64::from(accent.red()),
                    f64::from(accent.green()),
                    f64::from(accent.blue()),
                    alpha,
                );
                let _ = cr.fill();
            }
        });
    }
    {
        let (ip, shown, muted, ring) = (m.ip.clone(), shown.clone(), muted.clone(), ring.clone());
        wave.add_tick_callback(move |w, _| {
            let target = if muted.get() { 0.0 } else { f64::from(room::level(&ip)) };
            let prev = shown.get();
            let next = if target > prev { prev + (target - prev) * 0.55 } else { prev + (target - prev) * 0.12 };
            shown.set(next);
            if next > 0.06 {
                ring.add_css_class("speaking");
                w.add_css_class("speaking");
            } else {
                ring.remove_css_class("speaking");
                w.remove_css_class("speaking");
            }
            w.queue_draw();
            glib::ControlFlow::Continue
        });
    }
    MemberRow { row, muted_icon, wave, muted, screen_tag, btn_watch, btn_control, view }
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
