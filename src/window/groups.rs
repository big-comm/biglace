//! "Groups" section: the groups we're in, and groups we create ourselves.
//!
//! People in the same group reach each other's devices. Anyone can create a
//! group from here and becomes its owner: invites people into it, removes
//! them, deletes it. Groups the administrator wrote in the network's rules
//! are listed too, but only the administrator changes those.

use gtk4::{glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::invite::{self, GroupInfo, GroupRequest, GroupRequests, MemberError, MyGroups, Person};
use crate::tr;
use crate::trf;

use super::invites::InvitesPage;

/// Sentence for a failed group operation.
fn explain(err: &MemberError) -> String {
    match err {
        MemberError::Unsupported => tr!(
            "Groups from BigLace need a newer BigScale server. Ask your administrator to update it."
        ),
        MemberError::NotAMember => {
            tr!("This device isn't linked to a person's account, so it has no groups.")
        }
        MemberError::KeyMismatch => tr!(
            "The panel knows this device by a different key (BigLace was reinstalled?). Leave the network and join again to fix it."
        ),
        MemberError::Unverified => tr!(
            "The panel couldn't confirm this device. Make sure BigLace is up to date and stays open while connected, then try again."
        ),
        MemberError::OpenNetwork => tr!(
            "On this network everyone already reaches everyone, so groups aren't needed. The administrator can set access rules first."
        ),
        MemberError::TooMany => tr!("You already created the maximum number of groups."),
        MemberError::NotInGroup => tr!("You are no longer in that group."),
        MemberError::Other(code) => match code.as_str() {
            "name_taken" => tr!("A group with that name already exists. Choose another name."),
            "invalid_name" => tr!("Use lowercase letters, digits and hyphens, starting with a letter or digit."),
            "in_use" => tr!(
                "The administrator uses this group in the network's rules, so it can't be deleted. Ask the administrator."
            ),
            "not_owner" => tr!("Only the person who created the group can do that."),
            "owner_cannot_leave" => tr!("You created this group: delete it instead of leaving."),
            "not_member_group" => tr!("This group belongs to the administrator; only they can change it."),
            "not_found" => tr!("That person is no longer in the group."),
            "group_gone" => tr!("That group no longer exists, or whoever invited you left it."),
            "no_such_user" => tr!("That person is no longer on the network."),
            "already_member" => tr!("You're already in that group."),
            "nothing_to_join" => tr!("This invite only brings new people into the network, and you're already in it."),
            "used" => tr!("This invite was already used. Ask for a new one."),
            "expired" => tr!("This invite expired. Ask for a new one."),
            "revoked" | "unknown" => tr!("This invite is no longer valid. Ask for a new one."),
            "pending" => tr!("This invite is still waiting for an administrator's approval."),
            "too_many_attempts" => tr!("Too many attempts. Wait a minute and try again."),
            other => trf!("Could not reach the panel: {error}", "error" => other),
        },
    }
}

/// What someone typed → a name the panel accepts ("Família Silva" → "familia-silva").
pub fn group_name_from(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars().flat_map(char::to_lowercase) {
        let ch = match ch {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        };
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(32);
    out.trim_end_matches('-').to_string()
}

#[derive(Clone)]
pub struct GroupsPage {
    pub toolbar: libadwaita::ToolbarView,
    pub menu_btn: gtk4::MenuButton,
    stack: gtk4::Stack,
    list: gtk4::ListBox,
    /// "Join my group?" requests waiting for us to answer.
    requests_box: gtk4::Box,
    requests: gtk4::ListBox,
    /// Requests already announced with a desktop notification.
    notified: Rc<RefCell<std::collections::HashSet<String>>>,
    incoming: Rc<RefCell<Vec<GroupRequest>>>,
    error_page: libadwaita::StatusPage,
    state: Rc<RefCell<Option<MyGroups>>>,
    win: libadwaita::ApplicationWindow,
    toast: libadwaita::ToastOverlay,
    invites: InvitesPage,
    /// Told whether we're an admin after each refresh (menu entry).
    on_admin: Rc<dyn Fn(bool)>,
}

impl GroupsPage {
    pub fn build(
        win: &libadwaita::ApplicationWindow,
        toast: &libadwaita::ToastOverlay,
        invites: &InvitesPage,
        on_admin: impl Fn(bool) + 'static,
    ) -> Self {
        let toolbar = libadwaita::ToolbarView::new();
        let header = libadwaita::HeaderBar::new();
        header.set_title_widget(Some(&libadwaita::WindowTitle::new(&tr!("Groups"), "")));
        let btn_new = gtk4::Button::builder()
            .child(
                &libadwaita::ButtonContent::builder()
                    .icon_name("list-add-symbolic")
                    .label(tr!("New group"))
                    .build(),
            )
            .tooltip_text(tr!("Create a group"))
            .build();
        btn_new.add_css_class("suggested-action");
        let btn_refresh = gtk4::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(tr!("Refresh"))
            .build();
        // Same main menu as the Devices page (model set in setup_menu).
        let menu_btn = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr!("Main menu"))
            .build();
        header.pack_end(&menu_btn);
        header.pack_start(&btn_new);
        header.pack_end(&btn_refresh);
        toolbar.add_top_bar(&header);

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        body.set_margin_top(24);
        body.set_margin_bottom(24);
        body.set_margin_start(12);
        body.set_margin_end(12);
        let intro = gtk4::Label::new(Some(&tr!(
            "People in the same group reach each other's devices. Create groups for family, work or friends, and invite people into them."
        )));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        intro.add_css_class("dim-label");
        body.append(&intro);

        let requests_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        let requests_title = gtk4::Label::new(Some(&tr!("Invitations for you")));
        requests_title.add_css_class("heading");
        requests_title.set_xalign(0.0);
        requests_box.append(&requests_title);
        let requests = gtk4::ListBox::new();
        requests.add_css_class("boxed-list");
        requests.set_selection_mode(gtk4::SelectionMode::None);
        requests.update_property(&[gtk4::accessible::Property::Label(&tr!("Invitations for you"))]);
        requests_box.append(&requests);
        requests_box.set_visible(false);
        body.append(&requests_box);

        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.update_property(&[gtk4::accessible::Property::Label(&tr!("Your groups"))]);
        body.append(&list);

        let clamp = libadwaita::Clamp::builder().maximum_size(720).child(&body).build();
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let btn_first = gtk4::Button::builder()
            .label(tr!("Create a group"))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk4::Align::Center)
            .build();
        let empty = libadwaita::StatusPage::builder()
            .icon_name("system-users-symbolic")
            .title(tr!("No groups yet"))
            .description(tr!(
                "A group lets several people reach each other at once — the family, the office, a class."
            ))
            .child(&btn_first)
            .build();
        let loading = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        let error_page = libadwaita::StatusPage::builder()
            .icon_name("system-users-symbolic")
            .title(tr!("Groups unavailable"))
            .build();

        let stack = gtk4::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&scroll, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&error_page, Some("error"));
        toolbar.set_content(Some(&stack));

        let page = Self {
            toolbar,
            menu_btn,
            stack,
            list,
            requests_box,
            requests,
            notified: Rc::default(),
            incoming: Rc::default(),
            error_page,
            state: Rc::new(RefCell::new(None)),
            win: win.clone(),
            toast: toast.clone(),
            invites: invites.clone(),
            on_admin: Rc::new(on_admin),
        };
        for b in [&btn_new, &btn_first] {
            let page = page.clone();
            b.connect_clicked(move |_| page.show_create());
        }
        {
            let page = page.clone();
            btn_refresh.connect_clicked(move |_| page.refresh());
        }
        // New "join my group?" requests while connected (the list is only
        // loaded while signed in and online — see clear()).
        {
            let page = page.clone();
            glib::timeout_add_seconds_local(60, move || {
                if page.state.borrow().is_some() {
                    let p = page.clone();
                    super::run_in_background(invite::group_requests, move |r| {
                        if let Ok(r) = r {
                            if p.state.borrow().is_some() {
                                p.show_requests(&r);
                            }
                        }
                    });
                }
                glib::ControlFlow::Continue
            });
        }
        page
    }

    /// Reload from the panel (in the background).
    pub fn refresh(&self) {
        if self.state.borrow().is_none() {
            self.stack.set_visible_child_name("loading");
        }
        let page = self.clone();
        // Requests are optional: an older panel without them still lists groups.
        let load = || invite::my_groups().map(|g| (g, invite::group_requests().unwrap_or_default()));
        super::run_in_background(load, move |result| match result {
            Ok((mine, requests)) => {
                (page.on_admin)(mine.is_admin);
                page.fill(&mine);
                *page.state.borrow_mut() = Some(mine);
                page.show_requests(&requests);
            }
            Err(e) => {
                (page.on_admin)(false);
                *page.state.borrow_mut() = None;
                page.error_page
                    .set_description(Some(glib::markup_escape_text(&explain(&e)).as_str()));
                page.stack.set_visible_child_name("error");
            }
        });
    }

    /// Forget what we showed (signed out / disconnected).
    pub fn clear(&self) {
        *self.state.borrow_mut() = None;
        (self.on_admin)(false);
    }

    /// Which page to show: the list when there are groups or requests.
    fn show_list_or_empty(&self) {
        let groups = self.state.borrow().as_ref().is_some_and(|m| !m.groups.is_empty());
        let requests = !self.incoming.borrow().is_empty();
        self.stack
            .set_visible_child_name(if groups || requests { "list" } else { "empty" });
    }

    /// Show the requests waiting for us; announce the new ones.
    fn show_requests(&self, r: &GroupRequests) {
        if *self.incoming.borrow() != r.incoming {
            while let Some(child) = self.requests.first_child() {
                self.requests.remove(&child);
            }
            for req in &r.incoming {
                self.requests.append(&self.request_row(req));
            }
            self.requests_box.set_visible(!r.incoming.is_empty());
            *self.incoming.borrow_mut() = r.incoming.clone();
        }
        let app = self.win.application();
        let mut notified = self.notified.borrow_mut();
        for req in &r.incoming {
            if notified.insert(req.id.clone()) {
                if let Some(app) = &app {
                    let n = gtk4::gio::Notification::new(&tr!("Invitation to a group"));
                    n.set_body(Some(&trf!(
                        "{user} invites you to the group {group}. Open BigLace to accept or decline.",
                        "user" => req.from,
                        "group" => req.group
                    )));
                    app.send_notification(Some(&notification_id(&req.id)), &n);
                }
            }
        }
        drop(notified);
        self.show_list_or_empty();
    }

    fn request_row(&self, req: &GroupRequest) -> libadwaita::ActionRow {
        let row = libadwaita::ActionRow::builder()
            .title(glib::markup_escape_text(&trf!(
                "{user} invites you to {group}",
                "user" => req.from,
                "group" => req.group
            )).as_str())
            .subtitle(tr!("Joining lets you and the group's members reach each other's devices."))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("mail-unread-symbolic"));
        let decline = gtk4::Button::builder()
            .label(tr!("Decline"))
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        let accept = gtk4::Button::builder()
            .label(tr!("Accept"))
            .valign(gtk4::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        for (btn, yes) in [(&decline, false), (&accept, true)] {
            let (page, id, group) = (self.clone(), req.id.clone(), req.group.clone());
            let (a, d) = (accept.clone(), decline.clone());
            btn.connect_clicked(move |_| {
                a.set_sensitive(false);
                d.set_sensitive(false);
                if let Some(app) = page.win.application() {
                    app.withdraw_notification(&notification_id(&id));
                }
                let done = if yes {
                    trf!("You joined {group}.", "group" => group)
                } else {
                    tr!("Invitation declined.")
                };
                let id = id.clone();
                page.run(move || invite::answer_request(&id, yes), done);
            });
        }
        row.add_suffix(&decline);
        row.add_suffix(&accept);
        row
    }

    fn fill(&self, mine: &MyGroups) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for g in &mine.groups {
            self.list.append(&self.row(g, &mine.user));
        }
    }

    fn row(&self, g: &GroupInfo, me: &str) -> libadwaita::ExpanderRow {
        let owner_is_me = g.owner.as_deref() == Some(me);
        let people = if g.members.len() == 1 {
            tr!("1 person")
        } else {
            trf!("{count} people", "count" => g.members.len().to_string())
        };
        let origin = match (&g.owner, g.is_member_group()) {
            (Some(_), true) if owner_is_me => tr!("Created by you"),
            (Some(o), true) => trf!("Created by {user}", "user" => o),
            _ => tr!("Network group (administrator)"),
        };
        let row = libadwaita::ExpanderRow::builder()
            .title(glib::markup_escape_text(&g.name).as_str())
            .subtitle(glib::markup_escape_text(&format!("{origin} · {people}")).as_str())
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("system-users-symbolic"));

        for m in &g.members {
            let mut tags = Vec::new();
            if m == me {
                tags.push(tr!("you"));
            }
            if g.owner.as_deref() == Some(m.as_str()) {
                tags.push(tr!("owner"));
            }
            let member = libadwaita::ActionRow::builder()
                .title(glib::markup_escape_text(m).as_str())
                .subtitle(tags.join(" · "))
                .build();
            let avatar = libadwaita::Avatar::new(32, Some(m), true);
            crate::avatars::bind(&avatar, m);
            member.add_prefix(&avatar);
            if owner_is_me && m != me {
                let btn = gtk4::Button::builder()
                    .icon_name("list-remove-symbolic")
                    .valign(gtk4::Align::Center)
                    .css_classes(["flat", "circular"])
                    .tooltip_text(trf!("Remove {user} from the group", "user" => m))
                    .build();
                btn.update_property(&[gtk4::accessible::Property::Label(
                    &trf!("Remove {user} from the group", "user" => m),
                )]);
                let (page, group, user) = (self.clone(), g.name.clone(), m.clone());
                btn.connect_clicked(move |_| page.confirm_remove(&group, &user));
                member.add_suffix(&btn);
            }
            row.add_row(&member);
        }

        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        actions.set_margin_top(10);
        actions.set_margin_bottom(10);
        actions.set_margin_start(12);
        actions.set_margin_end(12);
        let invite_btn = gtk4::Button::builder()
            .label(tr!("Invite to this group"))
            .css_classes(["suggested-action"])
            .build();
        {
            let (page, group) = (self.clone(), g.name.clone());
            invite_btn.connect_clicked(move |_| page.show_people(&group));
        }
        actions.append(&invite_btn);
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        if g.is_member_group() && owner_is_me {
            let del = gtk4::Button::builder()
                .label(tr!("Delete group"))
                .css_classes(["destructive-action"])
                .build();
            let (page, group) = (self.clone(), g.name.clone());
            del.connect_clicked(move |_| page.confirm_delete(&group));
            actions.append(&del);
        } else if g.is_member_group() {
            let leave = gtk4::Button::builder()
                .label(tr!("Leave group"))
                .css_classes(["flat", "error"])
                .build();
            let (page, group, me) = (self.clone(), g.name.clone(), me.to_string());
            leave.connect_clicked(move |_| page.confirm_leave(&group, &me));
            actions.append(&leave);
        }
        let holder = gtk4::ListBoxRow::builder().activatable(false).child(&actions).build();
        row.add_row(&holder);
        row
    }

    fn ask(&self, heading: &str, body: &str, verb: &str, then: impl Fn() + 'static) {
        let dlg = libadwaita::MessageDialog::new(Some(&self.win), Some(heading), Some(body));
        dlg.add_response("cancel", &tr!("Cancel"));
        dlg.add_response("ok", verb);
        dlg.set_response_appearance("ok", libadwaita::ResponseAppearance::Destructive);
        dlg.set_default_response(Some("cancel"));
        dlg.set_close_response("cancel");
        dlg.connect_response(None, move |d, r| {
            d.close();
            if r == "ok" {
                then();
            }
        });
        dlg.present();
    }

    /// Run a group call in the background, then refresh or explain.
    fn run(&self, call: impl FnOnce() -> Result<(), MemberError> + Send + 'static, done: String) {
        let page = self.clone();
        super::run_in_background(call, move |r| {
            match r {
                Ok(()) => super::show_toast(&page.toast, &done, 3),
                Err(e) => super::show_toast(&page.toast, &explain(&e), 8),
            }
            page.refresh();
        });
    }

    fn confirm_remove(&self, group: &str, user: &str) {
        let page = self.clone();
        let (g, u) = (group.to_string(), user.to_string());
        self.ask(
            &trf!("Remove {user}?", "user" => user),
            &trf!(
                "{user} leaves the group {group} and no longer reaches its members through it.",
                "user" => user,
                "group" => group
            ),
            &tr!("Remove"),
            move || {
                let (g2, u2) = (g.clone(), u.clone());
                page.run(
                    move || invite::remove_member(&g2, &u2),
                    trf!("{user} was removed from the group.", "user" => u),
                );
            },
        );
    }

    fn confirm_leave(&self, group: &str, me: &str) {
        let page = self.clone();
        let (g, m) = (group.to_string(), me.to_string());
        self.ask(
            &trf!("Leave {group}?", "group" => group),
            &tr!("You stop reaching the group's members through it. To come back, someone has to invite you again."),
            &tr!("Leave"),
            move || {
                let (g2, m2) = (g.clone(), m.clone());
                page.run(move || invite::remove_member(&g2, &m2), tr!("You left the group."));
            },
        );
    }

    fn confirm_delete(&self, group: &str) {
        let page = self.clone();
        let g = group.to_string();
        self.ask(
            &trf!("Delete {group}?", "group" => group),
            &tr!("Its members stop reaching each other through this group. People stay on the network."),
            &tr!("Delete"),
            move || {
                let g2 = g.clone();
                page.run(move || invite::delete_group(&g2), tr!("Group deleted."));
            },
        );
    }

    /// An invite link opened on a device already signed in to that network:
    /// no new account, it just brings us into the link's group (after asking).
    pub fn open_link(&self, inv: invite::Invite) {
        let page = self.clone();
        let preview = inv.clone();
        super::run_in_background(move || invite::redeem_as_member(&preview, true), move |r| {
            let link = match r {
                Ok(l) => l,
                Err(e) => return super::show_toast(&page.toast, &explain(&e), 8),
            };
            let (heading, body) = match (&link.group, &link.invited_by) {
                (Some(g), Some(u)) => (
                    trf!("Join {group}?", "group" => g),
                    trf!("{user} invited you. You and the group's members will reach each other's devices.", "user" => u),
                ),
                (Some(g), None) => (
                    trf!("Join {group}?", "group" => g),
                    tr!("You and the group's members will reach each other's devices."),
                ),
                (None, Some(u)) => (
                    trf!("Connect with {user}?", "user" => u),
                    trf!("You and {user} will reach each other's devices.", "user" => u),
                ),
                (None, None) => return,
            };
            let dlg = libadwaita::MessageDialog::new(Some(&page.win), Some(&heading), Some(&body));
            dlg.add_response("cancel", &tr!("Cancel"));
            dlg.add_response("ok", &tr!("Join"));
            dlg.set_response_appearance("ok", libadwaita::ResponseAppearance::Suggested);
            dlg.set_default_response(Some("ok"));
            dlg.set_close_response("cancel");
            let page2 = page.clone();
            dlg.connect_response(None, move |d, r| {
                d.close();
                if r != "ok" {
                    return;
                }
                let inv = inv.clone();
                let done = match &link.group {
                    Some(g) => trf!("You joined {group}.", "group" => g),
                    None => tr!("Done. You can now reach each other's devices."),
                };
                page2.run(move || invite::redeem_as_member(&inv, false).map(|_| ()), done);
            });
            page.win.present();
            dlg.present();
        });
    }

    /// "Invite to this group": pick people already on the network (they get
    /// a request to accept), or share a link with someone who isn't yet.
    fn show_people(&self, group: &str) {
        let dlg = libadwaita::Window::builder()
            .transient_for(&self.win)
            .modal(true)
            .title(trf!("Invite to {group}", "group" => group))
            .default_width(460)
            .default_height(560)
            .build();
        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        outer.append(&libadwaita::HeaderBar::new());

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        body.set_vexpand(true);
        let intro = gtk4::Label::new(Some(&tr!(
            "Choose people on your network. Each one gets the invitation in BigLace and decides whether to join."
        )));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        intro.add_css_class("dim-label");
        body.append(&intro);
        let search = gtk4::SearchEntry::builder()
            .placeholder_text(tr!("Search people"))
            .build();
        body.append(&search);

        // Inviting someone not listed (the network shows only our
        // contacts): by the exact name they have here.
        let by_name = libadwaita::ActionRow::builder()
            .activatable(true)
            .subtitle(tr!("If someone on the network has exactly this name, they get the invitation."))
            .visible(false)
            .build();
        by_name.add_prefix(&gtk4::Image::from_icon_name("list-add-symbolic"));
        let by_name_list = gtk4::ListBox::new();
        by_name_list.add_css_class("boxed-list");
        by_name_list.set_selection_mode(gtk4::SelectionMode::None);
        by_name_list.append(&by_name);
        by_name_list.set_visible(false);
        body.append(&by_name_list);

        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.update_property(&[gtk4::accessible::Property::Label(&tr!("People on the network"))]);
        let status = libadwaita::ActionRow::builder().title(tr!("Loading…")).build();
        list.set_placeholder(Some(&status));
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        body.append(&scroll);
        {
            let search = search.clone();
            list.set_filter_func(move |row| {
                let q = search.text().to_lowercase();
                q.is_empty() || row.widget_name().to_lowercase().contains(&q)
            });
        }
        let contacts_only = Rc::new(std::cell::Cell::new(false));
        let chosen: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let (list, by_name, by_name_list, contacts_only) =
                (list.clone(), by_name.clone(), by_name_list.clone(), contacts_only.clone());
            search.connect_search_changed(move |s| {
                list.invalidate_filter();
                let typed = s.text().trim().to_lowercase();
                let listed = std::iter::successors(list.first_child(), |w| w.next_sibling())
                    .any(|w| w.widget_name() == typed.as_str());
                let show = contacts_only.get() && invite::user_name_ok(&typed) && !listed;
                if show {
                    by_name.set_title(glib::markup_escape_text(&trf!("Invite \"{name}\"", "name" => typed)).as_str());
                }
                by_name.set_visible(show);
                by_name_list.set_visible(show);
            });
        }

        let err = gtk4::Label::new(None);
        err.add_css_class("error");
        err.set_wrap(true);
        body.append(&err);
        let send = gtk4::Button::builder()
            .label(tr!("Send invitations"))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk4::Align::Center)
            .sensitive(false)
            .build();
        body.append(&send);
        let link = gtk4::Button::builder()
            .label(tr!("Someone not on the network yet? Share an invite link"))
            .css_classes(["flat"])
            .halign(gtk4::Align::Center)
            .build();
        body.append(&link);
        outer.append(&body);
        dlg.set_content(Some(&outer));

        {
            let (page, dlg, group) = (self.clone(), dlg.clone(), group.to_string());
            link.connect_clicked(move |_| {
                dlg.close();
                page.invites.invite_to(&page.win, &page.toast, Some(group.clone()));
            });
        }
        {
            let (page, dlg, group, chosen, err) = (self.clone(), dlg.clone(), group.to_string(), chosen.clone(), err.clone());
            send.connect_clicked(move |b| {
                let users = chosen.borrow().clone();
                if users.is_empty() {
                    return;
                }
                b.set_sensitive(false);
                err.set_text("");
                let (page, dlg, err, b, g2) = (page.clone(), dlg.clone(), err.clone(), b.clone(), group.clone());
                super::run_in_background(
                    move || invite::ask_into_group(&g2, &users),
                    move |r| match r {
                        Ok(n) => {
                            dlg.close();
                            let msg = if n == 1 {
                                tr!("Invitation sent. You'll see the person in the group once they accept.")
                            } else {
                                trf!("{count} invitations sent. People show up in the group as they accept.", "count" => n.to_string())
                            };
                            super::show_toast(&page.toast, &msg, 5);
                        }
                        Err(e) => {
                            b.set_sensitive(true);
                            err.set_text(&explain(&e));
                        }
                    },
                );
            });
        }

        {
            let (list, search, chosen, send) = (list.clone(), search.clone(), chosen.clone(), send.clone());
            by_name.connect_activated(move |_| {
                let typed = search.text().trim().to_lowercase();
                let person = Person { user: typed, online: false, state: "typed".into() };
                let row = person_row(&person, &chosen, &send);
                list.append(&row);
                libadwaita::prelude::ActionRowExt::activate(&row);
                search.set_text("");
            });
        }
        let g = group.to_string();
        let (list2, status2, send2, intro2) = (list.clone(), status.clone(), send.clone(), intro.clone());
        super::run_in_background(
            move || invite::people(&g),
            move |r| match r {
                Ok(found) => {
                    if found.directory == "contacts" {
                        contacts_only.set(true);
                        intro2.set_text(&tr!(
                            "Listed are the people you're already connected to. To invite someone else on the network, type their exact name. Each one gets the invitation in BigLace and decides whether to join."
                        ));
                        status2.set_title(&tr!("Nobody you're connected to yet. Type someone's exact name to invite them."));
                    } else {
                        status2.set_title(&tr!("Nobody else is on the network yet."));
                    }
                    for p in &found.people {
                        list2.append(&person_row(p, &chosen, &send2));
                    }
                }
                Err(e) => status2.set_title(glib::markup_escape_text(&explain(&e)).as_str()),
            },
        );
        dlg.present();
        search.grab_focus();
    }

    fn show_create(&self) {
        let dlg = libadwaita::Window::builder()
            .transient_for(&self.win)
            .modal(true)
            .title(tr!("New group"))
            .default_width(440)
            .build();
        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        outer.append(&libadwaita::HeaderBar::new());
        let pref = libadwaita::PreferencesPage::new();
        let grp = libadwaita::PreferencesGroup::builder()
            .description(tr!(
                "You'll be the group's owner: you invite people into it and can remove them. Everyone in the group reaches each other's devices."
            ))
            .build();
        let entry = libadwaita::EntryRow::builder().title(tr!("Group name")).build();
        grp.add(&entry);
        let hint = gtk4::Label::new(None);
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        hint.set_xalign(0.0);
        hint.set_margin_top(6);
        hint.set_wrap(true);
        grp.add(&hint);
        pref.add(&grp);
        outer.append(&pref);

        let err = gtk4::Label::new(None);
        err.add_css_class("error");
        err.set_wrap(true);
        err.set_margin_start(16);
        err.set_margin_end(16);
        outer.append(&err);
        let btn = gtk4::Button::builder()
            .label(tr!("Create group"))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk4::Align::Center)
            .margin_top(6)
            .margin_bottom(18)
            .sensitive(false)
            .build();
        outer.append(&btn);
        dlg.set_content(Some(&outer));

        hint.set_text(&tr!("For example: family, work, friends."));
        {
            let (hint, btn) = (hint.clone(), btn.clone());
            entry.connect_changed(move |e| {
                let name = group_name_from(&e.text());
                btn.set_sensitive(invite::group_name_ok(&name));
                hint.set_text(&if name.is_empty() {
                    tr!("For example: family, work, friends.")
                } else {
                    trf!("It will be called \"{name}\".", "name" => name)
                });
            });
        }
        {
            let btn = btn.clone();
            entry.connect_entry_activated(move |_| {
                if btn.is_sensitive() {
                    btn.emit_clicked();
                }
            });
        }
        {
            let (page, dlg, entry, err) = (self.clone(), dlg.clone(), entry.clone(), err.clone());
            btn.connect_clicked(move |b| {
                let name = group_name_from(&entry.text());
                err.set_text("");
                b.set_sensitive(false);
                let (page, dlg, err, b) = (page.clone(), dlg.clone(), err.clone(), b.clone());
                let n = name.clone();
                super::run_in_background(
                    move || invite::create_group(&n),
                    move |r| match r {
                        Ok(g) => {
                            dlg.close();
                            super::show_toast(
                                &page.toast,
                                &trf!("Group {group} created. Now invite people into it.", "group" => g.name),
                                5,
                            );
                            page.refresh();
                        }
                        Err(e) => {
                            b.set_sensitive(true);
                            err.set_text(&explain(&e));
                        }
                    },
                );
            });
        }
        dlg.present();
        entry.grab_focus();
    }
}

fn notification_id(request: &str) -> String {
    format!("biglace-group-request-{request}")
}

/// One person in the "Invite to this group" list: a check box when they can
/// be asked, or why not.
fn person_row(p: &Person, chosen: &Rc<RefCell<Vec<String>>>, send: &gtk4::Button) -> libadwaita::ActionRow {
    let mut details = match p.state.as_str() {
        "typed" => vec![tr!("typed name")],
        _ if p.online => vec![tr!("online")],
        _ => vec![tr!("offline")],
    };
    match p.state.as_str() {
        "member" => details.push(tr!("already in the group")),
        "pending" => details.push(tr!("invitation sent, waiting")),
        _ => {}
    }
    let row = libadwaita::ActionRow::builder()
        .title(glib::markup_escape_text(&p.user).as_str())
        .subtitle(details.join(" · "))
        .build();
    row.set_widget_name(&p.user);
    let avatar = libadwaita::Avatar::new(32, Some(&p.user), true);
    if p.state != "typed" {
        crate::avatars::bind(&avatar, &p.user);
    }
    row.add_prefix(&avatar);
    if p.state == "available" || p.state == "typed" {
        let check = gtk4::CheckButton::builder().valign(gtk4::Align::Center).build();
        check.update_property(&[gtk4::accessible::Property::Label(&trf!("Invite {user}", "user" => p.user))]);
        row.add_suffix(&check);
        row.set_activatable_widget(Some(&check));
        let (chosen, send, user) = (chosen.clone(), send.clone(), p.user.clone());
        check.connect_toggled(move |c| {
            let mut list = chosen.borrow_mut();
            list.retain(|u| u != &user);
            if c.is_active() {
                list.push(user.clone());
            }
            send.set_sensitive(!list.is_empty());
        });
    } else {
        row.set_sensitive(false);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::group_name_from;

    #[test]
    fn typed_names_become_group_names() {
        assert_eq!(group_name_from("Família Silva"), "familia-silva");
        assert_eq!(group_name_from("  Trabalho!! 2026 "), "trabalho-2026");
        assert_eq!(group_name_from("---"), "");
        assert_eq!(group_name_from(&"a".repeat(50)).len(), 32);
        assert!(crate::invite::group_name_ok(&group_name_from("Ação Social")));
        assert!(!crate::invite::group_name_ok("admins"));
    }
}
