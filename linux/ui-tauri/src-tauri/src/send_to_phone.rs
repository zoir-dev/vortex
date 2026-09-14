//! "Open this on my phone" — the mirror of browsing handoff, which until now
//! only ran phone→laptop.
//!
//! You are reading something on the laptop and want it in your hand: send the
//! URL, pick the phone up, tap once. Same request shape as `ring.rs` — a
//! monotonic unix-millis stamp carried in the outgoing AppState, acted on by
//! the phone on the rising edge — because that model has already proven itself
//! against a laptop restart and a lost heartbeat.
//!
//! The phone shows a NOTIFICATION rather than opening the page itself. Android
//! 10 blocks background activity starts, and even if it did not, a phone that
//! opened pages because another device said so is not something to build.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The text to hand over, and when it was queued (for [`PAYLOAD_TTL`]).
static PAYLOAD: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Longest thing worth sending this way. A URL or a short snippet is the point;
/// anything larger belongs in the clipboard sync or a file transfer, both of
/// which handle size properly. Also bounds what a heartbeat has to carry.
const MAX_LEN: usize = 2048;

/// How long the text stays in the outgoing snapshot.
///
/// Like a Share, this is an EVENT carried on a snapshot the laptop re-sends on
/// every heartbeat, and the phone acts on the rising edge of `seq` alone. So
/// once the phone has had several beats to see it there is nothing left to
/// deliver — and up to 2 KB of dead text was riding every BLE beat for the rest
/// of the process, fragmenting an AppState that otherwise fits one notify.
///
/// The seq is deliberately NOT rewound: the phone keeps advancing past it and
/// ignores a snapshot whose text is empty, so clearing costs nothing and a
/// later send still reads as new. Long enough that a phone reachable only over
/// LAN, or one that was briefly out of range, still gets several beats.
const PAYLOAD_TTL: Duration = Duration::from_secs(120);

/// Read by the outgoing AppState builders (BLE + LAN).
///
/// Expiry happens HERE, on read, rather than on a timer: `send` is called from
/// the tray menu and the `--share` entry point as well as a Tauri command, and
/// only some of those run inside a tokio runtime. A lazy check needs no
/// runtime and cannot be missed.
pub fn pending() -> (Option<String>, u64) {
    let mut text = None;
    if let Ok(mut g) = PAYLOAD.lock() {
        match g.as_ref() {
            Some((_, at)) if at.elapsed() >= PAYLOAD_TTL => {
                *g = None;
                tracing::info!("send-to-phone: delivery window closed, snapshot cleared");
            }
            Some((t, _)) => text = Some(t.clone()),
            None => {}
        }
    }
    (text, SEQ.load(Ordering::SeqCst))
}

/// Send `text` to the phone. Called by the tray, the `--share` entry point and
/// the UI.
pub fn send(text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to send".into());
    }
    if text.len() > MAX_LEN {
        return Err(format!("too long ({} bytes, limit {MAX_LEN})", text.len()));
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Never emit a value <= the last: two sends in the same millisecond, or a
    // clock that stepped back, would otherwise look like a replay and the
    // phone would ignore the second one.
    let next = SEQ
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prev| Some(now_ms.max(prev + 1)))
        .map(|prev| now_ms.max(prev + 1))
        .unwrap_or(now_ms);
    if let Ok(mut g) = PAYLOAD.lock() {
        *g = Some((text.to_string(), Instant::now()));
    }
    // Both transports, and now rather than on the next beat: the user is
    // reaching for the phone as they click.
    crate::presence::state_nudge().notify_one();
    if let Some(n) = crate::SYNC_NUDGE.get() {
        n.notify_one();
    }
    tracing::info!(seq = next, len = text.len(), "send-to-phone: queued");
    Ok(())
}

#[tauri::command]
pub(crate) fn send_to_phone(text: String) -> Result<(), String> {
    send(&text)
}
