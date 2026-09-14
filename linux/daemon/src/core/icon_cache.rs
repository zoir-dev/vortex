//! App-icon chunk reassembly + on-disk cache for mirrored notifications.
//!
//! The phone sends each app's icon PNG once, split across `ty::ICON` frames
//! (BLE notifies are small). We reassemble the chunks per package and cache
//! the PNG under `~/.cache/vortex/icons/<pkg>.png`; mirrored notifications
//! then point their `app_icon` at that file so the real app logo shows.

use std::collections::HashMap;
use std::path::PathBuf;

/// Parse an ICON frame plaintext:
/// `[app_id_len u8][app_id][total u16 BE][idx u16 BE][png-chunk bytes]`.
pub fn parse_chunk(plain: &[u8]) -> Option<(String, u16, u16, Vec<u8>)> {
    let id_len = *plain.first()? as usize;
    let mut o = 1;
    if plain.len() < o + id_len + 4 {
        return None;
    }
    let app_id = String::from_utf8(plain[o..o + id_len].to_vec()).ok()?;
    o += id_len;
    let total = u16::from_be_bytes([plain[o], plain[o + 1]]);
    o += 2;
    let idx = u16::from_be_bytes([plain[o], plain[o + 1]]);
    o += 2;
    Some((app_id, total, idx, plain[o..].to_vec()))
}

fn cache_dir() -> Option<PathBuf> {
    // Seam, not `$HOME` — unset on Windows, where no mirrored app icon could
    // ever be cached. Same `~/.cache/vortex/icons` on Linux.
    Some(crate::core::platform::paths().cache()?.join("icons"))
}

/// Keep only safe path chars so a malformed package can't escape the dir.
fn sanitize(app_id: &str) -> String {
    app_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Bundled generic fallback icon (a bell). Written into the cache dir so its
/// path lives under `~/.cache/vortex/` like every real app icon — which keeps
/// the laptop→phone capturer's "skip our own notifications" guard simple
/// (any app_icon path under the vortex cache is ours).
const GENERIC_ICON_PNG: &[u8] = include_bytes!("../assets/generic_icon.png");

/// Ensure the generic fallback icon exists in the cache and return its path.
/// Returns None only if HOME is unset.
pub fn ensure_generic() -> Option<PathBuf> {
    let mut p = cache_dir()?;
    let _ = crate::core::fs_private::create_private_dir(&p);
    p.push("_generic.png");
    if !p.exists() {
        let _ = crate::core::fs_private::write_private(&p, GENERIC_ICON_PNG);
    }
    Some(p)
}

/// The Vortex app logo, used for Vortex's OWN UI (e.g. the file-transfer pill).
/// Written to `cache/vortex.png` so `icon_path("vortex")` resolves to it.
const VORTEX_ICON_PNG: &[u8] = include_bytes!("../assets/vortex_icon.png");

/// Ensure the Vortex logo exists in the cache (at the `app_id = "vortex"` path)
/// and return it. Call once at startup so the transfer pill can resolve it.
pub fn ensure_vortex() -> Option<PathBuf> {
    let mut p = cache_dir()?;
    let _ = crate::core::fs_private::create_private_dir(&p);
    p.push("vortex.png");
    if !p.exists() {
        let _ = crate::core::fs_private::write_private(&p, VORTEX_ICON_PNG);
    }
    Some(p)
}

/// Path where an app's cached icon lives (may or may not exist yet).
pub fn icon_path(app_id: &str) -> Option<PathBuf> {
    if app_id.is_empty() {
        return None;
    }
    let mut p = cache_dir()?;
    p.push(format!("{}.png", sanitize(app_id)));
    Some(p)
}

/// Upper bound on declared chunk counts: icons are ≤ ~90 KB of 180-byte
/// chunks, so a larger declared total is hostile/corrupt and must not drive
/// the buffer allocation.
pub const MAX_CHUNKS: u16 = 512;

/// Cap on concurrently reassembling app ids. The phone interleaves at most a
/// few icons; an unbounded map keyed by attacker-chosen ids would let
/// half-finished buffers pile up forever.
pub const MAX_IN_FLIGHT: usize = 32;

/// Reassembles ICON chunks per app_id; writes the PNG to the cache and returns
/// its path once every chunk has arrived.
#[derive(Default)]
pub struct IconAssembler {
    partial: HashMap<String, (u16, Vec<Option<Vec<u8>>>)>,
}

impl IconAssembler {
    pub fn add(&mut self, app_id: String, total: u16, idx: u16, data: Vec<u8>) -> Option<PathBuf> {
        if total == 0 || total > MAX_CHUNKS || idx >= total {
            return None;
        }
        if self.partial.len() >= MAX_IN_FLIGHT && !self.partial.contains_key(&app_id) {
            return None;
        }
        let entry = self
            .partial
            .entry(app_id.clone())
            .or_insert_with(|| (total, vec![None; total as usize]));
        // A re-send with a different chunk count (new icon) restarts the buffer.
        if entry.0 != total {
            *entry = (total, vec![None; total as usize]);
        }
        entry.1[idx as usize] = Some(data);
        if entry.1.iter().any(|c| c.is_none()) {
            return None;
        }
        let mut bytes = Vec::new();
        for c in &entry.1 {
            bytes.extend_from_slice(c.as_ref().unwrap());
        }
        self.partial.remove(&app_id);
        let path = icon_path(&app_id)?;
        crate::core::fs_private::write_private(&path, &bytes)
            .ok()
            .map(|_| path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_total_above_cap() {
        let mut asm = IconAssembler::default();
        assert!(asm
            .add("com.app".into(), MAX_CHUNKS + 1, 0, b"x".to_vec())
            .is_none());
        assert!(asm.partial.is_empty());
    }

    #[test]
    fn caps_in_flight_app_ids() {
        let mut asm = IconAssembler::default();
        for i in 0..MAX_IN_FLIGHT {
            // total=2 with one chunk → stays partial.
            assert!(asm.add(format!("app{i}"), 2, 0, b"x".to_vec()).is_none());
        }
        assert_eq!(asm.partial.len(), MAX_IN_FLIGHT);
        // A brand-new id is refused while the table is full…
        assert!(asm.add("one-too-many".into(), 2, 0, b"x".to_vec()).is_none());
        assert_eq!(asm.partial.len(), MAX_IN_FLIGHT);
        // …but chunks for an id already in flight still land.
        assert!(asm.partial.contains_key("app0"));
    }
}
