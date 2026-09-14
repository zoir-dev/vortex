//! "Share via Vortex" in Windows Explorer — the counterpart of the Nautilus
//! extension and the Dolphin ServiceMenu that `install_linux.sh` writes.
//!
//! A classic shell verb under `HKEY_CURRENT_USER`, registered by the app on
//! every start. Deliberately not an installer step, for the same reason the
//! toast AUMID shortcut is not: a standalone .exe that someone unzipped should
//! work, and this needs no admin rights and no packaging.
//!
//! Rewritten on every run rather than only when absent. The command has the
//! exe's full path baked into it — Explorer will not search `PATH` — so a
//! binary that moved would otherwise leave a menu entry that launches nothing.
//! The Dolphin ServiceMenu is regenerated each install for the same reason.
//!
//! # Two limits worth knowing
//!
//! **Windows 11 files it under "Show more options".** The modern top-level
//! menu only takes entries from an `IExplorerCommand` in a signed MSIX
//! package; a registry verb cannot reach it. Shift+F10 opens the classic menu
//! directly, and on Windows 10 it is top-level as usual.
//!
//! **Multi-select launches the verb once per file.** Explorer's default for a
//! classic verb, capped at 15 selected items. Each launch forwards one path to
//! the running instance, so a multiple selection does arrive — as several
//! offers rather than one batch, which is what the phone's share queue already
//! copes with.

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// Our verb's key name. Prefixed so it cannot collide with another app's.
const VERB: &str = "Vortex.Share";

/// A NUL-terminated UTF-16 buffer, for the registry APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A UTF-16 buffer as the BYTES `RegSetValueExW` wants.
///
/// `cbData` is a byte count, not a character count — passing the `u16` length
/// writes half the string, and the truncation lands mid-path.
fn utf16_as_bytes(v: &[u16]) -> &[u8] {
    // SAFETY: same allocation and lifetime, length scaled; no alignment
    // concern going from wider to narrower.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// Write one string value (`None` name = the key's default value).
fn set_string(path: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let path_w = wide(path);
    let mut key = HKEY::default();
    // SAFETY: `key` is ours and closed on every path below.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .map_err(|e| format!("create {path}: {e}"))?;

    let name_w = name.map(wide);
    let value_w = wide(value);
    let r = unsafe {
        RegSetValueExW(
            key,
            name_w
                .as_ref()
                .map(|n| PCWSTR(n.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            Some(0),
            REG_SZ,
            Some(utf16_as_bytes(&value_w)),
        )
    }
    .ok()
    .map_err(|e| format!("write {path}: {e}"));
    unsafe {
        let _ = RegCloseKey(key);
    }
    r
}

/// Put "Share via Vortex" on files and folders.
///
/// Best-effort and quiet on failure: a missing context-menu entry is a
/// diminished app, not a broken one, and every other way of sharing still
/// works.
pub(crate) fn register_share_verb() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("shell: cannot resolve own path; no context menu ({e})");
            return;
        }
    };
    let exe = exe.to_string_lossy().to_string();
    // `%1` is the selected path. Quoted because a path with a space is the
    // normal case on Windows, not the exception.
    let command = format!("\"{exe}\" --share \"%1\"");

    // `*` is every file; `Directory` is folders, which the Linux side shares
    // too (it zips them on the way out).
    for class in ["*", "Directory"] {
        let base = format!("Software\\Classes\\{class}\\shell\\{VERB}");
        let steps = [
            (base.clone(), None, "Share via Vortex".to_string()),
            (base.clone(), Some("Icon"), exe.clone()),
            (format!("{base}\\command"), None, command.clone()),
        ];
        for (path, name, value) in steps {
            if let Err(e) = set_string(&path, name, &value) {
                tracing::warn!("shell: context menu for {class} not registered: {e}");
                break;
            }
        }
    }
    tracing::info!(%exe, "shell: 'Share via Vortex' registered for files and folders");
}
