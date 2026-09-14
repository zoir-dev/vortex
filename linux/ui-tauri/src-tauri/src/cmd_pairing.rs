//! Pairing + trust command handlers (Scan / Pair / ForgetPeer / ForgetAll),
//! split out of `run_worker`. Each takes `&WorkerCtx`; Scan/Pair also borrow the
//! loop-local `active_scan` handle so a new scan supersedes the previous one and
//! Pair can quiet the radio before connecting.

use std::time::Duration;

use tauri::Emitter;

#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::ble::scanner::run_filtered_scan;

#[cfg(target_os = "linux")]
use crate::ipc::{PairingResultDto, PairingStartedDto, ScanHitDto};
use crate::ipc::emit_peers;
#[cfg(target_os = "linux")]
use crate::pairing::do_pair;
use crate::pairing::send_revoke_to_peer;
use crate::worker_ctx::WorkerCtx;

/// Wipe every cached scrap of the peer's data — contacts, recents, SMS, notes,
/// and the LAN fast-path IP — and blank the matching UI pages. Forgetting a
/// peer must leave nothing of the old phone behind (a new or re-paired phone
/// starts clean), so this runs on both ForgetPeer and ForgetAll. V1 is
/// single-peer, so "the peer's data" is simply all of it.
///
/// Clipboard history is deliberately NOT wiped: it's a laptop-local feature
/// (the Super+V popup), not the peer's data, so it outlives the link.
pub(crate) fn purge_peer_cache(app: &tauri::AppHandle) {
    crate::contacts::clear(app);
    crate::call_log::clear(app);
    crate::sms::clear(app);
    crate::notes::clear(app);
    crate::lan::clear_last_peer_ip();
}

/// `UiCmd::Scan` — pairable-only BLE scan, superseding any running scan.
///
/// Linux-only: it holds a BlueZ adapter. The seam equivalent for other
/// platforms is `ble_portable::scan_for_ui`.
#[cfg(target_os = "linux")]
pub(crate) fn scan(ctx: &WorkerCtx, active_scan: &mut Option<tokio::task::JoinHandle<()>>) {
    // Supersede any still-running scan so handles don't leak.
    if let Some(prev) = active_scan.take() {
        prev.abort();
    }
    let app_c = ctx.app.clone();
    let adapter_c = ctx.adapter.clone();
    *active_scan = Some(tokio::spawn(async move {
        let _ = app_c.emit("vortex:busy", true);
        let app_for_cb = app_c.clone();
        let _ = tokio::time::timeout(
            Duration::from_secs(8),
            run_filtered_scan(adapter_c, move |c| {
                // Only surface pairable adv hits — a trusted-presence beacon
                // means the peer is already paired (or paired with a different
                // Linux), it must not show up as a fresh pair target.
                if !c.payload.flags.is_pairable() {
                    return;
                }
                let hit = ScanHitDto {
                    addr: c.address.to_string(),
                    rssi: c.rssi.unwrap_or(0),
                    instance: hex::encode(c.payload.payload_8),
                    name: c.local_name.clone(),
                };
                tracing::info!(
                    addr = %hit.addr,
                    rssi = hit.rssi,
                    instance = %hit.instance,
                    name = ?hit.name,
                    "scan hit"
                );
                let _ = app_for_cb.emit("vortex:scan_result", hit);
            }),
        )
        .await;
        let _ = app_c.emit::<Option<()>>("vortex:scan_done", None);
        let _ = app_c.emit("vortex:busy", false);
    }));
}

/// `UiCmd::Pair` — quiet the radio (abort+await any scan), then run IK pairing.
///
/// Linux-only for the same reason as [`scan`]; elsewhere the worker calls
/// `ble_portable::pair_by_scan`.
#[cfg(target_os = "linux")]
pub(crate) async fn pair(
    ctx: &WorkerCtx,
    addr_str: String,
    active_scan: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let app = &ctx.app;
    let _ = app.emit(
        "vortex:pairing_started",
        PairingStartedDto { peer_addr: addr_str.clone() },
    );
    // Quiet the radio before connecting. An in-flight pairable scan contends
    // with connection establishment and stretched the pair connect to ~10 s
    // (vs ~0.3 s for reconnect, which stops its scan first). Abort+await the
    // scan task so its discover_devices stream drops and StopDiscovery fires,
    // then poll until the adapter is no longer discovering (bounded).
    if let Some(h) = active_scan.take() {
        h.abort();
        let _ = h.await;
    }
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while ctx.adapter.is_discovering().await.unwrap_or(false) {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let result = do_pair(app, &ctx.adapter, &addr_str, &ctx.identity, ctx.peer_store.clone()).await;
    match result {
        Ok(_) => {
            let _ = app.emit(
                "vortex:pairing_result",
                PairingResultDto::Ok {
                    ok: true,
                    message: format!("trust persisted with {addr_str}"),
                },
            );
            emit_peers(app, ctx.peer_store.clone()).await;
        }
        Err(err) => {
            // Log it: the UI funnels every failure into the same "codes didn't
            // match" abort screen (PairingOverlay.vue keys off `ok` alone), so
            // without this the real reason — connect, bearer, discovery — is
            // lost entirely and the user is told it was a MITM scare.
            tracing::warn!(peer = %addr_str, "pairing failed: {err}");
            let _ = app.emit(
                "vortex:pairing_result",
                PairingResultDto::Err { ok: false, error: err },
            );
        }
    }
}

/// How long a switch window stays open (design doc §D9). Long enough to walk
/// to another machine and wake it, short enough that an unattended press stops
/// scanning-on-top-of-a-live-link — the most expensive radio state we have.
const SWITCH_WINDOW_SECS: u64 = 45;

/// One candidate device offered by a switch scan.
#[derive(serde::Serialize, Clone)]
struct SwitchCandidateDto {
    peer_static_pub: String,
    name: Option<String>,
    rssi: i16,
}

/// `UiCmd::SwitchPeer` — keep the current peer, look for another trusted one.
///
/// Explicitly NOT a release: the active link is held for the whole scan, so
/// the persistent reconnect loop has nothing to race back into and the laptop
/// cannot end up connected to nothing. Ownership only moves in
/// [`activate_peer`], once a replacement is actually in hand (§D3).
///
/// Returns immediately and does the scan on a spawned task. The worker's
/// command loop is strictly sequential — awaiting a 45 s scan here would stall
/// every other command behind it, including the 5 s earbuds heartbeat. Same
/// reason `UiCmd::Scan` spawns rather than awaiting.
/// Linux-only: it drives a BlueZ discovery to find the other trusted peers
/// on air. The seam has no multi-peer scan yet, so the dispatcher off Linux
/// simply has no arm for `SwitchPeer` — `ActivatePeer` still works, so a peer
/// already known can be made active there.
#[cfg(target_os = "linux")]
pub(crate) fn switch_peer(ctx: &WorkerCtx) {
    // A second press while a window is open is a no-op rather than a second
    // scan: two concurrent discoveries would fight over the adapter.
    if crate::arbiter::is_switching() {
        tracing::debug!("switch already in progress; ignoring");
        return;
    }
    let active = crate::arbiter::active();
    crate::arbiter::begin_switch(Duration::from_secs(SWITCH_WINDOW_SECS));

    let app = ctx.app.clone();
    let adapter = ctx.adapter.clone();
    let peer_store = ctx.peer_store.clone();
    tokio::spawn(async move {
        let _ = app.emit("vortex:switch_scanning", true);
        let candidates = crate::ble::scan_other_trusted_peers(
            &adapter,
            &peer_store,
            active,
            Duration::from_secs(SWITCH_WINDOW_SECS),
        )
        .await;
        let _ = app.emit("vortex:switch_scanning", false);

        // Cancelled while we were scanning — drop the result rather than
        // acting on a switch the user already backed out of.
        if !crate::arbiter::is_switching() {
            tracing::info!("switch window closed during scan; discarding candidates");
            return;
        }

        let dtos: Vec<SwitchCandidateDto> = candidates
            .iter()
            .map(|c| SwitchCandidateDto {
                peer_static_pub: hex::encode(c.peer_static_pub),
                name: c.name.clone(),
                rssi: c.rssi,
            })
            .collect();
        tracing::info!(count = dtos.len(), "switch scan finished");

        match candidates.as_slice() {
            // Nothing else in range: report it and close, leaving the current
            // peer untouched. The UI shows "no other device found".
            [] => {
                crate::arbiter::end_switch();
                let _ = app.emit("vortex:switch_candidates", dtos);
            }
            // Exactly one — no point asking which.
            [only] => {
                do_activate(&app, &peer_store, only.peer_static_pub).await;
            }
            // Several: let the user pick (§D8).
            _ => {
                let _ = app.emit("vortex:switch_candidates", dtos);
            }
        }
    });
}

/// `UiCmd::CancelSwitch` — close the window, change nothing.
pub(crate) async fn cancel_switch(ctx: &WorkerCtx) {
    crate::arbiter::end_switch();
    let _ = ctx.app.emit("vortex:switch_scanning", false);
    let _ = ctx
        .app
        .emit::<Vec<SwitchCandidateDto>>("vortex:switch_candidates", Vec::new());
}

/// `UiCmd::ActivatePeer` — hand session ownership to this trusted peer.
pub(crate) async fn activate_peer(ctx: &WorkerCtx, hex_str: String) {
    let Ok(bytes) = hex::decode(&hex_str) else { return };
    if bytes.len() != 32 {
        return;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    do_activate(&ctx.app, &ctx.peer_store, arr).await;
}

/// Shared body of "adopt this peer as the active one", callable both from the
/// command handler and from the spawned switch scan.
///
/// The ownership flip is atomic (§D4): the displaced peer stops being active
/// the instant this runs, even though its transport link may take a while to
/// drop. Without that ordering two phones would briefly both own the session
/// and both mirror notifications and clipboard into this laptop.
async fn do_activate(
    app: &tauri::AppHandle,
    peer_store: &std::sync::Arc<dyn vortex_l3_daemon::core::storage::peers::PeerStore>,
    peer_pub: [u8; 32],
) {
    let successor_name = {
        let ps = peer_store.clone();
        tokio::task::spawn_blocking(move || ps.load(&peer_pub).ok().and_then(|p| p.peer_name))
            .await
            .unwrap_or(None)
    };
    // Refuse to activate a peer we do not actually trust — for the command
    // path the hex arrives from the webview, so it is untrusted input.
    let ps = peer_store.clone();
    let known = tokio::task::spawn_blocking(move || ps.load(&peer_pub).is_ok())
        .await
        .unwrap_or(false);
    if !known {
        tracing::warn!(peer = %hex::encode(&peer_pub[..4]), "activate: not a trusted peer");
        return;
    }

    let displaced = crate::arbiter::force_activate(&peer_pub);
    crate::arbiter::end_switch();
    if let Some(prev) = displaced {
        send_release(&prev, successor_name).await;
    }
    let _ = app.emit("vortex:switch_scanning", false);
    let _ = app.emit::<Vec<SwitchCandidateDto>>("vortex:switch_candidates", Vec::new());
    // Blank the pages that were showing the old phone's data, then re-emit
    // peers so the UI's `active` flags follow the new owner.
    purge_peer_cache(app);
    emit_peers(app, peer_store.clone()).await;
}


/// Tell `peer_pub` it is no longer the active peer.
///
/// Best-effort by nature: it can only be delivered while a link to that peer is
/// still up. That is the normal case here — a switch is confirmed while the
/// displaced peer is still the live BLE session (see `BLE_SEALED_WRITER`) — but
/// if the link already dropped, the peer simply learns on next contact, which is
/// the behaviour we had before this frame existed. So a failure is logged at
/// debug, not surfaced: nothing is broken by it.
async fn send_release(peer_pub: &[u8; 32], successor_name: Option<String>) {
    use vortex_l3_daemon::core::ble::frame::{sub, ty};
    let Some(holder) = crate::BLE_SEALED_WRITER.get() else {
        tracing::debug!("RELEASE not sent: no BLE session holder yet");
        return;
    };
    let writer = { holder.lock().await.clone() };
    let Some(writer) = writer else {
        tracing::debug!(
            peer = %hex::encode(&peer_pub[..4]),
            "RELEASE not sent: no live BLE session"
        );
        return;
    };
    // Payload is the successor's display name, purely so the phone can say
    // "moved to <name>". Empty when unknown — the sub code is what carries
    // meaning, so an absent name must not change behaviour.
    let payload = successor_name.unwrap_or_default().into_bytes();
    // The kind rides as the first payload byte rather than in Frame.sub. The
    // sealed writer does now expose `sub` (the filesystem ops needed it), but
    // this is a shipped wire format and both ends already read it back this
    // way — changing it would only break compatibility for tidiness.
    let mut body = Vec::with_capacity(payload.len() + 1);
    body.push(sub::HANDOFF_RELEASE);
    body.extend_from_slice(&payload);
    match writer(ty::PEER_HANDOFF, 0, body).await {
        Ok(()) => tracing::info!(
            peer = %hex::encode(&peer_pub[..4]),
            "sent PeerHandoff.RELEASE to the displaced peer"
        ),
        Err(e) => tracing::debug!("RELEASE send failed: {e}"),
    }
}

/// `UiCmd::ForgetPeer` — forget locally now (instant UI), then best-effort
/// background revoke retries for up to 60 s so trust drops bidirectionally.
pub(crate) async fn forget_peer(ctx: &WorkerCtx, hex_str: String) {
    let Ok(bytes) = hex::decode(&hex_str) else { return };
    if bytes.len() != 32 {
        return;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    // Capture trust record + local counter BEFORE forgetting — the background
    // revoke task needs both, and they're gone after forget. Each peer_store
    // call wraps a blocking SecretService D-Bus round-trip, so route them
    // through spawn_blocking to avoid wedging the runtime when forget races a
    // live heartbeat.
    let ps = ctx.peer_store.clone();
    let arr_load = arr;
    let peer_for_revoke = tokio::task::spawn_blocking(move || ps.load(&arr_load).ok())
        .await
        .unwrap_or(None);
    let ps = ctx.peer_store.clone();
    let arr_load = arr;
    let counter_for_revoke =
        tokio::task::spawn_blocking(move || ps.load_counter(&arr_load).unwrap_or(0))
            .await
            .unwrap_or(0);
    // Forget locally immediately — UI should feel instant.
    let ps = ctx.peer_store.clone();
    let arr_forget = arr;
    let forget_result = tokio::task::spawn_blocking(move || ps.forget(&arr_forget)).await;
    match forget_result {
        Ok(Ok(())) => tracing::info!("peer_store.forget OK for {}", hex::encode(&arr[..8])),
        Ok(Err(e)) => {
            tracing::warn!("peer_store.forget FAILED for {}: {}", hex::encode(&arr[..8]), e)
        }
        Err(e) => tracing::warn!("peer_store.forget JOIN ERROR: {}", e),
    }
    // Drop the peer's BlueZ device object too. Vortex creates no BT bond on
    // Linux (see the 2026-06-02 note in `pairing.rs`), so this is normally not
    // a *bond* removal — it evicts the cached device entry whose stale RPA
    // otherwise gets re-served from the adapter's advertisement cache and
    // burns connect timeouts on the next pairing. If a bond *does* exist
    // (added by hand in the desktop's Bluetooth panel, or by an older build),
    // this drops it, which is what keeps the two sides from ending up in the
    // one-sided-bond state that fails with `timeout: service discovery`.
    // BlueZ-specific, so Linux-only: the stale device object and any bond are
    // BlueZ concepts. Everything else in this handler — the trust delete, the
    // cache purge, the revoke — is what makes forgetting work off Linux too.
    #[cfg(target_os = "linux")]
    if let Some(addr) = crate::ble::take_peer_addr(&arr) {
        crate::ble::forget_stale_device(&ctx.adapter, addr).await;
    }
    // Drop all of the forgotten phone's cached data + blank its UI pages.
    // Order matters: clear the in-page state (which reads the still-active
    // paths) BEFORE dropping the peer's directory and unsetting it.
    purge_peer_cache(&ctx.app);
    crate::peer_cache::remove_peer_dir(&arr);
    crate::arbiter::release(&arr);
    crate::arbiter::note_disconnected(&arr);
    emit_peers(&ctx.app, ctx.peer_store.clone()).await;
    // Background revoke retries (best-effort). Peer may be offline now; keep
    // trying for up to 60 s so a peer that comes back inside that window still
    // picks up the revoke and forgets us bidirectionally.
    if let Some(peer) = peer_for_revoke {
        let identity_c = ctx.identity.clone();
        let arr_c = arr;
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            let mut attempt: u32 = 0;
            while std::time::Instant::now() < deadline {
                attempt += 1;
                // Monotonically advance the IK counter on each attempt — replay
                // protection on the peer rejects equal/lower values.
                let counter = counter_for_revoke.saturating_add(attempt as u64);
                match send_revoke_to_peer(&identity_c, &peer, &arr_c, counter).await {
                    Ok(()) => {
                        tracing::info!(attempt, "revoke delivered to {}", hex::encode(&arr_c[..8]));
                        return;
                    }
                    Err(e) => {
                        tracing::debug!(attempt, "revoke attempt failed: {e}; will retry");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            tracing::warn!("revoke retries exhausted for {} after 60s", hex::encode(&arr_c[..8]));
        });
    }
}

/// `UiCmd::ForgetAll` — drop every trusted peer (local only).
pub(crate) async fn forget_all(ctx: &WorkerCtx) {
    // Collect the pubkeys before forgetting so the BlueZ cleanup below still
    // knows which peers existed (the store is empty by then).
    let ps = ctx.peer_store.clone();
    let pubs = tokio::task::spawn_blocking(move || {
        ps.list()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.peer_static_pub)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let ps = ctx.peer_store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(list) = ps.list() {
            for p in list {
                if let Err(e) = ps.forget(&p.peer_static_pub) {
                    tracing::warn!(
                        "ForgetAll: forget failed for {}: {}",
                        hex::encode(&p.peer_static_pub[..8]),
                        e
                    );
                }
            }
        }
    })
    .await;
    // Same BlueZ + per-peer cache cleanup as `forget_peer`, for every peer.
    for peer_pub in &pubs {
        #[cfg(target_os = "linux")]
        if let Some(addr) = crate::ble::take_peer_addr(peer_pub) {
            crate::ble::forget_stale_device(&ctx.adapter, addr).await;
        }
        crate::peer_cache::remove_peer_dir(peer_pub);
        crate::arbiter::release(peer_pub);
        crate::arbiter::note_disconnected(peer_pub);
    }
    purge_peer_cache(&ctx.app);
    emit_peers(&ctx.app, ctx.peer_store.clone()).await;
}
