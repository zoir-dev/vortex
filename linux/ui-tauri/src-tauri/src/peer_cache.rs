//! Per-peer cache namespacing.
//!
//! Phone-specific caches used to live directly in `~/.cache/vortex/`
//! (`sms.json`, `contacts.json`, `call_log.json`, …). With one trusted phone
//! that was fine; with two it is silent data corruption — each phone's sync
//! overwrites the other's file, so the SMS page shows whichever phone synced
//! last. This module moves them under a per-peer directory:
//!
//! ```text
//! ~/.cache/vortex/peers/<hex(peer_static_pub)[0..16]>/sms.json
//! ```
//!
//! **Why the public key and not the peer's name.** The display name arrives
//! from the peer's APPROVE payload — it is attacker-influenced (which is why
//! `sanitize_peer_name` exists), it can contain path separators, it collides
//! ("Laptop"), and it changes when the user renames the device. A public key
//! is stable, unique, and safe as a path component.
//!
//! Genuinely shared state stays global on purpose: notes/todos are one list
//! across all devices by design, and so is clipboard history.

use std::path::PathBuf;

/// `~/.cache/vortex` — the shared root (notes, clipboard, icons live here).
fn cache_root() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex");
    Some(p)
}

/// Directory for the active peer's caches, created if absent.
///
/// "Active" comes from [`crate::arbiter`] — the single owner of that notion,
/// so the cache paths and the session logic can never disagree about which
/// phone's data is on screen.
///
/// `None` when no peer is active — before the first pairing there is nothing
/// to cache, and every caller already treats `None` as "skip the cache".
pub(crate) fn peer_dir() -> Option<PathBuf> {
    let peer = crate::arbiter::active()?;
    let mut p = cache_root()?;
    p.push("peers");
    p.push(hex::encode(&peer[..8]));
    if let Err(e) = std::fs::create_dir_all(&p) {
        tracing::debug!("peer cache dir {}: {e}", p.display());
        return None;
    }
    // 0700 explicitly, on the peer dir AND the `peers/` parent.
    // `create_dir_all` applies the umask, which on most desktops yields 0755 —
    // and these directories hold SMS bodies and the full contact list. The
    // `~/.cache/vortex` root is already 0700 so nothing was actually exposed,
    // but relying on an ancestor's mode is a fragile way to protect this.
    restrict_to_owner(&p);
    if let Some(parent) = p.parent() {
        restrict_to_owner(parent);
    }
    Some(p)
}

/// Best-effort `chmod 0700`. A failure is not fatal — the 0700 cache root
/// still shields the contents — so we log and carry on.
fn restrict_to_owner(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(md) if md.permissions().mode() & 0o777 != 0o700 => {
            let mut perms = md.permissions();
            perms.set_mode(0o700);
            if let Err(e) = std::fs::set_permissions(dir, perms) {
                tracing::debug!("chmod 0700 {}: {e}", dir.display());
            }
        }
        _ => {}
    }
}

/// Path to `name` inside the active peer's directory, migrating a pre-existing
/// global file on first use.
///
/// The migration is safe precisely because the old layout could only ever hold
/// **one** phone's data: whatever is in the legacy path belongs to the single
/// peer that wrote it, which is the peer we are keying under now. It runs once
/// per file — after the rename the legacy path is gone — and a failed rename
/// just means the cache starts empty and refills on the next sync.
pub(crate) fn peer_file(name: &str) -> Option<PathBuf> {
    let dir = peer_dir()?;
    let new = dir.join(name);
    if !new.exists() {
        if let Some(legacy) = cache_root().map(|r| r.join(name)) {
            if legacy.is_file() {
                match std::fs::rename(&legacy, &new) {
                    Ok(()) => tracing::info!("migrated {} into per-peer cache", name),
                    Err(e) => tracing::debug!("migrate {name}: {e} (starting empty)"),
                }
            }
        }
    }
    Some(new)
}

/// Delete the active peer's whole cache directory. Used by `ForgetPeer` so a
/// forgotten phone leaves no SMS/contacts/call-log behind.
pub(crate) fn remove_peer_dir(peer_pub: &[u8; 32]) {
    let Some(mut p) = cache_root() else { return };
    p.push("peers");
    p.push(hex::encode(&peer_pub[..8]));
    if !p.exists() {
        return;
    }
    match std::fs::remove_dir_all(&p) {
        Ok(()) => tracing::info!("removed per-peer cache for {}", hex::encode(&peer_pub[..4])),
        Err(e) => tracing::warn!("could not remove {}: {e}", p.display()),
    }
}
