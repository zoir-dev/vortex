//! Notes + Todos — a unified notes/todos list, bidirectionally synced.
//! Each item is a free-text note OR a checkable todo (with an optional due-date
//! reminder). UNLIKE the contacts/SMS mirrors (phone-authoritative, one-way),
//! BOTH devices create/edit/delete, so the store is a state-based LWW-Element-
//! Set: items carry an `updated_at` clock + a `deleted` tombstone, and `merge`
//! keeps the newest version per id. This file owns the laptop's local store +
//! the Tauri CRUD commands + the `vortex:notes` emit; the sync wire (NOTES_SYNC
//! frame) lands in a follow-up.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// One note or todo. Identical shape on the phone (Kotlin `Note`) so the JSON
/// round-trips across the sync wire untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    /// UUID, minted by whichever device created the item (the Vue/Compose side
    /// generates it — no server, no central id authority).
    pub id: String,
    /// "note" | "todo".
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Todos only — checked off.
    #[serde(default)]
    pub done: bool,
    /// Todos only — epoch ms of the reminder, 0 = none.
    #[serde(default)]
    pub due_at: i64,
    /// LWW clock — epoch ms, bumped on every local change. The merge keeps the
    /// item with the greater `updated_at`.
    pub updated_at: i64,
    /// Tombstone — a deleted item is kept (so the delete propagates) but hidden
    /// from the UI. A delete with a greater `updated_at` beats a concurrent edit.
    #[serde(default)]
    pub deleted: bool,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `~/.cache/vortex/notes.json` — the full item array incl. tombstones (so a
/// delete still propagates after a restart).
fn cache_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/notes.json");
    Some(p)
}

fn load_store() -> Vec<Item> {
    cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Item>>(&b).ok())
        .unwrap_or_default()
}

fn save_store(items: &[Item]) {
    if let (Some(p), Ok(bytes)) = (cache_path(), serde_json::to_vec(items)) {
        let _ = vortex_l3_daemon::core::fs_private::write_private(&p, &bytes);
    }
}

/// In-memory store guarded for concurrent Tauri commands; lazily loaded from
/// disk and written back after each mutation (mirrors `clipboard.rs`).
static NOTES: Mutex<Option<Vec<Item>>> = Mutex::new(None);

fn with_notes<R>(f: impl FnOnce(&mut Vec<Item>) -> R) -> R {
    let mut g = NOTES.lock().unwrap_or_else(|e| e.into_inner());
    let items = g.get_or_insert_with(load_store);
    let r = f(items);
    save_store(items);
    r
}

/// The UI's view: live (non-deleted) items only; tombstones stay in the store
/// for sync but never reach the page.
fn visible(items: &[Item]) -> Vec<Item> {
    items.iter().filter(|i| !i.deleted).cloned().collect()
}

/// Push the current live list to the Vue page.
fn emit(app: &AppHandle, items: &[Item]) {
    let _ = app.emit("vortex:notes", visible(items));
}

/// Wipe the notes store (in-memory + disk) and blank the Notes page. Called on
/// peer forget. Notes are bidirectionally synced, so this drops the laptop's
/// copy of the shared list along with the peer relationship; re-pairing the
/// same phone re-syncs whatever it still holds.
pub(crate) fn clear(app: &AppHandle) {
    {
        let mut g = NOTES.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(Vec::new());
    }
    if let Some(p) = cache_path() {
        let _ = std::fs::remove_file(&p);
    }
    let _ = app.emit("vortex:notes", Vec::<Item>::new());
}

/// Tauri command: the cached live list (page populates instantly on open).
#[tauri::command]
pub(crate) fn get_notes() -> Vec<Item> {
    with_notes(|items| visible(items))
}

/// Tauri command: create or update an item. The frontend supplies the full
/// item (a new one carries a freshly-minted `id`); we stamp `updated_at` and
/// replace any same-id entry. Emits the fresh list.
#[tauri::command]
pub(crate) fn upsert_note(app: AppHandle, mut item: Item) {
    item.updated_at = now_ms();
    item.deleted = false;
    with_notes(|items| {
        match items.iter_mut().find(|i| i.id == item.id) {
            Some(existing) => *existing = item.clone(),
            None => items.push(item.clone()),
        }
        emit(&app, items);
    });
    mark_dirty();
}

/// Tauri command: check/uncheck a todo.
#[tauri::command]
pub(crate) fn toggle_todo(app: AppHandle, id: String, done: bool) {
    with_notes(|items| {
        if let Some(it) = items.iter_mut().find(|i| i.id == id) {
            it.done = done;
            it.updated_at = now_ms();
        }
        emit(&app, items);
    });
    mark_dirty();
}

/// Tauri command: delete an item (tombstone — kept for sync, hidden from UI).
#[tauri::command]
pub(crate) fn delete_note(app: AppHandle, id: String) {
    with_notes(|items| {
        if let Some(it) = items.iter_mut().find(|i| i.id == id) {
            it.deleted = true;
            it.updated_at = now_ms();
        }
        emit(&app, items);
    });
    mark_dirty();
}

// ── Bidirectional sync (NOTES_SYNC frame) ───────────────────────────────────
//
// State-based LWW-Element-Set: each device pushes its FULL item set (incl.
// tombstones) after a local edit; the receiver LWW-merges by id (greater
// `updated_at` wins, a tombstone propagates) and replies with the merged set
// ONLY if it holds items the sender lacked — converges in ≤2 rounds. The
// transport is generic (a `SealedWriter` + a raw-frame channel the BLE loop
// fills); ALL the logic lives in THIS file.

/// `ty::NOTES_SYNC` — mirrored so this module needn't reach the daemon's frame
/// table for one byte.
const NOTES_FRAME: u8 = 0x4D;
/// Chunk payload size — fits one BLE notify after AEAD overhead.
const CHUNK_DATA: usize = 400;

/// Fired to make the dirty-watcher push our set (set in [spawn_sync]).
static NOTES_DIRTY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();
fn mark_dirty() {
    if let Some(n) = NOTES_DIRTY.get() {
        n.notify_one();
    }
}

/// Full set incl. tombstones, read without re-saving.
fn snapshot() -> Vec<Item> {
    NOTES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(load_store).clone()
}

/// LWW union by id: keep the greater `updated_at`. Commutative + idempotent.
fn merge(local: &[Item], remote: &[Item]) -> Vec<Item> {
    use std::collections::HashMap;
    let mut by_id: HashMap<&str, Item> = HashMap::new();
    for it in local.iter().chain(remote.iter()) {
        match by_id.get(it.id.as_str()) {
            Some(cur) if cur.updated_at >= it.updated_at => {}
            _ => {
                by_id.insert(it.id.as_str(), it.clone());
            }
        }
    }
    by_id.into_values().collect()
}

/// Canonical change-signature (sorted `id:updated_at:deleted`) — equality test
/// independent of JSON key / item order.
fn sig(items: &[Item]) -> String {
    let mut v: Vec<String> = items
        .iter()
        .map(|i| format!("{}:{}:{}", i.id, i.updated_at, i.deleted))
        .collect();
    v.sort();
    v.join("|")
}

/// Serialise the full set into `[total u16][idx u16][data]` BLE chunks.
fn build_chunks(items: &[Item]) -> Vec<Vec<u8>> {
    let json = serde_json::to_vec(items).unwrap_or_else(|_| b"[]".to_vec());
    let total = json.len().div_ceil(CHUNK_DATA).max(1) as u16;
    json.chunks(CHUNK_DATA)
        .enumerate()
        .map(|(i, c)| {
            let mut f = Vec::with_capacity(4 + c.len());
            f.extend_from_slice(&total.to_be_bytes());
            f.extend_from_slice(&(i as u16).to_be_bytes());
            f.extend_from_slice(c);
            f
        })
        .collect()
}

fn parse_chunk(p: &[u8]) -> Option<(u16, u16, Vec<u8>)> {
    if p.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([p[0], p[1]]),
        u16::from_be_bytes([p[2], p[3]]),
        p[4..].to_vec(),
    ))
}

/// Reassembles a chunked full item set.
#[derive(Default)]
struct Assembler {
    parts: std::collections::BTreeMap<u16, Vec<u8>>,
    /// Chunk count of the set currently being assembled (0 = none in flight).
    last_total: u16,
}
impl Assembler {
    fn add(&mut self, total: u16, idx: u16, data: Vec<u8>) -> Option<Vec<Item>> {
        if total == 0 || total > 4096 {
            return None;
        }
        // A different chunk count means a DIFFERENT set, and the previous one
        // never completed. Without this the leftovers were mixed into the new
        // set and produced a buffer belonging to neither — so one dropped chunk
        // corrupted every later push until two happened to share a count.
        if total != self.last_total {
            self.parts.clear();
            self.last_total = total;
        }
        // Index 0 arriving again is the same signal at the same size.
        if idx == 0 && !self.parts.is_empty() {
            self.parts.clear();
        }
        self.parts.insert(idx, data);
        if self.parts.len() as u16 != total {
            return None;
        }
        self.last_total = 0;
        let buf: Vec<u8> = std::mem::take(&mut self.parts).into_values().flatten().collect();
        serde_json::from_slice::<Vec<Item>>(&buf).ok()
    }
}

#[cfg(test)]
mod assembler_tests {
    use super::Assembler;

    /// A dropped chunk must not poison the next set.
    #[test]
    fn a_new_set_discards_an_unfinished_one() {
        let mut a = Assembler::default();
        // Set A: three chunks, the middle one never arrives.
        assert!(a.add(3, 0, b"[".to_vec()).is_none());
        assert!(a.add(3, 2, b"]".to_vec()).is_none());
        // Set B: a complete, different-sized set must parse on its own.
        assert!(a.add(1, 0, b"[]".to_vec()).is_some_and(|v| v.is_empty()));
    }

    #[test]
    fn a_restarted_set_of_the_same_size_discards_the_old_parts() {
        let mut a = Assembler::default();
        assert!(a.add(2, 1, b"junk".to_vec()).is_none());
        // Sender restarts at idx 0 with the same chunk count.
        assert!(a.add(2, 0, b"[".to_vec()).is_none());
        assert!(a.add(2, 1, b"]".to_vec()).is_some_and(|v| v.is_empty()));
    }
}

async fn send_full(writer: &Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>>, items: &[Item]) {
    let w = { writer.lock().await.clone() };
    let Some(w) = w else { return };
    for chunk in build_chunks(items) {
        if w(NOTES_FRAME, chunk).await.is_err() {
            break;
        }
    }
}

/// Wire the sync: a dirty-watcher (pushes our set after a local edit) and a
/// raw-frame consumer (merges an inbound set, replies if we hold more).
/// `writer` is the generic sealed-frame writer the BLE loop fills on connect;
/// returns the raw-frame sender the BLE listener forwards NOTES_SYNC into.
pub(crate) fn spawn_sync(
    app: AppHandle,
    writer: Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>>,
) -> tokio::sync::mpsc::UnboundedSender<(u8, Vec<u8>)> {
    let notify = Arc::new(tokio::sync::Notify::new());
    let _ = NOTES_DIRTY.set(notify.clone());

    // Dirty-watcher: push the full set ~300ms after the last local edit.
    {
        let writer = writer.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                send_full(&writer, &snapshot()).await;
            }
        });
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    tokio::spawn(async move {
        let mut asm = Assembler::default();
        while let Some((ty, payload)) = rx.recv().await {
            if ty != NOTES_FRAME {
                continue; // the raw channel is generic — ignore other features
            }
            let Some((total, idx, data)) = parse_chunk(&payload) else {
                continue;
            };
            let Some(remote) = asm.add(total, idx, data) else {
                continue;
            };
            // Atomic LWW merge into the store.
            let (merged, changed) = with_notes(|v| {
                let merged = merge(v, &remote);
                let changed = sig(&merged) != sig(v);
                if changed {
                    *v = merged.clone();
                }
                (merged, changed)
            });
            if changed {
                emit(&app, &merged);
                tracing::info!(
                    count = merged.iter().filter(|i| !i.deleted).count(),
                    "notes: merged peer set"
                );
            }
            // Reply only if WE hold items the peer's set lacked → converges.
            if sig(&merged) != sig(&remote) {
                send_full(&writer, &merged).await;
            }
        }
    });
    tx
}

// ── Due-date reminders (laptop side) ────────────────────────────────────────
//
// Each device independently fires the reminder from the synced `due_at` (so it
// shows on all devices). A 30s poll fires a desktop notification when a
// todo's due time CROSSES while the daemon runs — matching the phone's alarms,
// which only fire FUTURE due times (a past-due item on startup is not re-fired).

/// Spawn the reminder poll (call once at worker start).
pub(crate) fn spawn_reminders() {
    tokio::spawn(async move {
        let mut last_scan = now_ms();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let now = now_ms();
            for it in snapshot() {
                if it.kind == "todo"
                    && !it.done
                    && !it.deleted
                    && it.due_at > last_scan
                    && it.due_at <= now
                {
                    fire_reminder(&it).await;
                }
            }
            last_scan = now;
        }
    });
}

async fn fire_reminder(it: &Item) {
    // Reuse the desktop-notification path. show() stamps a vortex-cache icon, so
    // the notif-capturer's loop-guard skips it — the laptop's own reminder is NOT
    // mirrored to the phone (the phone fires its own from the same due_at).
    let notif = vortex_l3_daemon::core::notif_mirror::NotificationMirror {
        app: "Reminder".to_string(),
        title: if it.title.is_empty() {
            "Todo".to_string()
        } else {
            it.title.clone()
        },
        text: "Reminder".to_string(),
        ..Default::default()
    };
    let _ = vortex_l3_daemon::core::notification_display::show(&notif, 0).await;
    tracing::info!("notes: fired todo reminder");
}
