//! Earbuds + state command handlers (RefreshState / RefreshLocalEarbuds /
//! RequestEarbudsSwitch / SendEarbudsClaim / ToggleEarbuds), split out of
//! `run_worker`. Each takes `&WorkerCtx`.

use std::time::Duration;

use tauri::Emitter;

use vortex_l3_daemon::core::lan::discovery::discover_first;

use crate::ipc::{emit_peers, switch_state_dto, EarbudsDto, IdentityInfo};
use crate::worker_ctx::WorkerCtx;

/// `UiCmd::RefreshState` — re-emit identity + peers + local-earbuds + switch
/// state (Vue calls this on mount and on webview reload so a refresh/HMR can't
/// strand the UI with empty state).
pub(crate) async fn refresh_state(ctx: &WorkerCtx) {
    let app = &ctx.app;
    let _ = app.emit("vortex:identity", IdentityInfo { ready: true });
    emit_peers(app, ctx.peer_store.clone()).await;
    let local = vortex_l3_daemon::core::earbuds::scan_local_earbuds(&ctx.adapter).await;
    crate::tray::update_battery_rows(local.as_ref(), None);
    let _ = app.emit("vortex:local_earbuds", local.map(EarbudsDto::from));
    // Also re-emit Idle for the switch state so the card doesn't sit on a stale
    // "Switching…" after reload.
    let _ = app.emit(
        "vortex:switch_state",
        switch_state_dto(&ctx.switch_orchestrator.state().borrow()),
    );
}

/// Re-scan local earbuds and publish everywhere (tray rows + the UI event).
/// Shared by the worker's refresh handler and the post-toggle nudge below.
pub(crate) async fn rescan_and_publish(app: &tauri::AppHandle, adapter: &bluer::Adapter) {
    let local = vortex_l3_daemon::core::earbuds::scan_local_earbuds(adapter).await;
    crate::tray::update_battery_rows(local.as_ref(), None);
    let _ = app.emit("vortex:local_earbuds", local.map(EarbudsDto::from));
}

/// `UiCmd::RefreshLocalEarbuds` — re-scan + emit the saved-buds card promptly.
/// Driven by the 5-second backend heartbeat in lib.rs (NOT the webview — WebKit
/// throttles hidden-window timers, which froze the tray when the window was
/// closed) plus ad-hoc UI nudges.
pub(crate) async fn refresh_local_earbuds(ctx: &WorkerCtx) {
    rescan_and_publish(&ctx.app, &ctx.adapter).await;
}

/// Decode a 32-byte hex peer key and look it up in the trust store.
async fn resolve_peer(
    ctx: &WorkerCtx,
    peer_static_pub: &str,
    what: &str,
) -> Option<vortex_l3_daemon::core::storage::peers::TrustedPeer> {
    let peer_bytes = match hex::decode(peer_static_pub) {
        Ok(v) if v.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&v);
            arr
        }
        _ => {
            tracing::warn!("{what}: bad peer_static_pub hex");
            return None;
        }
    };
    match ctx.peer_store.list() {
        Ok(mut list) => {
            list.retain(|p| p.peer_static_pub == peer_bytes);
            let found = list.into_iter().next();
            if found.is_none() {
                tracing::warn!("{what}: peer not in trust store");
            }
            found
        }
        Err(e) => {
            tracing::warn!("peer_store.list for {what}: {e}");
            None
        }
    }
}

/// `UiCmd::RequestEarbudsSwitch` — run the LAN AudioOp initiator to grab the
/// buds from the peer (discover on LAN, then IK + switch flow).
pub(crate) async fn request_switch(ctx: &WorkerCtx, peer_static_pub: String, mac: String) {
    let Some(peer) = resolve_peer(ctx, &peer_static_pub, "switch").await else {
        return;
    };
    let identity = ctx.identity.clone();
    let peer_store_c = ctx.peer_store.clone();
    let orch = ctx.switch_orchestrator.clone();
    let writers = ctx.session_writers.clone();
    let mac_c = mac.clone();
    tokio::spawn(async move {
        // Find the peer on LAN. 6 s mDNS budget mirrors the heartbeat path.
        let cand = match discover_first(Duration::from_secs(6)).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                tracing::warn!("switch: no LAN candidate (peer offline?)");
                return;
            }
            Err(e) => {
                tracing::warn!("switch: mdns error: {e}");
                return;
            }
        };
        let Some(ip) = cand
            .addresses
            .iter()
            .find(|a| matches!(a, std::net::IpAddr::V4(_)))
            .copied()
            .or_else(|| cand.addresses.first().copied())
        else {
            tracing::warn!("switch: candidate had no IP");
            return;
        };
        let addr = std::net::SocketAddr::new(ip, cand.port);
        let local_counter = peer_store_c.load_counter(&peer.peer_static_pub).unwrap_or(0);
        if let Err(e) = vortex_l3_daemon::core::audio_lan_session::start_initiator_session(
            addr,
            &identity.static_priv.0,
            &peer.peer_static_pub,
            &peer.prs,
            local_counter,
            mac_c,
            orch,
            writers,
        )
        .await
        {
            tracing::warn!("switch session failed: {e}");
        }
    });
}

/// `UiCmd::SendEarbudsClaim` — one-shot AudioOp Claim to the peer over LAN.
pub(crate) async fn send_claim(ctx: &WorkerCtx, peer_static_pub: String, mac: String) {
    let Some(peer) = resolve_peer(ctx, &peer_static_pub, "claim").await else {
        return;
    };
    let identity = ctx.identity.clone();
    let peer_store_c = ctx.peer_store.clone();
    let mac_c = mac.clone();
    tokio::spawn(async move {
        let cand = match discover_first(Duration::from_secs(6)).await {
            Ok(Some(c)) => c,
            _ => {
                tracing::warn!("claim: no LAN candidate");
                return;
            }
        };
        let Some(ip) = cand
            .addresses
            .iter()
            .find(|a| matches!(a, std::net::IpAddr::V4(_)))
            .copied()
            .or_else(|| cand.addresses.first().copied())
        else {
            return;
        };
        let addr = std::net::SocketAddr::new(ip, cand.port);
        let local_counter = peer_store_c.load_counter(&peer.peer_static_pub).unwrap_or(0);
        let nonce = peer_store_c.next_audio_out_nonce(&peer.peer_static_pub).unwrap_or(0);
        if let Err(e) = vortex_l3_daemon::core::audio_lan_session::send_one_shot(
            addr,
            &identity.static_priv.0,
            &peer.peer_static_pub,
            &peer.prs,
            local_counter,
            vortex_l3_daemon::core::audio_op::AudioOpFrame {
                nonce,
                op: vortex_l3_daemon::core::audio_op::AudioOp::Claim,
                mac: mac_c,
                ts: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            },
        )
        .await
        {
            tracing::warn!("send claim failed: {e}");
        }
    });
}

/// `UiCmd::ToggleEarbuds` — tray "Switch" toggle: hand the buds to the phone if
/// the laptop holds them, else grab them to the laptop.
pub(crate) async fn toggle_earbuds(ctx: &WorkerCtx) {
    let Some(saved) = vortex_l3_daemon::core::earbuds_store::load() else {
        tracing::warn!("tray toggle: no saved earbuds");
        return;
    };
    let mac = saved.address;
    let Ok(addr) = mac.parse::<bluer::Address>() else {
        return;
    };
    let peer_pub = match ctx.peer_store.list() {
        Ok(l) => l.into_iter().next().map(|p| p.peer_static_pub),
        Err(_) => None,
    };
    let Some(peer_pub) = peer_pub else {
        tracing::warn!("tray toggle: no trusted peer");
        return;
    };
    let adapter_c = ctx.adapter.clone();
    let orch = ctx.switch_orchestrator.clone();
    let app_c = ctx.app.clone();
    tokio::spawn(async move {
        let own = vortex_l3_daemon::core::audio_switch::audio_active(&adapter_c, addr).await;
        if own {
            tracing::info!("tray toggle: laptop holds buds → hand to phone");
            let _ = vortex_l3_daemon::core::audio_switch::disconnect_audio_initiate(&adapter_c, &mac)
                .await;
            orch.send_claim(peer_pub, mac).await;
        } else {
            tracing::info!("tray toggle: grabbing buds to laptop");
            let _ = orch.request(peer_pub, mac).await;
        }
        // Nudge the tray right after the hand-off settles (the switch itself
        // takes ~4.5 s end-to-end) instead of waiting out the 5 s heartbeat —
        // two probes so a slow BlueZ profile connect still gets caught.
        for wait_ms in [1500u64, 4000] {
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            rescan_and_publish(&app_c, &adapter_c).await;
        }
    });
}
