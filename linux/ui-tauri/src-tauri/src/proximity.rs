//! Proximity auto-lock/unlock — V2/V3 of the remote-lock feature.
//!
//! Auto-LOCK: when the phone leaves BLE range (no authenticated session
//! AND no token-validated trusted-presence advertisement for
//! [`AWAY_GRACE_MS`]) and the user is idle, lock the session — once, on
//! the present→absent edge.
//!
//! Auto-UNLOCK: a strictly *gated* unlock, never "unlock whenever the
//! phone is near": only when the USER wakes the locked laptop (key /
//! mouse / lid — Mutter user-active watch) while the authenticated BLE
//! session is live. Advertisements are NOT enough for unlocking (their
//! rotating token is a presence filter, not authentication). A manual
//! lock taken while the phone was present stays locked until the phone
//! does a real away→present cycle.
//!
//! The decision logic is a pure state machine ([`step`]) with injected
//! inputs so every edge is unit-testable without BLE or D-Bus.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// No session + no advertisement for this long = the phone probably
/// left. Kept short for a fast lock; the driver runs a 5s token-
/// validated CONFIRMATION SCAN before actually locking, so a churn
/// gap (session flapping while the phone sits right here) gets caught
/// there instead of needing a long grace.
pub(crate) const AWAY_GRACE_MS: u64 = 25_000;
/// Length of the last-chance confirmation scan before a lock. A present
/// phone is in the reconnect-seeking LOW_LATENCY tier (~100ms adv) when
/// this runs — ~20 advertising events fit in 2s even counting discovery
/// startup, so a present phone can't realistically be missed.
pub(crate) const CONFIRM_SCAN_MS: u64 = 2_000;
/// Auto-lock only fires when the user has been idle this long — the
/// key false-positive guard: if you're typing, you ARE the presence,
/// even when the phone's app got killed (MIUI OneKeyClean).
pub(crate) const IDLE_GATE_MS: u64 = 30_000;
/// Idle time at a lock edge above which the lock is taken to be the
/// screensaver's rather than the user's. Set well above the desktop's own idle
/// delay so a deliberate lock — always preceded by recent input — never
/// qualifies.
pub(crate) const IDLE_LOCK_MIN_MS: u64 = 120_000;
/// After a wake-while-locked with no live link yet (resume from
/// suspend), keep the unlock intent armed this long so the unlock
/// fires the moment the BLE session re-establishes. Sized for the
/// worst healthy reconnect after resume: fresh advertisement sighting
/// (~1-2s) + GATT connect (up to ~11s observed) + IK (~0.5s) — and
/// still short enough that the laptop can't unlock long after the
/// user walked off again.
pub(crate) const WAKE_WINDOW_MS: u64 = 20_000;
/// Minimum gap between two auto-unlocks (loop protection).
pub(crate) const UNLOCK_COOLDOWN_MS: u64 = 30_000;
/// An EAGER unlock (fired on the phone's arrival, before any user
/// input) that stays untouched this long gets re-locked — the user
/// only walked through BLE range, they didn't come to the laptop.
/// After that re-lock a single key/mouse touch unlocks instantly
/// (the link is already live).
pub(crate) const RELOCK_IDLE_MS: u64 = 90_000;

// --------------------------------------------------------------------------
// Settings (persisted; both default OFF — explicit opt-in)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct ProximitySettings {
    #[serde(default)]
    pub auto_lock: bool,
    #[serde(default)]
    pub auto_unlock: bool,
}

static AUTO_LOCK_ON: AtomicBool = AtomicBool::new(false);
static AUTO_UNLOCK_ON: AtomicBool = AtomicBool::new(false);

/// Latest "is the phone unlocked?" from the peer's AppState (owner-present gate).
/// Fail-closed: false until the phone reports `unlocked = Some(true)`. Updated
/// by [`note_phone_unlocked`] from every inbound phone AppState (BLE + LAN).
static LAST_PHONE_UNLOCKED: AtomicBool = AtomicBool::new(false);

/// Record the phone's keyguard state from an inbound AppState. `None` (old
/// phone / laptop sender) is treated as locked so auto-unlock can't fire on it.
pub(crate) fn note_phone_unlocked(unlocked: Option<bool>) {
    LAST_PHONE_UNLOCKED.store(unlocked == Some(true), Ordering::Relaxed);
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vortex").join("proximity.json"))
}

pub(crate) fn load_settings_into_statics() {
    let s: ProximitySettings = settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    AUTO_LOCK_ON.store(s.auto_lock, Ordering::Relaxed);
    AUTO_UNLOCK_ON.store(s.auto_unlock, Ordering::Relaxed);
}

#[tauri::command]
pub fn get_proximity_settings() -> ProximitySettings {
    ProximitySettings {
        auto_lock: AUTO_LOCK_ON.load(Ordering::Relaxed),
        auto_unlock: AUTO_UNLOCK_ON.load(Ordering::Relaxed),
    }
}

#[tauri::command]
pub fn set_proximity_settings(auto_lock: bool, auto_unlock: bool) -> Result<(), String> {
    AUTO_LOCK_ON.store(auto_lock, Ordering::Relaxed);
    AUTO_UNLOCK_ON.store(auto_unlock, Ordering::Relaxed);
    let s = ProximitySettings { auto_lock, auto_unlock };
    let path = settings_path().ok_or("no config dir")?;
    let bytes = serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?;
    vortex_l3_daemon::core::fs_private::write_private(&path, &bytes)
        .map_err(|e| e.to_string())
}

// --------------------------------------------------------------------------
// Pure state machine
// --------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProxState {
    /// Phone considered present on the previous step.
    present: bool,
    /// May auto-lock once when the phone leaves. Set ONLY by a real
    /// presence sighting (never at process start), cleared after firing.
    lock_armed: bool,
    /// Auto-unlock permitted for the CURRENT lock: we locked it, it was
    /// locked while the phone was away, or an away→present cycle
    /// happened while locked. A manual lock with the phone present
    /// leaves this false.
    unlock_armed: bool,
    /// Post-wake window deadline (0 = none). Only used in wake-gated
    /// mode (after an idle re-lock).
    wake_until_ms: u64,
    /// Previous LockedHint, for edge detection.
    last_locked: Option<bool>,
    /// True between our Lock action and the LockedHint edge it causes.
    we_locked: bool,
    last_auto_unlock_ms: u64,
    /// Previous step's `link_live`, for the drop-edge fast path.
    last_link_live: bool,
    /// Epoch-ms of an EAGER (arrival) unlock that hasn't seen any user
    /// input yet; 0 = none. Drives the unused→re-lock backstop.
    eager_at_ms: u64,
    /// Set by the idle re-lock: the next unlock requires an actual user
    /// wake (key/mouse/lid) — the eager path already fired and went
    /// unused, so "still in range" alone must not reopen the lid.
    /// Cleared by a fresh away→present cycle or any unlock.
    require_wake: bool,
}

pub(crate) struct Inputs {
    pub now_ms: u64,
    /// Authenticated BLE session live (writers map non-empty).
    pub link_live: bool,
    /// Epoch-ms of the last presence proof (`ble::LAST_PRESENCE_MS`).
    pub last_presence_ms: u64,
    /// logind LockedHint (None = unknown).
    pub locked: Option<bool>,
    /// Mutter idle time (None = unavailable → idle gate never passes).
    pub idle_ms: Option<u64>,
    /// Laptop Bluetooth adapter powered. Off = presence UNKNOWN: freeze.
    pub adapter_on: bool,
    pub auto_lock_on: bool,
    pub auto_unlock_on: bool,
    /// User-input edge (key/mouse/lid) since the previous step.
    pub user_active: bool,
    /// Is the paired phone currently unlocked? Auto-unlock requires this
    /// (owner-present model) so a locked/pocketed phone can't open the laptop.
    pub phone_unlocked: bool,
    /// Have we heard from the phone over ANY transport recently (BLE or LAN)?
    ///
    /// Presence used to be BLE-only, which made the laptop lock itself in front
    /// of its owner: turn the phone's Bluetooth off, or let MIUI kill the app,
    /// and the laptop saw "gone" while its own window still said "Online" off
    /// the LAN heartbeat. A phone answering us on the LAN is in the building.
    pub peer_contact_fresh: bool,
    /// Is the laptop playing audio or video right now?
    ///
    /// The idle gate asks "has a key been pressed in the last 30 s", and the
    /// answer is no for the entire length of a film. Watching something is not
    /// being away, and locking the screen during it is the machine being wrong
    /// about its owner in the most annoying possible way.
    pub media_active: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Action {
    None,
    Lock,
    Unlock,
    /// Re-lock an unused eager unlock. The driver skips the
    /// confirmation scan (the phone IS present — that's the point).
    RelockIdle,
}

pub(crate) fn step(s: &mut ProxState, i: &Inputs) -> Action {
    // Adapter off → we can't observe presence; do nothing and keep all
    // state frozen (turning BT off must not lock the laptop). Resetting
    // last_link_live WITHOUT a drop edge means powering BT off never
    // reads as "the phone left".
    if !i.adapter_on {
        s.last_locked = i.locked;
        s.last_link_live = false;
        return Action::None;
    }
    let link_dropped = s.last_link_live && !i.link_live;
    s.last_link_live = i.link_live;

    let present = i.link_live
        || i.peer_contact_fresh
        || (i.last_presence_ms != 0
            && i.now_ms.saturating_sub(i.last_presence_ms) < AWAY_GRACE_MS);

    // Any user input means the user really is here — an eager unlock
    // is "claimed" and must not be re-locked.
    if i.user_active {
        s.eager_at_ms = 0;
    }

    // Lock-state edges.
    if let (Some(prev), Some(cur)) = (s.last_locked, i.locked) {
        if !prev && cur {
            // Just locked. Ours / phone-away → unlock may fire on return;
            // a manual lock with the phone present is respected.
            // An IDLE lock is not a deliberate one.
            //
            // The rule was "ours, or the phone was away" — so GNOME's own idle
            // lock, the ordinary way a screen locks when you step away for six
            // minutes with the phone still on the desk, counted as a manual
            // lock and demanded a password on return.
            //
            // A deliberate lock still stays locked, and that matters: locking
            // your screen on purpose with your phone in your pocket must not be
            // undone by the phone in your pocket. Idle time at the moment of
            // the edge is what separates them — nobody presses Super+L after
            // sitting still for five minutes, and the screensaver never fires
            // while you are typing.
            let was_idle_lock = i
                .idle_ms
                .is_some_and(|ms| ms >= IDLE_LOCK_MIN_MS);
            s.unlock_armed = s.we_locked || !present || was_idle_lock;
            s.we_locked = false;
            s.eager_at_ms = 0;
        }
        if prev && !cur {
            // Unlocked (any path) — reset the per-lock state.
            s.unlock_armed = false;
            s.wake_until_ms = 0;
            s.we_locked = false;
            s.require_wake = false;
        }
    }
    s.last_locked = i.locked;

    if present {
        if !s.present {
            // Real away→present cycle: a fresh arrival earns a fresh
            // eager unlock (and re-arms it when locked).
            s.require_wake = false;
            if i.locked == Some(true) {
                s.unlock_armed = true;
            }
        }
        s.present = true;
        s.lock_armed = true;
    } else {
        s.present = false;
    }

    // ---- idle re-lock: an eager unlock nobody used ----
    if s.eager_at_ms != 0
        && i.locked == Some(false)
        && i.now_ms.saturating_sub(s.eager_at_ms) >= RELOCK_IDLE_MS
        && i.idle_ms.map(|v| v >= RELOCK_IDLE_MS).unwrap_or(false)
    {
        s.eager_at_ms = 0;
        s.require_wake = true;
        s.we_locked = true; // the locked edge re-arms unlock_armed
        return Action::RelockIdle;
    }

    // ---- auto-lock fast path: the live session just dropped ----
    // The supervision timeout already proves radio loss moments ago — no
    // need to age out the 25s grace first. One shot per drop; the
    // driver's confirmation scan still gates the actual lock, so a churn
    // drop (phone still here, app restarting) gets caught there and
    // falls back to the grace path.
    if i.auto_lock_on
        && link_dropped
        && s.lock_armed
        && !i.media_active
        // A BLE drop is only evidence of absence when it is the ONLY thing we
        // had. If the phone is still answering on the LAN it plainly has not
        // left — its Bluetooth went off, or MIUI killed the app.
        && !i.peer_contact_fresh
        && i.locked == Some(false)
        && i.idle_ms.map(|v| v >= IDLE_GATE_MS).unwrap_or(false)
    {
        s.lock_armed = false;
        s.we_locked = true;
        return Action::Lock;
    }

    // ---- auto-lock: once, on the sustained present→absent edge ----
    if i.auto_lock_on
        && !present
        && s.lock_armed
        && !i.media_active
        && i.locked == Some(false)
        && i.idle_ms.map(|v| v >= IDLE_GATE_MS).unwrap_or(false)
    {
        s.lock_armed = false;
        s.we_locked = true;
        return Action::Lock;
    }

    // ---- auto-unlock ----
    // EAGER mode (default): the phone arriving IS the trigger — unlock
    // the moment the authenticated link is live, no user wake needed
    // (the idle re-lock above covers a walk-past). WAKE-GATED mode
    // (require_wake, after an idle re-lock): a key/mouse/lid touch is
    // required, with a short window for a link that lands late.
    if i.auto_unlock_on && i.locked == Some(true) && s.unlock_armed && i.phone_unlocked {
        let cooled =
            i.now_ms.saturating_sub(s.last_auto_unlock_ms) >= UNLOCK_COOLDOWN_MS;
        if i.link_live && cooled && (!s.require_wake || i.user_active) {
            s.last_auto_unlock_ms = i.now_ms;
            s.unlock_armed = false;
            s.wake_until_ms = 0;
            if !i.user_active {
                // Nobody touched anything yet — this is an arrival
                // (eager) unlock; start the unused→re-lock clock.
                s.eager_at_ms = i.now_ms;
            }
            return Action::Unlock;
        }
        if s.require_wake {
            if i.user_active && !i.link_live {
                // Woke before BLE re-established (resume from suspend):
                // hold the intent briefly; unlock when the link lands.
                s.wake_until_ms = i.now_ms + WAKE_WINDOW_MS;
            } else if s.wake_until_ms != 0
                && i.now_ms <= s.wake_until_ms
                && i.link_live
                && cooled
            {
                s.last_auto_unlock_ms = i.now_ms;
                s.unlock_armed = false;
                s.wake_until_ms = 0;
                return Action::Unlock;
            }
        }
    }

    Action::None
}

// --------------------------------------------------------------------------
// Driver
// --------------------------------------------------------------------------

/// Early-wake for the watcher's sampling loop. Fired by user input
/// (Mutter watch) and by ble.rs on session up/down edges, so lock and
/// unlock decisions don't wait out the 2s tick. `notify_one` stores a
/// permit — an edge during a busy iteration lands on the next await.
pub(crate) fn nudge() -> &'static tokio::sync::Notify {
    static NUDGE: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    NUDGE.get_or_init(tokio::sync::Notify::new)
}

/// Say ONCE per process that unlock is refused because the polkit rule was
/// never installed. Without this the whole feature fails in a `tracing::warn!`
/// nobody reads: the Settings toggle stays on, the phone shows an unlock
/// button, and neither ever does anything. Once per process, because the
/// proximity loop retries every 2s and a notification per attempt would be
/// worse than the silence.
pub(crate) async fn warn_unlock_denied_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    desktop_notify(
        "Unlock needs a one-time permission",
        "The system blocks remote unlock until you run, once, in the vortex \
         folder:  sudo linux/packaging/install-unlock-rule.sh",
    )
    .await;
}

async fn desktop_notify(title: &str, text: &str) {
    let Ok(n) = serde_json::from_value::<
        vortex_l3_daemon::core::notif_mirror::NotificationMirror,
    >(serde_json::json!({ "app": "Vortex", "title": title, "text": text })) else {
        return;
    };
    let _ = vortex_l3_daemon::core::notification_display::show(&n, 0).await;
}

/// Spawn the proximity watcher: a 2s sampling loop (woken instantly by
/// user input) feeding the pure state machine, executing Lock/Unlock via
/// `session_lock` and narrating with desktop notifications.
pub(crate) fn spawn_proximity_watch(
    ble_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
    adapter: bluer::Adapter,
    peer_store: Arc<dyn vortex_l3_daemon::core::storage::peers::PeerStore>,
) {
    load_settings_into_statics();
    tokio::spawn(async move {
        use vortex_l3_daemon::core::session_lock;

        let active_flag = Arc::new(AtomicBool::new(false));
        {
            let af = active_flag.clone();
            if let Err(e) = session_lock::watch_user_active(move || {
                af.store(true, Ordering::Relaxed);
                nudge().notify_one();
            })
            .await
            {
                tracing::warn!("proximity: user-active watch unavailable: {e} (auto-unlock degraded)");
            }
        }

        let mut st = ProxState::default();
        loop {
            let auto_lock_on = AUTO_LOCK_ON.load(Ordering::Relaxed);
            let auto_unlock_on = AUTO_UNLOCK_ON.load(Ordering::Relaxed);
            let user_active = active_flag.swap(false, Ordering::Relaxed);
            if auto_lock_on || auto_unlock_on {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let link_live = !ble_writers.lock().await.is_empty();
                let last_presence_ms =
                    crate::ble::LAST_PRESENCE_MS.load(Ordering::Relaxed);
                let adapter_on = adapter.is_powered().await.unwrap_or(false);
                let locked = session_lock::locked_hint().await;
                // Idle time only matters for the lock decision; skip the
                // D-Bus call otherwise.
                let idle_ms = if auto_lock_on {
                    session_lock::idle_ms().await
                } else {
                    None
                };
                let action = step(
                    &mut st,
                    &Inputs {
                        now_ms,
                        link_live,
                        last_presence_ms,
                        locked,
                        idle_ms,
                        adapter_on,
                        auto_lock_on,
                        auto_unlock_on,
                        user_active,
                        phone_unlocked: LAST_PHONE_UNLOCKED.load(Ordering::Relaxed),
                        // The phone answering on the LAN is presence too. The
                        // window matches the pill sweeper's "we are in contact"
                        // threshold.
                        peer_contact_fresh: crate::ble::peer_contact_age_ms() < AWAY_GRACE_MS,
                        // Only costs a D-Bus round-trip when a lock is possible.
                        media_active: auto_lock_on
                            && vortex_l3_daemon::core::media_runtime::any_player_playing().await,
                    },
                );
                match action {
                    Action::Lock => {
                        // Last-chance confirmation scan: a short token-
                        // validated sweep. Catches the phone sitting right
                        // here while the session flaps (RPA churn) — the
                        // false positive that a short AWAY_GRACE (or the
                        // link-drop fast path) would otherwise turn into a
                        // spurious lock. A parallel scan by the reconnect
                        // loop counts too — it touches LAST_PRESENCE_MS
                        // through the same helper — but only an ADVANCE
                        // during our scan counts: pre-drop freshness (the
                        // last heartbeat seconds before the session died)
                        // must not veto the fast path.
                        let presence_before =
                            crate::ble::LAST_PRESENCE_MS.load(Ordering::Relaxed);
                        let found = crate::ble::find_trusted_presence_peer(
                            &adapter,
                            &peer_store,
                            std::time::Duration::from_millis(CONFIRM_SCAN_MS),
                        )
                        .await
                        .is_some()
                            || crate::ble::LAST_PRESENCE_MS.load(Ordering::Relaxed)
                                > presence_before;
                        if found {
                            tracing::info!(
                                "proximity: confirm-scan still sees the phone — not locking"
                            );
                            st.lock_armed = true;
                            st.we_locked = false;
                        } else {
                            tracing::info!("proximity: phone away + idle — locking session");
                            match session_lock::lock().await {
                                Ok(()) => {
                                    desktop_notify(
                                        "Locked",
                                        "Phone out of range — session locked.",
                                    )
                                    .await;
                                }
                                Err(e) => tracing::warn!("proximity lock failed: {e}"),
                            }
                        }
                    }
                    Action::Unlock => {
                        tracing::info!("proximity: phone present — unlocking");
                        match session_lock::unlock().await {
                            Ok(()) => {
                                desktop_notify(
                                    "Unlocked",
                                    "Welcome back — unlocked via phone presence.",
                                )
                                .await;
                            }
                            Err(e) => {
                                tracing::warn!("proximity unlock failed: {e}");
                                if session_lock::is_unlock_denied(&e) {
                                    warn_unlock_denied_once().await;
                                }
                            }
                        }
                    }
                    Action::RelockIdle => {
                        tracing::info!(
                            "proximity: eager unlock unused for {}s — locking again",
                            RELOCK_IDLE_MS / 1000
                        );
                        if let Err(e) = session_lock::lock().await {
                            tracing::warn!("proximity re-lock failed: {e}");
                        }
                    }
                    Action::None => {}
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                _ = nudge().notified() => {}
            }
        }
    });
}

// --------------------------------------------------------------------------
// Tests — every edge of the machine, no BLE/D-Bus needed
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The two guards that stop the laptop locking in front of its owner.
    ///
    /// Both were real: presence counted BLE only, so turning the phone's
    /// Bluetooth off (or MIUI killing the app) read as "left the building" while
    /// the laptop's own window still said Online off the LAN heartbeat; and the
    /// idle gate reads "idle" for the whole length of a film.
    #[test]
    fn lan_contact_alone_counts_as_present() {
        let mut st = ProxState { lock_armed: true, present: true, last_link_live: true, ..Default::default() };
        let mut i = base(100_000);
        i.link_live = false;          // BLE gone
        i.last_presence_ms = 0;       // no advertisement either
        i.peer_contact_fresh = true;  // …but the phone answers on the LAN
        i.idle_ms = Some(IDLE_GATE_MS);
        assert_eq!(step(&mut st, &i), Action::None, "a phone on the LAN is present");
    }

    #[test]
    fn playback_blocks_the_lock() {
        let mut st = ProxState { lock_armed: true, present: true, last_link_live: true, ..Default::default() };
        let mut i = base(100_000);
        i.link_live = false;
        i.last_presence_ms = 0;
        i.peer_contact_fresh = false; // genuinely absent
        i.media_active = true;        // but a video is playing here
        i.idle_ms = Some(IDLE_GATE_MS);
        assert_eq!(step(&mut st, &i), Action::None, "must not lock mid-playback");

        // …and with playback stopped, the same state DOES lock: the guard
        // narrows the rule, it does not disable it.
        let mut st2 = ProxState { lock_armed: true, present: true, last_link_live: true, ..Default::default() };
        let mut i2 = base(100_000);
        i2.link_live = false;
        i2.last_presence_ms = 0;
        i2.peer_contact_fresh = false;
        i2.media_active = false;
        i2.idle_ms = Some(IDLE_GATE_MS);
        assert_eq!(step(&mut st2, &i2), Action::Lock, "auto-lock still works");
    }

    /// GNOME's own idle lock, with the phone on the desk, must still unlock on
    /// return — while a DELIBERATE lock in the same conditions must not.
    ///
    /// Asserted on the ACTION rather than on `unlock_armed`, because the eager
    /// unlock consumes the flag within the same step.
    #[test]
    fn an_idle_lock_unlocks_on_return_but_a_manual_one_does_not() {
        for (idle_ms, should_unlock) in [(IDLE_LOCK_MIN_MS + 1, true), (1_000, false)] {
            let mut st = ProxState {
                present: true,
                last_locked: Some(false),
                ..Default::default()
            };
            let mut i = base(100_000);
            i.link_live = true; // phone right here, authenticated
            i.locked = Some(true); // …and the screen has just locked
            i.idle_ms = Some(idle_ms);
            let action = step(&mut st, &i);
            let unlocked = action == Action::Unlock;
            assert_eq!(
                unlocked, should_unlock,
                "a lock taken after {idle_ms}ms idle should {} unlock",
                if should_unlock { "" } else { "NOT" }
            );
        }
    }

    fn base(now_ms: u64) -> Inputs {
        Inputs {
            now_ms,
            link_live: false,
            last_presence_ms: 0,
            locked: Some(false),
            idle_ms: Some(IDLE_GATE_MS),
            adapter_on: true,
            auto_lock_on: true,
            auto_unlock_on: true,
            user_active: false,
            phone_unlocked: true,
            peer_contact_fresh: false,
            media_active: false,
        }
    }

    /// Walk the machine to "phone present (advertisement-fresh, no live
    /// session), unlocked" at t=now. Grace-path tests build on this; the
    /// link-drop fast-path tests establish their own live session.
    fn present_state(now: u64) -> ProxState {
        let mut s = ProxState::default();
        let mut i = base(now);
        i.last_presence_ms = now;
        assert_eq!(step(&mut s, &i), Action::None);
        s
    }

    #[test]
    fn no_lock_without_ever_seeing_the_phone() {
        let mut s = ProxState::default();
        // Fresh start, phone long gone: must NOT lock.
        let i = base(1_000_000);
        let mut s2 = s;
        assert_eq!(step(&mut s2, &i), Action::None);
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn locks_once_after_grace_and_idle() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Phone vanishes; before grace → nothing.
        let mut i = base(t0 + AWAY_GRACE_MS / 2);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // After grace + idle → lock fires exactly once.
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn idle_gate_blocks_lock_while_user_is_typing() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        i.idle_ms = Some(1_000); // user active at the keyboard
        assert_eq!(step(&mut s, &i), Action::None);
        // …and fires once they go idle.
        i.idle_ms = Some(IDLE_GATE_MS);
        assert_eq!(step(&mut s, &i), Action::Lock);
    }

    #[test]
    fn adapter_off_freezes_everything() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        let mut i = base(t0 + AWAY_GRACE_MS * 10);
        i.last_presence_ms = t0;
        i.adapter_on = false;
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn auto_lock_then_return_then_wake_unlocks() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Leave → lock.
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        // LockedHint flips.
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Phone returns (session up), user hits a key → unlock.
        let t2 = t1 + 60_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        i.user_active = true;
        assert_eq!(step(&mut s, &i), Action::Unlock);
        // Repeat wake (still locked per hint) — no second unlock.
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn auto_unlock_gated_on_phone_unlocked() {
        // Same arrival+wake scenario as auto_lock_then_return_then_wake_unlocks,
        // but with the owner-present gate: a LOCKED phone must veto the unlock even
        // when presence + live link + user-wake all say "go"; unlocking the phone
        // then lets it through.
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Phone returns + user wakes, but the phone is LOCKED → no unlock.
        let t2 = t1 + 60_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        i.user_active = true;
        i.phone_unlocked = false;
        assert_eq!(step(&mut s, &i), Action::None);
        // Owner unlocks the phone → the same inputs now unlock the laptop.
        i.phone_unlocked = true;
        assert_eq!(step(&mut s, &i), Action::Unlock);
    }

    #[test]
    fn manual_lock_with_phone_present_is_respected() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // User locks manually (Super+L) while phone sits next to them.
        let mut i = base(t0 + 2_000);
        i.link_live = true;
        i.last_presence_ms = t0 + 2_000;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Mouse nudge → must NOT unlock.
        let mut i2 = i;
        i2.now_ms = t0 + 4_000;
        i2.user_active = true;
        assert_eq!(step(&mut s, &i2), Action::None);
    }

    #[test]
    fn manual_lock_then_away_then_return_arms_unlock() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Manual lock with phone present.
        let mut i = base(t0 + 2_000);
        i.link_live = true;
        i.last_presence_ms = t0 + 2_000;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Phone leaves (locked stays true).
        let t1 = t0 + 2_000 + AWAY_GRACE_MS + 1_000;
        let mut i = base(t1);
        i.last_presence_ms = t0 + 2_000;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Phone returns + wake → unlock allowed now.
        let t2 = t1 + 10_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        i.user_active = true;
        assert_eq!(step(&mut s, &i), Action::Unlock);
    }

    #[test]
    fn wake_window_unlocks_when_link_lands_late() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Leave → lock → hint flips.
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Lid opens after suspend: user active, link NOT yet up.
        let t2 = t1 + 600_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.user_active = true;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // 4s later the BLE session lands → unlock fires from the window.
        let mut i = base(t2 + 4_000);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2 + 4_000;
        assert_eq!(step(&mut s, &i), Action::Unlock);
        // …but not again.
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn eager_unlock_fires_on_arrival_without_any_wake() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Leave → lock → hint flips.
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        assert_eq!(step(&mut s, &i), Action::None);
        // Phone walks back in; the session lands; NO key was touched —
        // it must still unlock (the user's "telefon keldi = men keldim").
        let t2 = t1 + 120_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        assert_eq!(step(&mut s, &i), Action::Unlock);
    }

    #[test]
    fn unused_eager_unlock_relocks_then_needs_a_wake() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        // Leave → lock; return → eager unlock.
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        step(&mut s, &i);
        let t2 = t1 + 60_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        assert_eq!(step(&mut s, &i), Action::Unlock);
        // Hint flips to unlocked; nobody touches anything for 90s.
        let t3 = t2 + 5_000;
        let mut i = base(t3);
        i.locked = Some(false);
        i.link_live = true;
        i.last_presence_ms = t3;
        i.idle_ms = Some(0);
        assert_eq!(step(&mut s, &i), Action::None);
        let t4 = t2 + RELOCK_IDLE_MS + 1_000;
        let mut i = base(t4);
        i.locked = Some(false);
        i.link_live = true;
        i.last_presence_ms = t4;
        i.idle_ms = Some(RELOCK_IDLE_MS);
        assert_eq!(step(&mut s, &i), Action::RelockIdle);
        // Hint flips locked. Still in range, still no input → must NOT
        // eagerly unlock again (require_wake).
        let t5 = t4 + 5_000;
        let mut i = base(t5);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t5;
        assert_eq!(step(&mut s, &i), Action::None);
        // A real key touch unlocks instantly (link already live).
        let t6 = t5 + 5_000;
        let mut i = base(t6);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t6;
        i.user_active = true;
        assert_eq!(step(&mut s, &i), Action::Unlock);
    }

    #[test]
    fn input_during_eager_window_prevents_relock() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        let t1 = t0 + AWAY_GRACE_MS + 5_000;
        let mut i = base(t1);
        i.last_presence_ms = t0;
        i.locked = Some(true);
        step(&mut s, &i);
        let t2 = t1 + 60_000;
        let mut i = base(t2);
        i.locked = Some(true);
        i.link_live = true;
        i.last_presence_ms = t2;
        assert_eq!(step(&mut s, &i), Action::Unlock);
        // User starts typing 10s after the eager unlock.
        let t3 = t2 + 10_000;
        let mut i = base(t3);
        i.locked = Some(false);
        i.link_live = true;
        i.last_presence_ms = t3;
        i.user_active = true;
        i.idle_ms = Some(0);
        assert_eq!(step(&mut s, &i), Action::None);
        // 90s+ later: no re-lock — the unlock was claimed.
        let t4 = t2 + RELOCK_IDLE_MS + 10_000;
        let mut i = base(t4);
        i.locked = Some(false);
        i.link_live = true;
        i.last_presence_ms = t4;
        i.idle_ms = Some(RELOCK_IDLE_MS);
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn link_drop_locks_fast_without_waiting_grace() {
        let t0 = 1_000_000;
        let mut s = ProxState::default();
        // Live session up.
        let mut i = base(t0);
        i.link_live = true;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // Session drops 2s later; presence timestamp is still FRESH
        // (last heartbeat) — the fast path must fire anyway.
        let mut i = base(t0 + 2_000);
        i.link_live = false;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
        // One-shot: the next tick (link still down) does nothing more.
        let mut i = base(t0 + 4_000);
        i.link_live = false;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn link_drop_fast_path_respects_idle_gate() {
        let t0 = 1_000_000;
        let mut s = ProxState::default();
        let mut i = base(t0);
        i.link_live = true;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // Drop while the user is typing → no fast lock…
        let mut i = base(t0 + 2_000);
        i.link_live = false;
        i.last_presence_ms = t0;
        i.idle_ms = Some(1_000);
        assert_eq!(step(&mut s, &i), Action::None);
        // …and the grace path still covers it once they go idle.
        let mut i = base(t0 + AWAY_GRACE_MS + 1_000);
        i.link_live = false;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::Lock);
    }

    #[test]
    fn adapter_power_off_is_not_a_link_drop() {
        let t0 = 1_000_000;
        let mut s = ProxState::default();
        let mut i = base(t0);
        i.link_live = true;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // BT powered off: frozen, and must not register a drop edge.
        let mut i = base(t0 + 2_000);
        i.adapter_on = false;
        i.link_live = false;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
        // Power back on, link still down, presence stale-but-frozen:
        // no phantom fast lock from the off/on cycle.
        let mut i = base(t0 + 4_000);
        i.link_live = false;
        i.last_presence_ms = t0;
        assert_eq!(step(&mut s, &i), Action::None);
    }

    #[test]
    fn disabled_toggles_do_nothing() {
        let t0 = 1_000_000;
        let mut s = present_state(t0);
        let mut i = base(t0 + AWAY_GRACE_MS + 1);
        i.last_presence_ms = t0;
        i.auto_lock_on = false;
        assert_eq!(step(&mut s, &i), Action::None);
    }
}
