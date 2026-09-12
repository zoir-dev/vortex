//! Active-peer arbiter: which trusted peer currently owns the session.
//!
//! A laptop may TRUST several phones but is ACTIVE with exactly one. The
//! distinction this module exists to enforce (design doc §D4):
//!
//! * **connected** — a transport link exists (BLE GATT and/or LAN). Several
//!   can overlap harmlessly, and briefly do during a handoff.
//! * **active** — that peer owns the mirrored state: notifications, clipboard,
//!   SMS/contacts/call-log pages, media. Exactly one, ever.
//!
//! Keeping them separate is what makes a handoff safe. If "connected" implied
//! "active", then connecting to the replacement before the old link finished
//! dropping would give two phones ownership at once — both mirroring
//! notifications and both syncing clipboard into the same laptop. With the
//! split, ownership flips atomically the moment a switch is confirmed and the
//! old transport can linger and die on its own schedule.
//!
//! A claim that loses gets [`Claim::Busy`] rather than silence, so the loser
//! can back off. A peer that cannot tell refusal from packet loss retries in a
//! tight loop against the phone's single GATT link.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Outcome of asking to become the active peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Caller is now the active peer (or already was).
    Granted,
    /// Refused — `current` owns the session. Maps to `PeerHandoff.BUSY`.
    Busy { current: [u8; 32] },
}

struct State {
    active: Option<[u8; 32]>,
    connected: HashSet<[u8; 32]>,
    /// Deadline of a user-initiated "switch" window, if one is open.
    ///
    /// Bounded because a switch means *seeking on top of a live connection* —
    /// the most expensive radio state there is (design doc §D9). Press Switch,
    /// walk into a room with no other device, and without this the scan runs
    /// forever.
    switching_until: Option<Instant>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            active: None,
            connected: HashSet::new(),
            switching_until: None,
        })
    })
}

/// The peer that currently owns the session, if any.
pub(crate) fn active() -> Option<[u8; 32]> {
    state().lock().ok().and_then(|s| s.active)
}

/// True when `peer_pub` owns the session.
pub(crate) fn is_active(peer_pub: &[u8; 32]) -> bool {
    active().as_ref() == Some(peer_pub)
}

/// Ask to become the active peer.
///
/// Idempotent for the peer that already owns the session, so a reconnect of
/// the active peer never has to be special-cased by callers.
pub(crate) fn claim(peer_pub: &[u8; 32]) -> Claim {
    let Ok(mut s) = state().lock() else {
        // A poisoned lock must not wedge the link. Granting is the safe
        // direction: the alternative is a laptop that can never own a session
        // again until restart.
        return Claim::Granted;
    };
    match s.active {
        Some(cur) if &cur != peer_pub => Claim::Busy { current: cur },
        Some(_) => Claim::Granted,
        None => {
            s.active = Some(*peer_pub);
            tracing::info!(peer = %hex::encode(&peer_pub[..4]), "active peer claimed");
            Claim::Granted
        }
    }
}

/// Move ownership to `peer_pub`, displacing whoever holds it.
///
/// Only for an explicit user switch — the one case where "someone else is
/// active" is not a reason to refuse, because the user just said so. Returns
/// the displaced peer so the caller can send it `PeerHandoff.RELEASE`.
pub(crate) fn force_activate(peer_pub: &[u8; 32]) -> Option<[u8; 32]> {
    let Ok(mut s) = state().lock() else { return None };
    let previous = s.active.filter(|p| p != peer_pub);
    s.active = Some(*peer_pub);
    s.switching_until = None;
    tracing::info!(
        peer = %hex::encode(&peer_pub[..4]),
        displaced = ?previous.map(|p| hex::encode(&p[..4])),
        "active peer switched"
    );
    previous
}

/// Give up ownership if `peer_pub` holds it. No-op for any other peer, so a
/// stale teardown cannot blank the current owner.
pub(crate) fn release(peer_pub: &[u8; 32]) {
    if let Ok(mut s) = state().lock() {
        if s.active.as_ref() == Some(peer_pub) {
            s.active = None;
            tracing::info!(peer = %hex::encode(&peer_pub[..4]), "active peer released");
        }
    }
}

/// Record that a transport link to `peer_pub` exists.
pub(crate) fn note_connected(peer_pub: &[u8; 32]) {
    if let Ok(mut s) = state().lock() {
        s.connected.insert(*peer_pub);
    }
}

/// Record that every transport link to `peer_pub` is gone.
///
/// Deliberately does NOT release ownership: a BLE drop during RPA churn is
/// routine and the peer is still the one whose data the UI shows. Ownership
/// changes only on an explicit switch or forget.
pub(crate) fn note_disconnected(peer_pub: &[u8; 32]) {
    if let Ok(mut s) = state().lock() {
        s.connected.remove(peer_pub);
    }
}

/// Whether any transport link to `peer_pub` is currently up.
///
/// Only the tests read this today. It stays because it is the other half of the
/// connected/active split this module exists for, and the `PeerHandoff.RELEASE`
/// path needs it: a displaced peer can only be *told* it was displaced while a
/// link to it is still up, otherwise the notice has to wait for next contact.
#[allow(dead_code)]
pub(crate) fn is_connected(peer_pub: &[u8; 32]) -> bool {
    state()
        .lock()
        .map(|s| s.connected.contains(peer_pub))
        .unwrap_or(false)
}

/// Open a bounded switch window: keep the current peer, start looking for
/// another remembered one.
pub(crate) fn begin_switch(ttl: Duration) {
    if let Ok(mut s) = state().lock() {
        s.switching_until = Some(Instant::now() + ttl);
        tracing::info!(ttl_s = ttl.as_secs(), "switch window opened");
    }
}

/// True while a switch window is open and unexpired. Reading it also closes an
/// expired window, so callers need no separate reaper.
pub(crate) fn is_switching() -> bool {
    let Ok(mut s) = state().lock() else { return false };
    match s.switching_until {
        Some(deadline) if Instant::now() < deadline => true,
        Some(_) => {
            s.switching_until = None;
            tracing::info!("switch window expired");
            false
        }
        None => false,
    }
}

/// Close a switch window early (user cancelled, or a replacement was chosen).
pub(crate) fn end_switch() {
    if let Ok(mut s) = state().lock() {
        s.switching_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> [u8; 32] {
        [n; 32]
    }

    // NOTE: the arbiter is process-global, so these run in one test to keep a
    // deterministic order rather than racing each other through the statics.
    #[test]
    fn ownership_lifecycle() {
        let a = peer(1);
        let b = peer(2);

        assert_eq!(claim(&a), Claim::Granted);
        assert!(is_active(&a));
        // Re-claiming by the owner is idempotent — a reconnect must not be
        // mistaken for a competing peer.
        assert_eq!(claim(&a), Claim::Granted);
        // A second peer is refused, and told who holds it.
        assert_eq!(claim(&b), Claim::Busy { current: a });

        // Losing the transport does NOT lose ownership: BLE drops during RPA
        // churn are routine and must not blank the UI's data source.
        note_connected(&a);
        note_disconnected(&a);
        assert!(is_active(&a));
        assert!(!is_connected(&a));

        // An explicit switch displaces the owner and names the displaced peer
        // so the caller can send it RELEASE.
        assert_eq!(force_activate(&b), Some(a));
        assert!(is_active(&b));
        assert!(!is_active(&a));

        // Releasing a peer that does not own anything is a no-op.
        release(&a);
        assert!(is_active(&b));
        release(&b);
        assert_eq!(active(), None);
    }

    #[test]
    fn switch_window_expires_on_read() {
        begin_switch(Duration::from_secs(60));
        assert!(is_switching());
        end_switch();
        assert!(!is_switching());
        // A zero TTL is already expired, and reading clears it.
        begin_switch(Duration::from_millis(0));
        assert!(!is_switching());
    }
}
