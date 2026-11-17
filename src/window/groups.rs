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

use crate::invite::{self, GroupInfo, MemberError, MyGroups};
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
    stack: gtk4::Stack,
    list: gtk4::ListBox,
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
            stack,
            list,
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
        page
    }

    /// Reload from the panel (in the background).
    pub fn refresh(&self) {
        if self.state.borrow().is_none() {
            self.stack.set_visible_child_name("loading");
        }
        let page = self.clone();
        super::run_in_background(invite::my_groups, move |result| match result {
            Ok(mine) => {
                (page.on_admin)(mine.is_admin);
                page.fill(&mine);
                page.stack
                    .set_visible_child_name(if mine.groups.is_empty() { "empty" } else { "list" });
                *page.state.borrow_mut() = Some(mine);
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
        let people = trf!("{count} people", "count" => g.members.len().to_string());
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
            member.add_prefix(&gtk4::Image::from_icon_name("avatar-default-symbolic"));
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
            invite_btn.connect_clicked(move |_| {
                page.invites.invite_to(&page.win, &page.toast, Some(group.clone()));
            });
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
