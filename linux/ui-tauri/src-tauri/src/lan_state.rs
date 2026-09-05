//! Inbound peer-state application — split out of `lan.rs`. Takes the AppState
//! a peer pushes (over BLE or the LAN heartbeat) and applies it to THIS laptop:
//! emits the UI/tray update, mirrors the phone's call into the call pill
//! (`dispatch_appstate_call`), and executes a remote lock/unlock command
//! (`dispatch_lock_command`, edge-triggered by seq). `spawn_state_consumer` is
//! the BLE-state channel task; the LAN heartbeat in `lan.rs` reuses the two
//! dispatch helpers.

use tauri::AppHandle;

use crate::{app_state_to_dto, MEDIA_WATCH};


/// Last call id seen via the AppState `call` field, so a transition to
/// `None` (call gone) can synthesize an `ended` to clear the pill even when
/// the BLE CALL frame's own `ended` was lost to a mid-call BLE drop.
static LAST_APPSTATE_CALL_ID: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// When we last saw a NON-None `call` in an AppState. A `None` only clears the
/// pill once the call has been absent for a short while — a single transient
/// `None` (one transport's heartbeat snapshot omitting the call while the other
/// still carries it, or a one-beat phone blip) must NOT clear-then-rebuild the
/// pill every heartbeat, which is exactly what made the call pill flicker.
static LAST_APPSTATE_CALL_SOME_AT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// Highest `lock_command_seq` we've already executed. BLE and LAN can both
/// deliver the same phone AppState (and the phone re-sends the snapshot on
/// every push until cleared), so the command must be edge-triggered.
static LAST_LOCK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Execute a phone-issued remote lock/unlock carried in its AppState
/// (`lock_command` one-shot + monotonic `lock_command_seq`). Called from
/// BOTH state ingress paths (LAN heartbeat + BLE STATE consumer), exactly
/// like `dispatch_appstate_call`. Trust model: the snapshot only ever
/// arrives over the Noise-authenticated transport from the paired phone.
pub(crate) fn dispatch_lock_command(state: &vortex_l3_daemon::core::appstate::AppState) {
    use std::sync::atomic::Ordering;
    let Some(cmd) = state.lock_command.clone() else { return };
    let seq = state.lock_command_seq;
    if seq == 0 || seq <= LAST_LOCK_SEQ.load(Ordering::Relaxed) {
        return;
    }
    // Owner-present gate (the ONE place the rule lives): only honour a remote
    // UNLOCK while the phone itself is unlocked = owner present. Locking is
    // always safe. Crucially we DON'T advance the seq when we hold an unlock,
    // so the same command (re-sent every heartbeat until its TTL) executes the
    // moment the user unlocks the phone. Mirrors the proximity auto-unlock gate,
    // so all three unlock paths (app button, notification, proximity) share it.
    if cmd == "unlock" && state.unlocked != Some(true) {
        tracing::info!(seq, "remote unlock held — phone is locked (owner-present gate)");
        return;
    }
    LAST_LOCK_SEQ.store(seq, Ordering::Relaxed);
    tokio::spawn(async move {
        let res = match cmd.as_str() {
            "lock" => vortex_l3_daemon::core::session_lock::lock().await,
            "unlock" => vortex_l3_daemon::core::session_lock::unlock().await,
            other => Err(format!("unknown lock command {other:?}")),
        };
        match res {
            Ok(()) => tracing::info!(%cmd, seq, "remote lock command executed"),
            Err(e) => {
                tracing::warn!(%cmd, seq, "remote lock command failed: {e}");
                // The phone's unlock button hits the same polkit gate as
                // proximity unlock; tell the user rather than dropping it.
                if vortex_l3_daemon::core::session_lock::is_unlock_denied(&e) {
                    crate::proximity::warn_unlock_denied_once().await;
                }
            }
        }
    });
}

/// Feed a peer AppState's `call` into the call consumer (the additive LAN/
/// BLE-STATE path for the call mirror). Idempotent: the consumer dedups by
/// (id, phase), so re-sending the current call on every heartbeat is free.
/// Best-effort — no call-mirror wired (None) just returns; never errors.
pub(crate) fn dispatch_appstate_call(
    call: &Option<vortex_l3_daemon::core::call_event::CallEvent>,
) {
    let Some(tx) = crate::CALL_MIRROR_TX.get() else { return };
    let mut last = match LAST_APPSTATE_CALL_ID.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    match call {
        Some(ev) => {
            if let Ok(mut t) = LAST_APPSTATE_CALL_SOME_AT.lock() {
                *t = Some(std::time::Instant::now());
            }
            let _ = tx.send(ev.clone());
            *last = Some(ev.id.clone());
        }
        None => {
            // Debounce: ignore a transient `None` while the call is still being
            // re-sent on the other transport / will return on the next beat.
            // Only a SUSTAINED absence (no `call` for >5s — the in-call LAN
            // heartbeat is pinned to ~2s, so that's ~2 missed beats) is a real
            // end. The normal call-end clears the pill immediately via the
            // explicit `ended` CALL frame; this synthesis is only the backstop
            // for a lost `ended`, so a few seconds' delay here is harmless and
            // it stops the pill flickering mid-call.
            let recent = LAST_APPSTATE_CALL_SOME_AT
                .lock()
                .ok()
                .and_then(|g| *g)
                .map(|t| t.elapsed() < std::time::Duration::from_secs(5))
                .unwrap_or(false);
            if recent {
                return;
            }
            // Call gone per AppState. If we were tracking one, clear the pill.
            if let Some(id) = last.take() {
                let _ = tx.send(vortex_l3_daemon::core::call_event::CallEvent {
                    id,
                    phase: vortex_l3_daemon::core::call_event::CallEvent::PHASE_ENDED.to_string(),
                    name: String::new(),
                    number: String::new(),
                    started_at: 0,
                    outgoing: false,
                    connected: false,
                    app_id: String::new(),
                    sent_at: 0,
                    muted: false,
                    speaker: false,
                    has_earbuds: false,
                });
            }
        }
    }
}

pub(crate) fn spawn_state_consumer(
    app: AppHandle,
    peer_store: std::sync::Arc<dyn vortex_l3_daemon::core::storage::peers::PeerStore>,
) -> tokio::sync::mpsc::UnboundedSender<([u8; 32], vortex_l3_daemon::core::appstate::AppState)> {
            let (ble_state_tx, mut ble_state_rx) = tokio::sync::mpsc::unbounded_channel::<(
                [u8; 32],
                vortex_l3_daemon::core::appstate::AppState,
            )>();
            {
                let app_state = app.clone();
                let peer_store = peer_store.clone();
                tokio::spawn(async move {
                    use tauri::Emitter;
                    // One bluer adapter for the whole consumer (a fresh
                    // session per state push would leak D-Bus connections) —
                    // used to resolve the LOCAL earbuds for the tray's buds
                    // row when the laptop owns them.
                    let adapter = match bluer::Session::new().await {
                        Ok(s) => s.default_adapter().await.ok(),
                        Err(_) => None,
                    };
                    while let Some((peer_pub, state)) = ble_state_rx.recv().await {
                        // An inbound BLE frame proves the phone is in range.
                        crate::ble::touch_presence();
                        // …and that we're in live contact (any-transport) — gates
                        // the disconnect-clear of mirror pills.
                        crate::ble::touch_peer_contact();
                        // The phone's self-reported Wi-Fi IP → cached-peer-IP
                        // fast path. THE fix for "mirror dials a dead address":
                        // while BLE is up the phone answers no mDNS, so this
                        // BLE-carried hint is the only way the cache tracks a
                        // DHCP renew / network change.
                        crate::lan::note_peer_reported_ip(&state);
                        // Bidirectional forget, on THIS path too. The phone sets
                        // `revoked` in the snapshot it pushes over both
                        // transports, but only the LAN heartbeat in `lan.rs` was
                        // acting on it — so forgetting the laptop on the phone
                        // left the laptop still trusting it whenever the revoke
                        // did not happen to land over LAN inside the phone's
                        // 1.5 s grace window (different network, mDNS not yet
                        // resolved, or simply a BLE-only moment). BLE carries
                        // the same flag reliably; honour it here and stop
                        // reading the peer's own snapshots afterwards.
                        if state.revoked {
                            tracing::info!(
                                "peer revoked us (via BLE); forgetting {}",
                                hex::encode(&peer_pub[..8])
                            );
                            let _ = peer_store.forget(&peer_pub);
                            crate::emit_peers(&app_state, peer_store.clone()).await;
                            continue;
                        }
                        // Vue UI — identical event to the LAN heartbeat path.
                        // Also feed the call carried in this STATE frame (a
                        // backstop if a dedicated CALL frame was dropped).
                        dispatch_appstate_call(&state.call);
                        // Browsing-handoff backstop: the page the phone is on,
                        // carried in this STATE frame (when the BLE HANDOFF frame
                        // didn't get through). Consumer dedups by URL.
                        crate::handoff::dispatch_appstate_handoff(&state.handoff);
                        // Laptop→phone screen mirror over the BLE STATE path too.
                        crate::laptop_cast::dispatch_request(state.laptop_mirror_req, state.laptop_mirror_extend);
                        // NB: Continuity Camera is dispatched LAN-ONLY (in lan.rs),
                        // NOT here — the video stream needs LAN anyway, and acting
                        // on the offer over both transports raced start/stop. The
                        // BLE STATE path only carries the level; LAN drives it.
                        // Remote lock/unlock button on the phone (seq-deduped).
                        dispatch_lock_command(&state);
                        // Media transport button on the phone's laptop-media
                        // notification (seq-deduped) → MPRIS.
                        crate::media_remote::dispatch_media_command(&state);
                        // Owner-present gate: record the phone's unlock state for
                        // proximity auto-unlock.
                        crate::proximity::note_phone_unlocked(state.unlocked);
                        let dto = app_state_to_dto(hex::encode(peer_pub), state.clone());
                        let _ = app_state.emit("vortex:peer_state", dto);
                        // Auto-pin the peer's earbuds locally so the card shows
                        // on this device too (no-op unless they're connected,
                        // carry an address, and we have none saved).
                        crate::earbuds::persist_peer_earbuds(&state);
                        // Shared smart-switch setting, LWW — adopt here too so a
                        // phone toggle pushed over BLE (~200 ms) takes effect
                        // immediately instead of waiting for the next LAN
                        // heartbeat (the consumer below only runs on a LAN pull).
                        if let Some(mw) = MEDIA_WATCH.get() {
                            if mw.apply_setting(state.smart_switch_enabled, state.smart_switch_changed_at) {
                                tracing::info!(
                                    enabled = state.smart_switch_enabled,
                                    "smart-switch: adopted peer setting (LWW, via BLE)"
                                );
                                let _ = app_state.emit("vortex:smart_switch", state.smart_switch_enabled);
                            }
                        }
                        // Tray rows + tooltip — same rendering as the LAN
                        // heartbeat path (shared helper). The buds row used to
                        // be "left to the next heartbeat", but on a BLE-only
                        // link (AP isolation / network change) the LAN
                        // heartbeat never completes, so the tray sat at
                        // "Buds --" forever even while the buds were live on
                        // the laptop.
                        let local_earbuds = match &adapter {
                            Some(a) => {
                                vortex_l3_daemon::core::earbuds::scan_local_earbuds(a).await
                            }
                            None => None,
                        };
                        crate::tray::update_battery_rows(
                            local_earbuds.as_ref(),
                            Some(&state),
                        );
                    }
                });
            }
    ble_state_tx
}
