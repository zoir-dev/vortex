//! SMS mirror consumer: reassembles the phone's recent SMS from BLE SMS chunks,
//! caches it to disk, and pushes it to the Vue UI's Messages page. Read-only
//! mirror — routed entirely separately from the audio handoff. Bodies are never
//! logged.

use std::path::PathBuf;

use tauri::{AppHandle, Emitter};

use vortex_l3_daemon::core::sms::{SmsAssembler, SmsMessage};

/// `~/.cache/vortex/sms.json` — survives a daemon restart so the page shows the
/// last-known messages instantly while a fresh sync arrives.
fn cache_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/sms.json");
    Some(p)
}

/// Spawn the SMS consumer; returns the sender the BLE listener feeds
/// `(total, idx, chunk)` into. On a complete list: validate → cache → emit
/// `vortex:sms` to the UI.
/// Validate a complete recent-SMS JSON blob, persist it to the disk cache
/// and push it to the UI. Shared by the BLE chunk consumer and the LAN
/// bulk-sync delivery. Bodies are never logged.
pub(crate) fn deliver(app: &AppHandle, json: &[u8], source: &str) {
    match serde_json::from_slice::<Vec<SmsMessage>>(json) {
        Ok(messages) => {
            tracing::info!(count = messages.len(), source, "← sms assembled");
            let known = get_sms();
            if let Some(p) = cache_path() {
                let _ = vortex_l3_daemon::core::fs_private::write_private(&p, json);
            }
            offer_login_code(&known, &messages);
            let _ = app.emit("vortex:sms", messages);
        }
        Err(e) => tracing::warn!(source, "sms JSON invalid: {e}; dropping"),
    }
}

/// A code older than this is not the one the user is waiting for.
const OTP_FRESH_MS: i64 = 5 * 60 * 1000;

/// If this delivery brought a NEW inbound message carrying a login code, put it
/// on the laptop's clipboard and say so.
///
/// The point is the paste: codes arrive while you are already in the field that
/// wants them, and walking to the phone to read six digits is the single most
/// common reason to pick it up at all.
///
/// Deliberately narrow, because clobbering the clipboard is rude. It fires only
/// for messages absent from the previous cache — so a first sync, a reconnect
/// or a full history import stays silent — and only for codes that arrived in
/// the last few minutes.
fn offer_login_code(known: &[SmsMessage], incoming: &[SmsMessage]) {
    // No prior cache means this is a first sync: everything looks new.
    if known.is_empty() {
        return;
    }
    let seen: std::collections::HashSet<&str> = known.iter().map(|m| m.id.as_str()).collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let Some((msg, code)) = incoming
        .iter()
        // Inbound only, genuinely new, and recent.
        .filter(|m| m.r#type == 1 && !seen.contains(m.id.as_str()))
        .filter(|m| now - m.date < OTP_FRESH_MS)
        .filter_map(|m| vortex_l3_daemon::core::sms::extract_otp(&m.body).map(|c| (m, c)))
        .max_by_key(|(m, _)| m.date)
    else {
        return;
    };

    // The code itself is never logged — it is a credential.
    let sender = msg.address.clone();
    tracing::info!(from = %sender, "sms: login code → clipboard");
    if let Err(e) = crate::clipboard_sync::set_local_secret(&code) {
        tracing::warn!("sms: could not put the login code on the clipboard: {e}");
        return;
    }
    tokio::spawn(async move {
        let Ok(n) = serde_json::from_value::<
            vortex_l3_daemon::core::notif_mirror::NotificationMirror,
        >(serde_json::json!({
            "app": "Vortex",
            // The code itself stays out of the notification. Shell
            // notifications linger in the message tray, so putting it in the
            // title left every OTP readable there long after it expired — and
            // the user does not need to be told the digits, they need to know
            // the paste is ready.
            "title": "Login code copied",
            "text": format!("from {sender} — paste with Ctrl+V"),
        })) else {
            return;
        };
        let _ = vortex_l3_daemon::core::notification_display::show(&n, 0).await;
    });
}

/// Wipe the cached SMS (live + full history + watermark) and blank the
/// Messages page. Called on peer forget so a new peer never sees the previous
/// phone's messages.
pub(crate) fn clear(app: &AppHandle) {
    for p in [cache_path(), history_path(), history_since_path()]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_file(&p);
    }
    let _ = app.emit("vortex:sms", Vec::<SmsMessage>::new());
    let _ = app.emit("vortex:sms-history", Vec::<SmsMessage>::new());
}

/// Sha256-hex of the cached recent-SMS JSON for the LAN bulk-sync hash
/// gate. Empty when no cache exists (the phone then always ships).
pub(crate) fn cache_hash() -> String {
    use sha2::{Digest, Sha256};
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .map(|b| hex::encode(Sha256::digest(&b)))
        .unwrap_or_default()
}

pub(crate) async fn spawn_consumer(
    app: AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16, Vec<u8>)>();
    tokio::spawn(async move {
        let mut asm = SmsAssembler::default();
        while let Some((total, idx, data)) = rx.recv().await {
            let Some(json) = asm.add(total, idx, data) else {
                continue;
            };
            deliver(&app, &json, "BLE");
        }
    });
    tx
}

/// Spawn the on-demand SMS-thread consumer: reassembles a single conversation's
/// page (the Messages page's infinite scroll) and emits `vortex:sms-thread` so
/// the UI MERGES it into the open thread. No disk cache — these pages are
/// ephemeral (re-requested on demand). Returns the sender the BLE listener feeds.
pub(crate) async fn spawn_thread_consumer(
    app: AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16, Vec<u8>)>();
    tokio::spawn(async move {
        let mut asm = SmsAssembler::default();
        while let Some((total, idx, data)) = rx.recv().await {
            let Some(json) = asm.add(total, idx, data) else {
                continue;
            };
            match serde_json::from_slice::<Vec<SmsMessage>>(&json) {
                Ok(messages) => {
                    tracing::info!(count = messages.len(), "← BLE sms-thread page assembled");
                    let _ = app.emit("vortex:sms-thread", messages);
                }
                Err(e) => tracing::warn!("sms-thread JSON invalid: {e}; dropping"),
            }
        }
    });
    tx
}

/// Tauri command: the cached SMS list (so the page is populated instantly on
/// open / after a daemon restart, before the next BLE sync).
#[tauri::command]
pub(crate) fn get_sms() -> Vec<SmsMessage> {
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<SmsMessage>>(&b).ok())
        .unwrap_or_default()
}

// ---- Full-history store (LAN bulk-sync watermark dataset) ----
//
// `sms_history.json` accumulates every message the phone has ever shipped
// (append-only; deletions on the phone are not mirrored). The `.since`
// sidecar holds the newest synced date so each heartbeat asks only for
// what's missing — reading one tiny file instead of parsing the store.

fn history_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/sms_history.json");
    Some(p)
}

fn history_since_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/sms_history.since");
    Some(p)
}

/// The history watermark: newest message date we've synced, 0 = nothing yet
/// (the phone then backfills everything).
pub(crate) fn history_since() -> i64 {
    history_since_path()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Merge a history batch into the store (dedup by id, date-sorted), advance
/// the watermark and push the full list to the UI. Bodies never logged.
pub(crate) fn merge_history(app: &AppHandle, json: &[u8]) {
    let batch: Vec<SmsMessage> = match serde_json::from_slice(json) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("sms-history JSON invalid: {e}; dropping");
            return;
        }
    };
    if batch.is_empty() {
        return;
    }
    let mut store: Vec<SmsMessage> = history_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut by_id: std::collections::HashMap<String, SmsMessage> =
        store.drain(..).map(|m| (m.id.clone(), m)).collect();
    let batch_len = batch.len();
    for m in batch {
        by_id.insert(m.id.clone(), m);
    }
    let mut merged: Vec<SmsMessage> = by_id.into_values().collect();
    merged.sort_by_key(|m| m.date);
    let since = merged.last().map(|m| m.date).unwrap_or(0);
    if let Some(p) = history_path() {
        if let Ok(bytes) = serde_json::to_vec(&merged) {
            let _ = vortex_l3_daemon::core::fs_private::write_private(&p, &bytes);
        }
    }
    if let Some(p) = history_since_path() {
        let _ = vortex_l3_daemon::core::fs_private::write_private(&p, since.to_string().as_bytes());
    }
    tracing::info!(
        batch = batch_len,
        total = merged.len(),
        since,
        "← sms history merged (LAN bulk-sync)"
    );
    let _ = app.emit("vortex:sms-history", merged);
}

/// Canonical id-list JSON of the history store (compact array, ids
/// ascending) — must be byte-identical to what the phone builds from the
/// same ids, since the bulk-sync gate compares sha256 of the two.
fn ids_json() -> Vec<u8> {
    let mut ids: Vec<i64> = get_sms_history()
        .iter()
        .filter_map(|m| m.id.parse::<i64>().ok())
        .collect();
    ids.sort_unstable();
    let strs: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    serde_json::to_vec(&strs).unwrap_or_else(|_| b"[]".to_vec())
}

/// Sha256-hex of the canonical id list, for the bulk-sync request.
pub(crate) fn ids_hash() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(ids_json()))
}

/// The phone's full id list arrived (our hash was stale): prune history
/// entries the phone no longer has — the deletion-reconcile pass.
pub(crate) fn reconcile_ids(app: &AppHandle, json: &[u8]) {
    let ids: Vec<String> = match serde_json::from_slice(json) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("sms-ids JSON invalid: {e}; dropping");
            return;
        }
    };
    let keep: std::collections::HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    let before = get_sms_history();
    let after: Vec<SmsMessage> = before
        .iter()
        .filter(|m| keep.contains(m.id.as_str()))
        .cloned()
        .collect();
    let pruned = before.len() - after.len();
    if pruned == 0 {
        // Hash mismatch but nothing to prune: the phone has ids we lack —
        // those are messages the history watermark hasn't caught up to yet
        // (or pre-store rows); the history dataset handles them.
        return;
    }
    if let Some(p) = history_path() {
        if let Ok(bytes) = serde_json::to_vec(&after) {
            let _ = vortex_l3_daemon::core::fs_private::write_private(&p, &bytes);
        }
    }
    tracing::info!(pruned, total = after.len(), "sms history pruned (phone deletions)");
    let _ = app.emit("vortex:sms-history", after);
}

/// Tauri command: the full synced SMS history (instant, from disk).
#[tauri::command]
pub(crate) fn get_sms_history() -> Vec<SmsMessage> {
    history_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<SmsMessage>>(&b).ok())
        .unwrap_or_default()
}
