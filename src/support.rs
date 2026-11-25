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

/// How long the network may still list a helper, online at first, after its access ended.
const SETTLING_MINUTES: i64 = 5;

/// `grant` stopped letting its helper in less than [`SETTLING_MINUTES`] before `now`.
fn just_ended(grant: &SupportGrant, now: &gtk4::glib::DateTime) -> bool {
    let stopped = gtk4::glib::DateTime::from_iso8601(grant.ended_at.as_deref().unwrap_or(&grant.ends_at), None);
    stopped.is_ok_and(|t| t <= *now && now.difference(&t) < gtk4::glib::TimeSpan::from_minutes(SETTLING_MINUTES))
}

/// What we know now.
pub fn state() -> SupportState {
    known().state.clone()
}

/// The device at `ip` reaches ours for support: kept out of our lists (we may
/// only answer it in chat). After the access ended the network still lists it
/// for a while, online at first, then offline; hidden then too.
pub fn hides(ip: &str, online: bool) -> bool {
    let now = gtk4::glib::DateTime::now_utc().ok();
    let settling = |g: &SupportGrant| now.as_ref().is_some_and(|now| just_ended(g, now));
    known().state.helped_by.iter().any(|g| (open(g) || settling(g) || !online) && g.helper_ips.iter().any(|a| a == ip))
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
        g.ended_at = gtk4::glib::DateTime::now_utc().and_then(|now| now.format_iso8601()).ok().map(String::from);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn just_ended_for_five_minutes() {
        let now = gtk4::glib::DateTime::from_iso8601("2026-11-25T12:00:00Z", None).expect("valid time");
        let grant = |ends_at: &str, ended_at: Option<&str>| SupportGrant {
            id: "5e1a".into(),
            helper: "tales".into(),
            helper_device: "lord".into(),
            helper_ips: vec!["100.64.0.2".into()],
            guest: "bruna".into(),
            ends_at: ends_at.into(),
            ended_at: ended_at.map(String::from),
        };
        let cases = [
            grant("2026-11-25T13:00:00Z", None),
            grant("2026-11-25T13:00:00Z", Some("2026-11-25T11:56:00Z")),
            grant("2026-11-25T11:58:00Z", None),
            grant("2026-11-25T13:00:00Z", Some("2026-11-25T11:54:00Z")),
            grant("2026-11-25T13:00:00Z", Some("not a date")),
        ];
        assert_eq!(cases.iter().map(|g| just_ended(g, &now)).collect::<Vec<_>>(), [false, true, true, false, false]);
    }
}
