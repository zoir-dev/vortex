//! Phone↔laptop clipboard sync + instant-share receive (universal-clipboard
//! style) — split out of `clipboard.rs`. Sends locally-copied text/images to the
//! phone, applies what the phone sends back to the system clipboard + history,
//! and pulls instant-share file/image offers to the user's download folder (with
//! batch consent — see [`downloads_dir`], which is localised, NOT always ~/Downloads). The
//! local-history capture/store stays in `clipboard.rs`; this module calls into
//! it (store_capture/hash_id/now_ms) and the capture loop calls back here
//! (queue_clipboard_for_sync / queue_clipboard_image_for_sync).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};

use sha2::{Digest, Sha256};

use crate::clipboard::{hash_id, now_ms, store_capture};

// --------------------------------------------------------------------------
// Phone sync (P2) — universal-clipboard style, text both ways
// --------------------------------------------------------------------------

/// A registered laptop→phone clipboard writer for the live peer link,
/// capturing its client + transport. Set by the BLE persistent loop on
/// connect, cleared on disconnect (mirrors `NotifWriter`).
pub(crate) type ClipboardWriter = std::sync::Arc<
    dyn Fn(vortex_l3_daemon::core::clipboard_mirror::ClipboardMirror)
            -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// Whether laptop↔phone clipboard sync is on. Default ON (user decision).
/// Local toggle in Settings; checked by both the send and receive paths.
pub(crate) static CLIPBOARD_SYNC: AtomicBool = AtomicBool::new(true);

/// Hash of the last text that crossed the link in EITHER direction — the
/// loop guard. When the watcher re-captures this exact text (e.g. right
/// after we set it from a received sync), it isn't bounced back.
static LAST_SYNC_SIG: Mutex<String> = Mutex::new(String::new());

/// Watcher (blocking thread) → async sender channel. Set once by
/// `spawn_clipboard_sync`; `None` until then (early captures just aren't
/// synced, which is fine — the next copy is).
static CLIPBOARD_SEND_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<String>> =
    std::sync::OnceLock::new();

/// Laptop→phone IMAGE writer (chunked PNG) for the live link, published by
/// the BLE loop on connect, cleared on disconnect.
pub(crate) type ClipboardImageWriter = std::sync::Arc<
    dyn Fn(Vec<u8>) -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

/// Loop guard for IMAGES (hash of the last image that crossed the link).
static LAST_SYNC_IMG: Mutex<String> = Mutex::new(String::new());

/// Watcher → async image-sender channel: (rgba-pixel loop-guard sig, PNG bytes).
static CLIPBOARD_IMG_SEND_TX: std::sync::OnceLock<
    tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
> = std::sync::OnceLock::new();

pub(crate) fn sync_sig(text: &str) -> String {
    hash_id(text.as_bytes())
}

/// Normalize copied text the SAME way on capture and on sync-receive, so the
/// loop-guard signature matches when the watcher reads back text we just set
/// from a received sync. Trims outer blank lines and collapses 3+ blank lines
/// to one. Idempotent: `tidy_text(tidy_text(x)) == tidy_text(x)`.
pub(crate) fn tidy_text(s: &str) -> String {
    let mut clean = s.trim_matches(|c: char| c == '\n' || c == '\r').to_string();
    while clean.contains("\n\n\n") {
        clean = clean.replace("\n\n\n", "\n\n");
    }
    clean
}

/// Canonical signature of a clipboard image, keyed on the RAW RGBA pixels
/// (NOT the PNG bytes). The PNG encoder isn't byte-identical across the phone's
/// original and our re-encode, so a PNG-byte hash made every synced image look
/// "new" and bounce back in a loop. RGBA is the common form the watcher
/// (arboard) and a decoded received PNG both share, so the guard truly matches.
pub(crate) fn img_sig(w: usize, h: usize, rgba: &[u8]) -> String {
    let mut hsh = Sha256::new();
    hsh.update((w as u32).to_le_bytes());
    hsh.update((h as u32).to_le_bytes());
    hsh.update(rgba);
    hex::encode(hsh.finalize())[..16].to_string()
}

/// Decode PNG → (w, h, RGBA8) for the loop-guard signature.
pub(crate) fn decode_png_rgba(png: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Some((w as usize, h as usize, img.into_raw()))
}

/// Called from the watcher on a NEW text capture — queue it for the phone.
/// The async sender applies the toggle, loop guard, and live-link check.
pub(crate) fn queue_clipboard_for_sync(text: &str) {
    if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = CLIPBOARD_SEND_TX.get() {
        let _ = tx.send(text.to_string());
    }
}

/// Called from the watcher on a NEW image capture — queue the PNG for the
/// phone (only if it fits the BLE size cap; larger waits for the LAN path).
pub(crate) fn queue_clipboard_image_for_sync(sig: String, png: &[u8]) {
    if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
        return;
    }
    if png.len() > vortex_l3_daemon::core::clipboard_mirror::MAX_BLE_IMAGE_BYTES {
        return;
    }
    if let Some(tx) = CLIPBOARD_IMG_SEND_TX.get() {
        let _ = tx.send((sig, png.to_vec()));
    }
}

/// The ONE process-lifetime arboard handle used by EVERY path that writes the
/// system clipboard (sync-received text + image, and the popup's pick). A
/// single owner is essential: each arboard handle owns the X11 CLIPBOARD
/// selection independently, so multiple handles race — whichever set LAST wins
/// ownership and the others' content silently vanishes before the user pastes.
/// Kept alive for the app's lifetime so the served selection survives.
static CLIP_SETTER: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Run `f` against the shared persistent clipboard setter, lazily creating it.
pub(crate) fn with_clip_setter<R>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, String>,
) -> Result<R, String> {
    let mut g = CLIP_SETTER.lock().map_err(|_| "clip setter poisoned")?;
    if g.is_none() {
        *g = Some(arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?);
    }
    f(g.as_mut().unwrap())
}

/// Decode PNG bytes → arboard ImageData (RGBA) and serve it on the system
/// clipboard via the shared persistent setter.
pub(crate) fn set_system_image(png: &[u8]) -> Result<(), String> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("decode png: {e}"))?
        .into_rgba8();
    let (w, h) = img.dimensions();
    let data = arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(img.into_raw()),
    };
    with_clip_setter(|cb| cb.set_image(data).map_err(|e| format!("set_image: {e}")))
}

/// Persistent arboard setter for sync-received text — kept alive so the
/// served X selection survives until the next set (a dropped Clipboard
/// would vanish before the user pastes).
pub(crate) fn set_system_text(text: String) -> Result<(), String> {
    with_clip_setter(|cb| cb.set_text(text).map_err(|e| format!("set_text: {e}")))
}

/// Put text on the clipboard for a local reason — i.e. it did NOT come from
/// the phone and must not be sent back there.
///
/// Arms the same loop guard the sync path uses before writing, so the watcher
/// recognises its own capture instead of bouncing the text to the phone, and
/// files it in the clipboard history the way any other copy would be.
pub(crate) fn set_local_text(text: &str) -> Result<(), String> {
    let text = tidy_text(text);
    if text.is_empty() {
        return Ok(());
    }
    if let Ok(mut g) = LAST_SYNC_SIG.lock() {
        *g = sync_sig(&text);
    }
    set_system_text(text.clone())?;
    crate::clipboard::store_capture("text", Some(text), None);
    Ok(())
}

/// Wire up clipboard sync (text + image). Returns the four handles the BLE
/// loop needs: `(text_recv_tx, text_writer, image_recv_tx, image_writer)`.
/// The `*_recv_tx` get incoming frames from the BLE listener; the `*_writer`
/// are published by the BLE loop on connect and called by our send tasks.
#[allow(clippy::type_complexity)]
pub(crate) fn spawn_clipboard_sync(
    app: AppHandle,
) -> (
    tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::clipboard_mirror::ClipboardMirror>,
    std::sync::Arc<tokio::sync::Mutex<Option<ClipboardWriter>>>,
    tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    std::sync::Arc<tokio::sync::Mutex<Option<ClipboardImageWriter>>>,
) {
    use vortex_l3_daemon::core::clipboard_mirror::{ClipboardMirror, ImageAssembler};

    let writer: std::sync::Arc<tokio::sync::Mutex<Option<ClipboardWriter>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let img_writer: std::sync::Arc<tokio::sync::Mutex<Option<ClipboardImageWriter>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));

    // Laptop → phone: drain the watcher's queue and push over the live link.
    let (send_tx, mut send_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let _ = CLIPBOARD_SEND_TX.set(send_tx);
    {
        let writer = writer.clone();
        tokio::spawn(async move {
            while let Some(text) = send_rx.recv().await {
                if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
                    continue;
                }
                let sig = sync_sig(&text);
                // Loop guard: don't bounce back what we just set from a
                // received sync.
                if LAST_SYNC_SIG.lock().map(|g| *g == sig).unwrap_or(false) {
                    continue;
                }
                let Some(w) = writer.lock().await.clone() else {
                    continue; // no live link — drop (ephemeral)
                };
                match w(ClipboardMirror::new(text, now_ms())).await {
                    Ok(()) => {
                        if let Ok(mut g) = LAST_SYNC_SIG.lock() {
                            *g = sig;
                        }
                        tracing::debug!("→ clipboard synced to phone");
                    }
                    Err(e) => tracing::warn!("clipboard sync send failed: {e}"),
                }
            }
        });
    }

    // Phone → laptop: a received CLIPBOARD frame → set our system clipboard
    // + add to history (so phone copies show up in Super+V).
    let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel::<ClipboardMirror>();
    {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(clip) = recv_rx.recv().await {
                if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
                    continue;
                }
                // Tidy with the SAME normalization the watcher applies on
                // re-capture, so the loop-guard signature seeded below matches
                // what the watcher will compute — else whitespace / 3+-newline
                // text bounces straight back to the phone (an echo loop).
                let text = tidy_text(&clip.text);
                if text.is_empty() {
                    continue;
                }
                // Set the loop guard BEFORE setting the clipboard, so the
                // watcher's capture of this text is recognised as already
                // synced and isn't bounced straight back to the phone.
                if let Ok(mut g) = LAST_SYNC_SIG.lock() {
                    *g = sync_sig(&text);
                }
                let t2 = text.clone();
                let _ = tokio::task::spawn_blocking(move || set_system_text(t2)).await;
                store_capture("text", Some(text), None);
                let _ = app.emit("vortex:clipboard", ());
                tracing::info!("clipboard: synced from phone");
            }
        });
    }

    // Laptop → phone IMAGE: drain the watcher's image queue, push chunked.
    let (img_send_tx, mut img_send_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
    let _ = CLIPBOARD_IMG_SEND_TX.set(img_send_tx);
    {
        let img_writer = img_writer.clone();
        tokio::spawn(async move {
            while let Some((sig, png)) = img_send_rx.recv().await {
                if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
                    continue;
                }
                if LAST_SYNC_IMG.lock().map(|g| *g == sig).unwrap_or(false) {
                    continue; // loop guard — just set from a received sync
                }
                let Some(w) = img_writer.lock().await.clone() else {
                    continue; // no live link
                };
                match w(png).await {
                    Ok(()) => {
                        if let Ok(mut g) = LAST_SYNC_IMG.lock() {
                            *g = sig;
                        }
                        tracing::debug!("→ clipboard image synced to phone");
                    }
                    Err(e) => tracing::warn!("clipboard image sync send failed: {e}"),
                }
            }
        });
    }

    // Phone → laptop IMAGE: reassemble CLIPBOARD_IMAGE chunks → set the
    // system clipboard image + add to history.
    let (img_recv_tx, mut img_recv_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u16, u16, Vec<u8>)>();
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut asm = ImageAssembler::default();
            while let Some((total, idx, data)) = img_recv_rx.recv().await {
                if let Some(png) = asm.add(total, idx, data) {
                    apply_synced_image(&app, png).await;
                }
            }
        });
    }

    (recv_tx, writer, img_recv_tx, img_writer)
}

/// Apply a fully-received synced image (from BLE chunks OR the LAN bulk-sync
/// pull): set it on the system clipboard + add it to history. Sets the loop
/// guard first so the watcher's capture of it isn't bounced back.
pub(crate) async fn apply_synced_image(app: &AppHandle, png: Vec<u8>) {
    if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
        return;
    }
    // Seed the loop guard with the RGBA-pixel signature the watcher will
    // compute when it reads this image back — NOT the PNG-byte hash, which
    // differs after arboard's decode + the watcher's re-encode (that mismatch
    // bounced every synced image back to the phone in an echo loop).
    if let Some(sig) = decode_png_rgba(&png).map(|(w, h, rgba)| img_sig(w, h, &rgba)) {
        if let Ok(mut g) = LAST_SYNC_IMG.lock() {
            *g = sig;
        }
    }
    let p2 = png.clone();
    match tokio::task::spawn_blocking(move || set_system_image(&p2)).await {
        Ok(Ok(())) => {
            store_capture("image", None, Some(png));
            let _ = app.emit("vortex:clipboard", ());
            tracing::info!("clipboard: image synced from phone");
        }
        Ok(Err(e)) => tracing::warn!("clipboard image apply failed: {e}"),
        Err(e) => tracing::warn!("clipboard image task join: {e}"),
    }
}

/// Where instant-share received files land: the user's REAL download folder,
/// which is localised — `~/Téléchargements` on a French desktop, `~/Downloads`
/// only on an English one. Hardcoding `~/Downloads` doesn't just miss it, it
/// silently *creates* a second, English-named folder beside the real one and
/// drops every received file where the user never looks. Resolved once per run
/// (neither `$HOME` nor the XDG config changes under us).
pub(crate) fn downloads_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let dir = xdg_download_dir(&home).unwrap_or_else(|| home.join("Downloads"));
        tracing::info!("received files → {}", dir.display());
        Some(dir)
    })
    .clone()
}

/// The download folder's own name ("Téléchargements"), for UI copy — so a
/// "Saved to …" message can never name a folder the file didn't go to.
pub(crate) fn downloads_label() -> String {
    downloads_dir()
        .and_then(|d| {
            d.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| "Downloads".to_string())
}

/// The configured `XDG_DOWNLOAD_DIR`: the environment first, else the
/// `user-dirs.dirs` file that `xdg-user-dir(1)` reads. Not required to exist —
/// a configured-but-missing folder is still the user's stated intent, and
/// `apply_synced_file` creates it; falling back to `~/Downloads` there would
/// reintroduce exactly the bug this avoids.
fn xdg_download_dir(home: &std::path::Path) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        if let Some(p) = expand_home(&v.to_string_lossy(), home) {
            return Some(p);
        }
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    expand_home(&parse_user_dirs(&text, "XDG_DOWNLOAD_DIR")?, home)
}

/// Pull one key out of a `user-dirs.dirs` file. It's shell-syntax:
/// `# comment` lines and `KEY="value"` assignments. Last assignment wins, as
/// a shell sourcing it would give.
fn parse_user_dirs(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        // Strip one layer of matching quotes.
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        if !v.is_empty() {
            found = Some(v.to_string());
        }
    }
    found
}

/// Expand the `$HOME/…` (or `~/…`) prefix the spec mandates for these values.
/// Anything else must already be absolute — a bare relative path is malformed,
/// and guessing at it could scatter files into the process's cwd.
fn expand_home(raw: &str, home: &std::path::Path) -> Option<PathBuf> {
    let raw = raw.trim();
    for prefix in ["$HOME", "${HOME}", "~"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = rest.trim_start_matches('/');
            return Some(if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            });
        }
    }
    let p = PathBuf::from(raw);
    p.is_absolute().then_some(p)
}

/// A non-clobbering path in `dir` for `name`: if it exists, append " (1)",
/// " (2)", … before the extension (same as a browser download).
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = std::path::Path::new(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..10_000 {
        let cand = dir.join(format!("{stem} ({n}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    first // give up after 10k — overwrite
}

/// Apply a fully-received FILE shared from the phone (instant-share style, NOT the
/// clipboard): save it to the user's download folder ([`downloads_dir`]) under its
/// original name and pop a desktop notification. Bytes are never logged; only size + name.
/// Returns the saved path on success (for the transfer panel), `None` on error.
pub(crate) async fn apply_synced_file(
    _app: &AppHandle,
    name: &str,
    _mime: &str,
    bytes: Vec<u8>,
) -> Option<PathBuf> {
    // Sanitise to a single path component (no traversal / separators).
    let safe = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "vortex-file".to_string());
    let Some(dir) = downloads_dir() else {
        tracing::warn!("received file: no HOME — dropped");
        return None;
    };
    let size = bytes.len();
    let safe2 = safe.clone();
    let saved = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&dir)?;
        let path = unique_path(&dir, &safe2);
        std::fs::write(&path, &bytes)?;
        Ok(path)
    })
    .await;
    let path = match saved {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::warn!("received file write failed: {e}");
            return None;
        }
        Err(e) => {
            tracing::warn!("received file task join: {e}");
            return None;
        }
    };
    // No per-file toast: the ongoing transfer notification (see `transfers`)
    // is the single in-place progress indicator.
    tracing::info!(bytes = size, name = %safe, "file received from phone → {}", path.display());
    Some(path)
}

type Offer = vortex_l3_daemon::core::clipboard_mirror::ClipboardImageOffer;

/// Instant-share-style consent + pull for a debounced batch of phone file offers: ask
/// the user once, and only on accept queue them for the LAN pull. Clipboard
/// images never reach here (they sync silently).
async fn flush_file_batch(batch: Vec<Offer>) {
    if batch.is_empty() {
        return;
    }
    let count = batch.len();
    let total: u64 = batch.iter().map(|o| o.bytes).sum();
    let label = if count == 1 {
        batch[0].name.clone()
    } else {
        format!("{count} files")
    };
    let accepted = crate::file_consent::request(&label, count, total).await;
    if !accepted {
        tracing::info!(count, "phone file offer(s) declined on laptop");
        return;
    }
    for offer in &batch {
        // Per-file receive pill entry + pull queue entry.
        let id = crate::transfers::start(&offer.name, offer.bytes);
        if let Some(q) = crate::PENDING_FILE_OFFERS.get() {
            if let Ok(mut g) = q.lock() {
                g.push_back((offer.token.clone(), offer.name.clone(), offer.mime.clone(), id));
            }
        }
    }
    tracing::info!(count, "phone file offer(s) accepted → LAN pull nudged");
    if let Some(nudge) = crate::SYNC_NUDGE.get() {
        nudge.notify_one();
    }
}

/// BLE image-offer consumer: clipboard images stash a pull token immediately;
/// FILE offers are debounced into one batch (so multiple files / a folder share
/// raise a SINGLE consent prompt) and pulled only after the user accepts.
pub(crate) fn spawn_image_offer_consumer() -> tokio::sync::mpsc::UnboundedSender<Offer> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Offer>();
    tokio::spawn(async move {
        let mut file_buf: Vec<Offer> = Vec::new();
        loop {
            // Block when idle; once files are buffered, wait only the debounce
            // window for more before prompting for the whole batch.
            let next = if file_buf.is_empty() {
                rx.recv().await
            } else {
                match tokio::time::timeout(std::time::Duration::from_millis(1200), rx.recv()).await {
                    Ok(v) => v,
                    Err(_) => {
                        flush_file_batch(std::mem::take(&mut file_buf)).await;
                        continue;
                    }
                }
            };
            let offer = match next {
                Some(o) => o,
                None => {
                    flush_file_batch(std::mem::take(&mut file_buf)).await;
                    break;
                }
            };
            if !CLIPBOARD_SYNC.load(Ordering::Relaxed) {
                continue;
            }
            if offer.is_file() {
                file_buf.push(offer);
            } else {
                // Clipboard image → single pending token (overwrites is fine),
                // pulled immediately (no consent — it's clipboard sync).
                if let Some(slot) = crate::PENDING_IMAGE_TOKEN.get() {
                    if let Ok(mut g) = slot.lock() {
                        *g = Some(offer.token.clone());
                    }
                }
                tracing::info!(bytes = offer.bytes, "clipboard image offer → LAN pull nudged");
                if let Some(nudge) = crate::SYNC_NUDGE.get() {
                    nudge.notify_one();
                }
            }
        }
    });
    tx
}

/// Settings toggle: enable/disable laptop↔phone clipboard sync.
#[tauri::command]
pub fn set_clipboard_sync(enabled: bool) {
    CLIPBOARD_SYNC.store(enabled, Ordering::Relaxed);
}

/// Current clipboard-sync on/off state (default on).
#[tauri::command]
pub fn get_clipboard_sync() -> bool {
    CLIPBOARD_SYNC.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real French `user-dirs.dirs`, verbatim shape (comment header, quoted
    /// `$HOME` values) — the case where hardcoding `~/Downloads` lost files.
    const FR: &str = r#"# This file is written by xdg-user-dirs-update
# If you want to change or add directories, just edit the line you're
XDG_DESKTOP_DIR="$HOME/Bureau"
XDG_DOWNLOAD_DIR="$HOME/Téléchargements"
XDG_DOCUMENTS_DIR="$HOME/Documents"
"#;

    #[test]
    fn parses_localised_download_dir() {
        let raw = parse_user_dirs(FR, "XDG_DOWNLOAD_DIR").expect("download dir");
        assert_eq!(raw, "$HOME/Téléchargements");
        assert_eq!(
            expand_home(&raw, std::path::Path::new("/home/cyril")),
            Some(PathBuf::from("/home/cyril/Téléchargements"))
        );
    }

    #[test]
    fn ignores_comments_and_other_keys() {
        assert_eq!(parse_user_dirs(FR, "XDG_MUSIC_DIR"), None);
        // A commented-out assignment must not win.
        let text = "#XDG_DOWNLOAD_DIR=\"$HOME/nope\"\nXDG_DOWNLOAD_DIR=\"$HOME/yes\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_DOWNLOAD_DIR"),
            Some("$HOME/yes".to_string())
        );
    }

    #[test]
    fn last_assignment_wins_like_a_shell() {
        let text = "XDG_DOWNLOAD_DIR=\"$HOME/first\"\nXDG_DOWNLOAD_DIR=\"$HOME/second\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_DOWNLOAD_DIR"),
            Some("$HOME/second".to_string())
        );
    }

    #[test]
    fn expands_home_forms_and_rejects_relative() {
        let home = std::path::Path::new("/home/cyril");
        for raw in ["$HOME/Dl", "${HOME}/Dl", "~/Dl"] {
            assert_eq!(expand_home(raw, home), Some(PathBuf::from("/home/cyril/Dl")));
        }
        // Download dir set to the home directory itself.
        assert_eq!(expand_home("$HOME/", home), Some(home.to_path_buf()));
        // Absolute paths pass through; relative ones are malformed → fall back.
        assert_eq!(expand_home("/data/dl", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(expand_home("Downloads", home), None);
        assert_eq!(expand_home("", home), None);
    }

    /// Unquoted and single-quoted values are valid shell too.
    #[test]
    fn handles_unquoted_and_single_quoted() {
        assert_eq!(
            parse_user_dirs("XDG_DOWNLOAD_DIR=$HOME/Dl\n", "XDG_DOWNLOAD_DIR"),
            Some("$HOME/Dl".to_string())
        );
        assert_eq!(
            parse_user_dirs("XDG_DOWNLOAD_DIR='$HOME/Dl'\n", "XDG_DOWNLOAD_DIR"),
            Some("$HOME/Dl".to_string())
        );
    }
}

