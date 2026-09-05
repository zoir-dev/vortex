//! Call mirroring (phone → laptop): the continuity-style call banner
//! (ringing → Accept/Decline) and, later, an in-call pill (caller + live
//! duration → Mute/End). The phone forwards `CallEvent`s over BLE (frame
//! `ty::CALL`); the daemon's audio-signal listener decodes them onto the
//! channel this module's consumer drains. Clicking a banner action sends a
//! `CallControl` back to the phone via the per-session [`crate::CallWriter`].
//! Routed entirely separately from the audio-handoff path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tauri::AppHandle;
use tokio::sync::Mutex;

use vortex_l3_daemon::core::call_event::{action, CallControl, CallEvent};
use vortex_l3_daemon::core::live_activity::LiveActivity;
use vortex_l3_daemon::core::notification_display;

/// Call-mirror channel handle, published once so the LAN heartbeat (and the
/// BLE STATE consumer) can feed a peer AppState's `call` into the same call
/// consumer the BLE CALL frame feeds — the additive LAN path that keeps the
/// banner/pill alive when BLE drops mid-call. Read-only after set; absent
/// (None) simply means no call mirror is wired — never an error.
pub(crate) static CALL_MIRROR_TX: std::sync::OnceLock<
    tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::call_event::CallEvent>,
> = std::sync::OnceLock::new();

/// Monotonic nonce for laptop→phone call-control commands, so the phone can
/// dedup the SAME command arriving over both the BLE CALL_CONTROL frame and
/// the AppState `call_control` fallback (each user click takes a fresh value).
pub(crate) static CALL_CONTROL_SEQ: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(1);

/// One-shot laptop→phone call-control command to ride the NEXT outgoing
/// AppState (the BLE-down fallback for the call banner/pill buttons). The
/// AppState builders take() it; set only when the BLE CALL_CONTROL write fails.
pub(crate) static PENDING_CALL_CONTROL: std::sync::Mutex<
    Option<vortex_l3_daemon::core::call_event::CallControl>,
> = std::sync::Mutex::new(None);

/// Laptop→phone call-control writer over the live BLE link (Accept/Decline/
/// End/Mute → CALL_CONTROL frame). Published per session, cleared on
/// disconnect; the call-banner consumer calls it when the user clicks an action.
pub(crate) type CallWriter = Arc<
    dyn Fn(vortex_l3_daemon::core::call_event::CallControl)
            -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// Global handle to the per-session [CallWriter] (the same one the persistent
/// BLE loop fills) so the `dial` Tauri command can place a laptop-initiated
/// call over the BLE fast-path, falling back to the AppState/LAN path.
pub(crate) static CALL_WRITER: std::sync::OnceLock<Arc<tokio::sync::Mutex<Option<CallWriter>>>> =
    std::sync::OnceLock::new();

/// Stable pill key for the in-call live activity (one call at a time).
pub(crate) const CALL_PILL_KEY: &str = "vortex-call";

/// How long without a single frame from the phone before a ringing banner is
/// taken down. Matches the pill sweeper's disconnect threshold — while a call
/// is mirrored the LAN heartbeat is pinned to 2s, so 35s of silence is a real
/// loss of contact, not an inter-beat gap.
const BANNER_STALE_MS: u64 = 35_000;

/// True while a call is mirrored on the laptop (ringing banner or in-call pill
/// up). The LAN heartbeat reads this to poll briskly (≈2 s) while it's set so
/// the call-END — which arrives as `call=None` on the next AppState — clears the
/// pill within a couple seconds even when the BLE fast-path call signal isn't
/// flowing (e.g. the laptop hasn't subscribed to AUDIO_SIGNAL, or the 2.4 GHz
/// radio is saturated by the call audio). Without this, the heartbeat can sit on
/// its idle cadence (up to 240 s when it trusts BLE) and the pill's locally
/// ticking timer keeps counting long after the caller hung up. Reset on end.
pub(crate) static CALL_PILL_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read by the LAN heartbeat's cadence selector (`lan::spawn_heartbeat`): poll
/// fast while a call is on screen so its end is caught promptly.
pub(crate) fn call_pill_active() -> bool {
    CALL_PILL_ACTIVE.load(Ordering::SeqCst)
}

/// Id of the call currently on screen (ringing or active), or "" when none.
/// Mirrors `CallState.call_id` to module level so the voice assistant can
/// accept/decline the active call (`vortex-ui-tauri --call-answer/-decline`)
/// without reaching into the consumer task. Set on phase changes below.
static CURRENT_CALL_ID: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn set_current_call_id(id: &str) {
    if let Ok(mut g) = CURRENT_CALL_ID.lock() {
        g.clear();
        g.push_str(id);
    }
}

/// Shared call-mirror state: the live banner's notification id (0 = none) and
/// the id of the call it belongs to (for routing action clicks back).
#[derive(Default)]
struct CallState {
    banner_id: u32,
    call_id: String,
    /// The incoming-call banner's caller line, kept so a user-dismissed
    /// (silenced) banner can be re-shown quietly with its Accept/Decline.
    ring_title: String,
    ring_body: String,
    ring_app_id: String,
    /// True once the user has dismissed the critical ringing banner (the
    /// "silence" gesture). A second dismissal of the quiet re-show is honoured
    /// (no further re-show), and it gates re-escalation.
    banner_silenced: bool,
    /// Last (id, phase) actually handled — dedups the SAME call event arriving
    /// over both transports (the dedicated BLE CALL frame AND the AppState
    /// `call` field carried on LAN/BLE STATE, re-sent every heartbeat).
    last_handled: (String, String),
    /// The active pill's republish inputs — kept so a re-sent active with
    /// CHANGED audio flags (mute/speaker toggled mid-call) updates the pill
    /// in place WITHOUT restarting the locally-ticking timer.
    pill_started_ms: i64,
    pill_title: String,
    pill_app_id: String,
    pill_text: String,
    pill_muted: bool,
    pill_speaker: bool,
    pill_earbuds: bool,
    /// Whether the pill is already showing the connected timer (vs "Calling…").
    /// Gates the one-shot "Calling… → timer" upgrade when an outgoing call's
    /// dialer-notification chronometer reports the callee answered.
    pill_connected: bool,
}

/// Send a call-control command to the phone over BOTH transports. BLE is the
/// instant fast-path, but a BLE `char.write` returns Ok even when the phone
/// DROPS the frame — under a call's 2.4 GHz contention its stateless-nonce
/// Noise recv-cipher desyncs and silently discards laptop writes (live-observed:
/// "AEAD open failed → recv cipher desynced"), so a BLE "success" is NO proof of
/// delivery. We therefore ALWAYS also ride the next AppState over LAN (reliable
/// TCP, a separate cipher); the phone dedups by `seq`, so the duplicate is
/// harmless and the buttons work even while BLE is desynced mid-call.
async fn send_call_control(
    act: &str,
    call_id: String,
    writer: &Arc<Mutex<Option<crate::CallWriter>>>,
) {
    if call_id.is_empty() {
        tracing::warn!(action = act, "call action but no active call; ignoring");
        return;
    }
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: call_id,
        action: act.to_string(),
        arg: String::new(),
        seq,
    };
    // BLE-FIRST: CALL_CONTROL frame (instant when the link is healthy). A
    // dropped frame is recovered by the phone's nonce-resync, so an Ok write
    // means delivered — LAN stays idle.
    let w = { writer.lock().await.clone() };
    let ble_ok = match w {
        Some(w) => match w(ctrl.clone()).await {
            Ok(()) => {
                tracing::info!(action = act, seq, "→ call-control sent (BLE)");
                true
            }
            Err(e) => {
                tracing::warn!(action = act, "BLE call-control failed: {e}");
                false
            }
        },
        None => false, // BLE link down
    };
    // LAN backstop ONLY when BLE is absent/failed (wedge or link down). The
    // phone dedups by seq, so even if both land it acts once.
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
        tracing::info!(action = act, seq, "→ call-control queued (LAN backstop)");
    }
}

/// Send a MEDIA transport command (the now-playing pill's buttons) to the
/// phone. Same dual-transport semantics as [`send_call_control`], but there
/// is no call to target — `id` stays empty and `arg` carries the player's
/// package name so the phone controls the right session.
async fn send_media_control(
    act: &str,
    pkg: String,
    writer: &Arc<Mutex<Option<crate::CallWriter>>>,
) {
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: String::new(),
        action: act.to_string(),
        arg: pkg,
        seq,
    };
    let w = { writer.lock().await.clone() };
    let ble_ok = match w {
        Some(w) => match w(ctrl.clone()).await {
            Ok(()) => {
                tracing::info!(action = act, seq, "→ media-control sent (BLE)");
                true
            }
            Err(e) => {
                tracing::warn!(action = act, "BLE media-control failed: {e}");
                false
            }
        },
        None => false,
    };
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
        tracing::info!(action = act, seq, "→ media-control queued (LAN backstop)");
    }
}

/// Accept or decline the call currently on screen — shared by the notification
/// buttons and the voice assistant. No-ops (with a log) if no call is active.
async fn control_active_call(act: &str) {
    let id = CURRENT_CALL_ID
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if id.is_empty() {
        tracing::warn!(action = act, "call-control requested but no active call");
        return;
    }
    if let Some(w) = crate::CALL_WRITER.get() {
        send_call_control(act, id, w).await;
    }
}

/// Voice assistant: answer the ringing call (`vortex-ui-tauri --call-answer`).
#[tauri::command]
pub(crate) async fn call_accept() {
    control_active_call(vortex_l3_daemon::core::call_event::action::ACCEPT).await;
}

/// Voice assistant: decline the ringing call (`vortex-ui-tauri --call-decline`).
#[tauri::command]
pub(crate) async fn call_decline() {
    control_active_call(vortex_l3_daemon::core::call_event::action::DECLINE).await;
}

/// Place a laptop-initiated outgoing call: send an `originate_call` CALL_CONTROL
/// to the phone (BLE fast-path, AppState/LAN fallback) carrying the number. The
/// phone dials it and the normal in-call pill appears. The number is never
/// logged.
/// Is this a plain phone number, rather than a control sequence?
///
/// `*` and `#` are not dialling digits on a mobile: they open MMI/USSD codes
/// that the network executes the moment the call is placed, with no dialog and
/// no undo. `*21*<number>#` switches on unconditional call forwarding;
/// `**04*old*new*new#` changes the SIM PIN. Numbers reaching `dial` are not all
/// typed by the user — Recents, "Call back" on a missed call, and the Call
/// button on an SMS thread all pass through whatever the other end supplied,
/// and an SMS sender ID can legitimately be an 11-character alphanumeric
/// string. So the shape of the number is checked here, where it is about to
/// leave the laptop.
///
/// Deliberately permissive about everything harmless: `+`, spaces, dashes,
/// brackets and dots are all normal ways to write a number.
fn is_plain_number(s: &str) -> bool {
    let mut digits = 0usize;
    for c in s.chars() {
        match c {
            '0'..='9' => digits += 1,
            '+' | ' ' | '-' | '(' | ')' | '.' => {}
            _ => return false,
        }
    }
    digits > 0
}

#[tauri::command]
pub(crate) async fn dial(number: String) {
    let number = number.trim().to_string();
    if number.is_empty() {
        tracing::warn!("dial: empty number");
        return;
    }
    if !is_plain_number(&number) {
        // Not logged verbatim: it may be exactly the sort of string worth
        // keeping out of a log.
        tracing::warn!(
            len = number.len(),
            "dial refused: not a plain phone number (MMI/USSD control codes are not dialled)"
        );
        return;
    }
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: String::new(),
        action: vortex_l3_daemon::core::call_event::action::ORIGINATE_CALL.to_string(),
        arg: number,
        seq,
    };
    let w = match crate::CALL_WRITER.get() {
        Some(m) => m.lock().await.clone(),
        None => None,
    };
    let ble_ok = match w {
        Some(w) => match w(ctrl.clone()).await {
            Ok(()) => {
                tracing::info!(seq, "→ dial sent (BLE)");
                true
            }
            Err(e) => {
                tracing::warn!("BLE dial failed: {e}");
                false
            }
        },
        None => false,
    };
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
        tracing::info!(seq, "→ dial queued (AppState/LAN fallback)");
    }
}

/// Send an SMS the laptop composed: a `send_sms` CALL_CONTROL to the phone
/// (BLE fast-path, AppState/LAN fallback) whose arg is `{"to":..,"body":..}`.
/// The phone sends it (or opens the SMS app pre-filled). Body never logged.
#[tauri::command]
pub(crate) async fn send_sms(number: String, body: String) {
    let number = number.trim().to_string();
    if number.is_empty() || body.is_empty() {
        tracing::warn!("send_sms: empty number/body");
        return;
    }
    let arg = serde_json::json!({ "to": number, "body": body }).to_string();
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: String::new(),
        action: vortex_l3_daemon::core::call_event::action::SEND_SMS.to_string(),
        arg,
        seq,
    };
    let w = match crate::CALL_WRITER.get() {
        Some(m) => m.lock().await.clone(),
        None => None,
    };
    let ble_ok = match w {
        Some(w) => match w(ctrl.clone()).await {
            Ok(()) => {
                tracing::info!(seq, "→ send_sms sent (BLE)");
                true
            }
            Err(e) => {
                tracing::warn!("BLE send_sms failed: {e}");
                false
            }
        },
        None => false,
    };
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
        tracing::info!(seq, "→ send_sms queued (AppState/LAN fallback)");
    }
}

/// Ask the phone to mark a conversation read (the laptop opened it). `thread`
/// is the SMS thread id (0 if unknown) and `number` the address fallback.
#[tauri::command]
pub(crate) async fn mark_sms_read(thread: i64, number: String) {
    let arg = serde_json::json!({ "thread": thread, "address": number.trim() }).to_string();
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: String::new(),
        action: vortex_l3_daemon::core::call_event::action::MARK_READ.to_string(),
        arg,
        seq,
    };
    let w = match crate::CALL_WRITER.get() {
        Some(m) => m.lock().await.clone(),
        None => None,
    };
    let ble_ok = match w {
        Some(w) => w(ctrl.clone()).await.is_ok(),
        None => false,
    };
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
    }
    tracing::info!(seq, ble = ble_ok, "→ mark_read sent");
}

/// Ask the phone for one page of a conversation's history (the Messages page's
/// infinite scroll): a `load_thread` CALL_CONTROL whose arg is
/// `{"thread":<id>,"address":"<num>","offset":<n>,"limit":<n>}`. The phone reads
/// that thread (newest-first, skipping `offset`) and replies with SMS_THREAD
/// frames, which the UI merges into the open thread.
#[tauri::command]
pub(crate) async fn load_sms_thread(thread: i64, number: String, offset: i64, limit: i64) {
    let arg = serde_json::json!({
        "thread": thread,
        "address": number.trim(),
        "offset": offset.max(0),
        "limit": limit.clamp(1, 200),
    })
    .to_string();
    let seq = crate::CALL_CONTROL_SEQ.fetch_add(1, Ordering::SeqCst);
    let ctrl = CallControl {
        id: String::new(),
        action: vortex_l3_daemon::core::call_event::action::LOAD_THREAD.to_string(),
        arg,
        seq,
    };
    let w = match crate::CALL_WRITER.get() {
        Some(m) => m.lock().await.clone(),
        None => None,
    };
    let ble_ok = match w {
        Some(w) => w(ctrl.clone()).await.is_ok(),
        None => false,
    };
    if !ble_ok {
        if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
            *g = Some(ctrl);
        }
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }
    }
    tracing::info!(seq, thread, offset, ble = ble_ok, "→ load_thread sent");
}

/// Current wall-clock time in epoch milliseconds.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}


/// Spawn the call-mirror consumer. Returns the sender the BLE listener feeds
/// `CallEvent`s into, plus the call-control writer handle the persistent BLE
/// loop fills per session (so banner clicks can reach the phone).
pub(crate) async fn spawn_consumer(
    _app: AppHandle,
    live_tx: tokio::sync::mpsc::UnboundedSender<LiveActivity>,
    mut call_action_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> (
    tokio::sync::mpsc::UnboundedSender<CallEvent>,
    Arc<Mutex<Option<crate::CallWriter>>>,
) {
    let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel::<CallEvent>();
    let call_writer: Arc<Mutex<Option<crate::CallWriter>>> = Arc::new(Mutex::new(None));
    let state: Arc<Mutex<CallState>> = Arc::new(Mutex::new(CallState::default()));
    // Bumped on every phase change; the in-call timer ticker stops once its
    // captured generation is stale (call answered again / ended).
    let tick_gen: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // ── Banner watchdog ──────────────────────────────────────────────────
    // Close the ringing banner when the phone stops answering.
    //
    // Everything else that clears the banner needs a message FROM the phone —
    // an `active` or an `ended`. So walking out of range while the phone rings,
    // and answering it there, left an urgency-critical "Incoming call" banner on
    // the laptop with live Accept and Decline buttons, for as long as the laptop
    // stayed up. Dismissing it did not help either: that is read as the silence
    // gesture and the banner comes straight back. Pressing Accept on it queued a
    // command that fired on the NEXT call.
    //
    // The pill sweeper next door already treats a stale peer contact as "clear
    // the mirrors"; the banner simply was not wired to it.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                if crate::ble::peer_contact_age_ms() <= BANNER_STALE_MS {
                    continue;
                }
                let id = {
                    let mut s = state.lock().await;
                    let id = s.banner_id;
                    if id != 0 {
                        s.banner_id = 0;
                        s.call_id.clear();
                    }
                    id
                };
                if id != 0 {
                    tracing::info!(
                        "call banner closed: no contact with the phone for over {}s — \
                         its buttons could not have worked",
                        BANNER_STALE_MS / 1000
                    );
                    let _ = notification_display::close(id).await;
                }
            }
        });
    }

    // ── Banner + pill driver: ringing → banner; active → in-call pill with a
    //    ticking timer; ended → clear both. ────────────────────────────────
    {
        let state = state.clone();
        let tick_gen = tick_gen.clone();
        let writer_for_keepalive = call_writer.clone();
        tokio::spawn(async move {
            while let Some(ev) = call_rx.recv().await {
                // A call frame is proof of live phone contact (the LAN heartbeat
                // is pinned to 2s while a call is mirrored) → gates the
                // disconnect-clear of mirror pills.
                crate::ble::touch_peer_contact();
                // Dedup the SAME (id, phase) arriving over both transports (BLE
                // CALL frame + AppState `call` re-sent every heartbeat). First
                // one wins; the rest are no-ops. An empty id (the LAN-synthesized
                // "ended") still passes once, then is recorded.
                {
                    let key = (ev.id.clone(), ev.phase.clone());
                    let mut s = state.lock().await;
                    // A ringing/active call whose pill the staleness sweeper
                    // already reaped (a >90s reconnect) is OFF-SCREEN — the
                    // phone re-asserts the same (id, phase) on reconnect, and we
                    // must REBUILD rather than dedup it away (else the call
                    // vanishes from the laptop for the rest of its duration).
                    // CALL_PILL_ACTIVE is our "is the pill still up?" truth (the
                    // sweeper clears it). The timer re-anchors to the absolute
                    // started_at, so it resumes at the correct elapsed time.
                    let pill_swept = matches!(
                        ev.phase.as_str(),
                        CallEvent::PHASE_RINGING | CallEvent::PHASE_ACTIVE
                    ) && !CALL_PILL_ACTIVE.load(Ordering::SeqCst);
                    if s.last_handled == key && !pill_swept {
                        // Same phase already handled. For an ACTIVE call the pill
                        // may still need an IN-PLACE update: the audio flags
                        // changed (mute/speaker toggled) OR an outgoing call just
                        // connected ("Calling…" → live timer). Anchor the timer
                        // ONCE on the connect transition (not every heartbeat,
                        // which would starve the D-Bus iface).
                        let audio_changed = (s.pill_muted, s.pill_speaker, s.pill_earbuds)
                            != (ev.muted, ev.speaker, ev.has_earbuds);
                        let connect_now = ev.outgoing && ev.connected && !s.pill_connected;
                        if ev.phase == CallEvent::PHASE_ACTIVE && (audio_changed || connect_now) {
                            s.pill_muted = ev.muted;
                            s.pill_speaker = ev.speaker;
                            s.pill_earbuds = ev.has_earbuds;
                            if connect_now {
                                s.pill_connected = true;
                                s.pill_text = "On call".to_string();
                                s.pill_started_ms = if ev.started_at > 0 {
                                    if ev.sent_at > 0 {
                                        ev.started_at + (now_millis() - ev.sent_at)
                                    } else {
                                        ev.started_at
                                    }
                                } else {
                                    now_millis()
                                };
                            }
                            let pill = LiveActivity {
                                key: CALL_PILL_KEY.to_string(),
                                app: "Phone".to_string(),
                                app_id: s.pill_app_id.clone(),
                                title: s.pill_title.clone(),
                                text: s.pill_text.clone(),
                                sub: String::new(),
                                progress: -1,
                                started_at: s.pill_started_ms,
                                muted: ev.muted,
                                speaker: ev.speaker,
                                has_earbuds: ev.has_earbuds,
                                ended: false,
                                playing: None,
                            };
                            drop(s);
                            let _ = live_tx.send(pill);
                            tracing::info!(connect = connect_now, "call pill updated (audio/connect)");
                        }
                        continue;
                    }
                    s.last_handled = key;
                }
                tracing::info!(
                    phase = %ev.phase,
                    named = !ev.name.is_empty(),
                    outgoing = ev.outgoing,
                    "call mirror event",
                );
                // Any phase change invalidates a running in-call ticker.
                tick_gen.fetch_add(1, Ordering::SeqCst);
                // Tell the LAN heartbeat to poll briskly while a call is on
                // screen (so its end clears the pill fast) and to relax once it
                // ends. Nudge on the rising edge so an in-progress idle sleep
                // (up to 240 s) breaks immediately instead of pinning the pill.
                match ev.phase.as_str() {
                    CallEvent::PHASE_RINGING | CallEvent::PHASE_ACTIVE => {
                        set_current_call_id(&ev.id);  // voice accept/decline target
                        if !CALL_PILL_ACTIVE.swap(true, Ordering::SeqCst) {
                            if let Some(n) = crate::SYNC_NUDGE.get() {
                                n.notify_waiters();
                            }
                        }
                    }
                    CallEvent::PHASE_ENDED => {
                        set_current_call_id("");
                        CALL_PILL_ACTIVE.store(false, Ordering::SeqCst);
                    }
                    _ => {}
                }
                match ev.phase.as_str() {
                    CallEvent::PHASE_RINGING if !ev.outgoing => {
                        // Caller line: contact name, else the number, else a
                        // generic label. Number never logged.
                        let title = if !ev.name.is_empty() {
                            ev.name.clone()
                        } else if !ev.number.is_empty() {
                            ev.number.clone()
                        } else {
                            "Unknown caller".to_string()
                        };
                        let body = if !ev.name.is_empty() && !ev.number.is_empty() {
                            format!("Incoming call · {}", ev.number)
                        } else {
                            "Incoming call".to_string()
                        };
                        let actions = vec![
                            ("call:accept".to_string(), "Accept".to_string()),
                            ("call:decline".to_string(), "Decline".to_string()),
                        ];
                        let prev = state.lock().await.banner_id;
                        match notification_display::show_call_banner(
                            &title, &body, &ev.app_id, &actions, prev, true,
                        )
                        .await
                        {
                            Ok(id) => {
                                let mut s = state.lock().await;
                                s.banner_id = id;
                                s.call_id = ev.id.clone();
                                // Stash the caller line + reset the silence flag so a
                                // user-dismissed (silenced) banner can be re-shown.
                                s.ring_title = title.clone();
                                s.ring_body = body.clone();
                                s.ring_app_id = ev.app_id.clone();
                                s.banner_silenced = false;
                                tracing::info!(id, "call banner shown (ringing)");
                            }
                            Err(e) => tracing::warn!("call banner show failed: {e}"),
                        }
                    }
                    // Answered → drop the ringing banner, show the in-call pill.
                    // The visible timer ticks LOCALLY in the GNOME extension
                    // (from started_at) so the daemon needn't republish every
                    // second — that 1s republish used to hold the D-Bus
                    // get_mut and starve the CallAction method (Mute/End clicks
                    // queued until call-end). Here we publish once + a slow
                    // keep-alive that just beats the staleness sweeper.
                    CallEvent::PHASE_ACTIVE => {
                        let id = {
                            let mut s = state.lock().await;
                            s.call_id = ev.id.clone(); // keep for Mute/End routing
                            s.banner_id
                        };
                        if id != 0 {
                            let _ = notification_display::close(id).await;
                            state.lock().await.banner_id = 0;
                        }
                        let caller = if !ev.name.is_empty() {
                            ev.name.clone()
                        } else if !ev.number.is_empty() {
                            ev.number.clone()
                        } else {
                            "Call".to_string()
                        };
                        // Outgoing & NOT yet connected → "Calling…", no timer
                        // (the dial moment isn't the connect moment). Once the
                        // phone's dialer-notification chronometer reports the
                        // callee answered, the phone re-sends with connected=true
                        // + the real connect time, and we fall through to the
                        // timer. Incoming answered calls connect on accept, so
                        // they tick immediately. The timer is ANCHORED TO THE
                        // LAPTOP CLOCK so the duration matches the phone to the
                        // second regardless of clock skew —
                        //   started_laptop = started_phone + (recv_now - sent_at)
                        // (the bracket is skew + one-way latency; ~tens of ms).
                        let (text, started_ms) = if ev.outgoing && !ev.connected {
                            ("Calling…".to_string(), 0i64)
                        } else {
                            let s = if ev.started_at > 0 {
                                if ev.sent_at > 0 {
                                    ev.started_at + (now_millis() - ev.sent_at)
                                } else {
                                    ev.started_at
                                }
                            } else {
                                now_millis()
                            };
                            ("On call".to_string(), s)
                        };
                        let app_id = ev.app_id.clone();
                        let (muted, speaker, has_earbuds) =
                            (ev.muted, ev.speaker, ev.has_earbuds);
                        // Stash the pill's republish inputs so a mid-call
                        // mute/speaker toggle (re-sent active) updates it in
                        // place without restarting the timer (dedup path above).
                        {
                            let mut s = state.lock().await;
                            s.pill_started_ms = started_ms;
                            s.pill_title = caller.clone();
                            s.pill_app_id = app_id.clone();
                            s.pill_text = text.clone();
                            s.pill_muted = muted;
                            s.pill_speaker = speaker;
                            s.pill_earbuds = has_earbuds;
                            // Connected baseline: incoming answers connect at once;
                            // an outgoing call starts "Calling…" (not connected) and
                            // flips when its dialer chronometer reports the answer.
                            s.pill_connected = !ev.outgoing || ev.connected;
                        }
                        let my_gen = tick_gen.load(Ordering::SeqCst);
                        let live_tx = live_tx.clone();
                        let tick_gen2 = tick_gen.clone();
                        let mk_pill = move || LiveActivity {
                            key: CALL_PILL_KEY.to_string(),
                            app: "Phone".to_string(),
                            app_id: app_id.clone(),
                            title: caller.clone(),
                            text: text.clone(),
                            sub: String::new(),
                            progress: -1,
                            started_at: started_ms,
                            muted,
                            speaker,
                            has_earbuds,
                            ended: false,
                            playing: None,
                        };
                        let _ = live_tx.send(mk_pill());
                        // Slow keep-alive: re-publish every 30s so the laptop's
                        // 90s staleness sweeper doesn't drop the pill on a long
                        // call. Far below the 1/sec that starved the D-Bus iface.
                        // SAFETY: stop refreshing once the BLE writer is gone
                        // (link dropped) — the call's `ended` rides BLE too, so
                        // a drop would otherwise strand the pill forever; letting
                        // the sweeper reap it (≤90s) self-heals. On reconnect the
                        // phone's replay=1 re-sends `active` and the pill returns.
                        let writer = writer_for_keepalive.clone();
                        let state_ka = state.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                                if tick_gen2.load(Ordering::SeqCst) != my_gen {
                                    break; // call ended / re-answered → stop
                                }
                                if writer.lock().await.is_none() {
                                    tracing::info!("call pill keep-alive: BLE link gone, letting it expire");
                                    break;
                                }
                                // Rebuild from the CURRENT stashed state so a
                                // mid-call connect ("Calling…" → timer) or
                                // mute/speaker change isn't reverted by a stale
                                // captured snapshot (the bug behind "it jumped
                                // back to Calling…").
                                let pill = {
                                    let s = state_ka.lock().await;
                                    LiveActivity {
                                        key: CALL_PILL_KEY.to_string(),
                                        app: "Phone".to_string(),
                                        app_id: s.pill_app_id.clone(),
                                        title: s.pill_title.clone(),
                                        text: s.pill_text.clone(),
                                        sub: String::new(),
                                        progress: -1,
                                        started_at: s.pill_started_ms,
                                        muted: s.pill_muted,
                                        speaker: s.pill_speaker,
                                        has_earbuds: s.pill_earbuds,
                                        ended: false,
                                        playing: None,
                                    }
                                };
                                let _ = live_tx.send(pill);
                            }
                        });
                        tracing::info!("in-call pill started");
                    }
                    // Ended → close banner + clear the pill.
                    CallEvent::PHASE_ENDED => {
                        let id = {
                            let mut s = state.lock().await;
                            let id = s.banner_id;
                            s.banner_id = 0;
                            s.call_id.clear();
                            id
                        };
                        if id != 0 {
                            let _ = notification_display::close(id).await;
                        }
                        // The banner was still up (never answered) and the
                        // call was incoming → MISSED: leave a notification
                        // with a Call-back action (handled by the action
                        // router's redial branch).
                        if id != 0 && !ev.outgoing && !ev.number.is_empty() {
                            let title = if ev.name.is_empty() {
                                ev.number.clone()
                            } else {
                                ev.name.clone()
                            };
                            let actions =
                                vec![(format!("call:redial:{}", ev.number), "Call back".to_string())];
                            match notification_display::show_call_banner(
                                &title,
                                "Missed call",
                                &ev.app_id,
                                &actions,
                                0,
                                false,
                            )
                            .await
                            {
                                Ok(_) => tracing::info!("missed-call notification shown"),
                                Err(e) => tracing::warn!("missed-call notification failed: {e}"),
                            }
                        }
                        // Clear the in-call pill (the ticker already stopped via
                        // the tick_gen bump at the top of this iteration).
                        let _ = live_tx.send(LiveActivity {
                            key: CALL_PILL_KEY.to_string(),
                            app: String::new(),
                            app_id: String::new(),
                            title: String::new(),
                            text: String::new(),
                            sub: String::new(),
                            progress: -1,
                            started_at: 0,
                            muted: false,
                            speaker: false,
                            has_earbuds: false,
                            ended: true,
                            playing: None,
                        });
                        tracing::info!("call ended; banner + pill cleared");
                    }
                    _ => {}
                }
            }
        });
    }

    // ── Action router: a "call:<verb>" ActionInvoked → CallControl → phone. ─
    // Disjoint from the notification-mirror's "act:<n>" keys, so this watcher
    // and that one ignore each other's signals.
    {
        let state = state.clone();
        let writer = call_writer.clone();
        let (act_tx, mut act_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
        tokio::spawn(notification_display::watch_actions(act_tx));
        tokio::spawn(async move {
            while let Some((_id, key)) = act_rx.recv().await {
                let Some(verb) = key.strip_prefix("call:") else {
                    continue; // a notification-mirror "act:<n>" — not ours
                };
                // Missed-call "Call back": carries its own number, no live
                // call to act on — dial directly and move on.
                if let Some(num) = verb.strip_prefix("redial:") {
                    let n = num.to_string();
                    tokio::spawn(async move {
                        dial(n).await;
                    });
                    continue;
                }
                let act = match verb {
                    "accept" => action::ACCEPT,
                    "decline" => action::DECLINE,
                    "end" => action::END,
                    "mute" => action::MUTE,
                    "unmute" => action::UNMUTE,
                    "speaker_on" => action::SPEAKER_ON,
                    "speaker_off" => action::SPEAKER_OFF,
                    "silence" => action::SILENCE,
                    "sms" => action::SMS_REJECT,
                    other => {
                        tracing::warn!(verb = other, "unknown call action; ignoring");
                        continue;
                    }
                };
                let call_id = {
                    let s = state.lock().await;
                    s.call_id.clone()
                };
                send_call_control(act, call_id, &writer).await;
            }
        });
    }

    // ── Banner dismissed (the "silence" gesture): the user closed the ringing
    //    banner. DON'T drop the call — silence the phone's ringer and re-show
    //    the call quietly (normal urgency) so Accept/Decline stay available in
    //    the notification list, like pressing a phone's side button once. ─────
    {
        let state = state.clone();
        let writer = call_writer.clone();
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, u32)>();
        tokio::spawn(notification_display::watch_closed(closed_tx));
        tokio::spawn(async move {
            while let Some((id, reason)) = closed_rx.recv().await {
                // reason 2 = dismissed by the user; 3 = our own close() on
                // answer/end (ignore that).
                if reason != 2 {
                    continue;
                }
                let (call_id, title, body, app_id) = {
                    let s = state.lock().await;
                    // Only OUR live ringing banner, and only the FIRST dismissal
                    // (a second close of the quiet re-show is honoured: no re-show).
                    if id != s.banner_id
                        || s.banner_id == 0
                        || s.call_id.is_empty()
                        || s.banner_silenced
                    {
                        continue;
                    }
                    (s.call_id.clone(), s.ring_title.clone(), s.ring_body.clone(), s.ring_app_id.clone())
                };
                // 1) Silence the phone's ringer (best-effort; the call stays up).
                send_call_control(action::SILENCE, call_id, &writer).await;
                // 2) Re-show the call QUIETLY so it stays answerable in the tray.
                let actions = vec![
                    ("call:accept".to_string(), "Accept".to_string()),
                    ("call:decline".to_string(), "Decline".to_string()),
                ];
                match notification_display::show_call_banner(&title, &body, &app_id, &actions, 0, false).await {
                    Ok(new_id) => {
                        let mut s = state.lock().await;
                        if !s.call_id.is_empty() {
                            s.banner_id = new_id;
                            s.banner_silenced = true;
                        }
                        tracing::info!(new_id, "ringing banner silenced + re-shown quietly");
                    }
                    Err(e) => tracing::warn!("silenced re-show failed: {e}"),
                }
            }
        });
    }

    // ── Pill-card actions: the GNOME extension's CallAction(verb) for the
    //    in-call pill's Mute/Speaker/End buttons → CallControl → phone. The
    //    now-playing pill's transport buttons ride the same channel with
    //    "media_<act>:<pkg>" verbs (no call to target). ──────────────────────
    {
        let state = state.clone();
        let writer = call_writer.clone();
        tokio::spawn(async move {
            while let Some(verb) = call_action_rx.recv().await {
                if verb.starts_with("media_") {
                    let (act, pkg) = match verb.split_once(':') {
                        Some((a, p)) => (a.to_string(), p.to_string()),
                        None => (verb.clone(), String::new()),
                    };
                    send_media_control(&act, pkg, &writer).await;
                    continue;
                }
                let call_id = {
                    let s = state.lock().await;
                    s.call_id.clone()
                };
                send_call_control(&verb, call_id, &writer).await;
            }
        });
    }

    (call_tx, call_writer)
}

#[cfg(test)]
mod dial_guard_tests {
    use super::is_plain_number;

    #[test]
    fn accepts_real_numbers_and_refuses_control_codes() {
        for ok in ["+998901234567", "0555 123 456", "(212) 555-0100", "555.0100"] {
            assert!(is_plain_number(ok), "should dial: {ok}");
        }
        for bad in [
            "*21*555123#",
            "**04*1111*2222*2222#",
            "#31#0555123456",
            "*#06#",
            "PAYPAL",
        ] {
            assert!(!is_plain_number(bad), "must not dial: {bad}");
        }
    }
}
