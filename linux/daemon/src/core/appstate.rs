//! Post-handshake app-state synchronization.
//!
//! Pairing (Noise XX) and trusted reconnect (Noise IK) cover only the
//! security primitives — identity authentication, replay defense,
//! liveness. Everything else the UI cares about — battery percentage,
//! locale, theme, connected earbuds — is **app-level state** and
//! evolves on its own cadence.
//!
//! This module owns that layer: a tiny JSON payload that each side
//! exchanges once over the Noise transport-mode cipher established at
//! the end of reconnect. Keeping it AEAD-wrapped means the same
//! authenticity guarantees as a hand-rolled signed frame, without
//! mixing application data into the handshake transcript.
//!
//! Wire encoding: each frame is `TRANSPORT_APP_DATA` (`0x31`), sub
//! `0x01`. Payload is the AEAD ciphertext of a UTF-8 JSON document.
//! Both sides:
//!   1. write their local snapshot,
//!   2. read the peer's snapshot,
//!   3. let the UI render whatever they want from it.
//!
//! Forward-compat: parsers MUST ignore unknown JSON fields. The `v`
//! field lets us reject obviously-wrong-shape payloads early.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use snow::TransportState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::warn;

use crate::core::ble::frame::{ty, Frame, FrameDecodeError, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD};

/// Current app-state schema version.
pub const APPSTATE_SCHEMA_V: u8 = 1;

/// Logical device class — what the UI puts on the icon and label.
/// Kept loose (string) so future classes (watch, speaker, etc.) don't
/// require a release on the other side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceClass {
    Unknown,
    Laptop,
    Phone,
    Tablet,
    Earbuds,
}

/// Information about a wireless audio device the peer can hand off.
/// `None` overall means the peer has no audio connected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EarbudsInfo {
    /// Friendly name as the OS reports it (already sanitized by caller).
    pub name: String,
    /// The buds' BD_ADDR (same physical device on both hosts) so the peer can
    /// persist them into its own store — lets the earbuds card auto-appear on a
    /// freshly-paired device instead of only the side that picked them. Empty
    /// for back-compat with peers that don't send it.
    #[serde(default)]
    pub address: String,
    /// Battery percentage 0..=100 if the buds expose it. A peer sending
    /// any value > 100 is silently dropped to `None` rather than
    /// rendered as nonsense in the UI.
    #[serde(default, deserialize_with = "deserialize_battery_pct")]
    pub battery: Option<u8>,
    /// True if the buds are currently routed to this side.
    pub connected: bool,
}

/// Custom deserializer that filters battery values to the 0..=100
/// semantic range. `u8` alone would accept up to 255; serde_json would
/// reject negative numbers. A misbehaving or hostile peer sending 150
/// is treated as if they sent `null` — we'd rather show "unknown"
/// than render a fake percentage.
fn deserialize_battery_pct<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw = Option::<u8>::deserialize(deserializer)?;
    Ok(raw.filter(|&b| b <= 100))
}

/// Snapshot the local side sends, and the peer's snapshot we receive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    /// Schema version. Receivers reject payloads whose `v` is greater
    /// than what they know how to parse (forward-compat: send the
    /// highest version you can speak).
    #[serde(default = "default_v")]
    pub v: u8,
    /// Battery percentage 0..=100, or None if the device has no battery
    /// (e.g. a desktop). Hostile / buggy values > 100 are filtered to
    /// `None` at deserialization time.
    #[serde(default, deserialize_with = "deserialize_battery_pct")]
    pub battery: Option<u8>,
    /// Device-class hint for the UI icon + label.
    pub class: DeviceClass,
    /// Display name the peer should show. Sanitized at the source.
    #[serde(default)]
    pub name: Option<String>,
    /// Locale code the peer is currently using (e.g. "uz", "en", "ru").
    /// Used by cross-device settings sync.
    #[serde(default)]
    pub locale: Option<String>,
    /// Unix-seconds timestamp of the last explicit locale change on the
    /// sender side. `0` means "I'm running the system default — I have
    /// no opinion to push." Last-writer-wins on receive: if the peer's
    /// `locale_changed_at` is strictly greater than ours, we adopt
    /// their locale and timestamp.
    #[serde(default)]
    pub locale_changed_at: u64,
    /// Theme hint — "dark" or "light".
    #[serde(default)]
    pub theme: Option<String>,
    /// Mirror of `locale_changed_at` for theme.
    #[serde(default)]
    pub theme_changed_at: u64,
    /// Earbuds info (if any).
    #[serde(default)]
    pub earbuds: Option<EarbudsInfo>,
    /// **Revocation signal.** When `true` the sender is telling the
    /// receiver "delete your trust record for me — I no longer trust
    /// you." Receiver acts immediately: drops the trusted peer entry,
    /// stops the trusted-presence advertising, refreshes the UI.
    /// Bidirectional forget: either side can issue.
    #[serde(default)]
    pub revoked: bool,
    /// "You claim the buds" signal. Set by the side currently holding
    /// the buds when the user taps swap; the receiver runs its own
    /// initiator flow on receipt. One-shot — sender clears it right
    /// after the AppState that carries it goes out so the peer
    /// doesn't re-trigger on every heartbeat.
    #[serde(default)]
    pub audio_claim_request: bool,
    /// Phase 2 — phone's current call state. Linux pauses MPRIS
    /// players on the transition `null` → `ringing`/`active` and
    /// resumes on `active` → `null`. Persistent across heartbeats
    /// (NOT one-shot) so a heartbeat dropped mid-call doesn't lose
    /// the pause record; Linux tracks the previous value and acts
    /// only on changes.
    #[serde(default)]
    pub call_phase: Option<String>,
    /// Call mirror (continuity banner + pill) — the full current call
    /// (caller, phase, start time) ADDITIVELY mirrored through AppState so it
    /// survives a BLE drop during a call (AppState rides BOTH LAN and the BLE
    /// STATE frame). The dedicated BLE CALL frame stays the fast path; the
    /// laptop dedups the two by (id, phase). `None` = no call. Best-effort:
    /// when neither transport carries it, the call mirror just isn't shown —
    /// never an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<crate::core::call_event::CallEvent>,
    /// Laptop→phone call-control command (Accept/Decline/End/Mute) carried
    /// ADDITIVELY over AppState as a one-shot, so the call banner/pill buttons
    /// still reach the phone when BLE is down mid-call (the dedicated BLE
    /// CALL_CONTROL frame stays the fast path). Sender sets it for the next
    /// outgoing AppState then clears it; the phone acts idempotently. `None`
    /// = nothing to do. Best-effort — absent transport is never an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_control: Option<crate::core::call_event::CallControl>,
    /// Laptop→phone notification action/reply INVOKE carried ADDITIVELY over
    /// AppState as the LAN backstop, so a clicked notification action still
    /// fires on the phone when the BLE NOTIFICATION write was dropped/wedged
    /// (BLE stays the fast path). Phone dedups by the mirror's `seq`. `None` =
    /// nothing to do. Best-effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notif_invoke: Option<crate::core::notif_mirror::NotificationMirror>,
    /// Phone→laptop browsing HANDOFF carried ADDITIVELY over AppState as the LAN
    /// backstop: the page the phone is on, so the laptop's "continue" pill still
    /// appears (and stays fresh) when the dedicated BLE HANDOFF frame is down.
    /// The laptop dedups by URL. Empty url / `None` = nothing to hand off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<crate::core::handoff::HandoffEvent>,
    /// Phase 3 — smart audio-follow. `true` while media is actively
    /// playing on THIS device. Advisory only: each side runs its own
    /// local auto-switch decision; this is carried so the peer can show
    /// an owner indicator and (future) tie-break simultaneous grabs.
    /// Older clients ignore the unknown field.
    #[serde(default)]
    pub media_playing: bool,
    /// Phase 3 — last-play-wins arbitration. MILLISECONDS THIS device has
    /// been in its current continuous playback session at send time (0 = not
    /// playing). A RELATIVE age, NOT an absolute timestamp, so it's immune to
    /// cross-device clock skew: the receiver re-anchors it to its OWN
    /// monotonic clock (`peer_start = my_mono_now - age`), which also corrects
    /// for transmission/heartbeat staleness. FROZEN across a hand-off
    /// pause/resume (the age keeps growing off the original play-start) so an
    /// auto-resume after we silenced media for a switch does NOT look newer.
    /// When both sides play, the SMALLER age (more recent play) wins the buds;
    /// the larger one yields — both the grab and the release paths gate on it,
    /// so the contention converges instead of ping-ponging. Older clients omit
    /// it (0) and fall back to the bool-only behaviour.
    #[serde(default)]
    pub media_play_age_ms: u64,
    /// Laptop→phone now-playing display: track title playing on the laptop
    /// (MPRIS `xesam:title`). Empty = nothing to show → the phone clears its
    /// laptop-media notification. Older clients ignore the unknown fields.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_title: String,
    /// Laptop→phone now-playing display: artist (MPRIS `xesam:artist[0]`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_artist: String,
    /// Laptop→phone now-playing display: player name (MPRIS `Identity`,
    /// e.g. "Spotify") — the notification's subtitle on the phone.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_app: String,
    /// Laptop→phone now-playing display: cover-art URL for the phone's media
    /// card (the art the system draws behind the title). Always http(s) — the
    /// PHONE fetches it, so a `file://` art path on the laptop is useless and
    /// gets dropped at the source. Empty = no art, the card falls back to the
    /// Vortex logo.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_art_url: String,
    /// Laptop→phone now-playing display: the RAW MPRIS playing state for
    /// the phone notification's ⏸/▶. Distinct from `media_playing`, which
    /// is the smart-switch's handoff-aware advisory (false while the buds
    /// are elsewhere even though a player is audible on the speakers).
    #[serde(default)]
    pub media_np_playing: bool,
    /// Phone→laptop media transport command: `"media_play_pause"` |
    /// `"media_next"` | `"media_prev"` (the phone's laptop-media
    /// notification buttons). Rides every snapshot until its TTL (the
    /// `lock_command` model — a lost BLE write can't eat the tap); the
    /// laptop dedups by `media_control_seq` and executes via MPRIS.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_control: String,
    /// Monotonic sequence for `media_control` duplicate suppression
    /// (shares the phone's persisted lock-command counter). `0` = none.
    #[serde(default)]
    pub media_control_seq: u64,
    /// Phase 3 — smart audio-follow on/off. Unlike locale/theme this is a
    /// SHARED system setting (not per-device): toggling it on either side
    /// applies to both, synced last-writer-wins via `smart_switch_changed_at`.
    /// Defaults to `true` so an older client that omits the field reads as
    /// "enabled" (the out-of-the-box behaviour).
    #[serde(default = "default_true")]
    pub smart_switch_enabled: bool,
    /// Unix-seconds timestamp of the last explicit smart-switch toggle.
    /// LWW: on receive, the peer's strictly-greater value is adopted.
    #[serde(default)]
    pub smart_switch_changed_at: u64,
    /// Whether this device is currently plugged in / charging. Carried so
    /// the peer can paint its battery indicator blue (with a charging
    /// glyph) instead of the usual green. Older clients ignore it.
    #[serde(default)]
    pub charging: bool,
    /// Laptop→phone: current lock-screen state (logind `LockedHint`), so
    /// the phone UI renders the right action (Lock vs Unlock). `None` =
    /// unknown / sender doesn't track it (phones don't send this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Phone→laptop: is the phone currently UNLOCKED (keyguard dismissed)?
    /// Gates proximity auto-unlock (owner-present gate: only unlock the laptop
    /// when the companion phone is itself unlocked / the owner is present).
    /// `Some(true)` = unlocked. `None` = sender doesn't report it (laptop, or an
    /// old phone) → treated as NOT unlocked, so auto-unlock fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlocked: Option<bool>,
    /// Phone→laptop one-shot remote-lock command: `"lock"` | `"unlock"`.
    /// Sender sets it for the next outgoing AppState then clears it
    /// (`audio_claim_request` pattern); the receiver executes via
    /// `session_lock` and dedups by `lock_command_seq` (BLE + LAN can
    /// deliver the same snapshot). Authentication is the transport
    /// itself: only the Noise-trusted peer can put a frame on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_command: Option<String>,
    /// Monotonic sequence for `lock_command` duplicate suppression.
    /// Persisted on the phone so an app restart can't reuse old values;
    /// `0` means "no command ever sent" and is never executed.
    #[serde(default)]
    pub lock_command_seq: u64,
    /// Phone→laptop: `true` while the user wants to VIEW the laptop's screen
    /// (laptop→phone mirror). A level, not an edge: the laptop starts casting on
    /// the false→true transition (pops its screen-share consent) and stops on
    /// true→false. Older clients omit it (false) = no request. See [`laptop_cast`].
    #[serde(default)]
    pub laptop_mirror_req: bool,
    /// Phone→laptop: which kind of screen the request is for — `Some(true)` a
    /// second monitor to drag windows onto, `Some(false)` a view of the screen
    /// the laptop already has.
    ///
    /// `None` = the phone did not say (an older client), and the laptop falls
    /// back to its own saved preference. That is why this is an Option and not a
    /// plain bool: a missing field must not read as "mirror, definitely".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub laptop_mirror_extend: Option<bool>,
    /// Laptop→phone: present while the laptop IS casting its screen — the
    /// connection params the phone's viewer dials. `None` = not casting. The
    /// media key rides here under the Noise-sealed transport; never log it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub laptop_cast: Option<LaptopCast>,
    /// Laptop→phone: why the last cast attempt failed, `None` when it didn't.
    ///
    /// Lets the phone stop asking and say something. Without it a request the
    /// laptop cannot satisfy is re-asserted on every heartbeat forever, with the
    /// reason only in the laptop's log — and since the phone's `requestView`
    /// ignores a tap while a request is already active, its UI wedges. Optional
    /// and skipped when absent, so older peers on both sides are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub laptop_cast_error: Option<String>,
    /// Laptop→phone: `true` while the user wants to use the phone's camera as a
    /// laptop webcam (phone-as-webcam). A level — the phone starts its camera
    /// on the false→true edge and stops on true→false. See [`camera_offer`].
    #[serde(default)]
    pub camera_req: bool,
    /// Laptop→phone: which lens — "front" (selfie) or "back". The phone re-opens
    /// the camera when this changes mid-stream. Empty = phone default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub camera_facing: String,
    /// Phone→laptop: present while the phone IS streaming its camera — the
    /// params the laptop dials to pull it into the v4l2 webcam. `None` = off.
    /// The media key rides here under the Noise-sealed transport; never log it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_offer: Option<CameraOffer>,
    /// Laptop→phone "ring my phone" (Find-My): the unix-millis the user last
    /// tapped Ring on the phone's device card. Carried in every heartbeat; the
    /// phone rings loudly (overriding silent/DND) on the RISING edge and dedups
    /// repeats by comparing against its last-seen value. Millis (not a 1-based
    /// counter) so the value only ever grows — a laptop restart can't regress it
    /// and make a fresh tap look like a replay. `0` = never rung. See [`ring`].
    #[serde(default)]
    pub ring_seq: u64,
    /// The sender's OWN current Wi-Fi IPv4 (dotted quad), carried on every
    /// push over BOTH transports. The laptop adopts it into its cached-peer-IP
    /// fast path, so a DHCP renew can't strand mirror/cast/camera on a dead
    /// address — crucially this also works while BLE is the only live link,
    /// when the phone answers no mDNS at all (multicast lock released).
    /// `None` when the sender has no Wi-Fi up (or is an old build).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_ip: Option<String>,
    /// The sender's display refresh rate in Hz, rounded. A screen mirror can
    /// never carry more frames per second than the panel it is capturing
    /// produces, so this is the only honest ceiling for the frame rate the
    /// receiver asks for — hardcoding one caps a 120 Hz phone at 60 and asks a
    /// 60 Hz phone for frames it will never make. `None` on an older build,
    /// where the receiver falls back to its own conservative default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_hz: Option<u32>,
    /// Sender's local unix timestamp (seconds). The receiver uses this
    /// only as a freshness hint for the UI — clock skew between sides
    /// is fine because security is rooted in the Noise handshake.
    #[serde(default)]
    pub ts: u64,
}

/// Laptop→phone screen-cast offer (rides [`AppState::laptop_cast`]): where the
/// phone's viewer connects and the key to open the sealed HEVC stream. The key
/// is a fresh random per-cast secret carried under the Noise-sealed transport —
/// distinct from any derived key, so it never collides nonces with the
/// phone→laptop direction. MUST match the Android `AppState.LaptopCast`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LaptopCast {
    /// The laptop's LAN IP the phone connects out to.
    pub ip: String,
    /// TCP port of the laptop's video server (`mirror_tcp::LAPTOP_VIDEO_PORT`).
    pub port: u16,
    /// Lowercase hex of the 32-byte laptop→phone media key (64 chars).
    pub key: String,
}

/// Phone→laptop webcam-camera offer (rides [`AppState::camera_offer`]): the
/// phone is serving its camera; the laptop dials its IP on this port and opens
/// the sealed H.264 stream with this key. The laptop already knows the phone's
/// IP (its LAN session), so only port + key travel. MUST match Android `CameraOffer`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CameraOffer {
    /// TCP port of the phone's camera server (`mirror_tcp::CAMERA_VIDEO_PORT`).
    pub port: u16,
    /// Lowercase hex of the 32-byte camera media key (64 chars).
    pub key: String,
    /// Clockwise degrees the laptop must rotate the frames to show them upright
    /// (the camera's sensor orientation: typically 90 back / 270 front). The
    /// laptop applies a `videoflip`. Changes on a lens flip.
    #[serde(default)]
    pub rot: u16,
}

fn default_v() -> u8 {
    APPSTATE_SCHEMA_V
}

fn default_true() -> bool {
    true
}

impl AppState {
    pub fn now_laptop() -> Self {
        let battery = crate::core::status::read_local_battery().0;
        let charging = crate::core::status::read_local_charging();
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        AppState {
            v: APPSTATE_SCHEMA_V,
            battery,
            class: DeviceClass::Laptop,
            name,
            locale: None,
            locale_changed_at: 0,
            theme: None,
            theme_changed_at: 0,
            earbuds: None,
            revoked: false,
            audio_claim_request: false,
            call_phase: None,
            call: None,
            call_control: None,
            notif_invoke: None,
            handoff: None,
            media_playing: false,
            media_play_age_ms: 0,
            media_title: String::new(),  // filled by the heartbeat builders
            media_artist: String::new(), // (lan.rs / ble.rs) from MPRIS
            media_app: String::new(),
            media_art_url: String::new(),
            media_np_playing: false,
            media_control: String::new(), // phone→laptop only
            media_control_seq: 0,
            // Enabled by default; the real value is loaded from the
            // smart-switch store + LWW-synced with the peer.
            smart_switch_enabled: true,
            smart_switch_changed_at: 0,
            charging,
            locked: None,
            unlocked: None, // laptop doesn't report a keyguard state
            lock_command: None,
            lock_command_seq: 0,
            laptop_mirror_req: false, // laptop is the caster, never the requester
            laptop_mirror_extend: None, // ditto — the phone picks the kind
            laptop_cast: None,        // filled while actively casting
            laptop_cast_error: None,  // set only when an attempt fails
            camera_req: false,        // filled by the UI when webcam is wanted
            camera_facing: String::new(), // filled by the UI front/back toggle
            camera_offer: None,       // laptop never offers a camera
            ring_seq: 0,              // bumped by the UI's "Ring my phone" tap
            wifi_ip: None,            // phone→laptop only (cached-peer-IP hint)
            display_hz: None,         // phone→laptop only (mirror frame-rate ceiling)
            ts,
        }
    }
}

/// Detect a sensible default locale from the system. Returns one of
/// the supported codes (`en`, `uz`, `ru`) or `en` as the fallback.
pub fn system_locale() -> String {
    let raw = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if raw.starts_with("uz") {
        "uz".into()
    } else if raw.starts_with("ru") {
        "ru".into()
    } else {
        "en".into()
    }
}

#[derive(Debug)]
pub enum AppStateError {
    Snow(snow::Error),
    Io(std::io::Error),
    Timeout(&'static str),
    Frame(FrameDecodeError),
    UnexpectedFrame { ty: u8, sub: u8 },
    Json(serde_json::Error),
    UnsupportedVersion(u8),
    OversizeFrame(usize),
}

impl std::fmt::Display for AppStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snow(e) => write!(f, "noise transport: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::Frame(e) => write!(f, "frame decode: {e}"),
            Self::UnexpectedFrame { ty, sub } => {
                write!(f, "unexpected frame type=0x{ty:02x} sub=0x{sub:02x}")
            }
            Self::Json(e) => write!(f, "app-state json: {e}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported app-state schema v={v}"),
            Self::OversizeFrame(n) => write!(f, "app-state frame too large: {n}"),
        }
    }
}

impl std::error::Error for AppStateError {}

impl From<snow::Error> for AppStateError {
    fn from(e: snow::Error) -> Self {
        Self::Snow(e)
    }
}
impl From<std::io::Error> for AppStateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for AppStateError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// LAN-side exchange: write our snapshot, read the peer's. Uses the
/// caller-provided `transport` so the AEAD cipher is rooted in the
/// session keys that the IK handshake just established.
pub async fn exchange_app_state(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    local: &AppState,
    wait: Duration,
) -> Result<AppState, AppStateError> {
    let json = serde_json::to_vec(local)?;
    let mut ct = vec![0u8; json.len() + 16];
    let ct_len = transport.write_message(&json, &mut ct)?;
    ct.truncate(ct_len);
    let frame = Frame::new(ty::TRANSPORT_APP_DATA, 0x01, ct);
    let frame_bytes = frame.encode();
    stream.write_all(&frame_bytes).await?;
    stream.flush().await?;

    let peer_frame = timeout(wait, read_frame(stream))
        .await
        .map_err(|_| AppStateError::Timeout("peer app-state"))??;
    if peer_frame.ty != ty::TRANSPORT_APP_DATA {
        return Err(AppStateError::UnexpectedFrame {
            ty: peer_frame.ty,
            sub: peer_frame.sub,
        });
    }
    let mut pt = vec![0u8; peer_frame.payload.len()];
    let pt_len = transport.read_message(&peer_frame.payload, &mut pt)?;
    pt.truncate(pt_len);
    let state: AppState = serde_json::from_slice(&pt)?;
    if state.v > APPSTATE_SCHEMA_V {
        warn!("peer sent app-state v={} (we know v={})", state.v, APPSTATE_SCHEMA_V);
    }
    Ok(state)
}

async fn read_frame(stream: &mut TcpStream) -> Result<Frame, AppStateError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if length > MAX_FRAME_PAYLOAD {
        return Err(AppStateError::OversizeFrame(length));
    }
    let mut full = vec![0u8; FRAME_HEADER_LEN + length];
    full[..FRAME_HEADER_LEN].copy_from_slice(&header);
    if length > 0 {
        stream.read_exact(&mut full[FRAME_HEADER_LEN..]).await?;
    }
    Frame::decode(&full).map_err(AppStateError::Frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let a = AppState {
            v: 1,
            battery: Some(75),
            class: DeviceClass::Laptop,
            name: Some("zoyirjon-Blade".into()),
            locale: Some("uz".into()),
            locale_changed_at: 1_700_000_100,
            theme: Some("dark".into()),
            theme_changed_at: 0,
            earbuds: Some(EarbudsInfo {
                name: "AirPods Pro".into(),
                address: "AA:BB:CC:DD:EE:FF".into(),
                battery: Some(60),
                connected: true,
            }),
            revoked: false,
            audio_claim_request: false,
            call_phase: None,
            call: None,
            call_control: None,
            notif_invoke: None,
            handoff: None,
            media_playing: false,
            media_play_age_ms: 0,
            media_title: "Bohemian Rhapsody".into(),
            media_artist: "Queen".into(),
            media_app: "Spotify".into(),
            media_art_url: "https://i.scdn.co/image/ab67616d0000b273".into(),
            media_np_playing: true,
            media_control: "media_play_pause".into(),
            media_control_seq: 3,
            smart_switch_enabled: true,
            smart_switch_changed_at: 0,
            charging: false,
            locked: Some(false),
            unlocked: Some(true),
            lock_command: Some("lock".into()),
            lock_command_seq: 7,
            laptop_mirror_req: false,
            laptop_mirror_extend: None,
            laptop_cast: None,
            laptop_cast_error: None,
            camera_req: false,
            camera_facing: String::new(),
            camera_offer: None,
            ring_seq: 0,
            wifi_ip: Some("192.168.1.42".into()),
            display_hz: Some(120),
            ts: 1_700_000_000,
        };
        let json = serde_json::to_vec(&a).unwrap();
        let b: AppState = serde_json::from_slice(&json).unwrap();
        assert_eq!(a.wifi_ip, b.wifi_ip);
        assert_eq!(a.display_hz, b.display_hz);
        assert_eq!(a.battery, b.battery);
        assert_eq!(a.class, b.class);
        assert_eq!(a.name, b.name);
        assert_eq!(a.locale, b.locale);
        assert_eq!(a.locale_changed_at, b.locale_changed_at);
        assert_eq!(a.theme, b.theme);
        assert_eq!(
            a.earbuds.unwrap().name,
            b.earbuds.unwrap().name
        );
        assert_eq!(a.locked, b.locked);
        assert_eq!(a.lock_command, b.lock_command);
        assert_eq!(a.lock_command_seq, b.lock_command_seq);
        assert_eq!(a.media_title, b.media_title);
        assert_eq!(a.media_artist, b.media_artist);
        assert_eq!(a.media_app, b.media_app);
        assert_eq!(a.media_art_url, b.media_art_url);
        assert_eq!(a.media_control, b.media_control);
        assert_eq!(a.media_control_seq, b.media_control_seq);
    }

    #[test]
    fn unknown_field_ignored() {
        let s = r#"{"v":1,"class":"phone","new_field":42}"#;
        let a: AppState = serde_json::from_str(s).unwrap();
        assert_eq!(a.class, DeviceClass::Phone);
        assert!(a.battery.is_none());
    }

    #[test]
    fn out_of_range_battery_drops_to_none() {
        // u8 already rejects negatives + values > 255; the custom
        // deserializer additionally filters 101..=255 to None.
        let s = r#"{"v":1,"class":"phone","battery":150}"#;
        let a: AppState = serde_json::from_str(s).unwrap();
        assert!(a.battery.is_none(), "battery > 100 must deserialize to None");

        let s2 = r#"{"v":1,"class":"phone","battery":100}"#;
        let a2: AppState = serde_json::from_str(s2).unwrap();
        assert_eq!(a2.battery, Some(100), "battery == 100 must pass through");

        // Earbuds.battery uses the same filter.
        let s3 = r#"{"v":1,"class":"phone","earbuds":{"name":"x","battery":200,"connected":true}}"#;
        let a3: AppState = serde_json::from_str(s3).unwrap();
        assert!(a3.earbuds.unwrap().battery.is_none(), "earbuds battery > 100 must be None");
    }
}
