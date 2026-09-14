//! Pairing: the trust-establishment handshake (do_pair), trust revocation
//! (send_revoke_to_peer), and the SAS redaction helper. Split out of lib.rs.

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;
use tracing::{info, warn};

#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::ble::client::VortexClient;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::pairing::handshake::{run_pairing_initiator, LocalDecision};
#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::platform::linux::LinuxGattLink;
use vortex_l3_daemon::core::storage::peers::{PeerStore, TrustedPeer};

use crate::{emit_peers, CmdChannel, PairDecisionState, UiCmd};

/// Redact a SAS string for logging: show first 3 + last 3 chars.
/// Full SAS is fine to display in the user-controlled UI but should
/// never persist in disk logs that may be shared with support.
pub(crate) fn redact_sas(sas: &str) -> String {
    let chars: Vec<char> = sas.chars().collect();
    if chars.len() <= 6 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}

/// This machine's friendly name, sent in the APPROVE frame so the phone shows
/// "MyLaptop" rather than a platform label. `/proc` on Linux, `COMPUTERNAME`
/// elsewhere; sanitised on the way out by the handshake either way.
pub(crate) fn local_device_name() -> Option<String> {
    // One source, shared with the AppState heartbeat: the pairing frame and the
    // heartbeat both carry this name, and if they disagree the heartbeat wins by
    // arriving second.
    vortex_l3_daemon::core::platform::host_name()
}

/// Pair over BlueZ: connect to a scanned BD_ADDR, then hand the link to
/// [`do_pair_over`].
#[cfg(target_os = "linux")]
pub(crate) async fn do_pair(
    app: &AppHandle,
    adapter: &bluer::Adapter,
    addr_str: &str,
    identity: &IdentityRecord,
    peer_store: Arc<dyn PeerStore>,
) -> Result<(), String> {
    let addr: bluer::Address = addr_str.parse().map_err(|e| format!("bad BD_ADDR: {e}"))?;

    // Pair-modal latency instrumentation: split the user-perceived
    // "click Pair on Linux → modal appears on Android" delay into its
    // stages (BlueZ hygiene, GATT connect+discovery, Noise XX → SAS).
    let t_pair = std::time::Instant::now();

    // BlueZ hygiene before re-connect (see daemon/src/main.rs note).
    if let Ok(device) = adapter.device(addr) {
        if device.is_connected().await.unwrap_or(false) {
            let _ = device.disconnect().await;
        }
    }
    let hygiene_ms = t_pair.elapsed().as_millis();

    let client = VortexClient::connect(adapter, addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    info!(hygiene_ms, connect_total_ms = t_pair.elapsed().as_millis(),
        "pair: connected to {addr_str}; running Noise XX");

    let local_name = crate::pairing::local_device_name();
    let link = LinuxGattLink::from_client(adapter.clone(), &client);
    do_pair_over(app, &link, identity, peer_store, local_name.as_deref()).await
}

/// The pairing handshake and everything after it: run Noise XX over `link`, show
/// the SAS, wait for the user on BOTH devices, and store the trust record.
///
/// Platform-neutral — this is the part that decides whether two devices trust
/// each other forever, and it should not exist twice. The transports differ
/// only in how the link was obtained.
pub(crate) async fn do_pair_over(
    app: &AppHandle,
    link: &dyn vortex_l3_daemon::core::platform::GattLink,
    identity: &IdentityRecord,
    peer_store: Arc<dyn PeerStore>,
    local_name: Option<&str>,
) -> Result<(), String> {
    let t_pair = std::time::Instant::now();
    let app_for_sas = app.clone();
    let outcome = run_pairing_initiator(
        link,
        &identity.static_priv.0,
        Duration::from_secs(60),
        move |sas: &str| {
            // Real dual-approval: emit the SAS, install a oneshot
            // sender into Tauri state, then await the user's click.
            // The UI calls `pair_decision(approve/reject)` to fire
            // the channel. A 60s timeout matches the V1 pairing window
            // — if the user walks away mid-SAS we count it as Reject
            // rather than hanging the handshake.
            let sas_string = sas.to_string();
            let app = app_for_sas.clone();
            async move {
                info!(sas.redacted = %redact_sas(&sas_string), "Linux awaiting user approval");
                let (tx, rx) = oneshot::channel::<LocalDecision>();
                // Install sender BEFORE emitting the SAS event so a
                // very-fast UI cannot race and fire pair_decision
                // before the slot is populated.
                if let Some(decision_state) = app.try_state::<PairDecisionState>() {
                    if let Ok(mut slot) = decision_state.0.lock() {
                        // Replace any stale sender from a previous
                        // pair attempt that bailed without clearing.
                        *slot = Some(tx);
                    } else {
                        warn!("PairDecisionState lock poisoned; defaulting to Reject");
                        return LocalDecision::Reject;
                    }
                } else {
                    warn!("PairDecisionState not managed; defaulting to Reject");
                    return LocalDecision::Reject;
                }
                info!(
                    sas_ready_ms = t_pair.elapsed().as_millis(),
                    "pair: Noise XX complete, SAS ready (≈ Android modal appears now)"
                );
                let _ = app.emit("vortex:pairing_sas", sas_string);
                match tokio::time::timeout(Duration::from_secs(60), rx).await {
                    Ok(Ok(decision)) => decision,
                    Ok(Err(_)) => {
                        warn!("pairing decision sender dropped; treating as Reject");
                        LocalDecision::Reject
                    }
                    Err(_) => {
                        warn!("pairing SAS approval timed out after 60s; treating as Reject");
                        // Clear the now-stale slot so a late click
                        // doesn't try to fire a closed channel.
                        if let Some(decision_state) = app.try_state::<PairDecisionState>() {
                            if let Ok(mut slot) = decision_state.0.lock() {
                                *slot = None;
                            }
                        }
                        LocalDecision::Reject
                    }
                }
            }
        },
        local_name,
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;

    let paired_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let trusted = TrustedPeer {
        peer_static_pub: outcome.xx.peer_static_pub,
        prs: outcome.prs,
        paired_at,
        peer_name: outcome.peer_name.clone(),
    };
    peer_store
        .save(&trusted)
        .map_err(|e| format!("save trust: {e}"))?;

    // Push the freshly-trusted peer to the UI immediately. Without this
    // the Vue `peers` list stays empty until some other event happens to
    // call emit_peers — the device just doesn't show up after pairing.
    emit_peers(app, peer_store.clone()).await;

    // NOTE: no BT bond here. We investigated LE bonding to resolve the
    // phone's rotating RPA (2026-06-02) and hit a hard platform wall: on this
    // dual-mode phone BlueZ routes every bond — `Device::pair()`, an
    // encryption-required GATT read, AND Android `createBond(TRANSPORT_LE)` —
    // over BR/EDR (shared public address → Android treats us as dual), which
    // fails the Just-Works negotiation and yields no IRK anyway. A prior
    // session also found BlueZ doesn't reliably resolve the RPA even when a
    // bond was somehow established. So the persistent loop reconnects via
    // presence-scan + Noise IK instead (see run_ble_persistent_loop); the
    // optimisation effort goes into making that reconnect fast and stable.
    Ok(())
}

/// Send a one-shot LAN reconnect whose AppState carries
/// `revoked=true`, so the peer drops their trust record for us.
/// Best-effort: any error here just means the caller retries (or gives
/// up after its deadline). Takes the trust record and counter directly
/// so the caller can capture them before local forget — once forget
/// runs, the secret-service entries are gone and we'd otherwise have
/// nothing to authenticate with.
pub(crate) async fn send_revoke_to_peer(
    identity: &IdentityRecord,
    peer: &TrustedPeer,
    peer_pub: &[u8; 32],
    local_counter: u64,
) -> Result<(), String> {
    let candidate = vortex_l3_daemon::core::lan::discovery::discover_first(
        Duration::from_secs(3),
    )
    .await
    .map_err(|e| format!("mdns: {e}"))?
    .ok_or_else(|| "no LAN candidate".to_string())?;
    let ip = candidate
        .addresses
        .iter()
        .find(|a| matches!(a, std::net::IpAddr::V4(_)))
        .copied()
        .or_else(|| candidate.addresses.first().copied())
        .ok_or_else(|| "no IP".to_string())?;
    let socket_addr = std::net::SocketAddr::new(ip, candidate.port);
    let mut state = vortex_l3_daemon::core::appstate::AppState::now_laptop();
    state.revoked = true;
    let _ = vortex_l3_daemon::core::lan::tcp_client::run_lan_reconnect(
        socket_addr,
        &identity.static_priv.0,
        &peer.peer_static_pub,
        &peer.prs,
        local_counter,
        state,
        Duration::from_secs(4),
        None, // revoke notification only — no bulk-sync
    )
    .await
    .map_err(|e| format!("revoke send: {e}"))?;
    tracing::info!("revoke notification delivered to {}", hex::encode(&peer_pub[..8]));
    Ok(())
}


#[tauri::command]
pub fn start_pair(addr: String, state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::Pair(addr)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pair_decision(approve: bool, state: State<'_, PairDecisionState>) -> Result<(), String> {
    // Take() rather than peek — the channel is single-use, and after
    // delivering the decision we must clear the slot so a stale second
    // click doesn't keep a dead sender around.
    let maybe = state.0.lock().map_err(|e| e.to_string())?.take();
    let Some(sender) = maybe else {
        // No pairing in flight, or decision already delivered. UI may
        // have double-clicked; ignoring is the right behaviour — we
        // do NOT want the second click to flip a successful Approve.
        return Ok(());
    };
    let decision = if approve { LocalDecision::Approve } else { LocalDecision::Reject };
    // Receiver was dropped (do_pair bailed via timeout or error path).
    // Same disposition as no-slot: best-effort, ignore.
    let _ = sender.send(decision);
    Ok(())
}

#[tauri::command]
pub fn forget_peer(peer_static_pub: String, state: State<'_, CmdChannel>) -> Result<(), String> {
    state
        .0
        .send(UiCmd::ForgetPeer(peer_static_pub))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn forget_all(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::ForgetAll).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn switch_peer(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::SwitchPeer).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn cancel_switch(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::CancelSwitch).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn activate_peer(peer_static_pub: String, state: State<'_, CmdChannel>) -> Result<(), String> {
    state
        .0
        .send(UiCmd::ActivatePeer(peer_static_pub))
        .map_err(|e| e.to_string())
}
