//! "Invites" section and the "Invite someone" dialog.
//!
//! Any member can invite: the panel (reached over the tailnet, identified by
//! the tunnel) creates a single-use link with no expiry. Whoever joins
//! through it reaches the inviter's devices and vice versa, plus optionally
//! one group the inviter is in.

use gtk4::{gdk, glib, prelude::*};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::invite::{self, MemberError, MyInvites, SentInvite};
use crate::tr;
use crate::trf;

/// Pixels per QR module and quiet-zone width (modules), as the spec asks.
const QR_SCALE: usize = 8;
const QR_MARGIN: usize = 4;

/// Black-on-white texture of the QR code for `text`.
pub fn qr_texture(text: &str) -> Option<gdk::MemoryTexture> {
    let modules = invite::qr_modules(text)?;
    let n = modules.len();
    let side = (n + QR_MARGIN * 2) * QR_SCALE;
    // RGBA: the format every GDK backend handles natively.
    let mut pixels = vec![255u8; side * side * 4];
    for (y, row) in modules.iter().enumerate() {
        for (x, &dark) in row.iter().enumerate() {
            if !dark {
                continue;
            }
            for dy in 0..QR_SCALE {
                let py = (y + QR_MARGIN) * QR_SCALE + dy;
                let start = (py * side + (x + QR_MARGIN) * QR_SCALE) * 4;
                for i in (start..start + QR_SCALE * 4).step_by(4) {
                    pixels[i..i + 3].fill(0); // RGB black, alpha stays opaque
                }
            }
        }
    }
    Some(gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(pixels),
        side * 4,
    ))
}

/// Sentence for a member-API failure.
fn explain(err: &MemberError) -> String {
    match err {
        MemberError::Unsupported => tr!(
            "Invites from BigLace need a BigScale server that supports them. \
             Ask your administrator to update the server, or to create the invite in the panel."
        ),
        MemberError::NotAMember => {
            tr!("This device can't send invites (it isn't linked to a person's account).")
        }
        MemberError::KeyMismatch => tr!(
            "The panel knows this device by a different key (BigLace was reinstalled?). Leave the network and join again to fix it."
        ),
        MemberError::Unverified => tr!(
            "The panel couldn't confirm this device. Make sure BigLace is up to date and stays open while connected, then try again."
        ),
        MemberError::NotInGroup => tr!("You are no longer in that group."),
        MemberError::OpenNetwork => tr!(
            "On this network everyone can reach everyone, so only the administrator can invite people."
        ),
        MemberError::TooMany => tr!("You have too many pending invites. Cancel some or try again tomorrow."),
        MemberError::Other(detail) => trf!("Could not reach the panel: {error}", "error" => detail),
    }
}

fn status_label(status: &str) -> String {
    match status {
        "active" => tr!("Waiting to be used"),
        "used" => tr!("Used"),
        "expired" => tr!("Expired"),
        "revoked" => tr!("Cancelled"),
        other => other.to_string(),
    }
}

#[derive(Clone)]
pub struct InvitesPage {
    pub toolbar: libadwaita::ToolbarView,
    pub menu_btn: gtk4::MenuButton,
    stack: gtk4::Stack,
    list: gtk4::ListBox,
    error_page: libadwaita::StatusPage,
    /// Last answer from the panel; the invite dialog offers its groups.
    state: Rc<RefCell<Option<MyInvites>>>,
}

impl InvitesPage {
    pub fn build(btn_invite: &gtk4::Button) -> Self {
        let toolbar = libadwaita::ToolbarView::new();
        let header = libadwaita::HeaderBar::new();
        header.set_title_widget(Some(&libadwaita::WindowTitle::new(&tr!("Invites"), "")));
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
        header.pack_end(&btn_refresh);
        toolbar.add_top_bar(&header);

        // Intro + primary action, then the list of sent invites.
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        body.set_margin_top(24);
        body.set_margin_bottom(24);
        body.set_margin_start(12);
        body.set_margin_end(12);

        let hero = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        hero.add_css_class("card");
        hero.add_css_class("invite-hero");
        let hero_title = gtk4::Label::new(Some(&tr!("Bring someone to your network")));
        hero_title.add_css_class("title-3");
        hero_title.set_xalign(0.0);
        let hero_text = gtk4::Label::new(Some(&tr!(
            "Create a link or QR code and send it to the person. When they join, \
             you two can reach each other's devices — no administrator needed. \
             The link works once and stays valid until it's used or you cancel it."
        )));
        hero_text.set_wrap(true);
        hero_text.set_xalign(0.0);
        hero_text.add_css_class("dim-label");
        btn_invite.set_halign(gtk4::Align::Start);
        hero.append(&hero_title);
        hero.append(&hero_text);
        hero.append(btn_invite);
        body.append(&hero);

        let sent_title = gtk4::Label::new(Some(&tr!("Invites you sent")));
        sent_title.add_css_class("heading");
        sent_title.set_xalign(0.0);
        body.append(&sent_title);

        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        let empty = libadwaita::ActionRow::builder()
            .title(tr!("No invites yet"))
            .build();
        list.set_placeholder(Some(&empty));
        body.append(&list);

        let clamp = libadwaita::Clamp::builder().maximum_size(720).child(&body).build();
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let loading = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        let error_page = libadwaita::StatusPage::builder()
            .icon_name("mail-send-symbolic")
            .title(tr!("Invites unavailable"))
            .build();

        let stack = gtk4::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&scroll, Some("list"));
        stack.add_named(&error_page, Some("error"));
        toolbar.set_content(Some(&stack));

        let page = Self {
            toolbar,
            menu_btn,
            stack,
            list,
            error_page,
            state: Rc::new(RefCell::new(None)),
        };
        {
            let page = page.clone();
            btn_refresh.connect_clicked(move |_| page.refresh());
        }
        page
    }

    /// Groups we can invite into, from the last refresh.
    pub fn groups(&self) -> Vec<String> {
        self.state.borrow().as_ref().map(|s| s.groups.clone()).unwrap_or_default()
    }

    /// Reload our invites from the panel (in the background).
    pub fn refresh(&self) {
        self.refresh_then(|_| {});
    }

    /// Reload, then run `done` with the error if the panel couldn't be used.
    pub fn refresh_then(&self, done: impl FnOnce(Option<MemberError>) + 'static) {
        if self.state.borrow().is_none() {
            self.stack.set_visible_child_name("loading");
        }
        let page = self.clone();
        super::run_in_background(invite::my_invites, move |result| match result {
            Ok(mine) => {
                page.fill(&mine.invites);
                *page.state.borrow_mut() = Some(mine);
                page.stack.set_visible_child_name("list");
                done(None);
            }
            Err(e) => {
                *page.state.borrow_mut() = None;
                // Descriptions are markup; the error may carry server text.
                page.error_page
                    .set_description(Some(glib::markup_escape_text(&explain(&e)).as_str()));
                page.stack.set_visible_child_name("error");
                done(Some(e));
            }
        });
    }

    /// Open "Invite someone" with fresh groups, or explain why we can't.
    pub fn invite(&self, parent: &libadwaita::ApplicationWindow, toast: &libadwaita::ToastOverlay) {
        self.invite_to(parent, toast, None);
    }

    /// Same, with `group` already chosen (from the Groups section).
    pub fn invite_to(
        &self,
        parent: &libadwaita::ApplicationWindow,
        toast: &libadwaita::ToastOverlay,
        group: Option<String>,
    ) {
        let (page, parent, toast) = (self.clone(), parent.clone(), toast.clone());
        self.refresh_then(move |err| match err {
            None => show_dialog(&parent, &page, group.as_deref()),
            Some(e) => super::show_toast(&toast, &explain(&e), 8),
        });
    }

    fn fill(&self, invites: &[SentInvite]) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for inv in invites {
            self.list.append(&self.row(inv));
        }
    }

    fn row(&self, inv: &SentInvite) -> libadwaita::ActionRow {
        let who = inv.uses.first().map(|u| u.user.clone());
        let title = match (&who, inv.note.is_empty()) {
            (Some(user), _) => trf!("{user} joined", "user" => user),
            (None, false) => inv.note.clone(),
            (None, true) => tr!("Invite"),
        };
        let mut details = vec![status_label(&inv.status)];
        if let Some(g) = &inv.group {
            details.push(trf!("group {group}", "group" => g));
        }
        if let Some(date) = inv.created_at.get(..10) {
            details.push(date.to_string());
        }
        let row = libadwaita::ActionRow::builder()
            .title(glib::markup_escape_text(&title).as_str())
            .subtitle(glib::markup_escape_text(&details.join(" · ")).as_str())
            .build();
        let icon = match inv.status.as_str() {
            "used" => "object-select-symbolic",
            "active" => "mail-send-symbolic",
            _ => "action-unavailable-symbolic",
        };
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        if inv.status == "active" {
            let btn = gtk4::Button::builder()
                .label(tr!("Cancel"))
                .valign(gtk4::Align::Center)
                .css_classes(["flat", "error"])
                .tooltip_text(tr!("Cancel this invite: the link stops working"))
                .build();
            let (page, id) = (self.clone(), inv.id.clone());
            btn.connect_clicked(move |b| {
                b.set_sensitive(false);
                let page = page.clone();
                let id = id.clone();
                super::run_in_background(move || invite::cancel(&id), move |_| page.refresh());
            });
            row.add_suffix(&btn);
        }
        row
    }
}

/// "Invite someone": pick an optional group and a note, then show the QR
/// code and link to share.
pub fn show_dialog(parent: &libadwaita::ApplicationWindow, page: &InvitesPage, preselect: Option<&str>) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Invite someone"))
        .default_width(480)
        .build();
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&libadwaita::HeaderBar::new());

    let steps = gtk4::Stack::new();
    steps.set_transition_type(gtk4::StackTransitionType::SlideLeft);
    steps.set_vhomogeneous(false);

    // ── Step 1: options ──
    // Radio rows instead of a drop-down: every choice gets a full line and
    // a sentence saying what it means, and a screen reader hears both.
    let form = libadwaita::PreferencesPage::new();
    let grp_note = libadwaita::PreferencesGroup::builder()
        .description(tr!(
            "The person you invite will reach your devices, and you theirs."
        ))
        .build();
    let row_note = libadwaita::EntryRow::builder()
        .title(tr!("Who is it for? (optional)"))
        .build();
    grp_note.add(&row_note);
    form.add(&grp_note);

    let groups = page.groups();
    let grp_access = libadwaita::PreferencesGroup::builder()
        .title(tr!("Access"))
        .build();
    let mut radios: Vec<gtk4::CheckButton> = Vec::new();
    let mut add_choice = |title: &str, subtitle: &str| {
        let check = gtk4::CheckButton::new();
        check.set_valign(gtk4::Align::Center);
        if let Some(first) = radios.first() {
            check.set_group(Some(first));
        } else {
            check.set_active(true);
        }
        let row = libadwaita::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        grp_access.add(&row);
        radios.push(check);
    };
    add_choice(
        &tr!("Just the two of us"),
        &tr!("You see each other's devices, nobody else"),
    );
    for g in &groups {
        add_choice(
            &trf!("Also the group {group}", "group" => g),
            &tr!("They join the group and see everyone in it"),
        );
    }
    if !groups.is_empty() {
        form.add(&grp_access);
    }
    if let Some(i) = preselect.and_then(|p| groups.iter().position(|g| g == p)) {
        radios[i + 1].set_active(true);
    }

    let lbl_err = gtk4::Label::new(None);
    lbl_err.add_css_class("error");
    lbl_err.set_wrap(true);
    lbl_err.set_margin_start(16);
    lbl_err.set_margin_end(16);

    let btn_create = gtk4::Button::builder()
        .label(tr!("Create invite"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_top(6)
        .margin_bottom(18)
        .build();
    let step1 = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    step1.append(&form);
    step1.append(&lbl_err);
    step1.append(&btn_create);
    steps.add_named(&step1, Some("form"));

    // ── Step 2: share ──
    let step2 = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    step2.set_margin_start(24);
    step2.set_margin_end(24);
    step2.set_margin_top(12);
    step2.set_margin_bottom(24);
    let picture = gtk4::Picture::builder()
        .width_request(240)
        .height_request(240)
        .halign(gtk4::Align::Center)
        .can_shrink(true)
        .build();
    picture.add_css_class("qr-code");
    picture.set_alternative_text(Some(&tr!("QR code of the invite link")));
    let hint = gtk4::Label::new(Some(&tr!(
        "Send the link or let the person scan the code with their phone. \
         It works once and stays valid until used or cancelled."
    )));
    hint.set_wrap(true);
    hint.set_justify(gtk4::Justification::Center);
    hint.add_css_class("dim-label");
    let link_label = gtk4::Label::builder()
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::Char)
        .css_classes(["monospace", "invite-link"])
        .build();
    let btn_copy = gtk4::Button::builder()
        .label(tr!("Copy link"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .build();
    let btn_done = gtk4::Button::builder()
        .label(tr!("Done"))
        .css_classes(["flat"])
        .halign(gtk4::Align::Center)
        .build();
    step2.append(&picture);
    step2.append(&hint);
    step2.append(&link_label);
    step2.append(&btn_copy);
    step2.append(&btn_done);
    steps.add_named(&step2, Some("share"));

    outer.append(&steps);
    dlg.set_content(Some(&outer));

    {
        let dlg = dlg.clone();
        btn_done.connect_clicked(move |_| dlg.close());
    }
    {
        let link_label = link_label.clone();
        btn_copy.connect_clicked(move |b| {
            b.clipboard().set_text(&link_label.text());
            b.set_label(&tr!("Copied!"));
        });
    }
    {
        let (page, steps, picture, link_label, lbl_err) =
            (page.clone(), steps.clone(), picture.clone(), link_label.clone(), lbl_err.clone());
        let row_note = row_note.clone();
        btn_create.connect_clicked(move |btn| {
            lbl_err.set_text("");
            let idx = radios.iter().position(|r| r.is_active()).unwrap_or(0);
            let group = (idx > 0).then(|| groups.get(idx - 1).cloned()).flatten();
            let note = row_note.text().trim().to_string();
            btn.set_sensitive(false);
            let (page, steps, picture, link_label, lbl_err, btn) = (
                page.clone(),
                steps.clone(),
                picture.clone(),
                link_label.clone(),
                lbl_err.clone(),
                btn.clone(),
            );
            super::run_in_background(
                move || invite::create(group.as_deref(), &note),
                move |result| {
                    btn.set_sensitive(true);
                    match result {
                        Ok(created) => {
                            if let Some(tex) = qr_texture(&created.link) {
                                picture.set_paintable(Some(&tex));
                            }
                            link_label.set_text(&created.link);
                            steps.set_visible_child_name("share");
                            page.refresh();
                        }
                        Err(e) => lbl_err.set_text(&explain(&e)),
                    }
                },
            );
        });
    }

    dlg.present();
}
