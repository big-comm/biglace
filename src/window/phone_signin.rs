//! "Sign in with my phone", as Telegram does: this computer shows a QR code,
//! BigLace on a phone already in an account scans it and approves, and this
//! computer joins that account (see `invite::start_link`). For a person whose
//! account began on the phone: no second invite, no key to copy.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::{gdk, glib, prelude::*};
use libadwaita::prelude::*;

use super::dialogs::{normalize_server_url, sanitize_hostname, validate_hostname};
use super::join_dialog::{apply_joined, explain, start_connecting};
use super::sidebar::Sidebar;
use crate::config;
use crate::invite::{self, JoinError, LinkAnswer};
use crate::{tr, trf};

/// How often the panel is asked whether the phone approved.
const POLL_EVERY: Duration = Duration::from_secs(2);

pub fn sign_in_with_phone(
    parent: &libadwaita::ApplicationWindow,
    overlay: &libadwaita::ToastOverlay,
    cfg: &Rc<RefCell<config::Config>>,
    sidebar: &Sidebar,
) {
    let dlg = libadwaita::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(tr!("Sign in with my phone"))
        .default_width(440)
        .build();
    super::dialogs::close_on_escape(&dlg);

    let (form, er_panel, er_device) = build_form(&cfg.borrow());
    let btn_show = gtk4::Button::builder()
        .label(tr!("Show the code"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .build();
    let lbl_err = gtk4::Label::builder().wrap(true).css_classes(["error"]).visible(false).build();

    let (code_box, picture, status, again) = build_code_area();

    dlg.set_content(Some(&layout(&[form.upcast_ref(), btn_show.upcast_ref(), lbl_err.upcast_ref(), code_box.upcast_ref()])));

    let view = CodeView { dlg: dlg.clone(), picture, status, again: again.clone(), generation: Rc::default() };
    let joining = Joining { overlay: overlay.clone(), cfg: cfg.clone(), sidebar: sidebar.clone(), win: parent.clone() };
    // Held by the buttons below, so it holds the widgets weakly.
    let start = {
        let (form, btn_show, code_box) = (form.downgrade(), btn_show.downgrade(), code_box.downgrade());
        Rc::new(move |panel: String, device: String| {
            let (Some(form), Some(btn_show), Some(code_box)) = (form.upgrade(), btn_show.upgrade(), code_box.upgrade()) else {
                return;
            };
            form.set_visible(false);
            btn_show.set_visible(false);
            code_box.set_visible(true);
            view.request(panel, device, joining.clone());
        })
    };
    {
        let (start, er_panel, er_device, lbl_err) = (start.clone(), er_panel.clone(), er_device.clone(), lbl_err.clone());
        btn_show.connect_clicked(move |_| match entered(&er_panel, &er_device) {
            Ok((panel, device)) => {
                lbl_err.set_visible(false);
                start(panel, device);
            }
            Err(msg) => {
                lbl_err.set_text(&msg);
                lbl_err.set_visible(true);
            }
        });
    }
    {
        let (start, er_panel, er_device) = (start.clone(), er_panel.clone(), er_device.clone());
        again.connect_clicked(move |_| start(normalize_server_url(&er_panel.text()), sanitize_hostname(&er_device.text())));
    }
    dlg.present();
}

/// The window's content: a header bar over `parts`, top to bottom.
fn layout(parts: &[&gtk4::Widget]) -> gtk4::Box {
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    body.set_margin_top(18);
    body.set_margin_bottom(24);
    body.set_margin_start(24);
    body.set_margin_end(24);
    for part in parts {
        body.append(*part);
    }
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&libadwaita::HeaderBar::new());
    outer.append(&body);
    outer
}

/// The network address and this device's name, prefilled from what we know.
fn build_form(c: &config::Config) -> (libadwaita::PreferencesGroup, libadwaita::EntryRow, libadwaita::EntryRow) {
    let form = libadwaita::PreferencesGroup::builder()
        .description(tr!(
            "Your phone must already be on the network, with BigLace. This computer joins the same account."
        ))
        .build();
    let er_panel = libadwaita::EntryRow::builder()
        .title(tr!("Network address"))
        .input_purpose(gtk4::InputPurpose::Url)
        .build();
    let er_device = libadwaita::EntryRow::builder().title(tr!("Device name on the network")).build();
    er_panel.set_text(if c.panel_url.is_empty() { &c.server_url } else { &c.panel_url });
    let name = if c.hostname.is_empty() { glib::host_name().to_string() } else { c.hostname.clone() };
    er_device.set_text(&sanitize_hostname(&name));
    form.add(&er_panel);
    form.add(&er_device);
    (form, er_panel, er_device)
}

/// The panel URL and device name typed, or why they can't be used.
fn entered(er_panel: &libadwaita::EntryRow, er_device: &libadwaita::EntryRow) -> Result<(String, String), String> {
    let panel = normalize_server_url(&er_panel.text());
    let device = sanitize_hostname(&er_device.text());
    validate_hostname(&device)?;
    if panel.is_empty() {
        return Err(tr!("Type the network address, e.g. vpn.example.org."));
    }
    Ok((panel, device))
}

/// Where the code shows (hidden until asked for): the steps, the QR code,
/// its status and "New code".
fn build_code_area() -> (gtk4::Box, gtk4::Picture, gtk4::Label, gtk4::Button) {
    let steps = gtk4::Label::new(Some(&tr!(
        "On the phone, open BigLace, tap “Scan QR code” and point it at this code. Then confirm on the phone."
    )));
    steps.set_wrap(true);
    steps.set_justify(gtk4::Justification::Center);
    let picture = gtk4::Picture::builder().width_request(260).height_request(260).can_shrink(false).build();
    picture.set_halign(gtk4::Align::Center);
    let status = gtk4::Label::builder().wrap(true).justify(gtk4::Justification::Center).css_classes(["dim-label"]).build();
    let again = gtk4::Button::builder().label(tr!("New code")).css_classes(["pill"]).halign(gtk4::Align::Center).visible(false).build();
    let code_box = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    code_box.append(&steps);
    code_box.append(&picture);
    code_box.append(&status);
    code_box.append(&again);
    code_box.set_visible(false);
    (code_box, picture, status, again)
}

/// What finishing the sign-in needs.
#[derive(Clone)]
struct Joining {
    overlay: libadwaita::ToastOverlay,
    cfg: Rc<RefCell<config::Config>>,
    sidebar: Sidebar,
    win: libadwaita::ApplicationWindow,
}

/// The code on screen and its countdown; `generation` stops an older code's polling.
#[derive(Clone)]
struct CodeView {
    dlg: libadwaita::Window,
    picture: gtk4::Picture,
    status: gtk4::Label,
    again: gtk4::Button,
    generation: Rc<Cell<u32>>,
}

impl CodeView {
    fn request(&self, panel: String, device: String, joining: Joining) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.again.set_visible(false);
        self.picture.set_paintable(gdk::Paintable::NONE);
        self.status.set_text(&tr!("Asking the panel for a code…"));
        let (view, panel_w, device_w) = (self.clone(), panel.clone(), device.clone());
        super::run_in_background(
            move || invite::start_link(&panel_w, &device_w),
            move |r| match r {
                Ok(req) => {
                    view.picture.set_paintable(super::invites::qr_texture(&req.link).as_ref());
                    view.poll(generation, panel, device, req, joining);
                }
                Err(e) => view.failed(&e),
            },
        );
    }

    /// Ask every [`POLL_EVERY`] while this code is the one shown and the window is open.
    fn poll(&self, generation: u32, panel: String, device: String, req: invite::LinkRequest, joining: Joining) {
        let view = self.clone();
        let asking = Rc::new(Cell::new(false));
        glib::timeout_add_local(POLL_EVERY, move || {
            if view.generation.get() != generation || !view.dlg.is_visible() {
                return glib::ControlFlow::Break;
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            let left = req.expires.saturating_sub(now) / 1000;
            if left == 0 {
                view.expired();
                return glib::ControlFlow::Break;
            }
            view.status.set_text(&trf!("Waiting for the phone · {time}", "time" => format!("{}:{:02}", left / 60, left % 60)));
            if asking.replace(true) {
                return glib::ControlFlow::Continue;
            }
            let (panel_w, poll, view2, asking, device, joining, panel2) =
                (panel.clone(), req.poll.clone(), view.clone(), asking.clone(), device.clone(), joining.clone(), panel.clone());
            super::run_in_background(
                move || invite::poll_link(&panel_w, &poll),
                move |r| {
                    asking.set(false);
                    if view2.generation.get() != generation || !view2.dlg.is_visible() {
                        return;
                    }
                    match r {
                        Ok(LinkAnswer::Pending) => {}
                        Ok(LinkAnswer::Approved(joined)) => {
                            view2.generation.set(generation + 1);
                            view2.joined(&panel2, &device, &joined, &joining);
                        }
                        Err(JoinError::InviteUnusable(_)) => view2.expired(),
                        Err(e) => view2.failed(&e),
                    }
                },
            );
            glib::ControlFlow::Continue
        });
    }

    fn joined(&self, panel: &str, device: &str, joined: &invite::Joined, j: &Joining) {
        let text = trf!("This computer joined the account {user}.", "user" => joined.user.as_str());
        if let Err(e) = apply_joined(&j.overlay, &j.cfg, &j.sidebar, panel, joined, device, &text) {
            self.status.set_text(&trf!("Error: {error}", "error" => e));
            return;
        }
        self.dlg.close();
        start_connecting(&j.win, &j.sidebar);
    }

    fn expired(&self) {
        self.picture.set_paintable(gdk::Paintable::NONE);
        self.status.set_text(&tr!("This code is no longer valid."));
        self.again.set_visible(true);
    }

    fn failed(&self, e: &JoinError) {
        self.picture.set_paintable(gdk::Paintable::NONE);
        self.status.set_text(&explain(e));
        self.again.set_visible(true);
    }
}
