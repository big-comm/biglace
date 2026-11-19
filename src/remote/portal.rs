//! Wayland host: the screen and the input go through the desktop's
//! remote-desktop portal (xdg-desktop-portal: GNOME, KDE Plasma, …). The
//! compositor shows its own "Allow remote control?" the first time; the
//! token it hands back is kept so the next sessions start without asking it
//! again (BigLace's own "Allow / Deny" still asks every time).

use std::os::fd::OwnedFd;
use std::sync::mpsc;

use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;

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
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// A started portal session: the PipeWire stream to capture and where to
/// send input. Dropping `input` ends the portal session.
pub struct Portal {
    pub fd: OwnedFd,
    pub node: u32,
    pub input: async_channel::Sender<Input>,
    /// The pointer is drawn into the frames.
    pub cursor_embedded: bool,
}

fn saved_token() -> Option<String> {
    std::fs::read_to_string(crate::config::private_file(TOKEN_FILE))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && t.len() < 512)
}

fn save_token(token: Option<&str>) {
    if let Some(t) = token {
        let _ = crate::config::write_private_file(TOKEN_FILE, t.as_bytes());
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
                let size = stream.size();
                let _ = ready_tx.send(Ok(Portal { fd, node, input: input_tx, cursor_embedded }));

                let (w, h) = size.map(|(w, h)| (f64::from(w), f64::from(h))).unwrap_or((1920.0, 1080.0));
                while let Ok(first) = input_rx.recv().await {
                  // Everything that piled up meanwhile, with stale moves dropped.
                  let mut batch = vec![first];
                  while let Ok(more) = input_rx.try_recv() {
                      batch.push(more);
                  }
                  for event in super::coalesce(batch) {
                    let r = match event {
                        Input::Move { x, y } => {
                            rd.notify_pointer_motion_absolute(
                                &session,
                                node,
                                x.clamp(0.0, 1.0) * w,
                                y.clamp(0.0, 1.0) * h,
                                Default::default(),
                            )
                            .await
                        }
                        Input::Button { button, pressed } => {
                            let code = match button {
                                1 => BTN_LEFT,
                                2 => BTN_MIDDLE,
                                3 => BTN_RIGHT,
                                _ => continue,
                            };
                            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                            rd.notify_pointer_button(&session, code, state, Default::default()).await
                        }
                        Input::Scroll { dx, dy } => {
                            let mut r = Ok(());
                            if dy != 0 {
                                r = rd
                                    .notify_pointer_axis_discrete(&session, Axis::Vertical, dy.clamp(-10, 10), Default::default())
                                    .await;
                            }
                            if dx != 0 && r.is_ok() {
                                r = rd
                                    .notify_pointer_axis_discrete(&session, Axis::Horizontal, dx.clamp(-10, 10), Default::default())
                                    .await;
                            }
                            r
                        }
                        Input::Key { keysym, pressed } => {
                            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                            rd.notify_keyboard_keysym(&session, keysym as i32, state, Default::default())
                                .await
                        }
                    };
                    if let Err(e) = r {
                        crate::tailscale::dbg(&format!("remote: portal input failed: {e}"));
                    }
                  }
                }
                let _ = session.close().await;
            });
        })?;
    ready_rx.recv()?
}
