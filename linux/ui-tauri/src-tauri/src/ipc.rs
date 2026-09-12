//! UI ⇄ worker protocol: the `UiCmd` channel commands plus the DTO layer
//! the backend emits to the Vue webview as Tauri events. Split out of lib.rs.

use std::sync::mpsc::Sender;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use vortex_l3_daemon::core::appstate::AppState;
use vortex_l3_daemon::core::storage::peers::PeerStore;

// --------------------------------------------------------------------------
// Channel protocol — identical in spirit to bin/ui.rs.
// --------------------------------------------------------------------------

pub(crate) enum UiCmd {
    Scan,
    Pair(String),
    ForgetPeer(String),
    ForgetAll,
    RefreshState,
    /// One-shot refresh of locally-connected earbuds — fires after the
    /// system Bluetooth state changes so the UI updates without
    /// waiting for the next LAN reconnect.
    RefreshLocalEarbuds,
    /// Earbuds-switch tap from the home card. Carries the target peer
    /// (hex) and the buds' Bluetooth address. Worker forwards to the
    /// orchestrator and the result surfaces via `vortex:switch_state`.
    RequestEarbudsSwitch { peer_static_pub: String, mac: String },
    /// Tell the peer to claim the buds (we currently hold them). Sent
    /// when the user taps swap on the side that owns the buds.
    SendEarbudsClaim { peer_static_pub: String, mac: String },
    /// System-tray "Switch earbuds" action. The worker picks the
    /// direction from `audio_active`: if the laptop holds the buds it
    /// hands them to the phone (disconnect + claim); otherwise it grabs
    /// them here.
    ToggleEarbuds,
    /// Start mirroring the phone's screen to this laptop: the worker opens the
    /// dedicated Noise mirror session to the phone and feeds GStreamer.
    StartMirror { width: u32, height: u32, fps: u32, bitrate: u32 },
    /// Stop the active screen-mirror session.
    StopMirror,
    /// "Switch device" on the connected card: keep the current phone, and
    /// start looking for another *already-trusted* one.
    ///
    /// Deliberately not a release — the link is held until a replacement is
    /// confirmed, so the reconnect loop has nothing to race back into and the
    /// laptop can never end up connected to nothing (design doc §D3).
    SwitchPeer,
    /// Close the switch window without changing anything (user cancelled, or
    /// it expired).
    CancelSwitch,
    /// Adopt this trusted peer (hex `peer_static_pub`) as the active one —
    /// either the single candidate found, or the user's pick from several.
    ActivatePeer(String),
}

/// Identity surface visible to the Vue layer. We deliberately keep
/// this tiny — the webview only needs to know "do we have an
/// identity ready" (a single bool would do); both `device_id` and
/// `static_pub` are stable per-install identifiers that would
/// become fingerprints if DevTools or a future CSP misstep exposed
/// the webview to a remote origin. The UI does not display either.
#[derive(Serialize, Clone)]
pub(crate) struct IdentityInfo {
    pub(crate) ready: bool,
}

#[derive(Serialize, Clone)]
pub(crate) struct ScanHitDto {
    pub(crate) addr: String,
    pub(crate) rssi: i16,
    pub(crate) instance: String,
    pub(crate) name: Option<String>,
}

#[derive(Serialize, Clone)]
pub(crate) struct TrustedPeerDto {
    peer_static_pub: String,
    paired_at: u64,
    peer_name: Option<String>,
    /// True for the peer that currently owns the session. With several
    /// trusted phones the UI has to distinguish "remembered" from "the one
    /// whose SMS and notifications you are looking at" — see `arbiter`.
    active: bool,
}

/// Per-peer AppState snapshot pushed to the UI so it can render
/// battery / locale / theme / earbuds for the paired device. Pairing
/// security is unaffected — this rides on the post-handshake
/// AEAD-wrapped app-data channel.
#[derive(Serialize, Clone)]
pub(crate) struct PeerStateDto {
    peer_static_pub: String,
    battery: Option<u8>,
    class: String,
    name: Option<String>,
    locale: Option<String>,
    theme: Option<String>,
    earbuds: Option<EarbudsDto>,
    charging: bool,
    ts: u64,
}

#[derive(Serialize, Clone)]
pub(crate) struct EarbudsDto {
    name: String,
    battery: Option<u8>,
    connected: bool,
}

impl From<vortex_l3_daemon::core::appstate::EarbudsInfo> for EarbudsDto {
    fn from(e: vortex_l3_daemon::core::appstate::EarbudsInfo) -> Self {
        EarbudsDto {
            name: e.name,
            battery: e.battery,
            connected: e.connected,
        }
    }
}

pub(crate) fn app_state_to_dto(peer_pub_hex: String, s: AppState) -> PeerStateDto {
    let class = match s.class {
        vortex_l3_daemon::core::appstate::DeviceClass::Laptop => "laptop",
        vortex_l3_daemon::core::appstate::DeviceClass::Phone => "phone",
        vortex_l3_daemon::core::appstate::DeviceClass::Tablet => "tablet",
        vortex_l3_daemon::core::appstate::DeviceClass::Earbuds => "earbuds",
        vortex_l3_daemon::core::appstate::DeviceClass::Unknown => "unknown",
    }
    .to_string();
    let dto = PeerStateDto {
        peer_static_pub: peer_pub_hex,
        battery: s.battery,
        class,
        name: s.name,
        locale: s.locale,
        theme: s.theme,
        earbuds: s.earbuds.map(|e| EarbudsDto {
            name: e.name,
            battery: e.battery,
            connected: e.connected,
        }),
        charging: s.charging,
        // Stamp OUR receive time, not the phone's `s.ts`. The UI's "online"
        // check is `laptop_now - ts < 180s`; trusting the phone's clock made a
        // connected phone read "offline" whenever its clock lagged ours (or it
        // re-broadcast a cached AppState with a stale ts). This dto is built
        // only when a frame actually arrives, so receive-time is the true
        // "last seen" — skew-proof.
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    // Cache the freshest dto per peer so the UI can PULL it (get_peer_states)
    // as a self-heal: a Tauri event listener that silently stops delivering
    // `vortex:peer_state` used to leave the card frozen at "offline" until a
    // manual reload re-subscribed. The poll rides the invoke-response channel
    // (independent of the event channel), so it recovers regardless.
    if let Ok(mut cache) = peer_state_cache().lock() {
        cache.insert(dto.peer_static_pub.clone(), dto.clone());
    }
    dto
}

/// Latest dto seen per peer (hex pub → dto), written by [`app_state_to_dto`].
fn peer_state_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, PeerStateDto>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PeerStateDto>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Tauri command: pull the latest per-peer state over the invoke-response
/// channel. The UI polls this every ~15s as a backstop to the pushed
/// `vortex:peer_state` events — if those stop arriving, the poll keeps the
/// online/battery card fresh (and the freshness `ts` lets it fall to "offline"
/// on a real disconnect via the UI's own ticker).
#[tauri::command]
pub(crate) fn get_peer_states() -> Vec<PeerStateDto> {
    // Re-stamp `ts` to now while we're in LIVE CONTACT with the phone. The
    // cached dto's `ts` is the receive time of the last STATE *frame*, but the
    // phone only pushes a frame on a state *change* (battery, earbuds, …). When
    // it sits idle the laptop's own BLE state-write beat (~12s, ble.rs) — and
    // the LAN heartbeat — keep `peer_contact` fresh, proving the link is up,
    // yet no frame re-stamps the dto. The cached `ts` then ages past the UI's
    // 180s "online" window and the card wrongly read "Offline" while perfectly
    // connected. Tying the freshness to peer-contact makes the indicator track
    // the live LINK, not the last state change; a genuine disconnect stops the
    // beat, contact goes stale, and the card falls to Offline as before.
    let in_contact = crate::ble::peer_contact_age_ms() < CONTACT_FRESH_MS;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    peer_state_cache()
        .lock()
        .map(|m| {
            m.values()
                .cloned()
                .map(|mut dto| {
                    if in_contact {
                        dto.ts = now;
                    }
                    dto
                })
                .collect()
        })
        .unwrap_or_default()
}

/// How recently we must have heard from the phone over ANY transport for the
/// link to count as "up" in [`get_peer_states`]. The BLE state-write beat and
/// the LAN heartbeat both land every ~12s, so 35s tolerates ~2 missed beats —
/// the same threshold the mirror-pill disconnect-clear uses.
const CONTACT_FRESH_MS: u64 = 35_000;

#[derive(Serialize, Clone)]
pub(crate) struct PairingStartedDto {
    pub(crate) peer_addr: String,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
pub(crate) enum PairingResultDto {
    Ok { ok: bool, message: String },
    Err { ok: bool, error: String },
}

pub(crate) struct CmdChannel(pub(crate) Sender<UiCmd>);

/// Translate the orchestrator's internal state into the externally
/// tagged JSON shape the Vue side expects (`{ kind: "...", ... }`).
/// Keeping the wire format Vue-side-friendly here means the UI never
/// has to deal with serde-tagged variant decoding manually.
pub(crate) fn switch_state_dto(
    s: &vortex_l3_daemon::core::audio_orchestrator::SwitchState,
) -> serde_json::Value {
    use vortex_l3_daemon::core::audio_orchestrator::SwitchState as S;
    match s {
        S::Idle => serde_json::json!({ "kind": "idle" }),
        S::Preparing => serde_json::json!({ "kind": "preparing" }),
        S::WaitingApproval => serde_json::json!({ "kind": "waiting_approval" }),
        S::WaitingReleased => serde_json::json!({ "kind": "waiting_released" }),
        S::Connecting => serde_json::json!({ "kind": "connecting" }),
        S::AlmostDone => serde_json::json!({ "kind": "almost_done" }),
        S::Failed(reason) => serde_json::json!({ "kind": "failed", "reason": reason }),
    }
}

pub(crate) async fn emit_peers(app: &AppHandle, store: Arc<dyn PeerStore>) {
    // `store.list()` issues a blocking SecretService D-Bus call. From
    // any async context (UiCmd handler, worker loop, etc.) the inner
    // `block_in_place` can wedge the current runtime thread — which
    // silently swallows the emit so Vue keeps showing an empty peer
    // list after a webview reload. `spawn_blocking` moves the call
    // off the runtime and unblocks the channel.
    let list_result = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.list()
    })
    .await;
    let list_result = match list_result {
        Ok(r) => r,
        Err(join_err) => {
            tracing::warn!("emit_peers join error: {join_err}");
            return;
        }
    };
    match list_result {
        Ok(list) => {
            let dtos: Vec<TrustedPeerDto> = list
                .into_iter()
                .map(|p| TrustedPeerDto {
                    active: crate::arbiter::is_active(&p.peer_static_pub),
                    peer_static_pub: hex::encode(p.peer_static_pub),
                    paired_at: p.paired_at,
                    peer_name: p.peer_name,
                })
                .collect();
            let _ = app.emit("vortex:peers", dtos);
        }
        Err(err) => {
            // Surface the failure so the UI can show "Trust store
            // locked / unavailable" instead of an empty list — empty
            // would invite the user to re-pair and silently create
            // a duplicate trust entry once the store unlocks.
            tracing::warn!("peer store list failed: {err}");
            let _ = app.emit("vortex:peer_store_error", err.to_string());
        }
    }
}
