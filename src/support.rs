//! Support access (the panel's support invites): someone's device reaches
//! ours for a while, never the other way. That device stays out of our lists,
//! and the window says who can reach us, until when, with a way to end it.

use crate::invite::{SupportGrant, SupportState};
use std::sync::{Mutex, PoisonError};

struct Known {
    state: SupportState,
    /// Peer addresses when the panel was last asked: a new one may be a helper.
    seen: Vec<String>,
    asked: bool,
}

static KNOWN: Mutex<Known> = Mutex::new(Known {
    state: SupportState { helping: Vec::new(), helped_by: Vec::new() },
    seen: Vec::new(),
    asked: false,
});

fn known() -> std::sync::MutexGuard<'static, Known> {
    KNOWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `grant` still lets its helper in.
pub fn open(grant: &SupportGrant) -> bool {
    let ends = gtk4::glib::DateTime::from_iso8601(&grant.ends_at, None);
    let now = gtk4::glib::DateTime::now_utc();
    grant.ended_at.is_none() && matches!((ends, now), (Ok(ends), Ok(now)) if ends > now)
}

/// What we know now.
pub fn state() -> SupportState {
    known().state.clone()
}

/// The device at `ip` reaches ours for support: it isn't one we may reach.
pub fn hides(ip: &str) -> bool {
    known().state.helped_by.iter().any(|g| open(g) && g.helper_ips.iter().any(|a| a == ip))
}

/// `login` helps us right now (its screen access never gets "always allow").
pub fn helps_me(login: &str) -> bool {
    known().state.helped_by.iter().any(|g| open(g) && g.helper == login)
}

/// Ask the panel again when a peer we hadn't seen shows up (a helper's
/// device appears that way), or the first time. Blocking: from the refresh
/// worker. True when what we know changed.
pub fn refresh(peer_ips: &[&str]) -> bool {
    {
        let k = known();
        if k.asked && peer_ips.iter().all(|ip| k.seen.iter().any(|s| s == ip)) {
            return false;
        }
    }
    let fetched = crate::invite::support();
    let mut k = known();
    k.seen = peer_ips.iter().map(|ip| (*ip).to_string()).collect();
    k.asked = true;
    match fetched {
        Ok(state) if state != k.state => {
            k.state = state;
            true
        }
        Ok(_) => false,
        // An older panel, or not on BigScale: nobody helps us.
        Err(e) => {
            crate::tailscale::dbg(&format!("support: {e:?}"));
            false
        }
    }
}

/// Ask the panel now, and keep the answer. Blocking.
pub fn fetch() -> Result<SupportState, crate::invite::MemberError> {
    let state = crate::invite::support()?;
    let mut k = known();
    k.state = state.clone();
    k.asked = true;
    Ok(state)
}

/// End `id` (we help, or we're helped). Blocking.
pub fn end(id: &str) -> Result<(), crate::invite::MemberError> {
    crate::invite::end_support(id)?;
    let mut k = known();
    let SupportState { helping, helped_by } = &mut k.state;
    for g in helping.iter_mut().chain(helped_by.iter_mut()).filter(|g| g.id == id) {
        g.ended_at = Some(String::new());
    }
    Ok(())
}
