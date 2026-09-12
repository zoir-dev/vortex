//! Persistent BLE listener for the AUDIO_SIGNAL characteristic
//! (spec, P2.13).
//!
//! After a successful IK reconnect handshake the daemon has a Noise
//! transport-mode cipher pair. Rather than letting it die with the BLE
//! connection, this module keeps the GATT link alive, subscribes to the
//! peer's AUDIO_SIGNAL characteristic, and feeds AUDIO_OP frames
//! straight into [`AudioOrchestrator`] — same dispatch path the LAN
//! session uses, just without the 5-second heartbeat round-trip.
//!
//! Lifecycle:
//!  - One listener per trusted peer; the caller (Tauri reconnect loop)
//!    spawns it after `run_ik_initiator` succeeds.
//!  - The listener returns when the notification stream ends, which is
//!    bluer's signal that the BLE GATT link dropped. Callers should
//!    restart it on the next reconnect.
//!  - Inbound: notifications from the peer (this listener loop).
//!  - Outbound: [`write_audio_op`] AEAD-seals an `AudioOpFrame` and
//!    writes it back over GATT WRITE on the same characteristic.
//!    Used by the orchestrator's BLE-fallback writer when the LAN
//!    session isn't open (call-handoff windows). The peer side
//!    (Android `onCharacteristicWriteRequest`) decrypts with its
//!    receive cipher and runs the same `on_incoming` dispatch as
//!    the LAN session would.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Cumulative BLE Noise receive-cipher recovery stats for this process —
/// surfaced in the resync / re-handshake logs so a beta run shows how often the
/// lossy BLE link desyncs the cipher and whether the 128-nonce skip absorbs it
/// (`RESYNC_EVENTS` / `RESYNC_FRAMES_SKIPPED`) or it escalates to a full
/// re-handshake (`REHANDSHAKE_EVENTS`). Observability only — affects nothing.
pub(crate) static RESYNC_EVENTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static RESYNC_FRAMES_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub(crate) static REHANDSHAKE_EVENTS: AtomicU64 = AtomicU64::new(0);

use futures::{pin_mut, StreamExt};
use snow::TransportState;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;

use super::client::VortexClient;
use super::frame::{ty, Frame};
use crate::core::appstate::AppState;
use crate::core::audio_op::{AudioOp, AudioOpFrame};
use crate::core::audio_orchestrator::SwitchOrchestrator;
use crate::core::media_runtime::{pause_playing_for_call, MediaStateStore};

/// Run the AUDIO_SIGNAL listener loop until the BLE notification stream
/// closes (peer drops, adapter goes down, etc).
///
/// Returns `Ok(())` on a clean end-of-stream so the caller can decide
/// whether to reconnect; `Err` for protocol-level problems that mean
/// the session is unsafe to continue (replayed nonces are NOT errors —
/// the orchestrator silently drops them, same as the LAN path).
pub async fn run_listener(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    peer_pub: [u8; 32],
    orchestrator: Arc<SwitchOrchestrator>,
    media_store: MediaStateStore,
    // Additive state-push channel: a STATE frame (battery/charging) is
    // decoded to an `AppState` and forwarded here as (peer_pub, state).
    // The UI layer turns it into the same peer-state update a LAN
    // heartbeat produces. `None` simply drops STATE frames (back-compat).
    // NEVER touches the audio handoff path above.
    state_tx: Option<tokio::sync::mpsc::UnboundedSender<([u8; 32], AppState)>>,
    // Additive notification-mirror channel: a NOTIFICATION frame is decoded
    // to a `NotificationMirror` and forwarded here for desktop display.
    // `None` drops them (back-compat). Never touches the audio handoff.
    notif_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::core::notif_mirror::NotificationMirror>,
    >,
    // Additive live-activity channel: a LIVE_ACTIVITY frame is decoded to a
    // `LiveActivity` and forwarded here to drive the laptop's top-bar pill.
    // `None` drops them (back-compat). Never touches the audio handoff.
    live_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::core::live_activity::LiveActivity>,
    >,
    // Additive app-icon channel: an ICON frame chunk is parsed to
    // (app_id, total, idx, data) and forwarded here for reassembly + caching.
    // `None` drops them (back-compat). Never touches the audio handoff.
    icon_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, u16, u16, Vec<u8>)>>,
    // Additive call-mirror channel: a CALL frame is decoded to a `CallEvent`
    // (ringing/active/ended + caller) and forwarded here to drive the laptop's
    // call banner + in-call pill. `None` drops them. Never touches the handoff.
    call_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::core::call_event::CallEvent>>,
    // Additive contacts-mirror channel: a CONTACTS frame chunk is parsed to
    // (total, idx, data) and forwarded here for reassembly + caching → the
    // laptop's Contacts page. `None` drops them. Never touches the handoff.
    contacts_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>>,
    // Additive call-log-mirror channel: a CALL_LOG frame chunk is parsed to
    // (total, idx, data) and forwarded here for reassembly + caching → the
    // laptop's Recents page. `None` drops them. Never touches the handoff.
    call_log_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>>,
    // Additive SMS-mirror channel: an SMS frame chunk is parsed to (total, idx,
    // data) and forwarded here for reassembly + caching → the laptop's Messages
    // page. `None` drops them. Never touches the handoff.
    sms_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>>,
    // Additive on-demand SMS-thread channel: an SMS_THREAD frame chunk (a single
    // conversation's page, requested by the laptop's infinite scroll) is parsed
    // to (total, idx, data) and forwarded here for reassembly → merged into the
    // open thread. `None` drops them. Never touches the handoff.
    sms_thread_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>>,
    // Additive clipboard-sync channel: a CLIPBOARD frame (phone copied text)
    // is parsed to a ClipboardMirror and forwarded here → the laptop sets its
    // system clipboard + adds it to history. `None` drops them.
    clipboard_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::core::clipboard_mirror::ClipboardMirror>,
    >,
    // Additive clipboard-IMAGE channel: a CLIPBOARD_IMAGE chunk (phone copied
    // an image) parsed to (total, idx, data) and forwarded here for reassembly
    // → the laptop sets its clipboard image + adds it to history.
    clipboard_image_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>>,
    // Additive clipboard-image-OFFER channel: a CLIPBOARD_IMAGE_OFFER (phone
    // shared/copied an image and stashed it) → the laptop pulls it over LAN.
    clipboard_offer_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::core::clipboard_mirror::ClipboardImageOffer>,
    >,
    // Additive browsing-handoff channel: a HANDOFF frame (the page the phone is
    // on) parsed to a `HandoffEvent` → the laptop opens it / shows a "continue
    // from phone" pill. `None` drops them. Never touches the audio handoff.
    handoff_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::core::handoff::HandoffEvent>>,
    // Generic additive-frame channel: any allowed frame WITHOUT a dedicated
    // handler above is forwarded raw as (peer_pub, frame_ty,
    // decrypted_payload) here, so a new feature (e.g. notes, peer handoff)
    // routes it entirely in its OWN module — no per-feature code in this
    // transport file. `None` drops them.
    //
    // The peer identity is part of the tuple because a frame's meaning can
    // depend on WHO sent it: `PEER_HANDOFF` says "you are no longer my active
    // peer", which is unactionable without knowing whose statement it is.
    // Inferring it from "whoever is active right now" would be a guess.
    raw_frame_tx: Option<
        tokio::sync::mpsc::UnboundedSender<([u8; 32], u8, Vec<u8>)>,
    >,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let notifies = char
        .notify()
        .await
        .map_err(|e| format!("subscribe AUDIO_SIGNAL: {e}"))?;
    pin_mut!(notifies);
    info!(addr = %client.address, "BLE audio-signal listener up");
    // Reconcile on (re)connect: ask the phone to re-send any active notification
    // we don't already have. Covers notifications posted while we were
    // disconnected (their notify had no subscriber) — the consumer carries our
    // known keys so only the genuinely-missing ones come back.
    if let Some(tx) = notif_tx.as_ref() {
        let _ = tx.send(crate::core::notif_mirror::NotificationMirror::catch_up_signal());
    }

    // Consecutive AEAD-open failures. A BLE notify lost on a flaky link (with no
    // disconnect) leaves our Noise receive nonce behind the phone's sender → every
    // later frame then fails. Once we see a few in a row the cipher is desynced
    // for good, so DROP the session: the persistent loop reconnects with a fresh
    // IK handshake, which resets BOTH directions' ciphers to nonce 0.
    let mut aead_fails = 0u32;
    // …EXCEPT during the first moments of a session: when the previous session
    // died mid-burst, BlueZ can still deliver its queued notifies (sealed with
    // the OLD cipher) after our fresh IK. Failing to open those is harmless —
    // a decrypt failure does NOT advance our receive nonce, so the next frame
    // sealed with the NEW cipher opens fine. Counting them tripped the 3-fail
    // rule and dropped a perfectly healthy session (live-observed churn loop:
    // drop → re-IK → re-subscribe → another stale drain → drop…).
    let session_start = tokio::time::Instant::now();
    const STALE_DRAIN_GRACE: Duration = Duration::from_secs(2);
    // How many consecutively dropped BLE frames we'll skip the receive nonce
    // over to resync, before giving up and dropping the session. Each skip is
    // one cheap AEAD attempt. Sized for the clipboard-IMAGE chunk storm
    // (hundreds of notify frames, some dropped in bursts) so the session
    // survives a big gap rather than tearing down + re-handshaking.
    const NONCE_RESYNC_WINDOW: u64 = 128;

    // Reassembles CLIPBOARD_TEXT chunks (phone→laptop long text) across notifies.
    let mut clip_text_asm = crate::core::clipboard_mirror::ImageAssembler::default();
    // Reassembles FRAG envelopes: one oversized sealed frame split across
    // notifies because Android truncates any single notify at ATT_MTU−3
    // (silently — the tail is just gone). The reassembled inner frame is
    // queued here and handled on the next loop pass exactly like a normal
    // arrival, so every frame type gets the fix for free.
    let mut frag_asm = crate::core::clipboard_mirror::ImageAssembler::default();
    let mut reassembled: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();

    loop {
        let raw: Vec<u8> = match reassembled.pop_front() {
            Some(inner) => inner,
            None => match notifies.next().await {
                Some(r) => r.to_vec(),
                None => break,
            },
        };
        info!("BLE AUDIO_SIGNAL raw notify: {} bytes", raw.len());
        let frame = match Frame::decode(&raw) {
            Ok(f) => f,
            Err(e) => {
                warn!("audio-signal frame decode: {e}; ignoring");
                continue;
            }
        };
        // FRAG: a slice of one oversized sealed frame — reassemble BEFORE the
        // AEAD stage (the envelope itself is not sealed; the inner frame is).
        if frame.ty == ty::FRAG {
            if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&frame.payload) {
                if let Some(inner) = frag_asm.add(total, idx, data) {
                    reassembled.push_back(inner);
                }
            } else {
                warn!("audio-signal FRAG chunk malformed; dropping");
            }
            continue;
        }
        if frame.ty != ty::AUDIO_OP
            && frame.ty != ty::STATE
            && frame.ty != ty::NOTIFICATION
            && frame.ty != ty::LIVE_ACTIVITY
            && frame.ty != ty::ICON
            && frame.ty != ty::CALL
            && frame.ty != ty::CONTACTS
            && frame.ty != ty::CALL_LOG
            && frame.ty != ty::SMS
            && frame.ty != ty::SMS_THREAD
            && frame.ty != ty::CLIPBOARD
            && frame.ty != ty::CLIPBOARD_IMAGE
            && frame.ty != ty::CLIPBOARD_IMAGE_OFFER
            && frame.ty != ty::CLIPBOARD_TEXT
            && frame.ty != ty::WIFI_DIRECT_OFFER
            && frame.ty != ty::HANDOFF
            && frame.ty != ty::NOTES_SYNC
            && frame.ty != ty::PEER_HANDOFF
        {
            warn!(
                "audio-signal unexpected frame ty=0x{:02x}; ignoring",
                frame.ty
            );
            continue;
        }
        // AEAD-open with the transport state's receive cipher. Snow's
        // `TransportState::read_message` advances the nonce on success
        // so any subsequent replay attempt fails fast. Both AUDIO_OP and
        // STATE frames ride the same ordered sealed stream, so they share
        // the nonce sequence — we just route on `frame.ty` after opening.
        let mut plain = vec![0u8; frame.payload.len()];
        let n = {
            // The 2026-06-11 switch freeze wedged this listener exactly
            // here, silently, while the seal path held the lock. Keep
            // waiting (the lock holder owns the cipher nonce sequence)
            // but make any future stall loud.
            let mut t = match tokio::time::timeout(Duration::from_secs(5), transport.lock()).await
            {
                Ok(g) => g,
                Err(_) => {
                    warn!("audio-signal: transport lock busy >5s (seal path stalled?) — still waiting");
                    transport.lock().await
                }
            };
            match t.read_message(&frame.payload, &mut plain) {
                Ok(n) => {
                    aead_fails = 0;
                    Some(n)
                }
                // The BLE notify channel is best-effort: a burst (e.g. many
                // emails arriving at once) can overflow the peripheral's
                // notify queue and DROP frames. Our Noise receive nonce is
                // then behind the sender's, so this frame — and every later
                // one — fails to open, and the old behaviour tore the whole
                // session down. snow does NOT advance the nonce on a failed
                // decrypt (it increments only after a successful one), so we
                // can skip the receive nonce forward a few steps and retry:
                // if a near nonce opens the frame, we've resynced past the
                // dropped frame(s) WITHOUT a re-handshake. Benefits every
                // frame type on this shared stream (audio/calls/SMS too).
                Err(_) => {
                    let base = t.receiving_nonce();
                    let mut recovered = None;
                    for skip in 1..=NONCE_RESYNC_WINDOW {
                        t.set_receiving_nonce(base + skip);
                        if let Ok(len) = t.read_message(&frame.payload, &mut plain) {
                            recovered = Some((skip, len));
                            break;
                        }
                    }
                    match recovered {
                        Some((skip, len)) => {
                            let events = RESYNC_EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
                            let frames =
                                RESYNC_FRAMES_SKIPPED.fetch_add(skip, Ordering::Relaxed) + skip;
                            warn!(
                                dropped = skip,
                                resync_events = events,
                                frames_skipped_total = frames,
                                rehandshakes = REHANDSHAKE_EVENTS.load(Ordering::Relaxed),
                                "audio-signal: resynced past dropped BLE frame(s) (no re-handshake)"
                            );
                            aead_fails = 0;
                            // A frame was just lost in-air. If it was a mirrored
                            // notification its payload is gone for good (BLE
                            // Notify is fire-and-forget, never retransmitted), so
                            // nudge the notif consumer to request a catch-up —
                            // the phone re-sends anything we're missing.
                            if let Some(tx) = notif_tx.as_ref() {
                                let _ = tx.send(
                                    crate::core::notif_mirror::NotificationMirror::catch_up_signal(),
                                );
                            }
                            Some(len)
                        }
                        None => {
                            // No nearby nonce opened it — a genuinely corrupt
                            // frame, or more than the window was lost. Restore
                            // the nonce and fall through to the failure path.
                            t.set_receiving_nonce(base);
                            None
                        }
                    }
                }
            }
        };
        let n = match n {
            Some(n) => n,
            None => {
                if session_start.elapsed() < STALE_DRAIN_GRACE {
                    warn!("audio-signal aead-open failed during stale-drain grace; dropping frame (not counted)");
                    continue;
                }
                aead_fails += 1;
                warn!("audio-signal aead-open failed (#{aead_fails}); dropping frame");
                if aead_fails >= 3 {
                    let rehandshakes = REHANDSHAKE_EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        rehandshakes,
                        resync_events = RESYNC_EVENTS.load(Ordering::Relaxed),
                        "audio-signal: receive cipher desynced ({aead_fails} consecutive \
                         AEAD failures) — dropping the BLE session to force a fresh IK \
                         handshake (resyncs both directions)"
                    );
                    return Ok(());
                }
                continue;
            }
        };

        // STATE frame: an app-state push (battery/charging). Decode and
        // hand to the UI channel — completely separate from the audio
        // handoff dispatch below.
        if frame.ty == ty::STATE {
            match serde_json::from_slice::<AppState>(&plain[..n]) {
                Ok(state) => {
                    info!(battery = ?state.battery, charging = state.charging, "← BLE state push");
                    if let Some(tx) = state_tx.as_ref() {
                        let _ = tx.send((peer_pub, state));
                    }
                }
                Err(e) => warn!("audio-signal STATE payload not AppState: {e}; dropping"),
            }
            continue;
        }

        // NOTIFICATION frame: a mirrored phone notification. Decode and
        // hand to the desktop-display channel — separate from the audio
        // handoff. Content is NOT logged (privacy).
        if frame.ty == ty::NOTIFICATION {
            match serde_json::from_slice::<crate::core::notif_mirror::NotificationMirror>(
                &plain[..n],
            ) {
                Ok(notif) => {
                    info!(app = %notif.app, "← BLE notification mirror");
                    if let Some(tx) = notif_tx.as_ref() {
                        let _ = tx.send(notif);
                    }
                }
                Err(e) => warn!("audio-signal NOTIFICATION payload invalid: {e}; dropping"),
            }
            continue;
        }

        // CLIPBOARD frame: the phone copied text → mirror it to the laptop
        // clipboard + history. Content is NOT logged (privacy); only length.
        if frame.ty == ty::CLIPBOARD {
            match serde_json::from_slice::<crate::core::clipboard_mirror::ClipboardMirror>(
                &plain[..n],
            ) {
                Ok(clip) => {
                    info!(chars = clip.text.chars().count(), "← BLE clipboard sync");
                    if let Some(tx) = clipboard_tx.as_ref() {
                        let _ = tx.send(clip);
                    }
                }
                Err(e) => warn!("audio-signal CLIPBOARD payload invalid: {e}; dropping"),
            }
            continue;
        }

        // CLIPBOARD_TEXT chunk: a piece of a LONG copied text (phone→laptop).
        // Reassemble locally and feed the same channel as single-frame
        // CLIPBOARD, so the downstream apply path is shared. Not logged.
        if frame.ty == ty::CLIPBOARD_TEXT {
            if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&plain[..n]) {
                if let Some(bytes) = clip_text_asm.add(total, idx, data) {
                    match String::from_utf8(bytes) {
                        Ok(text) => {
                            info!(chars = text.chars().count(), "← BLE clipboard text (chunked)");
                            if let Some(tx) = clipboard_tx.as_ref() {
                                let _ = tx.send(
                                    crate::core::clipboard_mirror::ClipboardMirror { text, ts: 0 },
                                );
                            }
                        }
                        Err(e) => warn!("CLIPBOARD_TEXT reassembly not UTF-8: {e}; dropping"),
                    }
                }
            } else {
                warn!("audio-signal CLIPBOARD_TEXT chunk malformed; dropping");
            }
            continue;
        }

        // WIFI_DIRECT_OFFER: the phone brought up a P2P group for a fast pull.
        // Hand {ssid, pass} to the UI hook → it joins + pulls over the direct link.
        if frame.ty == ty::WIFI_DIRECT_OFFER {
            match serde_json::from_slice::<serde_json::Value>(&plain[..n]) {
                Ok(v) => {
                    let ssid = v.get("ssid").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let pass = v.get("pass").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    if !ssid.is_empty() {
                        info!(%ssid, "← BLE Wi-Fi Direct offer");
                        crate::core::wifi_direct::report(ssid, pass);
                    }
                }
                Err(e) => warn!("audio-signal WIFI_DIRECT_OFFER invalid: {e}; dropping"),
            }
            continue;
        }

        // CLIPBOARD_IMAGE chunk: a piece of a copied image (phone→laptop).
        // Parse to (total, idx, data) and forward for reassembly.
        if frame.ty == ty::CLIPBOARD_IMAGE {
            if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&plain[..n]) {
                if let Some(tx) = clipboard_image_tx.as_ref() {
                    let _ = tx.send((total, idx, data));
                }
            } else {
                warn!("audio-signal CLIPBOARD_IMAGE chunk malformed; dropping");
            }
            continue;
        }

        // CLIPBOARD_IMAGE_OFFER: the phone has an image ready → the laptop
        // pulls it over LAN (reliable) by token.
        if frame.ty == ty::CLIPBOARD_IMAGE_OFFER {
            match serde_json::from_slice::<crate::core::clipboard_mirror::ClipboardImageOffer>(
                &plain[..n],
            ) {
                Ok(offer) => {
                    info!(bytes = offer.bytes, "← BLE clipboard image offer");
                    if let Some(tx) = clipboard_offer_tx.as_ref() {
                        let _ = tx.send(offer);
                    }
                }
                Err(e) => warn!("audio-signal CLIPBOARD_IMAGE_OFFER invalid: {e}; dropping"),
            }
            continue;
        }

        // LIVE_ACTIVITY frame: an ongoing progress pill (ride/nav/delivery).
        // Decode and hand to the top-bar-pill channel — separate from the
        // audio handoff. Content is NOT logged (privacy); only the app label.
        if frame.ty == ty::LIVE_ACTIVITY {
            match serde_json::from_slice::<crate::core::live_activity::LiveActivity>(&plain[..n]) {
                Ok(live) => {
                    info!(app = %live.app, ended = live.ended, "← BLE live activity");
                    if let Some(tx) = live_tx.as_ref() {
                        let _ = tx.send(live);
                    }
                }
                Err(e) => warn!("audio-signal LIVE_ACTIVITY payload invalid: {e}; dropping"),
            }
            continue;
        }

        // CALL frame: an incoming/ongoing phone call. Decode and hand to the
        // call-mirror channel (banner + in-call pill) — separate from the
        // audio handoff. Only phase + caller label logged (not the number).
        if frame.ty == ty::CALL {
            match crate::core::call_event::CallEvent::from_json(&plain[..n]) {
                Some(ev) => {
                    info!(phase = %ev.phase, named = !ev.name.is_empty(), "← BLE call event");
                    if let Some(tx) = call_tx.as_ref() {
                        let _ = tx.send(ev);
                    }
                }
                None => warn!("audio-signal CALL payload invalid; dropping"),
            }
            continue;
        }

        // HANDOFF frame: the page the phone is browsing. Decode and hand to the
        // handoff channel → the laptop opens it (Share) or shows a "continue
        // from phone" pill (live read). The URL is not logged (privacy).
        if frame.ty == ty::HANDOFF {
            match crate::core::handoff::HandoffEvent::from_json(&plain[..n]) {
                Some(ev) => {
                    info!(
                        open_now = ev.open_now,
                        has_url = !ev.url.is_empty(),
                        "← BLE handoff"
                    );
                    if let Some(tx) = handoff_tx.as_ref() {
                        let _ = tx.send(ev);
                    }
                }
                None => warn!("audio-signal HANDOFF payload invalid; dropping"),
            }
            continue;
        }

        // ICON frame: one chunk of an app-icon PNG. Parse and hand to the
        // reassembler/cache channel — separate from the audio handoff.
        if frame.ty == ty::ICON {
            if let Some((app_id, total, idx, data)) =
                crate::core::icon_cache::parse_chunk(&plain[..n])
            {
                if let Some(tx) = icon_tx.as_ref() {
                    let _ = tx.send((app_id, total, idx, data));
                }
            } else {
                warn!("audio-signal ICON chunk malformed; dropping");
            }
            continue;
        }

        // CONTACTS frame: one chunk of the phone's contacts list. Parse and
        // hand to the reassembler/cache channel — separate from the handoff.
        if frame.ty == ty::CONTACTS {
            if let Some((total, idx, data)) = crate::core::contacts::parse_chunk(&plain[..n]) {
                if let Some(tx) = contacts_tx.as_ref() {
                    let _ = tx.send((total, idx, data));
                }
            } else {
                warn!("audio-signal CONTACTS chunk malformed; dropping");
            }
            continue;
        }

        // CALL_LOG frame: one chunk of the phone's recent call log. Parse and
        // hand to the reassembler/cache channel — separate from the handoff.
        if frame.ty == ty::CALL_LOG {
            if let Some((total, idx, data)) = crate::core::call_log::parse_chunk(&plain[..n]) {
                if let Some(tx) = call_log_tx.as_ref() {
                    let _ = tx.send((total, idx, data));
                }
            } else {
                warn!("audio-signal CALL_LOG chunk malformed; dropping");
            }
            continue;
        }

        // SMS frame: one chunk of the phone's recent SMS. Parse and hand to the
        // reassembler/cache channel — separate from the handoff.
        if frame.ty == ty::SMS {
            if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&plain[..n]) {
                if let Some(tx) = sms_tx.as_ref() {
                    let _ = tx.send((total, idx, data));
                }
            } else {
                warn!("audio-signal SMS chunk malformed; dropping");
            }
            continue;
        }

        // SMS_THREAD frame: one chunk of a single conversation's page (the
        // laptop's on-demand infinite scroll). Same chunk format as SMS; routed
        // to its own channel so the UI MERGES it into the open thread.
        if frame.ty == ty::SMS_THREAD {
            if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&plain[..n]) {
                if let Some(tx) = sms_thread_tx.as_ref() {
                    let _ = tx.send((total, idx, data));
                }
            } else {
                warn!("audio-signal SMS_THREAD chunk malformed; dropping");
            }
            continue;
        }

        // An allowed frame that reached here without a dedicated handler above
        // and isn't an AUDIO_OP is an additive feature frame (e.g. NOTES_SYNC):
        // forward it raw to its own module. No feature logic in this file.
        if frame.ty != ty::AUDIO_OP {
            if let Some(tx) = raw_frame_tx.as_ref() {
                let _ = tx.send((peer_pub, frame.ty, plain[..n].to_vec()));
            }
            continue;
        }

        let af = match AudioOpFrame::from_json(&plain[..n]) {
            Ok(f) => f,
            Err(e) => {
                warn!("audio-signal payload not AudioOpFrame: {e}; dropping");
                continue;
            }
        };
        debug!(op = ?af.op, nonce = af.nonce, "← audio-signal");

        // FAST-PATH PAUSE: a Request frame means the phone is starting
        // a buds-claim flow (almost always because of an incoming
        // call). Pause MPRIS *before* the orchestrator releases the
        // buds — once the bluez sink goes away PipeWire migrates the
        // stream to laptop speakers and the player often auto-pauses,
        // leaving `pause_playing_for_call` with nothing to track on a
        // later resume. Doing it here, in parallel with the dispatch,
        // captures the playing set while it's still actually playing.
        if matches!(af.op, AudioOp::Request) {
            let store = media_store.clone();
            tokio::spawn(async move {
                let paused = pause_playing_for_call(&store).await;
                if !paused.is_empty() {
                    info!(?paused, "BLE fast-path: paused MPRIS for call");
                }
            });
        }

        // Dispatch on a fresh task so a slow responder can't stall the
        // notification stream. Same shape as audio_lan_session.rs uses.
        let orch = orchestrator.clone();
        let peer_copy = peer_pub;
        tokio::spawn(async move {
            let _ = orch.on_incoming(peer_copy, af).await;
        });
    }
    info!(addr = %client.address, "BLE audio-signal listener: stream closed");
    Ok(())
}

/// Write a single `AudioOpFrame` to the peer's AUDIO_SIGNAL
/// characteristic. AEAD-sealed with the transport state's SEND cipher
/// (same `TransportState` that drives the receive path in
/// [`run_listener`]). Mirror of Android's
/// `GattServer.sendAudioOpEncrypted` — gives the Linux side a
/// reverse channel for Approve / Released / Done frames that can land
/// even when the LAN session writer is unavailable (call-handoff
/// timing windows where the peer's mDNS hasn't re-resolved yet).
///
/// The peer-side handler (Android `onCharacteristicWriteRequest` for
/// `AUDIO_SIGNAL_UUID`) decrypts with its receive cipher and
/// dispatches into `SwitchOrchestrator.onIncoming` — same dispatch
/// path as the LAN session.
pub async fn write_audio_op(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    frame: AudioOpFrame,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let json = frame
        .to_json()
        .map_err(|e| format!("AudioOpFrame to_json: {e}"))?;
    // Snow's `write_message` appends a 16-byte AEAD tag; size the
    // ciphertext buffer accordingly and truncate to the bytes
    // actually written.
    let mut ct = vec![0u8; json.len() + 16];
    // Hold the lock across char.write — see write_state for why (nonce/wire
    // lockstep; otherwise concurrent writers desync the phone's recv cipher).
    let mut t = transport.lock().await;
    let n = t
        .write_message(&json, &mut ct)
        .map_err(|e| format!("audio-signal write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty::AUDIO_OP, 0, ct).encode();
    char.write(&wire)
        .await
        .map_err(|e| format!("BLE write to AUDIO_SIGNAL: {e}"))?;
    drop(t);
    debug!(op = ?frame.op, nonce = frame.nonce, "→ audio-signal (BLE write)");
    Ok(())
}

/// Send an already-framed laptop→phone payload over the AUDIO_SIGNAL
/// characteristic using a Write **Request** (with response).
///
/// bluer's default `char.write()` is a Write **Command** (no response), whose
/// ATT payload is hard-capped at `MTU − 3` and CANNOT be fragmented — a ~474 B
/// STATE frame returned "Failed to initiate write" on a BLE-only link, so the
/// phone showed the laptop disconnected (small frames like the 108 B IK
/// handshake slipped under the cap, which is why reconnect-control worked but
/// state did not). A Request is ACKed and uses the long-write (prepare/execute)
/// procedure, so any size splits + reassembles automatically; the ACK also
/// keeps our Noise send-nonce in lockstep with the phone's recv cipher (a
/// silently-dropped Command desynced it → "AEAD open failed" → session churn).
/// AUDIO_SIGNAL advertises PROPERTY_WRITE, so a Request is valid. Used for every
/// laptop→phone frame except the tiny latency-critical audio-op opcodes.
async fn write_framed(char: &Characteristic, wire: &[u8]) -> bluer::Result<()> {
    let req = CharacteristicWriteRequest { op_type: WriteOp::Request, ..Default::default() };
    char.write_ext(wire, &req).await
}

/// Push an `AppState` (battery/charging) to the peer over the AUDIO_SIGNAL
/// characteristic as a STATE frame (additive to [`write_audio_op`]). Same
/// AEAD-seal with the transport SEND cipher; the peer routes STATE frames
/// to its app-state handler, never to the audio orchestrator. Used by the
/// laptop's power-watcher to push instantly over BLE instead of waiting
/// for the LAN heartbeat.
pub async fn write_state(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    state: &AppState,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let json = serde_json::to_vec(state).map_err(|e| format!("AppState to_json: {e}"))?;
    let mut ct = vec![0u8; json.len() + 16];
    // Hold the transport lock ACROSS the BLE write: the AEAD nonce bump and the
    // on-wire send must stay in lockstep. Releasing it before char.write let two
    // concurrent laptop→phone writers (state heartbeat / audio-op / notification)
    // deliver frames out of nonce order, which PERMANENTLY desyncs the phone's
    // receive cipher — every later frame then fails "AEAD open failed" and the
    // phone shows the laptop disconnected over BLE.
    let mut t = transport.lock().await;
    let n = t
        .write_message(&json, &mut ct)
        .map_err(|e| format!("state write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty::STATE, 0, ct).encode();
    write_framed(char, &wire)
        .await
        .map_err(|e| format!("BLE write STATE to AUDIO_SIGNAL: {e}"))?;
    drop(t);
    debug!(battery = ?state.battery, charging = state.charging, "→ BLE state push");
    Ok(())
}

/// Push a mirrored desktop notification (laptop → phone) over the
/// AUDIO_SIGNAL characteristic as a NOTIFICATION frame. Same AEAD seal as
/// [`write_state`]; the phone routes NOTIFICATION frames to its own
/// notification display, never to the audio orchestrator. Content is not
/// logged.
pub async fn write_notification(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    notif: &crate::core::notif_mirror::NotificationMirror,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let json = serde_json::to_vec(notif).map_err(|e| format!("notif to_json: {e}"))?;
    let mut ct = vec![0u8; json.len() + 16];
    // Hold the lock across char.write — see write_state (nonce/wire lockstep).
    let mut t = transport.lock().await;
    let n = t
        .write_message(&json, &mut ct)
        .map_err(|e| format!("notif write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty::NOTIFICATION, 0, ct).encode();
    write_framed(char, &wire)
        .await
        .map_err(|e| format!("BLE write NOTIFICATION to AUDIO_SIGNAL: {e}"))?;
    debug!(app = %notif.app, "→ BLE notification push (laptop→phone)");
    Ok(())
}

/// Generic sealed-frame writer (laptop → phone): AEAD-seal `payload` and send
/// it as frame `ty` over AUDIO_SIGNAL, holding the transport lock across the
/// write (nonce/wire lockstep). Transport-level infrastructure with no
/// feature knowledge — a feature module (e.g. notes) supplies its own frame
/// type + payload, keeping all of its logic in its own file.
pub async fn write_sealed(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    ty: u8,
    payload: &[u8],
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let mut ct = vec![0u8; payload.len() + 16];
    let mut t = transport.lock().await;
    let n = t
        .write_message(payload, &mut ct)
        .map_err(|e| format!("sealed write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty, 0, ct).encode();
    write_framed(char, &wire)
        .await
        .map_err(|e| format!("BLE write 0x{ty:02x} to AUDIO_SIGNAL: {e}"))?;
    Ok(())
}

/// Push laptop-copied text to the phone (laptop → phone) as a CLIPBOARD
/// frame. Same AEAD seal / lock-across-write nonce discipline as the other
/// writers; the phone routes CLIPBOARD frames to its system clipboard.
/// Content is not logged (only length).
pub async fn write_clipboard(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    clip: &crate::core::clipboard_mirror::ClipboardMirror,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    // Long text would overflow a single BLE frame → chunk it over CLIPBOARD_TEXT
    // (same `[total][idx][data]` wire + 12ms pacing as the image sender). Short
    // text keeps the fast single-frame CLIPBOARD path.
    if clip.text.len() > crate::core::clipboard_mirror::MAX_SINGLE_FRAME_TEXT_BYTES {
        let chunks = crate::core::clipboard_mirror::build_text_chunks(&clip.text);
        let total = chunks.len();
        for payload in chunks {
            let mut ct = vec![0u8; payload.len() + 16];
            // Hold the lock ACROSS the write, exactly like the single-frame
            // writers (see write_state). Sealing under the lock but writing
            // outside it lets a concurrent writer — the 12 s state heartbeat,
            // a mirrored notification — seal a HIGHER nonce and reach the wire
            // FIRST. The phone's receive cipher then desyncs for every later
            // frame, which showed up live as bursts of "resynced past dropped
            // BLE frame(s)" and a listener stuck >5 s on the transport lock
            // right after a long clipboard copy. Sealing order and wire order
            // have to be one critical section.
            {
                let mut t = transport.lock().await;
                let n = t
                    .write_message(&payload, &mut ct)
                    .map_err(|e| format!("clipboard-text write_message: {e}"))?;
                ct.truncate(n);
                let wire = Frame::new(ty::CLIPBOARD_TEXT, 0, ct).encode();
                write_framed(char, &wire)
                    .await
                    .map_err(|e| format!("BLE write CLIPBOARD_TEXT to AUDIO_SIGNAL: {e}"))?;
            }
            // Pace OUTSIDE the lock so other writers can interleave BETWEEN
            // chunks — each chunk is atomic, which is all the nonce discipline
            // needs, and holding across the sleep would stall the listener.
            tokio::time::sleep(Duration::from_millis(12)).await;
        }
        debug!(
            chars = clip.text.chars().count(),
            total, "→ BLE clipboard text push chunked (laptop→phone)"
        );
        return Ok(());
    }
    let json = serde_json::to_vec(clip).map_err(|e| format!("clipboard to_json: {e}"))?;
    let mut ct = vec![0u8; json.len() + 16];
    let mut t = transport.lock().await;
    let n = t
        .write_message(&json, &mut ct)
        .map_err(|e| format!("clipboard write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty::CLIPBOARD, 0, ct).encode();
    write_framed(char, &wire)
        .await
        .map_err(|e| format!("BLE write CLIPBOARD to AUDIO_SIGNAL: {e}"))?;
    debug!(chars = clip.text.chars().count(), "→ BLE clipboard push (laptop→phone)");
    Ok(())
}

/// Push a laptop-copied IMAGE to the phone (laptop → phone) as a series of
/// CLIPBOARD_IMAGE chunk frames. Each chunk is AEAD-sealed and paced so the
/// BLE notify queue doesn't overflow (same discipline as the icon sender).
pub async fn write_clipboard_image(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    png: &[u8],
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let chunks = crate::core::clipboard_mirror::build_image_chunks(png);
    let total = chunks.len();
    for payload in chunks {
        let mut ct = vec![0u8; payload.len() + 16];
        // Lock per chunk (not across the whole image) so other writers can
        // interleave BETWEEN chunks — but the seal and the write of one chunk
        // must stay inside a single critical section. The old code dropped the
        // lock after sealing and claimed "nonce order is still preserved per
        // write", which does not hold with a concurrent writer: it can seal a
        // later nonce and hit the wire first, permanently desyncing the phone's
        // receive cipher. Same fix as the CLIPBOARD_TEXT loop above.
        {
            let mut t = transport.lock().await;
            let n = t
                .write_message(&payload, &mut ct)
                .map_err(|e| format!("clipboard-image write_message: {e}"))?;
            ct.truncate(n);
            let wire = Frame::new(ty::CLIPBOARD_IMAGE, 0, ct).encode();
            write_framed(char, &wire)
                .await
                .map_err(|e| format!("BLE write CLIPBOARD_IMAGE to AUDIO_SIGNAL: {e}"))?;
        }
        // Pace so the BLE stack doesn't drop notifies under the chunk storm.
        tokio::time::sleep(Duration::from_millis(12)).await;
    }
    debug!(bytes = png.len(), total, "→ BLE clipboard image push (laptop→phone)");
    Ok(())
}

/// Push a laptop→phone call-control command (accept/decline/end/mute/…) as a
/// CALL_CONTROL frame (0x38). Same lock-across-write nonce discipline as the
/// other writers; routed separately from the audio handoff.
pub async fn write_call_control(
    client: &VortexClient,
    transport: Arc<Mutex<TransportState>>,
    ctrl: &crate::core::call_event::CallControl,
) -> Result<(), String> {
    let char = client
        .audio_signal
        .as_ref()
        .ok_or_else(|| "peer has no AUDIO_SIGNAL characteristic".to_string())?;
    let json = ctrl.to_json();
    let mut ct = vec![0u8; json.len() + 16];
    let mut t = transport.lock().await;
    let n = t
        .write_message(&json, &mut ct)
        .map_err(|e| format!("call-control write_message: {e}"))?;
    ct.truncate(n);
    let wire = Frame::new(ty::CALL_CONTROL, 0, ct).encode();
    write_framed(char, &wire)
        .await
        .map_err(|e| format!("BLE write CALL_CONTROL to AUDIO_SIGNAL: {e}"))?;
    debug!(action = %ctrl.action, "→ BLE call-control push (laptop→phone)");
    Ok(())
}
