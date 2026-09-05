//! Clipboard HISTORY — P1 of the clipboard feature (see the agreed plan):
//! a Windows-Win+V-style local history of everything copied on this
//! laptop (text AND images — screenshots land as `image/png`), shown in
//! a frameless popup bound to a GNOME custom shortcut (default Super+V).
//!
//! Capture strategy: GNOME has no wlroots data-control protocol, so
//! `wl-paste --watch` is unavailable — we POLL the clipboard through
//! `arboard`, which holds ONE persistent X11 (XWayland) connection that
//! Mutter keeps in sync with the Wayland clipboard. NOT short-lived
//! `wl-paste` subprocesses: each of those creates a transient Wayland
//! surface, which flashed a barely-visible ghost window on every poll
//! (live-hit here AND in the old ecosystem project, which moved to
//! arboard for exactly this reason).
//!
//! Storage: `~/.cache/vortex/clipboard/` (0600/0700) — `index.json`
//! holds the entries (text inline, images as sibling PNG files),
//! newest-first, deduped by content hash, capped by count and total
//! bytes. Pinned entries survive eviction.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

use crate::clipboard_sync::{
    img_sig, queue_clipboard_for_sync, queue_clipboard_image_for_sync, tidy_text,
    with_clip_setter,
};

/// Poll cadence. The list-types probe is a ~5ms subprocess; content is
/// only read when the type set suggests something might have changed.
const POLL_MS: u64 = 700;
/// History caps: entries and total on-disk bytes (images dominate).
const MAX_ENTRIES: usize = 1000;
const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
/// How long an unpinned clip is kept.
///
/// The caps above are about disk, not about time, so without this an entry
/// survived until a thousand newer ones pushed it out — which on normal use is
/// months. People copy passwords, one-time codes, private URLs and card numbers,
/// and the phone's own password-manager flag cannot be read below Android 13, so
/// on this deployment those DO arrive here and get written to disk. Keeping them
/// for months is not a history feature, it is a liability.
///
/// Seven days keeps Super+V genuinely useful — you can still fetch what you
/// copied last week — while making the store forget on its own. Pinning is the
/// deliberate way to keep something longer.
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Per-item guards: a copied novel is truncated, a >20MB image skipped.
/// Matches the sync cap (`clipboard_mirror::MAX_CLIPBOARD_TEXT_CHARS`) so the
/// watcher doesn't truncate text below what the long-text path can carry.
const MAX_TEXT_CHARS: usize = 65_536;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

// --------------------------------------------------------------------------
// Store
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClipEntry {
    /// Content hash (sha256 hex, first 16 chars) — the identity used for
    /// dedup and by every command.
    pub id: String,
    /// "text" | "image"
    pub kind: String,
    /// Inline text (kind == "text").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// PNG filename inside the clipboard dir (kind == "image").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Content size (text bytes / PNG bytes) for the eviction budget.
    pub bytes: u64,
    /// Capture time (unix millis) — display only.
    pub ts_ms: u64,
    /// Pinned entries are never evicted by the caps.
    #[serde(default)]
    pub pinned: bool,
}

fn clip_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/vortex/clipboard"))
}

fn index_path() -> Option<PathBuf> {
    clip_dir().map(|d| d.join("index.json"))
}

fn load_index() -> Vec<ClipEntry> {
    index_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_index(entries: &[ClipEntry]) {
    let Some(dir) = clip_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    // Clipboard content is sensitive (passwords get copied) — keep the
    // whole directory private, same policy as the SMS caches.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    if let Ok(bytes) = serde_json::to_vec(entries) {
        let _ = vortex_l3_daemon::core::fs_private::write_private(&dir.join("index.json"), &bytes);
    }
}

/// In-memory mirror of the index, guarded for the watcher + commands.
static ENTRIES: Mutex<Option<Vec<ClipEntry>>> = Mutex::new(None);

fn with_entries<R>(f: impl FnOnce(&mut Vec<ClipEntry>) -> R) -> R {
    let mut g = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    let entries = g.get_or_insert_with(load_index);
    let r = f(entries);
    save_index(entries);
    r
}

pub(crate) fn hash_id(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())[..16].to_string()
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Insert (or re-surface) a captured entry. Returns true when the list
/// changed in a way the popup should re-render.
pub(crate) fn store_capture(kind: &str, text: Option<String>, png: Option<Vec<u8>>) -> bool {
    let id = match (&text, &png) {
        (Some(t), _) => hash_id(t.as_bytes()),
        (_, Some(p)) => hash_id(p),
        _ => return false,
    };
    with_entries(|entries| {
        // Dedup: same content again just moves to the front (Win+V
        // semantics — re-copying or re-selecting surfaces it).
        if let Some(pos) = entries.iter().position(|e| e.id == id) {
            if pos == 0 {
                return false;
            }
            let e = entries.remove(pos);
            entries.insert(0, e);
            return true;
        }
        let (file, bytes) = match &png {
            Some(p) => {
                let Some(dir) = clip_dir() else { return false };
                let _ = std::fs::create_dir_all(&dir);
                let name = format!("{id}.png");
                if vortex_l3_daemon::core::fs_private::write_private(&dir.join(&name), p).is_err() {
                    return false;
                }
                (Some(name), p.len() as u64)
            }
            None => (None, text.as_deref().map(|t| t.len() as u64).unwrap_or(0)),
        };
        entries.insert(
            0,
            ClipEntry {
                id,
                kind: kind.to_string(),
                text,
                file,
                bytes,
                ts_ms: now_ms(),
                pinned: false,
            },
        );
        evict(entries);
        true
    })
}

/// Enforce the age and count/bytes caps, oldest-unpinned first. Deletes the
/// evicted images' PNG files.
fn evict(entries: &mut Vec<ClipEntry>) {
    let dir = clip_dir();
    // Age first: expiry is about not holding on to sensitive content, so it
    // must not depend on the disk caps ever being reached.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now > 0 {
        let mut expired = Vec::new();
        entries.retain(|e| {
            // A clock that jumped backwards must not wipe the history, so only
            // an entry genuinely older than the window goes.
            let keep = e.pinned || e.ts_ms > now.saturating_sub(MAX_AGE_MS);
            if !keep {
                if let Some(f) = e.file.clone() {
                    expired.push(f);
                }
            }
            keep
        });
        if let Some(d) = &dir {
            for f in expired {
                let _ = std::fs::remove_file(d.join(f));
            }
        }
    }
    loop {
        let total: u64 = entries.iter().map(|e| e.bytes).sum();
        let over = entries.len() > MAX_ENTRIES || total > MAX_TOTAL_BYTES;
        if !over {
            return;
        }
        let Some(pos) = entries.iter().rposition(|e| !e.pinned) else {
            return; // everything pinned — caps yield
        };
        let e = entries.remove(pos);
        if let (Some(d), Some(f)) = (&dir, &e.file) {
            let _ = std::fs::remove_file(d.join(f));
        }
    }
}

// --------------------------------------------------------------------------
// Capture (arboard — one persistent XWayland connection, no ghost windows)
// --------------------------------------------------------------------------

/// Encode an arboard RGBA image as PNG bytes for on-disk storage.
fn rgba_to_png(img: &arboard::ImageData) -> Option<Vec<u8>> {
    let buf: image::RgbaImage = image::RgbaImage::from_raw(
        img.width as u32,
        img.height as u32,
        img.bytes.clone().into_owned(),
    )?;
    let mut out = std::io::Cursor::new(Vec::new());
    buf.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// One poll round against the persistent clipboard handle. Returns true
/// when the history changed. `last_sig` dedups unchanged rounds cheaply
/// (text hash / image dims+raw hash) so a parked screenshot isn't
/// re-encoded every tick. `last_state` logs read-state TRANSITIONS only
/// (text/image/empty/error) so a capture-miss is visible in the log
/// without spamming a line every 700ms tick.
fn poll_once(cb: &mut arboard::Clipboard, last_sig: &mut String, last_state: &mut u8) -> bool {
    // Try text first. Note: a NON-empty text wins; an empty/whitespace
    // text must FALL THROUGH to the image probe (some apps offer an empty
    // text/plain target alongside an image — returning early there dropped
    // the image entirely).
    match cb.get_text() {
        Ok(text) if !text.trim().is_empty() => {
            log_clip_state(last_state, 1, "text");
            // Dedup on the RAW text so identity is exact…
            let sig = format!("t:{}", hash_id(text.as_bytes()));
            if sig == *last_sig {
                return false;
            }
            *last_sig = sig;
            // Concealed-clipboard handling (Linux side): a password-manager copy
            // (KeePassXC/KDE mark it with the `x-kde-passwordManagerHint` target)
            // is neither stored nor synced. Best-effort, fails OPEN.
            if clipboard_is_secret() {
                tracing::info!("clipboard: sensitive (password-manager) — skipped");
                return false;
            }
            // …but store a tidied copy: trim the leading/trailing blank
            // lines a copy often carries, and collapse 3+ consecutive
            // blank lines to one, so a YAML/code block doesn't balloon the
            // card. The meaningful content + internal layout is preserved.
            let mut clean = tidy_text(&text);
            if clean.chars().count() > MAX_TEXT_CHARS {
                clean = clean.chars().take(MAX_TEXT_CHARS).collect();
            }
            tracing::info!(chars = clean.chars().count(), "clipboard: text captured");
            // Sync hook: a NEW text copy → queue it for the phone (the async
            // sender applies the on/off toggle, the loop guard, and the live
            // link). Reached only on an actual change (dedup returned above).
            queue_clipboard_for_sync(&clean);
            return store_capture("text", Some(clean), None);
        }
        Ok(_) => { /* empty text — fall through to the image probe */ }
        Err(arboard::Error::ContentNotAvailable) => { /* no text — maybe image */ }
        Err(e) => {
            // A real read error (timeout / connection): the copy is NOT
            // seen this tick. Log the transition so a miss is diagnosable.
            log_clip_state(last_state, 3, &format!("text read error: {e}"));
        }
    }
    match cb.get_image() {
        Ok(img) => {
            log_clip_state(last_state, 2, "image");
            let sig = format!("i:{}x{}:{}", img.width, img.height, hash_id(&img.bytes));
            if sig == *last_sig {
                return false;
            }
            *last_sig = sig;
            // Loop-guard signature keyed on the RGBA pixels (see img_sig),
            // computed BEFORE re-encoding so it matches the one apply_synced_image
            // seeds from a received image's decoded pixels — no echo bounce.
            let sync_sig = img_sig(img.width, img.height, &img.bytes);
            let Some(png) = rgba_to_png(&img) else {
                return false;
            };
            if png.len() > MAX_IMAGE_BYTES {
                tracing::warn!(bytes = png.len(), "clipboard: image too large, dropped");
                return false;
            }
            tracing::info!(w = img.width, h = img.height, "clipboard: image captured");
            // Sync hook: a new image copy → queue it for the phone (capped /
            // loop-guarded by the async sender). Reached only on a change.
            queue_clipboard_image_for_sync(sync_sig, &png);
            return store_capture("image", None, Some(png));
        }
        Err(arboard::Error::ContentNotAvailable) => log_clip_state(last_state, 0, "empty"),
        Err(e) => log_clip_state(last_state, 3, &format!("image read error: {e}")),
    }
    false
}

/// Best-effort "is the current CLIPBOARD a password-manager secret?" probe
/// (concealed-clipboard handling, Linux side). KeePassXC / KDE apps advertise a
/// `x-kde-passwordManagerHint` selection target (value "secret") on a copied
/// password. We ask the CLIPBOARD owner to convert to that target: if it's
/// offered, the copy is secret → don't store or sync it.
///
/// Fails OPEN on ANY error — a probe failure must never silently halt normal
/// clipboard sync.
///
/// It has to ask about the SAME selection arboard reads, or it answers about
/// text that was never captured. Under Wayland those are two different
/// selections: arboard (with `wayland-data-control`) reads the Wayland one,
/// while the X11 probe below sees whatever XWayland last got handed — which on
/// Plasma is frequently minutes stale. An X11-only probe would then report "not
/// secret" for a password copy it simply cannot see, and the password would sync
/// to the phone. So Wayland is asked first, on Wayland's own terms.

/// The Wayland arm: list the MIME types the selection offers and look for the
/// hint among them. Cheaper than the X11 round trip — no conversion request,
/// the compositor already knows the offer's type list.
///
/// `None` means "could not ask" (no Wayland display, no data-control protocol),
/// which hands the question to the X11 probe rather than answering it wrongly.
fn wayland_clipboard_is_secret() -> Option<bool> {
    use wl_clipboard_rs::paste::{get_mime_types, ClipboardType, Seat};

    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
        Ok(types) => Some(types.iter().any(|t| t == "x-kde-passwordManagerHint")),
        // An empty clipboard is a definite "not secret", not a failure to ask.
        Err(wl_clipboard_rs::paste::Error::ClipboardEmpty) => Some(false),
        Err(_) => None,
    }
}

fn clipboard_is_secret() -> bool {
    if let Some(secret) = wayland_clipboard_is_secret() {
        return secret;
    }
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt, CreateWindowAux, WindowClass,
    };
    use x11rb::protocol::Event;

    struct Probe {
        conn: x11rb::rust_connection::RustConnection,
        win: u32,
        clipboard: u32,
        hint: u32,
        prop: u32,
    }

    fn build() -> Option<Probe> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        let win = conn.generate_id().ok()?;
        conn.create_window(
            0, win, root, 0, 0, 1, 1, 0,
            WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new(),
        )
        .ok()?
        .check()
        .ok()?;
        let clipboard = conn.intern_atom(false, b"CLIPBOARD").ok()?.reply().ok()?.atom;
        let hint = conn
            .intern_atom(false, b"x-kde-passwordManagerHint")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let prop = conn
            .intern_atom(false, b"VORTEX_CLIP_SECRET")
            .ok()?
            .reply()
            .ok()?
            .atom;
        Some(Probe { conn, win, clipboard, hint, prop })
    }

    fn query(p: &Probe) -> Option<bool> {
        p.conn
            .convert_selection(p.win, p.clipboard, p.hint, p.prop, 0u32)
            .ok()?;
        p.conn.flush().ok()?;
        // Bounded wait for the SelectionNotify (never block the watcher).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(60);
        loop {
            match p.conn.poll_for_event().ok()? {
                Some(Event::SelectionNotify(ev)) => {
                    // property == NONE (0) → the owner didn't offer the target.
                    if ev.property == 0 {
                        return Some(false);
                    }
                    let r = p
                        .conn
                        .get_property(false, p.win, p.prop, AtomEnum::ANY, 0, 64)
                        .ok()?
                        .reply()
                        .ok()?;
                    // ONLY a real password manager (KeePassXC/KDE) sets this hint
                    // target's VALUE to "secret". Permissive owners (xclip, the
                    // GNOME clipboard bridge) answer an unknown target by echoing
                    // the clipboard TEXT — so a non-empty property is NOT enough;
                    // we must see the literal "secret" or we'd false-flag normal
                    // copies and silently break text sync.
                    let val = String::from_utf8_lossy(&r.value);
                    let secret = val.trim().eq_ignore_ascii_case("secret");
                    let _ = p.conn.delete_property(p.win, p.prop);
                    let _ = p.conn.flush();
                    return Some(secret);
                }
                Some(_) => continue,
                None => {
                    if std::time::Instant::now() >= deadline {
                        return Some(false);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }

    // Outer Option = "init attempted?"; inner = Some(probe) / None(init failed).
    static PROBE: Mutex<Option<Option<Probe>>> = Mutex::new(None);
    let mut g = match PROBE.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if g.is_none() {
        *g = Some(build());
    }
    match g.as_ref().and_then(|o| o.as_ref()) {
        Some(p) => query(p).unwrap_or(false),
        None => false, // init failed earlier — fail open
    }
}

/// Log a clipboard read-state change once, on transition (not every tick).
fn log_clip_state(last_state: &mut u8, state: u8, label: &str) {
    if *last_state != state {
        *last_state = state;
        if state == 3 {
            tracing::warn!("clipboard read: {label}");
        } else {
            tracing::debug!("clipboard read: {label}");
        }
    }
}

/// One-shot capture of whatever is on the clipboard RIGHT NOW, using a
/// FRESH arboard connection. The long-lived watcher's X connection can go
/// stale (it stops seeing new selection owners after the clipboard churns,
/// so a just-copied item only appeared after an app restart). The popup
/// calls this every time it opens so the current clipboard is always in the
/// list — independent of the watcher's connection health. A fresh `last_sig`
/// means it always tries; `store_capture` dedups by content (a re-copy just
/// surfaces to the front, never a duplicate).
#[tauri::command]
pub async fn clipboard_capture_now(app: AppHandle) {
    let changed = tokio::task::spawn_blocking(|| {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("clipboard capture-now: arboard unavailable: {e}");
                return false;
            }
        };
        let mut sig = String::new();
        let mut state = 255u8;
        poll_once(&mut cb, &mut sig, &mut state)
    })
    .await
    .unwrap_or(false);
    if changed {
        let _ = app.emit("vortex:clipboard", ());
    }
}

/// Spawn the history watcher (plain thread — arboard is blocking and the
/// emit handle is Send). Emits `vortex:clipboard` whenever the history
/// changes so an open popup re-renders live.
pub(crate) fn spawn_clipboard_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("clipboard watcher unavailable: {e}");
                return;
            }
        };
        let mut last_sig = String::new();
        let mut last_state = 255u8;
        loop {
            if poll_once(&mut cb, &mut last_sig, &mut last_state) {
                let _ = app.emit("vortex:clipboard", ());
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    });
}

// --------------------------------------------------------------------------
// Tauri commands
// --------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct ClipEntryDto {
    id: String,
    kind: String,
    /// Truncated preview for the list (full text is applied on select).
    text: Option<String>,
    /// Absolute PNG path for `convertFileSrc`.
    path: Option<String>,
    bytes: u64,
    ts_ms: u64,
    pinned: bool,
}

#[tauri::command]
pub fn clipboard_history() -> Vec<ClipEntryDto> {
    let dir = clip_dir();
    with_entries(|entries| {
        entries
            .iter()
            .map(|e| ClipEntryDto {
                id: e.id.clone(),
                kind: e.kind.clone(),
                text: e.text.as_ref().map(|t| t.chars().take(400).collect()),
                path: match (&dir, &e.file) {
                    (Some(d), Some(f)) => Some(d.join(f).to_string_lossy().into_owned()),
                    _ => None,
                },
                bytes: e.bytes,
                ts_ms: e.ts_ms,
                pinned: e.pinned,
            })
            .collect()
    })
}

/// Put an entry back into the system clipboard. arboard again — the
/// process is long-lived, so the served selection stays alive (and
/// Mutter mirrors it to the Wayland side).
#[tauri::command]
pub async fn clipboard_select(app: AppHandle, id: String) -> Result<(), String> {
    enum Payload {
        Text(String),
        Image(PathBuf),
    }
    let payload = with_entries(|entries| {
        entries.iter().find(|e| e.id == id).map(|e| {
            if let Some(t) = &e.text {
                Some(Payload::Text(t.clone()))
            } else {
                match (clip_dir(), &e.file) {
                    (Some(d), Some(f)) => Some(Payload::Image(d.join(f))),
                    _ => None,
                }
            }
        })
    })
    .flatten()
    .ok_or("entry not found")?;

    tokio::task::spawn_blocking(move || -> Result<(), String> {
        // Use the ONE shared setter (see CLIP_SETTER) — a separate handle here
        // would race the sync setters for X11 CLIPBOARD ownership, so a pick
        // could silently lose to (or steal from) an in-flight synced item.
        with_clip_setter(|cb| match payload {
            Payload::Text(t) => cb.set_text(t).map_err(|e| format!("set_text: {e}")),
            Payload::Image(path) => {
                let png = std::fs::read(&path).map_err(|e| format!("read png: {e}"))?;
                let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                    .map_err(|e| format!("decode png: {e}"))?
                    .into_rgba8();
                let (w, h) = img.dimensions();
                cb.set_image(arboard::ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: std::borrow::Cow::Owned(img.into_raw()),
                })
                .map_err(|e| format!("set_image: {e}"))
            }
        })
    })
    .await
    .map_err(|e| format!("join: {e}"))??;

    // Surface the just-picked entry to the front ourselves and tell the UI,
    // instead of waiting for the watcher to observe our own write — two
    // arboard handles in one process race on X selection ownership, so the
    // re-capture path isn't reliable (the entry would appear to stay put).
    with_entries(|entries| {
        if let Some(pos) = entries.iter().position(|e| e.id == id) {
            if pos != 0 {
                let e = entries.remove(pos);
                entries.insert(0, e);
            }
        }
    });
    let _ = app.emit("vortex:clipboard", ());
    Ok(())
}

#[tauri::command]
pub fn clipboard_pin(id: String, pinned: bool) {
    with_entries(|entries| {
        if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
            e.pinned = pinned;
        }
    });
}

#[tauri::command]
pub fn clipboard_delete(id: String) {
    with_entries(|entries| {
        if let Some(pos) = entries.iter().position(|e| e.id == id) {
            let e = entries.remove(pos);
            if let (Some(d), Some(f)) = (clip_dir(), &e.file) {
                let _ = std::fs::remove_file(d.join(f));
            }
        }
    });
}

/// Full entry by id for the preview pane — like `clipboard_history` but a
/// single item with the text UNtruncated (the list truncates to 400 chars
/// for speed; the preview shows everything).
#[tauri::command]
pub fn clipboard_get(id: String) -> Option<ClipEntryDto> {
    let dir = clip_dir();
    with_entries(|entries| {
        entries.iter().find(|e| e.id == id).map(|e| ClipEntryDto {
            id: e.id.clone(),
            kind: e.kind.clone(),
            text: e.text.clone(),
            path: match (&dir, &e.file) {
                (Some(d), Some(f)) => Some(d.join(f).to_string_lossy().into_owned()),
                _ => None,
            },
            bytes: e.bytes,
            ts_ms: e.ts_ms,
            pinned: e.pinned,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms_test() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Fresh by default: these fixtures exercise the SIZE caps, so they must
    /// not also be expired by the age cap (`ts_ms: 0` meant 1970).
    fn text_entry(id: &str, bytes: u64, pinned: bool) -> ClipEntry {
        text_entry_at(id, bytes, pinned, now_ms_test())
    }

    fn text_entry_at(id: &str, bytes: u64, pinned: bool, ts_ms: u64) -> ClipEntry {
        ClipEntry {
            id: id.into(),
            kind: "text".into(),
            text: Some("x".into()),
            file: None,
            bytes,
            ts_ms,
            pinned,
        }
    }

    #[test]
    fn evict_drops_entries_past_the_age_cap() {
        let now = now_ms_test();
        let mut v = vec![
            text_entry_at("fresh", 1, false, now),
            text_entry_at("old", 1, false, now - MAX_AGE_MS - 1),
            text_entry_at("old_but_pinned", 1, true, now - MAX_AGE_MS - 1),
        ];
        evict(&mut v);
        let ids: Vec<&str> = v.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"fresh"), "a fresh clip must survive");
        assert!(
            ids.contains(&"old_but_pinned"),
            "pinning is the way to keep something past the window"
        );
        assert!(
            !ids.contains(&"old"),
            "an unpinned clip older than the window must be forgotten \
             even though the size caps are nowhere near"
        );
    }

    #[test]
    fn evict_drops_oldest_unpinned_beyond_count() {
        let n = MAX_ENTRIES + 10;
        let mut v: Vec<ClipEntry> =
            (0..n).map(|i| text_entry(&format!("e{i}"), 1, false)).collect();
        evict(&mut v);
        assert_eq!(v.len(), MAX_ENTRIES);
        // Newest (front) survive; the tail got dropped.
        assert_eq!(v[0].id, "e0");
        assert_eq!(v.last().unwrap().id, format!("e{}", MAX_ENTRIES - 1));
    }

    #[test]
    fn evict_skips_pinned() {
        let n = MAX_ENTRIES + 10;
        let mut v: Vec<ClipEntry> =
            (0..n).map(|i| text_entry(&format!("e{i}"), 1, i == n - 1)).collect();
        evict(&mut v);
        assert!(
            v.iter().any(|e| e.id == format!("e{}", n - 1)),
            "pinned tail entry must survive"
        );
        assert_eq!(v.len(), MAX_ENTRIES);
    }

    #[test]
    fn evict_respects_byte_budget() {
        let mut v: Vec<ClipEntry> = (0..10)
            .map(|i| text_entry(&format!("e{i}"), MAX_TOTAL_BYTES / 4, false))
            .collect();
        evict(&mut v);
        let total: u64 = v.iter().map(|e| e.bytes).sum();
        assert!(total <= MAX_TOTAL_BYTES);
        assert_eq!(v[0].id, "e0");
    }
}
