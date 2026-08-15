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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;

use vortex_l3_daemon::core::notification_display;

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
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/vortex/file_auto_accept"))
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
pub(crate) async fn request(label: &str, count: usize, total: u64) -> bool {
    if auto_accept() {
        // No banner at all — the transfer pill (see `transfers`) still reports
        // what arrived and where it was saved, so the receive stays visible.
        tracing::info!(count, bytes = total, "auto-accept on → file batch accepted without asking");
        return true;
    }
    let title = if count > 1 {
        format!("Phone wants to send {count} files")
    } else {
        "Phone wants to send a file".to_string()
    };
    let body = format!("{label} · {}", fmt_bytes(total));
    let actions = vec![
        ("fc:accept".to_string(), "Accept".to_string()),
        ("fc:decline".to_string(), "Decline".to_string()),
    ];
    let id = match notification_display::show_call_banner(&title, &body, "vortex", &actions, 0, true).await
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
    let _ = notification_display::close(id).await;
    decision
}

/// Spawn-once router: forward `fc:*` ActionInvoked clicks to their waiter.
pub(crate) async fn watch() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
    tokio::spawn(notification_display::watch_actions(tx));
    while let Some((id, key)) = rx.recv().await {
        let accept = match key.as_str() {
            "fc:accept" => true,
            "fc:decline" => false,
            _ => continue, // not ours (call:/act: handled by their own watchers)
        };
        let waiter = registry().lock().ok().and_then(|mut g| g.remove(&id));
        if let Some(sender) = waiter {
            let _ = sender.send(accept);
        }
    }
}
