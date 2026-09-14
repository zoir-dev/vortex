//! Call-log mirror consumer: reassembles the phone's recent call log from BLE
//! CALL_LOG chunks, caches it to disk, and pushes it to the Vue UI's Recents
//! page. Read-only mirror — routed entirely separately from the audio handoff.

use std::path::PathBuf;

use tauri::{AppHandle, Emitter};

use vortex_l3_daemon::core::call_log::{CallLogAssembler, CallLogEntry};

/// `~/.cache/vortex/call_log.json` — survives a daemon restart so the page
/// shows the last-known list instantly while a fresh sync arrives.
fn cache_path() -> Option<PathBuf> {
    crate::peer_cache::peer_file("call_log.json")
}

/// Validate a complete call-log JSON blob, persist it to the disk cache and
/// push it to the UI. Shared by the BLE chunk consumer and the LAN
/// bulk-sync delivery.
pub(crate) fn deliver(app: &AppHandle, json: &[u8], source: &str) {
    match serde_json::from_slice::<Vec<CallLogEntry>>(json) {
        Ok(entries) => {
            tracing::info!(count = entries.len(), source, "← call log assembled");
            if let Some(p) = cache_path() {
                let _ = vortex_l3_daemon::core::fs_private::write_private(&p, json);
            }
            let _ = app.emit("vortex:call_log", entries);
        }
        Err(e) => tracing::warn!(source, "call-log JSON invalid: {e}; dropping"),
    }
}

/// Wipe the cached call log (live + full history + watermark) and blank the
/// Recents page. Called on peer forget so a new peer never sees the previous
/// phone's calls.
pub(crate) fn clear(app: &AppHandle) {
    for p in [cache_path(), history_path(), history_since_path()]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_file(&p);
    }
    let _ = app.emit("vortex:call_log", Vec::<CallLogEntry>::new());
    let _ = app.emit("vortex:call-log-history", Vec::<CallLogEntry>::new());
}

/// Sha256-hex of the cached call-log JSON for the LAN bulk-sync hash gate.
/// Empty when no cache exists (the phone then always ships).
pub(crate) fn cache_hash() -> String {
    use sha2::{Digest, Sha256};
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .map(|b| hex::encode(Sha256::digest(&b)))
        .unwrap_or_default()
}

// ---- Full-history store (LAN bulk-sync watermark dataset) ----
// The twin of sms.rs's history store; see there for the model.

fn history_path() -> Option<PathBuf> {
    crate::peer_cache::peer_file("call_log_history.json")
}

fn history_since_path() -> Option<PathBuf> {
    crate::peer_cache::peer_file("call_log_history.since")
}

/// The history watermark: newest call date we've synced, 0 = nothing yet.
pub(crate) fn history_since() -> i64 {
    history_since_path()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Merge a history batch into the store (dedup by id, date-sorted), advance
/// the watermark and push the full list to the UI.
pub(crate) fn merge_history(app: &AppHandle, json: &[u8]) {
    let batch: Vec<CallLogEntry> = match serde_json::from_slice(json) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("call-log-history JSON invalid: {e}; dropping");
            return;
        }
    };
    if batch.is_empty() {
        return;
    }
    let mut store: Vec<CallLogEntry> = history_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut by_id: std::collections::HashMap<String, CallLogEntry> =
        store.drain(..).map(|e| (e.id.clone(), e)).collect();
    let batch_len = batch.len();
    for e in batch {
        by_id.insert(e.id.clone(), e);
    }
    let mut merged: Vec<CallLogEntry> = by_id.into_values().collect();
    merged.sort_by_key(|e| e.date);
    let since = merged.last().map(|e| e.date).unwrap_or(0);
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
        "← call-log history merged (LAN bulk-sync)"
    );
    let _ = app.emit("vortex:call-log-history", merged);
}

/// Tauri command: the full synced call-log history (instant, from disk).
#[tauri::command]
pub(crate) fn get_call_log_history() -> Vec<CallLogEntry> {
    history_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<CallLogEntry>>(&b).ok())
        .unwrap_or_default()
}

/// Spawn the call-log consumer; returns the sender the BLE listener feeds
/// `(total, idx, chunk)` into. On a complete list: validate → cache → emit
/// `vortex:call_log` to the UI.
pub(crate) async fn spawn_consumer(
    app: AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16, Vec<u8>)>();
    tokio::spawn(async move {
        let mut asm = CallLogAssembler::default();
        while let Some((total, idx, data)) = rx.recv().await {
            let Some(json) = asm.add(total, idx, data) else {
                continue;
            };
            deliver(&app, &json, "BLE");
        }
    });
    tx
}

/// Tauri command: the cached call log (so the page is populated instantly on
/// open / after a daemon restart, before the next BLE sync).
#[tauri::command]
pub(crate) fn get_call_log() -> Vec<CallLogEntry> {
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<CallLogEntry>>(&b).ok())
        .unwrap_or_default()
}
