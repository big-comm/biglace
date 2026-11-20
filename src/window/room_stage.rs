//! The room's video stage, Jitsi-style: everyone in a grid, or whoever is
//! speaking large with the others in a strip below. A person's camera when
//! it's on, their photo otherwise; a glowing border on whoever talks.

use gtk4::{gdk, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::room::{self, camera, MemberView};
use crate::tr;

/// One person on the stage.
struct Tile {
    ip: String,
    root: gtk4::Overlay,
    picture: gtk4::Picture,
    avatar: libadwaita::Avatar,
    name: gtk4::Label,
    muted: gtk4::Image,
    /// Last picture shown (redraw only on a new one).
    serial: Cell<u64>,
    camera: Cell<bool>,
}

impl Tile {
    fn new(m: &MemberView) -> Rc<Self> {
        let picture = gtk4::Picture::builder()
            .content_fit(gtk4::ContentFit::Cover)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .visible(false)
            .build();
        let avatar = libadwaita::Avatar::new(72, Some(&m.user), true);
        avatar.set_halign(gtk4::Align::Center);
        avatar.set_valign(gtk4::Align::Center);
        crate::avatars::bind(&avatar, &m.user);
        let back = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        back.add_css_class("stage-tile-back");
        back.append(&picture);
        let root = gtk4::Overlay::new();
        root.add_css_class("stage-tile");
        root.set_overflow(gtk4::Overflow::Hidden);
        root.set_child(Some(&back));
        root.add_overlay(&avatar);
        let label_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        label_box.add_css_class("stage-name");
        label_box.set_halign(gtk4::Align::Start);
        label_box.set_valign(gtk4::Align::End);
        let muted = gtk4::Image::from_icon_name("microphone-disabled-symbolic");
        muted.set_visible(false);
        let name = gtk4::Label::new(Some(&if m.me { format!("{} ({})", m.user, tr!("you")) } else { m.user.clone() }));
        label_box.append(&muted);
        label_box.append(&name);
        root.add_overlay(&label_box);
        Rc::new(Self {
            ip: m.ip.clone(),
            root,
            picture,
            avatar,
            name,
            muted,
            serial: Cell::new(0),
            camera: Cell::new(false),
        })
    }

    fn update(&self, m: &MemberView) {
        self.muted.set_visible(m.muted);
        self.camera.set(m.camera);
        if !m.camera {
            self.picture.set_visible(false);
            self.avatar.set_visible(true);
            self.serial.set(0);
        }
        let _ = &self.name;
    }

    /// Show a new camera picture and the speaking glow; returns whether
    /// they speak.
    fn refresh(&self) -> bool {
        let speaking = room::speaking(room::level(&self.ip));
        if speaking {
            self.root.add_css_class("speaking");
        } else {
            self.root.remove_css_class("speaking");
        }
        if !self.camera.get() {
            return speaking;
        }
        let Some(f) = camera::frame(&self.ip) else { return speaking };
        if f.serial == self.serial.get() {
            return speaking;
        }
        self.serial.set(f.serial);
        let tex = gdk::MemoryTexture::new(f.width as i32, f.height as i32, gdk::MemoryFormat::R8g8b8a8, &f.data, f.stride);
        self.picture.set_paintable(Some(&tex));
        self.picture.set_visible(true);
        self.avatar.set_visible(false);
        speaking
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Grid,
    Speaker,
}

#[derive(Clone)]
pub struct Stage {
    pub widget: gtk4::Box,
    grid: gtk4::FlowBox,
    big: gtk4::Box,
    strip: gtk4::Box,
    views: gtk4::Stack,
    tiles: Rc<RefCell<Vec<Rc<Tile>>>>,
    layout: Rc<Cell<Layout>>,
    /// Who's large in speaker view, and since when.
    speaker: Rc<RefCell<(String, std::time::Instant)>>,
    /// A look is due once a voice may have gone silent without a word.
    recheck: Rc<Cell<bool>>,
}

impl Stage {
    pub fn build() -> Self {
        let grid = gtk4::FlowBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(1)
            .max_children_per_line(4)
            .column_spacing(8)
            .row_spacing(8)
            .valign(gtk4::Align::Start)
            .build();
        let big = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        big.set_vexpand(true);
        big.add_css_class("stage-big");
        let strip = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        strip.set_halign(gtk4::Align::Center);
        let speaker_view = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        speaker_view.append(&big);
        speaker_view.append(&strip);
        let views = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).vhomogeneous(false).build();
        views.add_named(&grid, Some("grid"));
        views.add_named(&speaker_view, Some("speaker"));
        let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        widget.add_css_class("room-stage");
        widget.append(&views);
        let stage = Self {
            widget,
            grid,
            big,
            strip,
            views,
            tiles: Rc::default(),
            layout: Rc::new(Cell::new(Layout::Grid)),
            speaker: Rc::new(RefCell::new((String::new(), std::time::Instant::now()))),
            recheck: Rc::default(),
        };
        // Redrawn only when a picture or a voice changes.
        {
            let s = stage.clone();
            let changes = room::stage_wakeups();
            glib::spawn_future_local(async move {
                while changes.recv().await.is_ok() {
                    s.refresh();
                }
            });
        }
        stage.widget.connect_map(|_| room::stage_changed());
        #[cfg(debug_assertions)]
        if room::preview() {
            glib::timeout_add_local(std::time::Duration::from_millis(50), || {
                room::stage_changed();
                glib::ControlFlow::Continue
            });
        }
        stage
    }

    fn refresh(&self) {
        if !self.widget.is_mapped() {
            return;
        }
        let mut speaking = false;
        for t in self.tiles.borrow().iter() {
            speaking |= t.refresh();
        }
        if self.layout.get() == Layout::Speaker {
            self.follow_speaker();
        }
        // A voice that stops arriving sends no last level: look again once
        // it would count as silence.
        if speaking && !self.recheck.replace(true) {
            let s = self.clone();
            glib::timeout_add_local_once(std::time::Duration::from_micros(room::LEVEL_STALE_US), move || {
                s.recheck.set(false);
                s.refresh();
            });
        }
    }

    pub fn set_layout(&self, layout: Layout) {
        self.layout.set(layout);
        self.arrange();
    }

    /// The people in the room changed.
    pub fn update(&self, members: &[MemberView]) {
        {
            let mut tiles = self.tiles.borrow_mut();
            tiles.retain(|t| members.iter().any(|m| m.ip == t.ip));
            for m in members {
                match tiles.iter().find(|t| t.ip == m.ip) {
                    Some(t) => t.update(m),
                    None => {
                        let t = Tile::new(m);
                        t.update(m);
                        tiles.push(t);
                    }
                }
            }
        }
        self.arrange();
    }

    /// Put the tiles where the layout wants them.
    fn arrange(&self) {
        for parent in [self.grid.upcast_ref::<gtk4::Widget>(), self.big.upcast_ref(), self.strip.upcast_ref()] {
            while let Some(c) = parent.first_child() {
                if let Some(fb) = parent.downcast_ref::<gtk4::FlowBox>() {
                    fb.remove(&c);
                } else if let Some(b) = parent.downcast_ref::<gtk4::Box>() {
                    b.remove(&c);
                }
            }
        }
        let tiles = self.tiles.borrow();
        match self.layout.get() {
            Layout::Grid => {
                self.views.set_visible_child_name("grid");
                let per_line = match tiles.len() {
                    0..=1 => 1,
                    2..=4 => 2,
                    5..=9 => 3,
                    _ => 4,
                };
                self.grid.set_max_children_per_line(per_line);
                self.grid.set_min_children_per_line(per_line.min(tiles.len() as u32).max(1));
                for t in tiles.iter() {
                    t.root.set_size_request(240, 135);
                    self.grid.insert(&t.root, -1);
                }
            }
            Layout::Speaker => {
                self.views.set_visible_child_name("speaker");
                let speaker = self.speaker.borrow().0.clone();
                let main = tiles.iter().find(|t| t.ip == speaker).or_else(|| tiles.first());
                for t in tiles.iter() {
                    if main.is_some_and(|m| Rc::ptr_eq(m, t)) {
                        t.root.set_size_request(480, 270);
                        t.root.set_vexpand(true);
                        self.big.append(&t.root);
                    } else {
                        t.root.set_size_request(160, 90);
                        t.root.set_vexpand(false);
                        self.strip.append(&t.root);
                    }
                }
            }
        }
    }

    /// Speaker view: whoever talks (not us, unless alone) goes large, held
    /// for a moment so it doesn't flicker between people.
    fn follow_speaker(&self) {
        let me = room::my_ip().unwrap_or_default();
        let loudest = self
            .tiles
            .borrow()
            .iter()
            .filter(|t| t.ip != me)
            .map(|t| (t.ip.clone(), room::level(&t.ip)))
            .filter(|(_, l)| *l > 0.12)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(ip, _)| ip);
        let Some(ip) = loudest else { return };
        let change = {
            let cur = self.speaker.borrow();
            cur.0 != ip && cur.1.elapsed() > std::time::Duration::from_millis(1500)
        };
        if change {
            *self.speaker.borrow_mut() = (ip, std::time::Instant::now());
            self.arrange();
        }
    }
}
