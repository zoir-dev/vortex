//! Contacts mirror consumer: reassembles the phone's contacts list from BLE
//! CONTACTS chunks, caches it to disk, and pushes it to the Vue UI's Contacts
//! page. Read-only mirror — routed entirely separately from the audio handoff.

use std::path::PathBuf;

use tauri::{AppHandle, Emitter};

use vortex_l3_daemon::core::contacts::{Contact, ContactsAssembler};

/// `~/.cache/vortex/contacts.json` — survives a daemon restart so the page
/// shows the last-known list instantly while a fresh sync arrives.
fn cache_path() -> Option<PathBuf> {
    crate::peer_cache::peer_file("contacts.json")
}

/// Validate a complete contacts JSON blob, persist it to the disk cache and
/// push it to the UI. Shared by the BLE chunk consumer and the LAN
/// bulk-sync delivery (same payload, different transports).
pub(crate) fn deliver(app: &AppHandle, json: &[u8], source: &str) {
    match serde_json::from_slice::<Vec<Contact>>(json) {
        Ok(contacts) => {
            tracing::info!(count = contacts.len(), source, "← contacts assembled");
            if let Some(p) = cache_path() {
                let _ = vortex_l3_daemon::core::fs_private::write_private(&p, json);
            }
            let _ = app.emit("vortex:contacts", contacts);
        }
        Err(e) => tracing::warn!(source, "contacts JSON invalid: {e}; dropping"),
    }
}

/// Sha256-hex of the cached contacts JSON — the laptop's side of the LAN
/// bulk-sync hash gate. Empty string when no cache exists yet (the phone
/// then always ships the full list).
pub(crate) fn cache_hash() -> String {
    use sha2::{Digest, Sha256};
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .map(|b| hex::encode(Sha256::digest(&b)))
        .unwrap_or_default()
}

/// Spawn the contacts consumer; returns the sender the BLE listener feeds
/// `(total, idx, chunk)` into. On a complete list: validate → cache → emit
/// `vortex:contacts` to the UI.
pub(crate) async fn spawn_consumer(
    app: AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16, Vec<u8>)>();
    tokio::spawn(async move {
        let mut asm = ContactsAssembler::default();
        while let Some((total, idx, data)) = rx.recv().await {
            let Some(json) = asm.add(total, idx, data) else {
                continue;
            };
            deliver(&app, &json, "BLE");
        }
    });
    tx
}

/// Wipe the cached contacts and blank the UI page. Called on peer forget so a
/// new (or no) peer never sees the previous phone's contacts.
pub(crate) fn clear(app: &AppHandle) {
    if let Some(p) = cache_path() {
        let _ = std::fs::remove_file(&p);
    }
    let _ = app.emit("vortex:contacts", Vec::<Contact>::new());
}

/// Tauri command: the cached contacts list (so the page is populated instantly
/// on open / after a daemon restart, before the next BLE sync).
#[tauri::command]
pub(crate) fn get_contacts() -> Vec<Contact> {
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Contact>>(&b).ok())
        .unwrap_or_default()
}

/// Resolve a contact's first phone number by exact (case-insensitive) name —
/// used to turn a mirrored WhatsApp notification's title (the sender's display
/// name) into a `wa.me/<number>` deep link. Returns None when the name isn't in
/// the mirrored contacts (then the caller just opens the app generically).
pub(crate) fn lookup_number_by_name(name: &str) -> Option<String> {
    let want = name.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    get_contacts()
        .into_iter()
        .find(|c| c.name.trim().to_lowercase() == want)
        .and_then(|c| c.numbers.into_iter().find(|n| !n.trim().is_empty()))
}
