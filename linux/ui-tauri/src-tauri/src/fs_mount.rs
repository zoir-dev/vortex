//! Presenting the phone's storage as a filesystem — the part the rest of the
//! app talks to (design doc §8 step 6).
//!
//! Three files sit behind this one:
//!
//! * [`crate::fs_vfs`] — everything between a mount and the wire: caching, the
//!   path walk, pipelined ranged reads, the concurrency cap. OS-independent,
//!   and where the interesting bugs live, so it is written and tested once.
//! * [`crate::fs_fuse`] — the Linux adapter. A real mount under
//!   `$XDG_RUNTIME_DIR`.
//! * [`crate::fs_projfs`] — the Windows adapter, on ProjFS. A projected
//!   directory under `%LOCALAPPDATA%`.
//!
//! The split is the design doc's: "the mount adapter is swappable; the protocol
//! is the investment". Changing how a desktop presents the files must never
//! require touching the phone, and adding the second desktop did not.
//!
//! # Why not the WebDAV gateway the doc sequenced first
//!
//! One gateway would have served both OSes, which is why it was first. It buys
//! less than it looks like on either:
//!
//! * **Linux:** GVFS and KIO mount `davs://` *inside the file manager's own
//!   process*, so only that program's file dialogs see the files — `cp`, `mpv`,
//!   a text editor's Open box cannot. A FUSE mount is a path, so everything
//!   can.
//! * **Windows:** the WebClient redirector caps a file at ~50 MB by default.
//!   Escaping a 64 MB cap into a 50 MB one would be absurd, and ProjFS ships in
//!   Windows 10 1809+ with no third-party install.
//!
//! So both went native, and no HTTP server, port or auth story exists on either.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use crate::fs_fuse as backend;
#[cfg(target_os = "windows")]
use crate::fs_projfs as backend;

/// Where the phone's files appear.
pub(crate) fn mount_point() -> PathBuf {
    backend::mount_point()
}

/// Whether the phone's files are currently mounted.
pub(crate) fn is_mounted() -> bool {
    backend::is_mounted()
}

/// Mount the phone's storage. Returns the mount point.
///
/// Idempotent: a second call while mounted returns the same path rather than
/// tearing the mount down under whoever is using it.
pub(crate) async fn mount() -> Result<PathBuf, String> {
    if is_mounted() {
        return Ok(mount_point());
    }
    backend::mount().await
}

/// Unmount, if mounted.
pub(crate) fn unmount() {
    backend::unmount();
}

/// Detach the mount on the process's way out, so nothing is left behind that
/// outlives the server serving it.
pub(crate) fn unmount_on_exit() {
    backend::unmount_on_exit();
}

/// Open the phone's storage in the desktop file manager.
///
/// Mounts on demand: the button IS the request, so asking the user to mount
/// first would be a step that exists only because the code is in two pieces.
///
/// Returns the path so the UI can name it in a tooltip; the interesting half of
/// the result is the error, which is what a phone that is not reachable looks
/// like from here.
#[tauri::command]
pub async fn open_phone_files() -> Result<String, String> {
    let dir = mount().await?;
    let path = dir.to_string_lossy().to_string();
    // `explorer.exe` on Windows, `xdg-open` on Linux — both take a directory
    // and bring up the platform's file manager on it.
    //
    // `tokio::process`, not `std::process`: a `std` `Child` dropped without
    // `wait()` stays a zombie for the parent's whole life, and this app runs
    // for days. Same reason `handoff::open_url` does it this way.
    #[cfg(target_os = "windows")]
    let opener = "explorer.exe";
    #[cfg(not(target_os = "windows"))]
    let opener = "xdg-open";
    tokio::process::Command::new(opener)
        .arg(&path)
        .spawn()
        .map_err(|e| format!("cannot open the file manager: {e}"))?;
    tracing::info!(%path, "fs-mount: opened in the file manager");
    Ok(path)
}
