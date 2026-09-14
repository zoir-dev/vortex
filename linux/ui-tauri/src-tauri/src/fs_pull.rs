//! Pull the files the phone has offered, through the ranged-read protocol.
//!
//! Replaces the bulk-sync `clipboard_file` path, which moved a file by holding
//! all of it in memory on BOTH sides: the phone read it into a `ByteArray` to
//! hash for its token, and the laptop reassembled the chunks into a `Vec<u8>`
//! before writing. That is what made an 835 MB share an `OutOfMemoryError` on
//! the phone and why a 64 MB cap existed at all.
//!
//! Here the file is streamed: [`crate::fs_link::read_all`] issues bounded
//! ranged reads and each one is written straight to disk, so peak memory is one
//! chunk regardless of size. It also inherits the transport choice — Wi-Fi when
//! reachable, Bluetooth otherwise — for free, because it is the same client.

use std::io::{Seek, SeekFrom, Write};

/// Wake the puller. Called when an offer is accepted, and again after each
/// file, so a batch drains without waiting on a heartbeat tick.
pub(crate) fn nudge() {
    if let Some(n) = NUDGE.get() {
        n.notify_one();
    }
}

static NUDGE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Notify>> = std::sync::OnceLock::new();

/// Start the drain loop. One at a time on purpose: the phone serves from a
/// single link, and several concurrent pulls would interleave ranged reads over
/// one socket without arriving any sooner.
pub(crate) fn spawn() {
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let _ = NUDGE.set(notify.clone());
    tokio::spawn(async move {
        loop {
            notify.notified().await;
            while let Some((token, name, _mime, id, kind)) = pop_front() {
                pull_one(&token, &name, id, &kind).await;
            }
        }
    });
}

/// Tokens currently being streamed.
///
/// The phone re-announces an offer it has not seen fetched, so the offer
/// handler must be able to tell "not started" from "in progress". Its existing
/// guard is a 60 s TTL on *completed* pulls, which cannot cover this: a pull is
/// no longer in the queue but not yet complete, and a big file takes longer
/// than any fixed window — the first 151 MB test ran 77 s and duplicated
/// itself. Membership here lasts exactly as long as the transfer, whatever
/// that turns out to be.
static IN_FLIGHT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// True while `token` is being streamed, so a re-announce is ignored rather
/// than queued a second time.
pub(crate) fn is_in_flight(token: &str) -> bool {
    IN_FLIGHT
        .lock()
        .map(|g| g.iter().any(|t| t == token))
        .unwrap_or(false)
}

fn mark_in_flight(token: &str) {
    if let Ok(mut g) = IN_FLIGHT.lock() {
        g.push(token.to_string());
    }
}

fn clear_in_flight(token: &str) {
    if let Ok(mut g) = IN_FLIGHT.lock() {
        g.retain(|t| t != token);
    }
}

/// `(token, name, mime, id, kind)`. `kind` is what the phone called this file —
/// a capture it sent by itself, or something a person shared — and it decides
/// which folder it lands in, so it has to survive the move to ranged reads.
fn pop_front() -> Option<(String, String, String, u64, String)> {
    crate::PENDING_FILE_OFFERS
        .get()
        .and_then(|m| m.lock().ok().and_then(|mut g| g.pop_front()))
}

async fn pull_one(token: &str, name: &str, id: u64, kind: &str) {
    mark_in_flight(token);
    // Sanitise to a single path component. The name comes from the phone, and
    // a `../` in it would otherwise choose where on this laptop the file lands.
    let safe = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "vortex-file".to_string());
    // Captures (the phone sent it by itself) go under the picture folder, and
    // anything a person shared under downloads — `receive_root` is the single
    // owner of that rule, and the bulk path this replaces used it too.
    let subdir = vortex_l3_daemon::core::clipboard_mirror::subdir_for_kind(kind);
    let Some(dir) = crate::clipboard_sync::receive_root(subdir)
        .map(|d| crate::clipboard_sync::receive_dir(&d, subdir))
    else {
        tracing::warn!("file pull: no HOME — dropped");
        crate::transfers::fail(id);
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("file pull: cannot create {}: {e}", dir.display());
        crate::transfers::fail(id);
        return;
    }
    let path = crate::clipboard_sync::unique_path(&dir, &safe);
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("file pull: cannot create {}: {e}", path.display());
            crate::transfers::fail(id);
            return;
        }
    };

    let started = std::time::Instant::now();
    // Seek to the offset we are handed rather than appending: `read_all` is
    // sequential today, but its sink contract carries an offset so a future
    // pipelined reader cannot silently write bytes out of order.
    let sink = |offset: u64, bytes: &[u8]| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)
    };
    let addr = format!("share:{token}");
    match crate::fs_link::read_all(&addr, sink).await {
        Ok(n) => {
            // Mark it pulled BEFORE completing, so a re-announce racing this
            // moment cannot queue the same file again.
            crate::clipboard_sync::note_pulled(token);
            crate::transfers::complete(id);
            let secs = started.elapsed().as_secs_f64();
            tracing::info!(
                bytes = n,
                name = %safe,
                "file received from phone in {secs:.1}s → {}",
                path.display()
            );
        }
        Err(code) => {
            // Leave no half-written file behind: a truncated download in the
            // user's folder looks like a real one and is worse than nothing.
            drop(file);
            let _ = std::fs::remove_file(&path);
            tracing::warn!(name = %safe, "file pull failed (errno {code})");
            crate::transfers::fail(id);
        }
    }
    // Cleared only after `note_pulled` has run on the success path, so there is
    // no instant where the token is neither in flight nor recently pulled — a
    // re-announce landing in that gap would queue the file all over again. On
    // failure it is deliberately NOT noted, so a later re-announce can retry.
    clear_in_flight(token);
    crate::lan::note_queue_progress();
}
