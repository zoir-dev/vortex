//! Command-line exercise of the filesystem protocol (design doc §8 step 1:
//! "No mount yet — validate over the existing session with a CLI").
//!
//! There is no UI for browsing yet, so without this the only way to reach
//! [`crate::fs_link`] would be to build one first and debug two new things at
//! once. These flags drive the client directly over whatever session is already
//! up, which is the same reason `--mirror` exists. `--fs-mount` is here for a
//! second reason as well: mounting the phone is a deliberate act, not something
//! that should happen behind the user's back on every connect.
//!
//! Results go to the app log (`~/.cache/vortex/vortex.log`), not the invoking
//! terminal: single-instance forwards the argv to the *running* process, which
//! is the one holding the peer session. `--fs-get` additionally writes the file
//! it fetched, so a run can be checked against the phone's copy with `md5sum`
//! rather than by reading log lines.

use std::io::{Seek, SeekFrom, Write};

use vortex_l3_daemon::core::fs_proto as p;

/// Human-readable errno, so a log line says "not permitted" rather than 13.
fn code_name(c: i32) -> &'static str {
    match c {
        p::code::NOENT => "no such file",
        p::code::ACCES => "not permitted",
        p::code::IO => "I/O error",
        p::code::BADF => "bad handle",
        p::code::INVAL => "invalid request",
        p::code::NOTSUP => "not supported",
        p::code::ISDIR => "is a directory",
        p::code::ROFS => "read-only",
        _ => "unknown error",
    }
}

/// `--fs-ls [path]` — list a directory, following pagination to the end.
///
/// The empty path is the peer's synthetic root, which is how you discover what
/// it shares without being told out of band.
pub(crate) async fn ls(path: String) {
    let mut cursor = 0u32;
    let mut page = 0u32;
    let mut total = 0usize;
    loop {
        match crate::fs_link::list(&path, cursor).await {
            Ok((entries, next)) => {
                for e in &entries {
                    // Tab-separated so a log line can be cut apart; `path` last
                    // because it is the long opaque one (a document URI on
                    // Android) and reads better at the end.
                    tracing::info!(
                        "fs-cli: {}\t{:>10}\t{}\t{}",
                        if e.is_dir { "dir " } else { "file" },
                        e.size,
                        e.name,
                        e.path,
                    );
                }
                total += entries.len();
                match next {
                    Some(c) => {
                        page += 1;
                        cursor = c;
                        // A peer that keeps handing back a cursor without
                        // advancing would loop us forever; stop and say so
                        // rather than spin.
                        if page > 10_000 {
                            tracing::warn!("fs-cli: ls gave up after {page} pages");
                            break;
                        }
                    }
                    None => break,
                }
            }
            Err(c) => {
                tracing::warn!("fs-cli: ls {path:?} failed: {} ({c})", code_name(c));
                return;
            }
        }
    }
    tracing::info!("fs-cli: ls {path:?} → {total} entries in {} page(s)", page + 1);
}

/// `--fs-stat <path>`
pub(crate) async fn stat(path: String) {
    match crate::fs_link::stat(&path).await {
        Ok(e) => tracing::info!(
            "fs-cli: stat {path:?} → name={} dir={} size={} mtime={} readonly={}",
            e.name,
            e.is_dir,
            e.size,
            e.mtime,
            e.readonly,
        ),
        Err(c) => tracing::warn!("fs-cli: stat {path:?} failed: {} ({c})", code_name(c)),
    }
}

/// `--fs-get <remote> <local>` — stream a remote file to a local path.
///
/// The one that actually proves the design: it runs open → many ranged reads →
/// close, and its peak memory is one chunk no matter how big the file is. That
/// is the property `MAX_FILE_BYTES` exists to work around today, so a large
/// file fetched here is the evidence the cap can go.
pub(crate) async fn get(remote: String, local: String) {
    let started = std::time::Instant::now();
    let mut file = match std::fs::File::create(&local) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("fs-cli: cannot create {local:?}: {e}");
            return;
        }
    };
    // Seek per chunk rather than trusting arrival order: `read_all` is
    // sequential today, but the sink contract passes an offset precisely so a
    // future pipelined reader does not silently write bytes in the wrong place.
    let sink = |offset: u64, bytes: &[u8]| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)
    };
    match crate::fs_link::read_all(&remote, sink).await {
        Ok(n) => {
            let secs = started.elapsed().as_secs_f64();
            let rate = if secs > 0.0 {
                n as f64 / secs / 1024.0
            } else {
                0.0
            };
            tracing::info!(
                "fs-cli: get {remote:?} → {local:?}: {n} bytes in {secs:.1}s ({rate:.0} KiB/s)"
            );
        }
        Err(c) => tracing::warn!("fs-cli: get {remote:?} failed: {} ({c})", code_name(c)),
    }
}

/// `--fs-mount` / `--fs-umount` — put the phone's storage on the filesystem.
///
/// The mount is not automatic: it costs a kernel session (FUSE) or a
/// virtualization instance (ProjFS), and one pointing at a phone that is not
/// here is worse than none. The home screen's folder button is the everyday
/// way in; this stays because a flag can be scripted and a button cannot.
pub(crate) async fn mount() {
    match crate::fs_mount::mount().await {
        Ok(dir) => tracing::info!("fs-cli: mounted at {}", dir.display()),
        Err(e) => tracing::warn!("fs-cli: mount failed: {e}"),
    }
}

/// Route an `--fs-*` flag. Returns false when `argv` holds none, so the caller
/// can fall through to its other flags.
pub(crate) fn dispatch(argv: &[String]) -> bool {
    if argv.iter().any(|a| a == "--fs-umount") {
        if crate::fs_mount::is_mounted() {
            crate::fs_mount::unmount();
        } else {
            tracing::info!("fs-cli: nothing mounted");
        }
        return true;
    }
    if argv.iter().any(|a| a == "--fs-mount") {
        tauri::async_runtime::spawn(mount());
        return true;
    }
    if let Some(pos) = argv.iter().position(|a| a == "--fs-ls") {
        // Optional: no path means the synthetic root listing the peer's shares.
        let path = argv.get(pos + 1).cloned().unwrap_or_default();
        tauri::async_runtime::spawn(ls(path));
        return true;
    }
    if let Some(pos) = argv.iter().position(|a| a == "--fs-stat") {
        let Some(path) = argv.get(pos + 1).cloned() else {
            tracing::warn!("fs-cli: --fs-stat needs a path");
            return true;
        };
        tauri::async_runtime::spawn(stat(path));
        return true;
    }
    if let Some(pos) = argv.iter().position(|a| a == "--fs-get") {
        let (Some(remote), Some(local)) = (argv.get(pos + 1).cloned(), argv.get(pos + 2).cloned())
        else {
            tracing::warn!("fs-cli: --fs-get needs <remote> <local>");
            return true;
        };
        tauri::async_runtime::spawn(get(remote, local));
        return true;
    }
    false
}
