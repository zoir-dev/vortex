//! Instant-share-style receive consent for phone→laptop file shares. When the phone
//! offers a file batch (BLE `ClipboardImageOffer` with `is_file()`), the
//! offer-consumer debounces them and calls [`request`], which pops a desktop
//! banner with Accept / Decline (reusing the call-banner D-Bus path) and waits
//! for the user. The laptop only pulls the bytes on accept.
//!
//! A single global [`watch`] task routes `ActionInvoked("fc:accept"/"fc:decline")`
//! signals back to the waiting `request` by notification id. It runs alongside
//! the call module's own action watcher (signals are broadcast; each filters by
//! key prefix), so the two never collide.
//!
//! The same banner + router also carries the AFTER: a screenshot or photo the
//! phone sent by itself lands with a "Copy / Open" notification
//! ([`notify_received`]), whose `fc:copy` / `fc:open` clicks come back through
//! the same [`watch`]. One notification path for phone files, before and after.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;


/// Auto-accept incoming file batches instead of asking. OFF unless the user
/// turns it on: this removes a consent gate, so it can only ever be a
/// deliberate opt-in — never a default, and never inferred from anything else.
static AUTO_ACCEPT: AtomicBool = AtomicBool::new(false);
/// Guards the one-time load of the persisted choice.
static AUTO_ACCEPT_LOADED: OnceLock<()> = OnceLock::new();

/// `~/.local/share/vortex/file_auto_accept` — "1" / "0". Persisted (unlike the
/// clipboard-sync toggle) because a consent setting that silently reverted on
/// restart would leave the user believing files are still gated when they are
/// not, or waiting for a prompt that no longer comes.
fn auto_accept_path() -> Option<PathBuf> {
    // Linux keeps its existing location. Moving it to the seam's `config()`
    // (`~/.config/vortex`) would read as "auto-accept was never enabled" on
    // every machine that already has this set — silently re-gating a user's
    // choice, which is the mirror image of the hazard in the doc comment above.
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/share/vortex/file_auto_accept"))
    }
    // Everywhere else, through the seam. `$HOME` is a Unix variable that Windows
    // does not set, so this resolved to `None` there: the value could not be
    // read and `set_file_auto_accept` failed outright with "no HOME". The toggle
    // was unusable — and it is the only way round a platform whose consent
    // banner cannot be shown at all (see `notify`), so it has to work there
    // most of all.
    #[cfg(not(target_os = "linux"))]
    Some(
        vortex_l3_daemon::core::platform::paths()
            .config()?
            .join("file_auto_accept"),
    )
}

/// The current setting, loading the persisted value on first use. Anything
/// unreadable or unrecognised means OFF — a consent bypass must never be the
/// consequence of a missing or corrupt file.
fn auto_accept() -> bool {
    AUTO_ACCEPT_LOADED.get_or_init(|| {
        let on = auto_accept_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        AUTO_ACCEPT.store(on, Ordering::Relaxed);
        tracing::info!(auto_accept = on, "file receive consent");
    });
    AUTO_ACCEPT.load(Ordering::Relaxed)
}

/// Settings read: whether incoming files are accepted without a prompt.
#[tauri::command]
pub fn get_file_auto_accept() -> bool {
    auto_accept()
}

/// Settings toggle: accept incoming file batches without prompting.
#[tauri::command]
pub fn set_file_auto_accept(enabled: bool) -> Result<(), String> {
    // Run the loader FIRST: if it fired later it would clobber this choice
    // with the on-disk value.
    let _ = auto_accept();
    AUTO_ACCEPT.store(enabled, Ordering::Relaxed);
    let path = auto_accept_path().ok_or("no HOME")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, if enabled { "1" } else { "0" }).map_err(|e| e.to_string())?;
    tracing::info!(enabled, "file auto-accept setting changed");
    Ok(())
}

/// notification id → the waiter to resolve when the user clicks Accept/Decline.
static REGISTRY: OnceLock<Mutex<HashMap<u32, oneshot::Sender<bool>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<u32, oneshot::Sender<bool>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// notification id → the saved capture its Copy / Open buttons act on.
/// Bounded ([`RECEIVED_MAX`]): a notification the user never touches is never
/// reported closed to us, so the oldest entries are dropped rather than kept
/// for the life of the process.
static RECEIVED: Mutex<Vec<(u32, PathBuf)>> = Mutex::new(Vec::new());
const RECEIVED_MAX: usize = 64;

/// A capture the phone sent by itself has landed — say so, with the two
/// things the user is about to do with a screenshot: paste it somewhere, or
/// look at it. Posted through the same banner helper the consent prompt uses
/// (same icon, same action plumbing); the clicks come back via [`watch`].
///
/// Normal urgency: a screenshot arriving is worth a banner, not a persistent
/// alert. The buttons keep it in the notification list until it is dismissed.
pub(crate) async fn notify_received(path: PathBuf, kind: &str) {
    let title = match kind {
        "screenshot" => "Screenshot from your phone",
        "photo" => "Photo from your phone",
        "screen_recording" => "Screen recording from your phone",
        "video" => "Video from your phone",
        _ => "File from your phone",
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let folder = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = format!("{name} · saved to {folder}");
    let actions = vec![
        ("fc:copy".to_string(), "Copy".to_string()),
        ("fc:open".to_string(), "Open".to_string()),
    ];
    match crate::notify::show_banner(title, &body, "vortex", &actions, 0, true).await {
        Ok(id) => {
            if let Ok(mut g) = RECEIVED.lock() {
                g.push((id, path));
                if g.len() > RECEIVED_MAX {
                    let excess = g.len() - RECEIVED_MAX;
                    g.drain(..excess);
                }
            }
        }
        // The file is saved either way; only the shortcut to it is lost.
        Err(e) => tracing::warn!("received-file notification failed ({e}); file is in place"),
    }
}

/// The saved path behind a Copy / Open click, taking it out of the table:
/// the notification is closed once acted on, so a second click cannot come.
fn take_received(id: u32) -> Option<PathBuf> {
    let mut g = RECEIVED.lock().ok()?;
    let pos = g.iter().position(|(i, _)| *i == id)?;
    Some(g.remove(pos).1)
}

/// Act on a received capture's button. Both are best-effort with a log line:
/// the file is already where the notification said.
async fn act_on_received(id: u32, key: &str) {
    let Some(path) = take_received(id) else {
        tracing::info!(id, key, "received-file action for a notification no longer tracked");
        return;
    };
    match key {
        "fc:open" => {
            // Detached, like the handoff module's opener — the desktop's own
            // handler for the type (image viewer), not ours.
            match std::process::Command::new("xdg-open").arg(&path).spawn() {
                Ok(_) => tracing::info!("opened {}", path.display()),
                Err(e) => tracing::warn!("xdg-open {} failed: {e}", path.display()),
            }
        }
        "fc:copy" => {
            let p = path.clone();
            let res = tokio::task::spawn_blocking(move || {
                let bytes = std::fs::read(&p).map_err(|e| format!("read: {e}"))?;
                crate::clipboard_sync::copy_image_file_to_clipboard(&bytes)
            })
            .await;
            match res {
                Ok(Ok(())) => tracing::info!("copied {} to the clipboard", path.display()),
                Ok(Err(e)) => tracing::warn!("copy {} failed: {e}", path.display()),
                Err(e) => tracing::warn!("copy task join: {e}"),
            }
        }
        _ => {}
    }
    let _ = crate::notify::close(id).await;
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KB", n / 1024)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", n as f64 / 1024.0 / 1024.0 / 1024.0)
    }
}

/// Pop the consent banner and await the user's decision (true = accept). Times
/// out to a decline after 45 s. Fails CLOSED (decline) if the banner can't show
/// — consent must never be silently bypassed.
///
/// `kind` is the offer's (`"screenshot"` / `"photo"` / empty) and only changes
/// the wording: a capture the phone sent by itself is still gated here exactly
/// like a share, because "my phone may send its pictures" and "my laptop may
/// receive files unasked" are two decisions on two devices. Auto-accept is the
/// laptop's, and it is the same opt-in for both.
pub(crate) async fn request(label: &str, count: usize, total: u64, kind: &str) -> bool {
    if auto_accept() {
        // No banner at all — the transfer pill (see `transfers`) still reports
        // what arrived and where it was saved, so the receive stays visible.
        tracing::info!(count, bytes = total, "auto-accept on → file batch accepted without asking");
        return true;
    }
    let title = match (kind, count > 1) {
        ("screenshot", false) => "Phone took a screenshot".to_string(),
        ("screenshot", true) => format!("Phone took {count} screenshots"),
        ("photo", false) => "Phone took a photo".to_string(),
        ("photo", true) => format!("Phone took {count} photos"),
        ("screen_recording", false) => "Phone made a screen recording".to_string(),
        ("screen_recording", true) => format!("Phone made {count} screen recordings"),
        ("video", false) => "Phone recorded a video".to_string(),
        ("video", true) => format!("Phone recorded {count} videos"),
        (_, true) => format!("Phone wants to send {count} files"),
        (_, false) => "Phone wants to send a file".to_string(),
    };
    let body = format!("{label} · {}", fmt_bytes(total));
    let actions = vec![
        ("fc:accept".to_string(), "Accept".to_string()),
        ("fc:decline".to_string(), "Decline".to_string()),
    ];
    let id = match crate::notify::show_banner(&title, &body, "vortex", &actions, 0, true).await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("file-consent banner failed ({e}); declining");
            return false;
        }
    };
    let (tx, rx) = oneshot::channel();
    if let Ok(mut g) = registry().lock() {
        g.insert(id, tx);
    }
    let decision = match tokio::time::timeout(Duration::from_secs(45), rx).await {
        Ok(Ok(d)) => d,
        _ => false, // timeout, or sender dropped → decline
    };
    if let Ok(mut g) = registry().lock() {
        g.remove(&id);
    }
    let _ = crate::notify::close(id).await;
    decision
}

/// Spawn-once router: forward `fc:*` ActionInvoked clicks to their waiter.
pub(crate) async fn watch() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
    crate::notify::watch_actions(tx);
    while let Some((id, key)) = rx.recv().await {
        let accept = match key.as_str() {
            "fc:accept" => true,
            "fc:decline" => false,
            "fc:copy" | "fc:open" => {
                // Off the router. `fc:copy` reads the whole file and hands it to
                // the clipboard; awaiting that here meant a consent prompt's
                // Accept sat unanswered behind someone copying a large image,
                // and a consent prompt is the one thing on this channel with a
                // deadline (it declines after 45s).
                tokio::spawn(async move { act_on_received(id, &key).await });
                continue;
            }
            _ => continue, // not ours (call:/act: handled by their own watchers)
        };
        let waiter = registry().lock().ok().and_then(|mut g| g.remove(&id));
        if let Some(sender) = waiter {
            let _ = sender.send(accept);
        }
    }
}
