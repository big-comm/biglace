//! The clipboard on Wayland compositors with ext-data-control (KWin,
//! wlroots): seen and set with no BigLace window in focus. A connection of
//! our own, its events read on a thread; requests go from the main thread
//! (proxies are thread-safe).

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use super::{copied, Content, MAX_IMAGE, MAX_TEXT, PNG, UTF8_TEXT};

/// Offered with everything we put on the clipboard: our own copy, not read back.
const OURS: &str = "application/x-biglace-clipboard";
const TEXT: [&str; 5] = [UTF8_TEXT, "UTF8_STRING", "text/plain", "STRING", "TEXT"];

pub struct Clipboard {
    conn: Connection,
    qh: QueueHandle<State>,
    data_control: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
}

impl Clipboard {
    /// Connect and follow the clipboard; an error when the compositor has no ext-data-control.
    pub fn start() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let data_control: ExtDataControlManagerV1 = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let device = data_control.get_data_device(&seat, &qh, ());
        let mut state = State { conn: conn.clone(), current: None, seen_first: false };
        std::thread::Builder::new()
            .name("biglace-clipboard".into())
            .spawn(move || {
                while queue.blocking_dispatch(&mut state).is_ok() {}
                crate::tailscale::dbg("clipboard: the Wayland connection closed");
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, qh, data_control, device })
    }

    /// Own the clipboard with `c` until something else is copied.
    pub fn set(&self, c: Content) -> Result<(), String> {
        let mimes: &[&str] = match &c {
            Content::Text(_) => &TEXT,
            Content::Png(_) => &[PNG],
        };
        let source = self.data_control.create_data_source(&self.qh, Arc::new(c));
        for m in mimes {
            source.offer((*m).to_string());
        }
        source.offer(OURS.to_string());
        self.device.set_selection(Some(&source));
        self.conn.flush().map_err(|e| e.to_string())
    }
}

struct State {
    conn: Connection,
    /// The offer now on the clipboard (destroyed when replaced).
    current: Option<ExtDataControlOfferV1>,
    /// The clipboard at startup is not news: it is remembered, not sent.
    seen_first: bool,
}

impl State {
    fn selection(&mut self, offer: Option<ExtDataControlOfferV1>) {
        if let Some(old) = self.current.take() {
            old.destroy();
        }
        let first = !std::mem::replace(&mut self.seen_first, true);
        let Some(offer) = offer else { return };
        let mimes = offer.data::<Mutex<Vec<String>>>().map(|m| m.lock().map(|m| m.clone()).unwrap_or_default()).unwrap_or_default();
        let pick = if mimes.iter().any(|m| m == OURS) {
            None
        } else if mimes.iter().any(|m| m == PNG) {
            Some((PNG, MAX_IMAGE))
        } else {
            TEXT.iter().find(|t| mimes.iter().any(|m| m == *t)).map(|t| (*t, MAX_TEXT))
        };
        if let Some((mime, limit)) = pick {
            if let Err(e) = self.receive(&offer, mime, limit, first) {
                crate::tailscale::dbg(&format!("clipboard: can't read it: {e}"));
            }
        }
        self.current = Some(offer);
    }

    /// Ask the owner for `mime` and read it on a thread of its own: an owner that never writes holds only that thread.
    fn receive(&self, offer: &ExtDataControlOfferV1, mime: &str, limit: usize, first: bool) -> std::io::Result<()> {
        let (mut reader, writer) = std::io::pipe()?;
        offer.receive(mime.to_string(), writer.as_fd());
        self.conn.flush().map_err(std::io::Error::other)?;
        drop(writer);
        let image = mime == PNG;
        std::thread::Builder::new().name("biglace-clipboard-read".into()).spawn(move || {
            let mut bytes = Vec::new();
            let Ok(n) = (&mut reader).take(limit as u64 + 1).read_to_end(&mut bytes) else { return };
            if n > limit {
                return;
            }
            let c = if image {
                Content::Png(bytes)
            } else {
                match String::from_utf8(bytes) {
                    Ok(t) => Content::Text(t),
                    Err(_) => return,
                }
            };
            if first {
                super::fresh(&c);
            } else {
                copied(c);
            }
        })?;
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, event: ext_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_data_control_device_v1::Event::Selection { id } => state.selection(id),
            // The middle-click selection is not shared.
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            ext_data_control_device_v1::Event::Finished => crate::tailscale::dbg("clipboard: the compositor ended the data device"),
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, Mutex::new(Vec::<String>::new())),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, Mutex<Vec<String>>> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        mimes: &Mutex<Vec<String>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Ok(mut m) = mimes.lock() {
                m.push(mime_type);
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Arc<Content>> for State {
    fn event(
        _: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        content: &Arc<Content>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { fd, .. } => write_out(fd, content.clone()),
            ext_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

/// Hand our content to the program pasting it, on a thread: a slow reader doesn't stall the clipboard.
fn write_out(fd: OwnedFd, content: Arc<Content>) {
    let _ = std::thread::Builder::new().name("biglace-clipboard-write".into()).spawn(move || {
        let bytes: &[u8] = match &*content {
            Content::Text(t) => t.as_bytes(),
            Content::Png(p) => p,
        };
        let _ = std::fs::File::from(fd).write_all(bytes);
    });
}
