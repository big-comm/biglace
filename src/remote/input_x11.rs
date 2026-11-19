//! Replay the viewer's input on an X11 host with the XTest extension.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::Input;

// X11 core event codes used by XTest's fake_input.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

pub struct Injector {
    conn: RustConnection,
    root: xproto::Window,
    width: u16,
    height: u16,
    /// keysym → (keycode, typed with Shift), from the server's current
    /// keyboard mapping.
    keymap: std::collections::HashMap<u32, (u8, bool)>,
    /// The viewer holds a Shift of its own.
    shift_held: bool,
}

impl Injector {
    /// Connect to this session's X server ($DISPLAY). None when there's no
    /// X server or no XTest — the session then stays view-only.
    pub fn connect() -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        conn.xtest_get_version(2, 2).ok()?.reply().ok()?;
        let screen = conn.setup().roots.get(screen_num)?.clone();
        let mut injector = Self {
            root: screen.root,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
            keymap: Default::default(),
            shift_held: false,
            conn,
        };
        injector.load_keymap();
        Some(injector)
    }

    fn load_keymap(&mut self) {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let Ok(reply) = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|_| ())
            .and_then(|c| c.reply().map_err(|_| ()))
        else {
            return;
        };
        let per = reply.keysyms_per_keycode.max(1) as usize;
        // First two columns: unshifted, then shifted — a keysym found
        // unshifted anywhere is typed that way.
        for column in 0..2.min(per) {
            for (i, syms) in reply.keysyms.chunks(per).enumerate() {
                let code = min as usize + i;
                if let Some(&sym) = syms.get(column).filter(|s| **s != 0) {
                    self.keymap.entry(sym).or_insert((code as u8, column == 1));
                }
            }
        }
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
    }

    pub fn apply(&mut self, event: &Input) {
        match *event {
            Input::Move { x, y } => {
                let px = (x.clamp(0.0, 1.0) * f64::from(self.width.saturating_sub(1))).round() as i16;
                let py = (y.clamp(0.0, 1.0) * f64::from(self.height.saturating_sub(1))).round() as i16;
                self.fake(MOTION_NOTIFY, 0, px, py);
            }
            Input::Button { button, pressed } => {
                if (1..=3).contains(&button) {
                    self.fake(if pressed { BUTTON_PRESS } else { BUTTON_RELEASE }, button, 0, 0);
                }
            }
            Input::Scroll { dx, dy } => {
                // Wheel = buttons 4/5 (vertical) and 6/7 (horizontal).
                let steps = |n: i32, neg: u8, pos: u8| -> Vec<u8> {
                    let b = if n < 0 { neg } else { pos };
                    std::iter::repeat_n(b, n.unsigned_abs().min(10) as usize).collect()
                };
                for b in steps(dy, 4, 5).into_iter().chain(steps(dx, 6, 7)) {
                    self.fake(BUTTON_PRESS, b, 0, 0);
                    self.fake(BUTTON_RELEASE, b, 0, 0);
                }
            }
            Input::Key { keysym, pressed } => {
                if keysym == 0xffe1 || keysym == 0xffe2 {
                    self.shift_held = pressed;
                }
                if let Some(&(code, shifted)) = self.keymap.get(&keysym) {
                    let shifted = shifted && !self.shift_held;
                    // A symbol typed with Shift ("!" on most layouts) as a
                    // phone sends it: Shift around it here.
                    let shift = self.keymap.get(&0xffe1).filter(|_| shifted).map(|(c, _)| *c);
                    if let (Some(s), true) = (shift, pressed) {
                        self.fake(KEY_PRESS, s, 0, 0);
                    }
                    self.fake(if pressed { KEY_PRESS } else { KEY_RELEASE }, code, 0, 0);
                    if let (Some(s), false) = (shift, pressed) {
                        self.fake(KEY_RELEASE, s, 0, 0);
                    }
                }
            }
        }
        let _ = self.conn.flush();
    }
}
