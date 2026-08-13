//! Pairing + trust command handlers (Scan / Pair / ForgetPeer / ForgetAll),
//! split out of `run_worker`. Each takes `&WorkerCtx`; Scan/Pair also borrow the
//! loop-local `active_scan` handle so a new scan supersedes the previous one and
//! Pair can quiet the radio before connecting.

use std::time::Duration;

use tauri::Emitter;

use vortex_l3_daemon::core::ble::scanner::run_filtered_scan;

use crate::ipc::{emit_peers, PairingResultDto, PairingStartedDto, ScanHitDto};
use crate::pairing::{do_pair, send_revoke_to_peer};
use crate::worker_ctx::WorkerCtx;

/// Wipe every cached scrap of the peer's data — contacts, recents, SMS, notes,
/// and the LAN fast-path IP — and blank the matching UI pages. Forgetting a
/// peer must leave nothing of the old phone behind (a new or re-paired phone
/// starts clean), so this runs on both ForgetPeer and ForgetAll. V1 is
/// single-peer, so "the peer's data" is simply all of it.
///
/// Clipboard history is deliberately NOT wiped: it's a laptop-local feature
/// (the Super+V popup), not the peer's data, so it outlives the link.
fn purge_peer_cache(app: &tauri::AppHandle) {
    crate::contacts::clear(app);
    crate::call_log::clear(app);
    crate::sms::clear(app);
    crate::notes::clear(app);
    crate::lan::clear_last_peer_ip();
}

/// `UiCmd::Scan` — pairable-only BLE scan, superseding any running scan.
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
    // Drop all of the forgotten phone's cached data + blank its UI pages.
    purge_peer_cache(&ctx.app);
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
    purge_peer_cache(&ctx.app);
    emit_peers(&ctx.app, ctx.peer_store.clone()).await;
}
