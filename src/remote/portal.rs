//! Wayland host: the screen and the input go through the desktop's
//! remote-desktop portal (xdg-desktop-portal: GNOME, KDE Plasma, …). The
//! compositor shows its own "Allow remote control?" the first time; the
//! token it hands back is kept so the next sessions start without asking it
//! again (BigLace's own "Allow / Deny" still asks every time).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::pin::pin;
use std::sync::mpsc;
use std::time::Duration;

use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use futures_util::future::{select, Either};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent, Region};
use xkbcommon::xkb;

use super::Input;

const TOKEN_FILE: &str = "remote-desktop.token";

/// Shown to the viewer when this desktop's remote-desktop portal has no
/// working backend.
pub const NO_PORTAL: &str = "this computer's desktop has no working remote-desktop portal \
     (xdg-desktop-portal). Check its configuration in ~/.config/xdg-desktop-portal/";

/// Whether the desktop's remote-desktop portal can actually share a screen
/// and take input. Blocking (a D-Bus round trip); call once.
pub fn available() -> bool {
    zbus::block_on(async {
        let Ok(rd) = RemoteDesktop::new().await else { return false };
        rd.available_device_types().await.is_ok_and(|d| !d.is_empty())
    })
}

// evdev button codes (linux/input-event-codes.h).
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

fn evdev_button(button: u8) -> Option<u16> {
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        _ => None,
    }
}

/// A started portal session: the PipeWire stream to capture and where to
/// send input. Dropping `input` ends the portal session.
pub struct Portal {
    pub fd: OwnedFd,
    pub node: u32,
    /// The node's `object.serial` (portal v6+), which PipeWire never reuses.
    pub serial: Option<u64>,
    pub input: async_channel::Sender<Input>,
    /// How many input events the compositor has been handed so far (sent
    /// ones can then be awaited: key releases must land before we go).
    pub handled: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The pointer is drawn into the frames.
    pub cursor_embedded: bool,
}

fn saved_token() -> Option<String> {
    std::fs::read_to_string(crate::config::private_file(TOKEN_FILE))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && t.len() < 512)
}

/// No token back means the portal will not restore this session: a kept one
/// would be offered again, and refused, every time.
fn save_token(token: Option<&str>) {
    let saved = match token {
        Some(t) => crate::config::write_private_file(TOKEN_FILE, t.as_bytes()),
        None => match std::fs::remove_file(crate::config::private_file(TOKEN_FILE)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    if let Err(e) = saved {
        crate::tailscale::dbg(&format!("remote: can't keep the portal's restore token: {e}"));
    }
}

/// Start a portal session on its own thread. Blocks until the compositor
/// answers (the person may have to click "Share") and returns the stream,
/// or why it failed.
pub fn start(embed_cursor: bool) -> anyhow::Result<Portal> {
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<Portal>>();
    std::thread::Builder::new()
        .name("biglace-portal".into())
        .spawn(move || {
            zbus::block_on(async move {
                let (input_tx, input_rx) = async_channel::unbounded::<Input>();
                let setup = async {
                    let rd = RemoteDesktop::new().await?;
                    let sc = Screencast::new().await?;
                    // A portal frontend with no backend for these (e.g. a
                    // portals.conf pointing GNOME at another desktop's
                    // backends) offers nothing: say so instead of failing
                    // on an obscure "invalid argument" later.
                    if rd.available_device_types().await.map_or(true, |d| d.is_empty()) {
                        anyhow::bail!(NO_PORTAL);
                    }
                    let session = rd.create_session(Default::default()).await?;
                    let token = saved_token();
                    rd.select_devices(
                        &session,
                        SelectDevicesOptions::default()
                            .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
                            .set_persist_mode(PersistMode::ExplicitlyRevoked)
                            .set_restore_token(token.as_deref()),
                    )
                    .await?;
                    // No pointer in the frames (the viewer draws its own):
                    // Hidden where offered, else Metadata (also kept out of
                    // the frames); some compositors offer only one of them.
                    // Viewers that understand it get the pointer in the
                    // frames instead (its real shape; the portal doesn't
                    // tell the shape apart).
                    let modes = sc.available_cursor_modes().await.unwrap_or(CursorMode::EMBEDDED);
                    let order = if embed_cursor {
                        [CursorMode::EMBEDDED, CursorMode::HIDDEN, CursorMode::METADATA]
                    } else {
                        [CursorMode::HIDDEN, CursorMode::METADATA, CursorMode::EMBEDDED]
                    };
                    let cursor = order.into_iter().find(|m| modes.contains(*m)).unwrap_or(CursorMode::EMBEDDED);
                    sc.select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(cursor)
                            .set_sources(SourceType::MONITOR)
                            .set_multiple(false),
                    )
                    .await?;
                    let response = rd.start(&session, None, Default::default()).await?.response()?;
                    save_token(response.restore_token());
                    let stream = response
                        .streams()
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("no screen was shared"))?
                        .clone();
                    let fd = sc.open_pipe_wire_remote(&session, Default::default()).await?;
                    anyhow::Ok((rd, session, stream, fd, cursor == CursorMode::EMBEDDED))
                };
                let (rd, session, stream, fd, cursor_embedded) = match setup.await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let node = stream.pipe_wire_node_id();
                let serial = stream.pipewire_serial();
                let size = stream.size();
                let place = StreamPlace { mapping_id: stream.mapping_id().map(str::to_owned), position: stream.position() };
                let handled = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let _ = ready_tx.send(Ok(Portal { fd, node, serial, input: input_tx, handled: handled.clone(), cursor_embedded }));

                let mut libei = match connect_libei(&rd, &session, place).await {
                    Ok(libei) => {
                        crate::tailscale::dbg("remote: portal input goes through libei (ConnectToEIS)");
                        Some(libei)
                    }
                    Err(NoLibei::Notify(e)) => {
                        crate::tailscale::dbg(&format!("remote: portal input goes through the portal's Notify calls: {e:#}"));
                        None
                    }
                    Err(NoLibei::Lost(e)) => {
                        crate::tailscale::dbg(&format!("remote: libei failed after ConnectToEIS, closing the portal session: {e:#}"));
                        drop(input_rx);
                        let _ = session.close().await;
                        return;
                    }
                };
                // Input that came while there was nowhere to send it would
                // land at once, long after the viewer's hand moved on.
                let mut stale = 0;
                while input_rx.try_recv().is_ok() {
                    stale += 1;
                }
                handled.fetch_add(stale, std::sync::atomic::Ordering::AcqRel);
                let (w, h) = size.map(|(w, h)| (f64::from(w), f64::from(h))).unwrap_or((1920.0, 1080.0));
                loop {
                    let next = match libei.as_mut() {
                        Some(libei) if !libei.open => {
                            crate::tailscale::dbg("remote: closing the portal session, its libei input is gone");
                            break;
                        }
                        Some(libei) => {
                            let woke = {
                                let recv = pin!(input_rx.recv());
                                match select(recv, libei.events.next()).await {
                                    Either::Left((input, _)) => Either::Left(input),
                                    Either::Right((event, _)) => Either::Right(event),
                                }
                            };
                            match woke {
                                Either::Left(input) => input,
                                Either::Right(event) => {
                                    libei.handle(event);
                                    continue;
                                }
                            }
                        }
                        None => input_rx.recv().await,
                    };
                    let Ok(first) = next else { break };
                    // Everything that piled up meanwhile, with stale moves dropped.
                    let mut batch = vec![first];
                    while let Ok(more) = input_rx.try_recv() {
                        batch.push(more);
                    }
                    let count = batch.len() as u64;
                    for event in super::coalesce(batch) {
                        match &mut libei {
                            Some(libei) => libei.send(event),
                            None => {
                                if let Err(e) = notify(&rd, &session, node, (w, h), event).await {
                                    crate::tailscale::dbg(&format!("remote: portal input failed: {e}"));
                                }
                            }
                        }
                    }
                    if let Some(libei) = &mut libei {
                        libei.flush();
                    }
                    handled.fetch_add(count, std::sync::atomic::Ordering::AcqRel);
                }
                // Closed before the session: its end ends the capture, and the
                // host then reads a closed input as input that was lost.
                drop(input_rx);
                if let Some(libei) = libei {
                    libei.end();
                }
                let _ = session.close().await;
            });
        })?;
    ready_rx.recv()?
}

/// One event through the portal's own Notify* calls, for portals without
/// ConnectToEIS.
async fn notify(
    rd: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    node: u32,
    (w, h): (f64, f64),
    event: Input,
) -> Result<(), ashpd::Error> {
    match event {
        Input::Move { x, y } => {
            rd.notify_pointer_motion_absolute(session, node, x.clamp(0.0, 1.0) * w, y.clamp(0.0, 1.0) * h, Default::default())
                .await
        }
        Input::Button { button, pressed } => {
            let Some(code) = evdev_button(button) else { return Ok(()) };
            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
            rd.notify_pointer_button(session, i32::from(code), state, Default::default()).await
        }
        Input::Scroll { dx, dy } => {
            if dy != 0 {
                rd.notify_pointer_axis_discrete(session, Axis::Vertical, dy.clamp(-10, 10), Default::default()).await?;
            }
            if dx != 0 {
                rd.notify_pointer_axis_discrete(session, Axis::Horizontal, dx.clamp(-10, 10), Default::default()).await?;
            }
            Ok(())
        }
        Input::Key { keysym, pressed } => {
            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
            rd.notify_keyboard_keysym(session, keysym as i32, state, Default::default()).await
        }
    }
}

/// The RemoteDesktop portal version that brought ConnectToEIS.
fn offers_libei(portal_version: u32) -> bool {
    portal_version >= 2
}

/// A few seconds of pointer moves, tens of KiB.
const MAX_UNSENT_EVENTS: usize = 2048;

/// A compositor that never answers must not keep input waiting for good.
const LIBEI_TIMEOUT: Duration = Duration::from_secs(10);

/// Why input can't go through libei.
enum NoLibei {
    /// The portal handed out no socket: its Notify* calls still work.
    Notify(anyhow::Error),
    /// The portal handed out a socket, and refuses Notify* from then on.
    Lost(anyhow::Error),
}

impl NoLibei {
    fn after(socket_handed_out: bool, error: anyhow::Error) -> Self {
        if socket_handed_out {
            Self::Lost(error)
        } else {
            Self::Notify(error)
        }
    }
}

/// Ask the portal for a libei socket and handshake as a sender.
async fn connect_libei(rd: &RemoteDesktop, session: &Session<RemoteDesktop>, stream: StreamPlace) -> Result<Libei, NoLibei> {
    if !offers_libei(rd.version()) {
        return Err(NoLibei::Notify(anyhow::anyhow!("portal version {} has no ConnectToEIS", rd.version())));
    }
    let connect = async {
        let fd = rd.connect_to_eis(session, Default::default()).await.map_err(|e| NoLibei::after(false, e.into()))?;
        let context = ei::Context::new(UnixStream::from(fd)).map_err(|e| NoLibei::after(true, e.into()))?;
        context
            .handshake_async_io("BigLace", ei::handshake::ContextType::Sender)
            .await
            .map_err(|e| NoLibei::after(true, e.into()))
    };
    let (connection, events) = match select(pin!(connect), async_io::Timer::after(LIBEI_TIMEOUT)).await {
        Either::Left((connected, _)) => connected?,
        // The call may have handed out a socket meanwhile.
        Either::Right(_) => {
            let late = anyhow::anyhow!("ConnectToEIS got no answer in {} s", LIBEI_TIMEOUT.as_secs());
            return Err(NoLibei::after(true, late));
        }
    };
    Ok(Libei {
        connection,
        events,
        stream,
        resumed: Vec::new(),
        keys: Keys::default(),
        sequence: 0,
        unsent: 0,
        open: true,
        warned_no_device: false,
        warned_no_key: false,
        warned_no_region: false,
    })
}

/// Where the portal says the shared stream sits in the compositor's logical
/// space.
struct StreamPlace {
    mapping_id: Option<String>,
    position: Option<(i32, i32)>,
}

/// A libei sender connection from ConnectToEIS.
struct Libei {
    connection: reis::event::Connection,
    events: reis::async_io::EiConvertEventStream,
    stream: StreamPlace,
    /// Only devices the server resumed, and we started emulating on, take input.
    resumed: Vec<Device>,
    keys: Keys,
    sequence: u32,
    /// Events sent since the socket last took everything.
    unsent: usize,
    open: bool,
    warned_no_device: bool,
    warned_no_key: bool,
    warned_no_region: bool,
}

impl Libei {
    fn handle(&mut self, event: Option<Result<EiEvent, reis::Error>>) {
        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(e)) => {
                crate::tailscale::dbg(&format!("remote: libei connection failed: {e}"));
                self.open = false;
                return;
            }
            None => {
                crate::tailscale::dbg("remote: the libei connection closed");
                self.open = false;
                return;
            }
        };
        match event {
            EiEvent::SeatAdded(added) => added.seat.bind_capabilities(
                DeviceCapability::PointerAbsolute | DeviceCapability::Button | DeviceCapability::Scroll | DeviceCapability::Keyboard,
            ),
            EiEvent::DeviceAdded(added) => {
                if let Some(keymap) = added.device.keymap() {
                    match load_keymap(keymap) {
                        Ok(keymap) => self.keys.set_keymap(keymap),
                        Err(e) => crate::tailscale::dbg(&format!("remote: can't read the libei keymap: {e:#}")),
                    }
                }
            }
            EiEvent::DeviceResumed(resumed) => {
                if !self.resumed.contains(&resumed.device) {
                    resumed.device.device().start_emulating(resumed.serial, self.sequence);
                    self.sequence = self.sequence.wrapping_add(1);
                    self.resumed.push(resumed.device);
                }
            }
            EiEvent::DevicePaused(paused) => {
                if paused.device.has_capability(DeviceCapability::Keyboard) {
                    self.keys.let_go();
                }
                self.resumed.retain(|d| *d != paused.device);
            }
            EiEvent::DeviceRemoved(removed) => self.resumed.retain(|d| *d != removed.device),
            EiEvent::KeyboardModifiers(modifiers) => self.keys.set_group(modifiers.group),
            EiEvent::Disconnected(gone) => {
                crate::tailscale::dbg(&format!("remote: libei disconnected us: {:?} {:?}", gone.reason, gone.explanation));
                self.open = false;
            }
            _ => {}
        }
        self.flush();
    }

    fn send(&mut self, event: Input) {
        if !self.open {
            return;
        }
        let capability = match event {
            Input::Move { .. } => DeviceCapability::PointerAbsolute,
            Input::Button { .. } => DeviceCapability::Button,
            Input::Scroll { .. } => DeviceCapability::Scroll,
            Input::Key { .. } => DeviceCapability::Keyboard,
        };
        let Some(device) = self.resumed.iter().find(|d| d.has_capability(capability)).cloned() else {
            log_once(&mut self.warned_no_device, "remote: libei has no resumed device for the viewer's input yet");
            return;
        };
        match event {
            Input::Move { x, y } => {
                let Some(pointer) = device.interface::<ei::PointerAbsolute>() else { return };
                let Some((x, y)) = region_point(device.regions(), &self.stream, x, y) else {
                    log_once(&mut self.warned_no_region, "remote: no libei pointer region matches the shared screen");
                    return;
                };
                pointer.motion_absolute(x, y);
            }
            Input::Button { button, pressed } => {
                let Some(buttons) = device.interface::<ei::Button>() else { return };
                let Some(code) = evdev_button(button) else { return };
                let state = if pressed { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released };
                buttons.button(u32::from(code), state);
            }
            Input::Scroll { dx, dy } => {
                let Some(scroll) = device.interface::<ei::Scroll>() else { return };
                // One wheel step is 120 in libei.
                scroll.scroll_discrete(dx.clamp(-10, 10) * 120, dy.clamp(-10, 10) * 120);
            }
            Input::Key { keysym, pressed } => {
                let Some(keyboard) = device.interface::<ei::Keyboard>() else { return };
                let code = if pressed {
                    let Some(code) = self.keys.press(keysym) else {
                        // Which keys, never: the log would record what the viewer types.
                        log_once(&mut self.warned_no_key, "remote: the viewer typed keys this keyboard layout has not");
                        return;
                    };
                    code
                } else {
                    let Some(code) = self.keys.release(keysym) else { return };
                    code
                };
                let state = if pressed { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released };
                keyboard.key(code, state);
            }
        }
        device.device().frame(self.connection.serial(), now_micros());
        self.unsent += 1;
    }

    /// reis keeps what the socket did not take, so a server that stops
    /// reading would have it grow with every event: past a bound, the
    /// connection counts as lost.
    fn flush(&mut self) {
        match self.connection.flush() {
            Ok(()) => self.unsent = 0,
            Err(rustix::io::Errno::AGAIN) if self.unsent <= MAX_UNSENT_EVENTS => {}
            Err(rustix::io::Errno::AGAIN) => {
                crate::tailscale::dbg("remote: the libei server stopped reading input");
                self.open = false;
            }
            Err(e) => {
                crate::tailscale::dbg(&format!("remote: can't send libei input: {e}"));
                self.open = false;
            }
        }
    }

    fn end(mut self) {
        let serial = self.connection.serial();
        for device in &self.resumed {
            device.device().stop_emulating(serial);
        }
        self.connection.connection().disconnect();
        self.flush();
    }
}

fn log_once(logged: &mut bool, message: &str) {
    if !*logged {
        crate::tailscale::dbg(message);
        *logged = true;
    }
}

/// The viewer's point (0..1 of the shared stream) in the device's logical
/// coordinates: inside the region the stream maps to by id, else by
/// position, else the device's only region.
fn region_point(regions: &[Region], stream: &StreamPlace, x: f64, y: f64) -> Option<(f32, f32)> {
    let by_id = |r: &&Region| stream.mapping_id.is_some() && r.mapping_id == stream.mapping_id;
    let by_position = |r: &&Region| {
        stream.position.is_some_and(|(px, py)| i64::from(px) == i64::from(r.x) && i64::from(py) == i64::from(r.y))
    };
    let only = match regions {
        [only] => Some(only),
        _ => None,
    };
    let region = regions.iter().find(by_id).or_else(|| regions.iter().find(by_position)).or(only)?;
    // A region ends before x + width.
    let along = |at: f64, size: u32| {
        let size = f64::from(size);
        (at.clamp(0.0, 1.0) * size).clamp(0.0, (size - 1.0).max(0.0))
    };
    let px = f64::from(region.x) + along(x, region.width);
    let py = f64::from(region.y) + along(y, region.height);
    Some((px as f32, py as f32))
}

/// Real keymaps are tens of KiB.
const MAX_KEYMAP_BYTES: u32 = 1 << 20;

fn load_keymap(keymap: &reis::event::Keymap) -> anyhow::Result<xkb::Keymap> {
    if keymap.type_ != ei::keyboard::KeymapType::Xkb {
        anyhow::bail!("not an XKB keymap");
    }
    if keymap.size > MAX_KEYMAP_BYTES {
        anyhow::bail!("{} bytes is over the {MAX_KEYMAP_BYTES} byte limit", keymap.size);
    }
    let mut text = vec![0; keymap.size as usize];
    std::fs::File::from(keymap.fd.try_clone()?).read_exact_at(&mut text, 0)?;
    while text.last() == Some(&0) {
        text.pop();
    }
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(&context, String::from_utf8(text)?, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .ok_or_else(|| anyhow::anyhow!("libxkbcommon refused it"))
}

/// The server's keymap as the keys that type each keysym, and the key each
/// held keysym went down on.
#[derive(Default)]
struct Keys {
    keymap: Option<xkb::Keymap>,
    /// The keyboard's active layout.
    group: u32,
    /// Keysym to evdev keycode, for `keymap` and `group`.
    codes: HashMap<u32, u32>,
    /// So a release lifts the key its press went down on.
    held: HashMap<u32, u32>,
}

impl Keys {
    fn set_keymap(&mut self, keymap: xkb::Keymap) {
        self.codes = keycode_map(&keymap, self.group);
        self.keymap = Some(keymap);
    }

    fn set_group(&mut self, group: u32) {
        if group != self.group {
            self.group = group;
            if let Some(keymap) = &self.keymap {
                self.codes = keycode_map(keymap, group);
            }
        }
    }

    fn press(&mut self, keysym: u32) -> Option<u32> {
        let code = *self.codes.get(&keysym)?;
        self.held.insert(keysym, code);
        Some(code)
    }

    fn release(&mut self, keysym: u32) -> Option<u32> {
        self.held.remove(&keysym)
    }

    /// A paused keyboard starts over with every key up.
    fn let_go(&mut self) {
        self.held.clear();
    }
}

/// Keysym to the evdev keycode of the key that types it: in layout `group`
/// first, then at the lowest shift level (the viewer sends its own Shift).
fn keycode_map(keymap: &xkb::Keymap, group: u32) -> HashMap<u32, u32> {
    let mut best: HashMap<u32, (bool, u32, u32)> = HashMap::new();
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        // XKB keycodes are evdev ones plus 8.
        let Some(code) = raw.checked_sub(8) else { continue };
        let key = xkb::Keycode::new(raw);
        for layout in 0..keymap.num_layouts_for_key(key) {
            for level in 0..keymap.num_levels_for_key(key, layout) {
                let rank = (layout != group, level, code);
                for sym in keymap.key_get_syms_by_level(key, layout, level) {
                    let kept = best.entry(sym.raw()).or_insert(rank);
                    if rank < *kept {
                        *kept = rank;
                    }
                }
            }
        }
    }
    best.into_iter().map(|(sym, (_, _, code))| (sym, code)).collect()
}

/// libei timestamps are CLOCK_MONOTONIC microseconds.
fn now_micros() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec.unsigned_abs() * 1_000_000 + now.tv_nsec.unsigned_abs() / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_token_forgets_saved_one() {
        let _env = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!("biglace-portal-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        save_token(Some("restore-me"));
        assert_eq!(saved_token().as_deref(), Some("restore-me"));
        save_token(None);
        assert_eq!(saved_token(), None);
        save_token(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn libei_needs_portal_version_two() {
        assert!(!offers_libei(1));
        assert!(offers_libei(2));
        assert!(offers_libei(5));
    }

    /// The RemoteDesktop portal refuses Notify* once ConnectToEIS succeeded.
    #[test]
    fn notify_only_before_libei_socket() {
        assert!(matches!(NoLibei::after(false, anyhow::anyhow!("refused")), NoLibei::Notify(_)));
        assert!(matches!(NoLibei::after(true, anyhow::anyhow!("handshake")), NoLibei::Lost(_)));
    }

    fn region(x: u32, y: u32, width: u32, height: u32, mapping_id: Option<&str>) -> Region {
        Region { x, y, width, height, scale: 1.0, mapping_id: mapping_id.map(str::to_owned) }
    }

    #[test]
    fn point_lands_in_the_stream_region() {
        let two = [region(0, 0, 1920, 1080, Some("DP-1")), region(1920, 0, 1280, 1024, Some("HDMI-1"))];
        let by_id = StreamPlace { mapping_id: Some("HDMI-1".into()), position: Some((0, 0)) };
        assert_eq!(region_point(&two, &by_id, 0.5, 0.5), Some((2560.0, 512.0)));
        let by_position = StreamPlace { mapping_id: None, position: Some((1920, 0)) };
        assert_eq!(region_point(&two, &by_position, 0.5, 0.5), Some((2560.0, 512.0)));
        let unknown = StreamPlace { mapping_id: Some("eDP-1".into()), position: None };
        assert_eq!(region_point(&two, &unknown, 0.5, 0.5), None);

        let one = [region(100, 50, 800, 600, None)];
        assert_eq!(region_point(&one, &unknown, 0.25, 0.5), Some((300.0, 350.0)));
        assert_eq!(region_point(&one, &unknown, 1.0, -3.0), Some((899.0, 50.0)));
        assert_eq!(region_point(&[], &unknown, 0.5, 0.5), None);
    }

    fn keymap(layouts: &str) -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "evdev", "pc105", layouts, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .expect("xkeyboard-config has the us and de layouts")
    }

    #[test]
    fn release_lifts_its_pressed_key() {
        let mut keys = Keys::default();
        keys.set_keymap(keymap("us,de"));
        assert_eq!(keys.press(0x79), Some(21)); // y on KEY_Y in us
        keys.set_group(1);
        assert_eq!(keys.release(0x79), Some(21), "not KEY_Z, where de has y");
        assert_eq!(keys.release(0x79), None, "already up");
        assert_eq!(keys.release(0x7a), None, "never pressed");
        assert_eq!(keys.press(0x6c6), None, "on no key here");
        assert_eq!(keys.release(0x6c6), None);
    }

    #[test]
    fn paused_keyboard_holds_no_key() {
        let mut keys = Keys::default();
        keys.set_keymap(keymap("us"));
        assert_eq!(keys.press(0x61), Some(30));
        keys.let_go();
        assert_eq!(keys.release(0x61), None, "the pause lifted it");
        assert_eq!(keys.press(0x61), Some(30), "typing goes on after the resume");
    }

    /// Expected codes from linux/input-event-codes.h.
    #[test]
    fn keysym_finds_its_evdev_key() {
        let us = keymap("us");
        let us = keycode_map(&us, 0);
        assert_eq!(us.get(&0x61), Some(&30)); // a: KEY_A
        assert_eq!(us.get(&0x21), Some(&2)); // exclam: KEY_1, shifted
        assert_eq!(us.get(&0xff0d), Some(&28)); // Return: KEY_ENTER
        assert_eq!(us.get(&0xffe1), Some(&42)); // Shift_L: KEY_LEFTSHIFT
        assert_eq!(us.get(&0x6c6), None); // Cyrillic_ef: not on a us keyboard

        let us_de = keymap("us,de");
        assert_eq!(keycode_map(&us_de, 0).get(&0x79), Some(&21)); // y: KEY_Y in us
        assert_eq!(keycode_map(&us_de, 1).get(&0x79), Some(&44)); // y: KEY_Z in de
        assert_eq!(keycode_map(&us_de, 0).get(&0xfc), Some(&26)); // udiaeresis: KEY_LEFTBRACE, de only
    }
}
