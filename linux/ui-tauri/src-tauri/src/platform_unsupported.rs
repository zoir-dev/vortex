//! Tauri commands for the subsystems a non-Linux build does not have.
//!
//! The frontend is one shared bundle: the same HTML and the same `invoke()`
//! calls ship on every platform. So the earbuds, camera and screen-cast
//! commands have to EXIST on Windows even though the features behind them do
//! not — otherwise Tauri's command list and the UI disagree, and every affected
//! button fails with "command not found" instead of something a user can read.
//!
//! Pairing is NOT here: it works on every platform now — the XX handshake and
//! the trust save are shared, and only the way the link is obtained differs.
//!
//! Aliased over the real modules in `lib.rs`, which is what keeps the
//! `generate_handler!` list single-sourced. Duplicating that list per platform
//! was the alternative, and 62 entries copied twice is a list that drifts.
//!
//! Signatures match the Linux ones EXCEPT where the real return type is itself
//! Linux-only (`scan_bluetooth_devices`, `get_saved_earbuds` name daemon types
//! behind a BlueZ gate). The contract only has to hold per platform: these
//! return an error or an empty value, and the shape the frontend sees for a
//! failed call is the same either way.

use crate::ipc::CmdChannel;
use tauri::State;

/// One message, so the UI can say the same thing everywhere and a log line
/// names the reason rather than just failing.
pub(crate) const UNSUPPORTED: &str = "not available on this platform yet";

// ── Earbuds hand-off (PulseAudio + BlueZ on Linux) ────────────────────────

#[tauri::command]
pub fn refresh_local_earbuds(_state: State<'_, CmdChannel>) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub fn open_bluetooth_settings() -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

/// Empty rather than an error: the UI renders this as a device list, and an
/// empty list is a truthful "none found" it already knows how to draw.
#[tauri::command]
pub async fn scan_bluetooth_devices() -> Result<Vec<serde_json::Value>, String> {
    Ok(Vec::new())
}

#[tauri::command]
pub async fn save_earbuds(_address: String, _name: String) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub fn clear_earbuds() -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub fn get_saved_earbuds() -> Option<serde_json::Value> {
    None
}

#[tauri::command]
pub fn request_earbuds_switch(
    _peer_static_pub: String,
    _mac: String,
    _state: State<'_, CmdChannel>,
) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub fn send_earbuds_claim(
    _peer_static_pub: String,
    _mac: String,
    _state: State<'_, CmdChannel>,
) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

/// The smart-switch toggle is a SETTING, and settings are shared state with the
/// phone (last-write-wins over the heartbeat). Accepting the write and
/// remembering it locally is better than refusing: the user's choice survives,
/// and it takes effect if this machine ever grows an audio backend.
#[tauri::command]
pub fn set_smart_switch_enabled(enabled: bool) {
    SMART_SWITCH.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
pub fn get_smart_switch_enabled() -> bool {
    SMART_SWITCH.load(std::sync::atomic::Ordering::Relaxed)
}

static SMART_SWITCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

// ── Continuity camera (GStreamer) ─────────────────────────────────────────

#[tauri::command]
/// Answers with the same error the other stubs do, now that the Linux side is
/// fallible: a caller that logs the reason gets one instead of silent success.
pub(crate) fn set_camera_request(_on: bool) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub(crate) fn set_camera_facing(_facing: String) {}

/// Read by the LAN heartbeat when it fills the outgoing AppState. Always false
/// here, so the phone is never asked for a camera this machine cannot decode.
pub(crate) fn camera_wanted() -> bool {
    false
}

pub(crate) fn camera_facing() -> String {
    String::new()
}

// ── Laptop→phone screen cast (ScreenCast portal) ──────────────────────────

#[tauri::command]
pub(crate) fn set_extend_mode(_on: bool) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

#[tauri::command]
pub(crate) fn get_extend_mode() -> bool {
    false
}

/// Same role as `camera_wanted`: the heartbeat asks, and the answer is "we are
/// not casting", so no offer is advertised to the phone.
pub(crate) fn current_offer() -> Option<vortex_l3_daemon::core::appstate::LaptopCast> {
    None
}

pub(crate) fn current_error() -> Option<String> {
    None
}

// ── Proximity auto-lock / auto-unlock ─────────────────────────────────────

/// The settings still round-trip: they are the user's stated intent, stored
/// locally, and the watcher that acts on them is what is missing. Reporting
/// them as always-off would silently discard a choice.
#[tauri::command]
pub fn get_proximity_settings() -> serde_json::Value {
    serde_json::json!({ "auto_lock": false, "auto_unlock": false })
}

#[tauri::command]
pub fn set_proximity_settings(_auto_lock: bool, _auto_unlock: bool) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

// ── No-op dispatchers ─────────────────────────────────────────────────────
//
// The heartbeat hands every incoming peer AppState to these. Doing nothing is
// the correct behaviour, not a stub: the phone is telling us about a cast it
// would like, a camera it is offering, or earbuds it has — and this machine has
// nowhere to put any of them. Making them no-ops here keeps the heartbeat's flow
// identical on both platforms instead of threading `cfg` through the middle of
// it.

/// The phone offers its camera as a webcam. Nothing decodes it here.
pub(crate) fn dispatch_offer(
    _offer: &Option<vortex_l3_daemon::core::appstate::CameraOffer>,
    _phone_ip: Option<std::net::IpAddr>,
) {
}

/// The phone asks us to cast our screen to it. Nothing captures it here.
pub(crate) fn dispatch_request(_req: bool, _extend: Option<bool>) {}

/// Auto-pin the peer's earbuds locally so the card shows on this device too.
/// There is no local audio device to pin them to.
pub(crate) fn persist_peer_earbuds(_state: &vortex_l3_daemon::core::appstate::AppState) {}

/// The phone tells us whether it is unlocked, for proximity auto-unlock. There
/// is nothing to unlock here — Windows has no programmatic unlock at all, which
/// `SessionControl::can_unlock` already reports — so the value is dropped.
pub(crate) fn note_phone_unlocked(_unlocked: Option<bool>) {}
