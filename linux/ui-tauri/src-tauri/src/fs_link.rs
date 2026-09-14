//! The laptop's end of the ranged-filesystem protocol — both ends of it.
//!
//! Two independent halves, deliberately in one module because they share a
//! transport and nothing else:
//!
//! * **Server.** An inbound `FS_REQ` is served against this laptop's files
//!   through [`vortex_l3_daemon::core::fs_server`], gated by the roots config,
//!   and answered with `FS_META` / `FS_DATA` / `FS_ERR`. This is what lets the
//!   phone browse the laptop.
//! * **Client.** [`round_trip`] issues an op to the phone and awaits its reply,
//!   correlated by request id. `fs_mount` (FUSE, Linux) and [`crate::fs_pull`]
//!   (file transfer) sit on top of it.
//!
//! Requests are **pipelined**: a file manager stats everything in view at once,
//! so a request/response lock would feel broken. Each in-flight id owns a
//! oneshot channel; replies may arrive in any order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;
use vortex_l3_daemon::core::ble::frame::{ty, RawFrame};
use vortex_l3_daemon::core::fs_proto::{self as p, code};
use vortex_l3_daemon::core::fs_server::{self, FsHandles, Served};

/// How long a request waits before it is abandoned.
///
/// A definite failure beats an indefinite hang: a file manager blocked on a
/// read that will never be answered is this feature's worst outcome, and a
/// phone can vanish mid-op (Doze, process death, out of range) without ever
/// sending `FS_ERR`.
#[allow(dead_code)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Locally-generated code for "there is no link to the peer".
///
/// `EHOSTDOWN`, so a mount adapter can hand the file manager an accurate
/// message rather than a generic I/O error. NOT part of [`p::code`]: it is
/// never sent and never received, because a peer that could answer would not
/// be down. It only travels from [`send`] to whoever asked.
#[allow(dead_code)]
pub(crate) const NO_LINK: i32 = 112;

/// Ranged reads kept in flight by [`read_all`].
///
/// Hides the round trip, not the disk: the peer serves reads under one lock so
/// they do not overlap there. Modest on purpose — replies are 48 KiB each, and
/// on a BLE fallback every one is ~96 paced notify fragments, so a large window
/// would flood a link that cannot absorb it.
const READ_WINDOW: usize = 4;

/// A reply, as delivered to whoever is waiting on a request id.
#[derive(Debug)]
pub enum Reply {
    Meta(p::FsReply),
    Data(p::FsData),
    Err(p::FsErr),
}

struct State {
    /// Waiters by request id.
    inflight: Mutex<HashMap<u32, oneshot::Sender<Reply>>>,
    next_id: AtomicU32,
    /// Files WE serve to the phone, and the policy gate over them.
    roots: p::Roots,
    /// Handles the phone holds on our files. One table for the laptop as a
    /// whole rather than per-peer: a handle is minted and used over the same
    /// session, and `clear_handles` drops the lot when a session ends.
    handles: FsHandles,
    writer: Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>>,
}

static STATE: OnceLock<Arc<State>> = OnceLock::new();

/// Wire up the module. `writer` is the generic sealed-frame writer the BLE loop
/// fills on connect.
pub(crate) fn init(writer: Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>>) {
    let roots = fs_server::load_roots();
    if roots.is_empty() {
        tracing::info!("fs: no served roots; the phone will not be able to browse this laptop");
    }
    let _ = STATE.set(Arc::new(State {
        inflight: Mutex::new(HashMap::new()),
        next_id: AtomicU32::new(1),
        roots,
        handles: FsHandles::new(),
        writer,
    }));
}

/// Drop every handle the phone holds. Called when a session ends so a reconnect
/// starts from a clean table instead of inheriting ids that no longer resolve.
pub(crate) fn clear_handles() {
    if let Some(s) = STATE.get() {
        s.handles.clear();
        // Nothing will ever answer these now; failing the waiters is what turns
        // a hung mount into an honest I/O error.
        let waiters: Vec<_> = s
            .inflight
            .lock()
            .map(|mut g| g.drain().map(|(_, tx)| tx).collect())
            .unwrap_or_default();
        for tx in waiters {
            let _ = tx.send(Reply::Err(p::FsErr::new(0, code::IO, "session ended")));
        }
    }
}

/// Route one filesystem frame. Never blocks the dispatcher: serving an op does
/// real I/O, so it goes to a blocking thread.
pub(crate) fn dispatch(f: RawFrame) {
    let Some(state) = STATE.get().cloned() else {
        tracing::warn!("fs: frame before init; dropping");
        return;
    };
    match f.ty {
        ty::FS_REQ => {
            tokio::spawn(async move { serve_request(state, f).await });
        }
        ty::FS_META => match serde_json::from_slice::<p::FsReply>(&f.payload) {
            Ok(reply) => deliver(&state, reply.id(), Reply::Meta(reply)),
            Err(e) => tracing::warn!("fs: malformed FS_META: {e}"),
        },
        ty::FS_DATA => match p::decode_data(&f.payload) {
            Some((id, offset, eof, bytes)) => deliver(
                &state,
                id,
                Reply::Data(p::FsData {
                    id,
                    offset,
                    eof,
                    bytes: bytes.to_vec(),
                }),
            ),
            None => tracing::warn!("fs: truncated FS_DATA"),
        },
        ty::FS_ERR => match serde_json::from_slice::<p::FsErr>(&f.payload) {
            Ok(e) => {
                tracing::debug!(id = e.id, code = e.code, "fs: peer refused an op");
                deliver(&state, e.id, Reply::Err(e))
            }
            Err(e) => tracing::warn!("fs: malformed FS_ERR: {e}"),
        },
        other => tracing::warn!("fs: not a filesystem frame: 0x{other:02x}"),
    }
}

fn deliver(state: &State, id: u32, reply: Reply) {
    let waiter = state.inflight.lock().ok().and_then(|mut g| g.remove(&id));
    match waiter {
        Some(tx) => {
            let _ = tx.send(reply);
        }
        // Not an error worth shouting about: the waiter may have timed out
        // moments earlier, and the phone had already committed to answering.
        None => tracing::debug!(id, "fs: reply for an unknown request id"),
    }
}

/// Serve an inbound request against this laptop's files.
async fn serve_request(state: Arc<State>, f: RawFrame) {
    let op = f.sub;
    let served = {
        let state = state.clone();
        let payload = f.payload.clone();
        // `fs_server` is synchronous and touches the disk; a stalled network
        // filesystem must not wedge the async executor.
        match tokio::task::spawn_blocking(move || {
            fs_server::serve(&state.roots, &state.handles, op, &payload)
        })
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("fs: serve task failed: {e}");
                return;
            }
        }
    };
    let (ty_byte, payload) = match served {
        Served::Meta(reply) => (ty::FS_META, serde_json::to_vec(&reply).unwrap_or_default()),
        Served::Data(bytes) => (ty::FS_DATA, bytes),
        Served::Err(e) => (ty::FS_ERR, serde_json::to_vec(&e).unwrap_or_default()),
    };
    if send(&state, ty_byte, 0, payload).await.is_err() {
        // The peer asked over a link that has since gone. Nothing to do but
        // drop it: it will not be waiting on a reply it can receive.
        tracing::debug!("fs: dropped a reply, no link");
    }
}

/// Put one frame on the best transport available.
///
/// `Err` means it reached neither, which a client must be told rather than
/// left to discover through the 20 s timeout: a file manager blocked for 20 s
/// per operation on a phone that is simply not here is this feature's worst
/// outcome (design doc §6, "an honest, immediate error rather than hanging").
async fn send(state: &State, ty_byte: u8, sub: u8, payload: Vec<u8>) -> Result<(), ()> {
    // Wi-Fi first, Bluetooth second (design doc §6). The two carry identical
    // frames, so this is only a routing choice — but a 48 KiB read is one TCP
    // frame and ~96 paced BLE fragments, which is the difference between a
    // copy taking a second and taking minutes.
    if let Some(w) = crate::fs_lan::writer().await {
        match w(ty_byte, sub, payload.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // The session looked alive and wasn't — a phone that changed
                // network, or a socket the peer dropped. Fall through to BLE
                // rather than failing the request: the caller cannot retry a
                // transport it does not know about.
                tracing::warn!("fs: LAN send failed ({e}); falling back to BLE");
                crate::fs_lan::close().await;
            }
        }
    }
    let w = { state.writer.lock().await.clone() };
    let Some(w) = w else {
        tracing::debug!("fs: no writer (link down)");
        return Err(());
    };
    if let Err(e) = w(ty_byte, sub, payload).await {
        tracing::warn!("fs: send 0x{ty_byte:02x} failed: {e}");
        return Err(());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

// The `#[allow(dead_code)]` on each item below, rather than a blanket one on the
// module, so genuine dead code here is still reported. They are needed because
// not every op has a consumer on every platform: `write` waits on design doc
// §8 step 5, and the FUSE consumer of the read path is Linux-only until ProjFS
// lands.

/// Issue one op to the phone and await its reply.
///
/// `payload` must already be the encoded request body for `op` (JSON, or
/// JSON + binary tail for WRITE) with its `id` field set to the value this
/// function allocated — which is why callers go through the typed wrappers
/// below rather than calling this directly.
#[allow(dead_code)]
async fn round_trip(op: u8, id: u32, payload: Vec<u8>) -> Result<Reply, i32> {
    let Some(state) = STATE.get().cloned() else {
        return Err(code::IO);
    };
    let (tx, rx) = oneshot::channel();
    {
        let mut g = state.inflight.lock().map_err(|_| code::IO)?;
        g.insert(id, tx);
    }
    if send(&state, ty::FS_REQ, op, payload).await.is_err() {
        if let Ok(mut g) = state.inflight.lock() {
            g.remove(&id);
        }
        return Err(NO_LINK);
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
        Ok(Ok(reply)) => Ok(reply),
        // Sender dropped: the session ended under us.
        Ok(Err(_)) => Err(code::IO),
        Err(_) => {
            // Stop tracking it, or a late reply keeps a dead entry alive.
            if let Ok(mut g) = state.inflight.lock() {
                g.remove(&id);
            }
            tracing::warn!(id, "fs: request timed out");
            Err(code::IO)
        }
    }
}

#[allow(dead_code)]
fn next_id() -> Result<u32, i32> {
    let state = STATE.get().ok_or(code::IO)?;
    // Wrapping is fine — ids only need to be unique among what is in flight,
    // and 0 is reserved for "no particular request" in FS_ERR.
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    Ok(if id == 0 { 1 } else { id })
}

/// One page of a directory on the phone.
#[allow(dead_code)]
pub(crate) async fn list(path: &str, cursor: u32) -> Result<(Vec<p::FsEntry>, Option<u32>), i32> {
    let id = next_id()?;
    let req = p::ListReq {
        id,
        path: path.to_string(),
        cursor,
    };
    match round_trip(p::op::LIST, id, serde_json::to_vec(&req).unwrap_or_default()).await? {
        Reply::Meta(p::FsReply::List { entries, cursor, .. }) => Ok((entries, cursor)),
        Reply::Err(e) => Err(e.code),
        other => {
            tracing::warn!("fs: LIST answered with {other:?}");
            Err(code::IO)
        }
    }
}

/// Stat one path on the phone.
#[allow(dead_code)]
pub(crate) async fn stat(path: &str) -> Result<p::FsEntry, i32> {
    let id = next_id()?;
    let req = p::StatReq {
        id,
        path: path.to_string(),
    };
    match round_trip(p::op::STAT, id, serde_json::to_vec(&req).unwrap_or_default()).await? {
        Reply::Meta(p::FsReply::Stat { entry, .. }) => Ok(entry),
        Reply::Err(e) => Err(e.code),
        other => {
            tracing::warn!("fs: STAT answered with {other:?}");
            Err(code::IO)
        }
    }
}

/// Open a path on the phone; returns `(handle, size)`.
#[allow(dead_code)]
pub(crate) async fn open(path: &str, write: bool) -> Result<(u64, u64), i32> {
    let id = next_id()?;
    let req = p::OpenReq {
        id,
        path: path.to_string(),
        write,
    };
    match round_trip(p::op::OPEN, id, serde_json::to_vec(&req).unwrap_or_default()).await? {
        Reply::Meta(p::FsReply::Open { handle, size, .. }) => Ok((handle, size)),
        Reply::Err(e) => Err(e.code),
        other => {
            tracing::warn!("fs: OPEN answered with {other:?}");
            Err(code::IO)
        }
    }
}

/// Read a bounded range. A short result is normal; `eof` says whether the file
/// ends here.
#[allow(dead_code)]
pub(crate) async fn read(handle: u64, offset: u64, len: u32) -> Result<(Vec<u8>, bool), i32> {
    if len == 0 || len > p::MAX_READ_LEN {
        return Err(code::INVAL);
    }
    let id = next_id()?;
    let req = p::ReadReq {
        id,
        handle,
        offset,
        len,
    };
    match round_trip(p::op::READ, id, serde_json::to_vec(&req).unwrap_or_default()).await? {
        Reply::Data(d) => {
            // A reply for the right id but the wrong offset would silently
            // corrupt whatever is assembling the file.
            if d.offset != offset {
                tracing::warn!(
                    want = offset,
                    got = d.offset,
                    "fs: READ answered for the wrong offset"
                );
                return Err(code::IO);
            }
            Ok((d.bytes, d.eof))
        }
        Reply::Err(e) => Err(e.code),
        other => {
            tracing::warn!("fs: READ answered with {other:?}");
            Err(code::IO)
        }
    }
}

/// Write a bounded range. Returns the bytes the peer accepted.
#[allow(dead_code)]
pub(crate) async fn write(handle: u64, offset: u64, bytes: &[u8]) -> Result<u32, i32> {
    let id = next_id()?;
    let req = p::WriteReq {
        id,
        handle,
        offset,
    };
    match round_trip(p::op::WRITE, id, p::encode_write(&req, bytes)).await? {
        Reply::Meta(p::FsReply::Wrote { bytes, .. }) => Ok(bytes),
        Reply::Err(e) => Err(e.code),
        other => {
            tracing::warn!("fs: WRITE answered with {other:?}");
            Err(code::IO)
        }
    }
}

/// Release a handle. Best-effort — a failure here is logged, not propagated:
/// the peer expires handles on its own, so a lost CLOSE is not a leak.
#[allow(dead_code)]
pub(crate) async fn close(handle: u64) {
    let Ok(id) = next_id() else { return };
    let req = p::CloseReq { id, handle };
    let payload = serde_json::to_vec(&req).unwrap_or_default();
    if let Err(c) = round_trip(p::op::CLOSE, id, payload).await {
        tracing::debug!(handle, code = c, "fs: CLOSE not acknowledged");
    }
}

/// Read a whole file by ranges, calling `sink` with each slice in order.
///
/// This is the replacement for buffering a file in memory: peak usage is one
/// [`p::MAX_READ_LEN`] slice regardless of file size, which is what retires the
/// `MAX_FILE_BYTES` cap and the 835 MB `OutOfMemoryError` behind it.
///
/// Stops at EOF, at `size` when the peer reported one, or at a short read that
/// is followed by no progress.
#[allow(dead_code)]
pub(crate) async fn read_all(
    path: &str,
    mut sink: impl FnMut(u64, &[u8]) -> std::io::Result<()>,
) -> Result<u64, i32> {
    use futures::stream::{FuturesOrdered, StreamExt};

    let (handle, size) = open(path, false).await?;
    let mut offset = 0u64;

    // Pipelined when the size is known: keep [`READ_WINDOW`] reads outstanding
    // so the next request is on the wire while the current reply is still
    // arriving. Sequentially, every chunk paid a full round trip before the
    // next was even sent, which is most of the cost on a link this fast.
    //
    // `FuturesOrdered` yields in ISSUE order, which is also offset order, so
    // the sink is still called front to back and a caller that simply appends
    // stays correct. Each future carries the offset it asked for rather than
    // trusting a running counter — a short read would otherwise silently shift
    // every later chunk.
    //
    // Without a size there is nothing to size a window against, and
    // speculative reads past the end would be pure waste, so that case stays
    // sequential and follows EOF.
    let result = if size > 0 {
        let mut pending = FuturesOrdered::new();
        let mut next = 0u64;
        loop {
            while pending.len() < READ_WINDOW && next < size {
                let at = next;
                pending.push_back(async move { (at, read(handle, at, p::MAX_READ_LEN).await) });
                next = next.saturating_add(p::MAX_READ_LEN as u64);
            }
            let Some((at, res)) = pending.next().await else {
                break Ok(offset);
            };
            let (bytes, eof) = match res {
                Ok(v) => v,
                Err(c) => break Err(c),
            };
            if !bytes.is_empty() {
                if let Err(e) = sink(at, &bytes) {
                    tracing::warn!("fs: sink failed at offset {at}: {e}");
                    break Err(code::IO);
                }
                offset = at + bytes.len() as u64;
            } else if !eof {
                tracing::warn!(at, "fs: read stalled without EOF");
                break Err(code::IO);
            }
            if eof {
                break Ok(offset);
            }
        }
    } else {
        loop {
            let (bytes, eof) = match read(handle, offset, p::MAX_READ_LEN).await {
                Ok(v) => v,
                Err(c) => break Err(c),
            };
            if !bytes.is_empty() {
                if let Err(e) = sink(offset, &bytes) {
                    tracing::warn!("fs: sink failed at offset {offset}: {e}");
                    break Err(code::IO);
                }
                offset += bytes.len() as u64;
            }
            if eof {
                break Ok(offset);
            }
            if bytes.is_empty() {
                // No EOF flag and no bytes: the peer is not making progress and
                // a retry loop here would spin forever.
                tracing::warn!(offset, "fs: read stalled without EOF");
                break Err(code::IO);
            }
        }
    };
    close(handle).await;
    result
}
