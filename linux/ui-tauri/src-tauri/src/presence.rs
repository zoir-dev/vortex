//! Phone-presence bookkeeping: how recently we heard from the peer, and over
//! which kind of link.
//!
//! Lifted out of `ble` because none of it is BLE. The comments in here always
//! said "over ANY transport" — the LAN heartbeat, the BLE listener and the
//! handoff path all stamp these, and the readers (the call pill, the handoff
//! pill, the tray's Offline state) care only about liveness. Living in the BLE
//! module meant a build without a BlueZ transport had no way to tell whether
//! the phone was there.

pub(crate) fn state_nudge() -> &'static tokio::sync::Notify {
    static NUDGE: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    NUDGE.get_or_init(tokio::sync::Notify::new)
}

/// Epoch-ms of the last PROOF the phone was nearby: a token-validated
/// trusted-presence advertisement or a live-session event. The proximity
/// watcher treats "no BLE session AND this stale" as the phone having
/// left (advertisements keep this fresh during RPA-churn reconnects, so
/// a flapping session alone never reads as absence).
pub(crate) static LAST_PRESENCE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn touch_presence() {
    LAST_PRESENCE_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Epoch-ms of the last AppState/frame received over ANY transport (BLE OR
/// LAN). Distinct from [`LAST_PRESENCE_MS`] (BLE-only, feeds proximity auto-
/// lock): this answers "are we in LIVE CONTACT with the phone right now?" and
/// is used to clear mirror pills (call / handoff) the instant we go fully
/// offline — their buttons (Accept / Mute / open) are dead without a link, so a
/// lingering pill is misleading. Touched by both the BLE STATE consumer and the
/// LAN heartbeat.
pub(crate) static LAST_PEER_CONTACT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn touch_peer_contact() {
    LAST_PEER_CONTACT_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Ms since we last heard from the phone over any transport (huge if never).
pub(crate) fn peer_contact_age_ms() -> u64 {
    let last = LAST_PEER_CONTACT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    now_ms().saturating_sub(last)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
