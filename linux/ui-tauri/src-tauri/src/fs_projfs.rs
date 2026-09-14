//! The Windows mount adapter: the phone's storage projected with ProjFS.
//!
//! Everything between this and the wire — caching, the path walk, pipelined
//! reads — is [`crate::fs_vfs`]; this file is only the translation between that
//! and the Projected File System. Its Linux counterpart is
//! [`crate::fs_fuse`].
//!
//! # Why ProjFS and not the WebDAV gateway
//!
//! WebDAV was sequenced first because one gateway serves both OSes, but on
//! Windows the redirector caps a file at ~50 MB (`FileSizeLimitInBytes`), needs
//! the WebClient service running, and wants the unfamiliar `\\host@port\` form.
//! Escaping a 64 MB cap into a 50 MB one would be absurd. ProjFS ships in
//! Windows 10 1809+ with **no third-party install** — it is what VFS for Git
//! uses — and gives a real directory with no size ceiling and proper seeking.
//!
//! It is an optional Windows *feature*, though, off by default on client SKUs.
//! [`start`] says so plainly when it is missing rather than failing obscurely.
//!
//! # How this differs from the FUSE adapter, and why
//!
//! The Linux side never blocks: FUSE hands it one request at a time on one
//! thread, so answering inline would serialise the whole mount behind one round
//! trip at a time. ProjFS is the opposite — it runs **its own thread pool** and
//! is designed for providers that block a pool thread while fetching. So the
//! callbacks here are straightforwardly synchronous: they block on the async
//! runtime and return an answer.
//!
//! That is safe only because the pool is sized above [`fs_vfs::MAX_INFLIGHT`]:
//! the semaphore in the shared layer runs out before ProjFS's threads do, so
//! the cap on concurrency is ours and a thundering thumbnailer cannot exhaust
//! the pool and wedge Explorer. If that ever stops holding, the escape hatch is
//! ProjFS's own: return `HRESULT_FROM_WIN32(ERROR_IO_PENDING)` and finish later
//! with `PrjCompleteCommand`. It costs an owned copy of everything in the
//! callback data, which is why it is not the starting point.
//!
//! # Hydration, and what that means for staleness
//!
//! Unlike FUSE, ProjFS writes fetched content into the real directory and
//! serves later reads from it without asking us. That is design doc §7's
//! content cache, for free — and it is also a correctness problem, because a
//! file that changes on the phone is not re-fetched. [`start`] therefore clears
//! the projection each time it mounts, so a session begins from the phone's
//! current truth. Within one session a file changed on the phone still shows
//! its old content; fixing that properly means giving placeholders a ContentID
//! derived from size and mtime and driving `PrjUpdateFileIfNeeded`, which is
//! the natural companion to the daemon cache layer (§8 step 3).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_HOST_DOWN, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_IO_DEVICE, ERROR_NOT_SUPPORTED,
    ERROR_TIMEOUT, ERROR_WRITE_PROTECT,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
use windows::Win32::Storage::ProjectedFileSystem::*;

use vortex_l3_daemon::core::fs_proto::{self as p, code};

use crate::fs_vfs::{FsRemote, LinkRemote, Vfs, MAX_INFLIGHT};

/// `S_OK`. Spelled out rather than imported so every callback's success path
/// reads the same as its failure paths.
const OK: HRESULT = HRESULT(0);

/// This provider's virtualization instance id.
///
/// Fixed rather than freshly generated: it identifies *Vortex's* projection of
/// a directory, and the directory outlives a run. A new id each start would
/// make Windows treat the same folder as a different projection every time.
const INSTANCE_ID: GUID = GUID::from_u128(0x7b1f9a42_5d33_4c86_9e10_2f6a4c8d1b57);

/// Bytes fetched from the phone per `PrjWriteFileData` call.
///
/// ProjFS may ask for a whole file in one callback, and a 3.4 GB request must
/// not become a 3.4 GB allocation — the flat-memory property is the entire
/// point of the ranged protocol. Rounded up to the volume's write alignment at
/// use, since every chunk but the file's last must be a multiple of it.
const HYDRATE_CHUNK: u32 = 1024 * 1024;

/// How long a metadata callback waits on the phone before giving up.
///
/// Deliberately far below the protocol's own 20 s reply timeout: that one
/// bounds a REQUEST, this one bounds how long Explorer is allowed to look
/// frozen. Hydration is not held to it — copying a large file over a slow link
/// legitimately takes minutes, and ProjFS is built to show that as a slow copy
/// rather than a hang.
const META_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Locally-generated: the phone did not answer in time. Never on the wire, the
/// same way [`crate::fs_link::NO_LINK`] is not.
const TIMED_OUT: i32 = -1;

/// ProjFS threads. Above [`MAX_INFLIGHT`] on purpose — see the module docs:
/// the shared semaphore is meant to be what limits concurrency, not the pool.
const POOL_THREADS: u32 = MAX_INFLIGHT as u32 * 2;

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// A protocol code as an `HRESULT`.
///
/// The reason [`code`] is errno-shaped is that both mount adapters have to turn
/// it back into an OS error; this is that, for the other OS. What Explorer
/// shows the user comes straight from here, so [`crate::fs_link::NO_LINK`]
/// becoming `ERROR_HOST_DOWN` rather than a generic device error is the
/// difference between an accurate message and a puzzling one.
fn hresult_of(c: i32) -> HRESULT {
    let win = match c {
        code::NOENT => ERROR_FILE_NOT_FOUND,
        code::ACCES => ERROR_ACCESS_DENIED,
        code::BADF => ERROR_INVALID_HANDLE,
        code::INVAL => ERROR_INVALID_PARAMETER,
        code::NOTSUP => ERROR_NOT_SUPPORTED,
        // No Win32 equivalent of EISDIR in this position; a caller that opened
        // a directory as a file gets the same refusal it would from NTFS.
        code::ISDIR => ERROR_ACCESS_DENIED,
        code::ROFS => ERROR_WRITE_PROTECT,
        crate::fs_link::NO_LINK => ERROR_HOST_DOWN,
        // Asked, and nothing came back in the time a file manager can wait.
        TIMED_OUT => ERROR_TIMEOUT,
        // Includes `code::IO`, and anything a future peer invents.
        _ => ERROR_IO_DEVICE,
    };
    HRESULT::from_win32(win.0)
}

/// Seconds since the Unix epoch as a Windows `FILETIME` tick count.
///
/// FILETIME counts 100 ns intervals from 1601-01-01, which is
/// [`EPOCH_DELTA`](self) seconds before the Unix epoch. Saturating, because the
/// protocol's 0 ("the peer cannot tell") and the negative value a badly-set
/// phone clock produces must not overflow a mount into a panic.
fn filetime_of(unix_secs: i64) -> i64 {
    /// Seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DELTA: i64 = 11_644_473_600;
    unix_secs
        .saturating_add(EPOCH_DELTA)
        .saturating_mul(10_000_000)
        .max(0)
}

/// A protocol entry as the metadata ProjFS stores in a placeholder.
///
/// `FILE_ATTRIBUTE_READONLY` on everything: the projection is read-only until
/// design doc §8 step 5, and an attribute that says so lets Explorer grey out
/// the operations rather than offer them and fail. It is advisory, which is why
/// [`notification`] refuses the operations outright as well.
fn basic_info(e: &p::FsEntry) -> PRJ_FILE_BASIC_INFO {
    let t = filetime_of(e.mtime);
    let attrs = if e.is_dir {
        FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_READONLY.0
    } else {
        FILE_ATTRIBUTE_READONLY.0
    };
    PRJ_FILE_BASIC_INFO {
        IsDirectory: e.is_dir,
        FileSize: e.size as i64,
        // The protocol carries one timestamp. Reporting it as all four is
        // better than reporting three zeroes, which Explorer renders as 1601.
        CreationTime: t,
        LastAccessTime: t,
        LastWriteTime: t,
        ChangeTime: t,
        FileAttributes: attrs,
    }
}

/// A NUL-terminated UTF-16 buffer, for passing a Rust string to Win32.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a `PCWSTR` the OS handed us. Null and invalid both become empty, which
/// every caller here treats as "the root" or "no pattern" — the right reading
/// in both cases.
///
/// # Safety
/// `s` must be null or point at a NUL-terminated UTF-16 string.
unsafe fn from_wide(s: PCWSTR) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe { s.to_string() }.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Provider state
// ---------------------------------------------------------------------------

/// One directory enumeration in progress.
///
/// A snapshot, deliberately: a directory listed while the phone is adding files
/// must not grow under the caller mid-enumeration, and ProjFS's restart flag
/// rewinds *this* list rather than re-reading the folder.
struct EnumSession {
    entries: Vec<p::FsEntry>,
    /// Index of the next entry to hand back.
    next: usize,
    /// The search expression, captured on the first call and on every restart.
    ///
    /// ProjFS passes it once and may pass null afterwards, so a provider that
    /// does not save it will filter the first page and nothing after it.
    pattern: Option<String>,
}

struct Provider {
    vfs: Arc<Vfs<LinkRemote>>,
    rt: tokio::runtime::Handle,
    enums: Mutex<HashMap<u128, EnumSession>>,
    /// The volume's required alignment for `PrjWriteFileData`, learned once the
    /// instance is up. 1 until then, which is a no-op rounding.
    write_alignment: AtomicU32,
}

impl Provider {
    /// Run one async operation to completion from a ProjFS pool thread.
    ///
    /// Blocking is correct here — see the module docs. `Handle::block_on`
    /// rather than a runtime of our own so the work lands on the same executor
    /// (and the same link) as everything else, and it cannot panic for being
    /// inside an async context because ProjFS's threads are not tokio's.
    fn block<T>(&self, f: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(f)
    }

    /// Run one metadata operation, and give up quickly if the phone is silent.
    ///
    /// A callback holds a ProjFS pool thread for as long as it runs, and every
    /// Explorer action on the projection is a callback — so the protocol's 20 s
    /// reply timeout is the length of time Explorer appears hung when the phone
    /// stops answering. Observed exactly that: a phone that went quiet mid
    /// transfer left the folder frozen until the whole timeout expired, twice.
    ///
    /// Five seconds is already far longer than a listing takes on a working
    /// link (11 ms over Wi-Fi, ~2.5 s over BLE), so this only ever fires when
    /// something is genuinely wrong — and then "the host is down" in a moment
    /// beats a frozen window for twenty seconds.
    ///
    /// Abandoning the future is safe: `fs_link` keeps its in-flight entry keyed
    /// by request id, and drops it when the late reply arrives or when the
    /// session ends, so nothing accumulates.
    fn block_meta<T>(&self, f: impl std::future::Future<Output = Result<T, i32>>) -> Result<T, i32> {
        self.block(async move {
            match tokio::time::timeout(META_TIMEOUT, f).await {
                Ok(v) => v,
                Err(_) => Err(TIMED_OUT),
            }
        })
    }

    /// Resolve a ProjFS-relative path to the peer's opaque address.
    fn resolve(&self, rel: &str) -> Result<p::FsEntry, i32> {
        self.block_meta(self.vfs.resolve(rel))
    }
}

/// The running instance. `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` is a raw
/// pointer, so it is not `Send` on its own; it is only ever touched under this
/// mutex and only handed back to ProjFS.
struct Instance {
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    /// The leaked provider handed to ProjFS as the instance context. Reclaimed
    /// after `PrjStopVirtualizing` returns, which is when no callback can still
    /// be looking at it.
    provider: *const Provider,
    root: PathBuf,
}

// SAFETY: both fields are only dereferenced by ProjFS callbacks (on ProjFS's
// own threads, which is what the pointers are for) and by [`stop`], which holds
// the mutex and runs after virtualization has been told to stop.
unsafe impl Send for Instance {}

static INSTANCE: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();

fn instance_slot() -> &'static Mutex<Option<Instance>> {
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// The provider behind a callback.
///
/// # Safety
/// `cb` must be a callback-data pointer ProjFS handed us, whose
/// `InstanceContext` is the provider leaked by [`start`].
unsafe fn provider<'a>(cb: *const PRJ_CALLBACK_DATA) -> Option<(&'a Provider, &'a PRJ_CALLBACK_DATA)> {
    let data = unsafe { cb.as_ref() }?;
    let p = unsafe { (data.InstanceContext as *const Provider).as_ref() }?;
    Some((p, data))
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

/// Begin enumerating a directory.
///
/// The listing is fetched here rather than on the first `get_enumeration` so
/// that a failure — an unreachable phone, a folder that has gone — surfaces at
/// the point Windows can still report it cleanly, and so the sort happens once
/// per enumeration rather than once per page.
unsafe extern "system" fn start_enumeration(
    cb: *const PRJ_CALLBACK_DATA,
    enum_id: *const GUID,
) -> HRESULT {
    let (Some((prov, data)), Some(id)) = (unsafe { provider(cb) }, unsafe { enum_id.as_ref() })
    else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    let rel = unsafe { from_wide(data.FilePathName) };
    let entry = match prov.resolve(&rel) {
        Ok(e) => e,
        Err(c) => return hresult_of(c),
    };
    if !entry.is_dir {
        return HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0);
    }
    let mut entries = match prov.block_meta(prov.vfs.listing(&entry.path)) {
        Ok(v) => (*v).clone(),
        Err(c) => return hresult_of(c),
    };
    // ProjFS requires entries in ITS collation order, not ours: it merges our
    // list with what is already on disk, and a differently-ordered list makes
    // that merge drop or duplicate entries.
    entries.sort_by(|a, b| compare_names(&a.name, &b.name));
    match prov.enums.lock() {
        Ok(mut g) => {
            g.insert(
                id.to_u128(),
                EnumSession {
                    entries,
                    next: 0,
                    pattern: None,
                },
            );
            OK
        }
        Err(_) => HRESULT::from_win32(ERROR_IO_DEVICE.0),
    }
}

/// ProjFS's own filename collation, which is not Rust's `Ord`.
fn compare_names(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (wide(a), wide(b));
    let r = unsafe { PrjFileNameCompare(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) };
    r.cmp(&0)
}

/// Hand back the next page of a directory.
unsafe extern "system" fn get_enumeration(
    cb: *const PRJ_CALLBACK_DATA,
    enum_id: *const GUID,
    search_expression: PCWSTR,
    buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    let (Some((prov, data)), Some(id)) = (unsafe { provider(cb) }, unsafe { enum_id.as_ref() })
    else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    let Ok(mut sessions) = prov.enums.lock() else {
        return HRESULT::from_win32(ERROR_IO_DEVICE.0);
    };
    let Some(session) = sessions.get_mut(&id.to_u128()) else {
        // No session: ProjFS asked about an enumeration we never started.
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };

    // The search expression arrives on the FIRST call and on every restart, and
    // may be null on the calls between. Saving it is the provider's job — a
    // provider that re-reads it each time filters page one and nothing after.
    let restart = data.Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0 != 0;
    if restart {
        session.next = 0;
        session.pattern = Some(unsafe { from_wide(search_expression) });
    } else if session.pattern.is_none() {
        session.pattern = Some(unsafe { from_wide(search_expression) });
    }
    // An empty expression means "everything"; so does `*`, but ProjFS sends the
    // empty one and calling `PrjFileNameMatch` with it would reject every name.
    let pattern = session
        .pattern
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "*")
        .map(wide);

    while session.next < session.entries.len() {
        let e = &session.entries[session.next];
        let name = wide(&e.name);
        if let Some(pat) = &pattern {
            if !unsafe { PrjFileNameMatch(PCWSTR(name.as_ptr()), PCWSTR(pat.as_ptr())) } {
                session.next += 1;
                continue;
            }
        }
        let info = basic_info(e);
        if let Err(err) =
            unsafe { PrjFillDirEntryBuffer(PCWSTR(name.as_ptr()), Some(&info), buffer) }
        {
            // The buffer is full. Stop WITHOUT consuming this entry — ProjFS
            // will call again and it must be the first one next time.
            if err.code() == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) {
                return OK;
            }
            tracing::warn!("fs-projfs: filling a directory entry failed: {err}");
            return err.code();
        }
        session.next += 1;
    }
    // Ran out of entries: an empty (or short) reply is how the end is reported.
    OK
}

unsafe extern "system" fn end_enumeration(
    cb: *const PRJ_CALLBACK_DATA,
    enum_id: *const GUID,
) -> HRESULT {
    let (Some((prov, _)), Some(id)) = (unsafe { provider(cb) }, unsafe { enum_id.as_ref() }) else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    if let Ok(mut g) = prov.enums.lock() {
        g.remove(&id.to_u128());
    }
    OK
}

/// A `stat`: give ProjFS the metadata for one path so it can create a
/// placeholder for it.
unsafe extern "system" fn get_placeholder_info(cb: *const PRJ_CALLBACK_DATA) -> HRESULT {
    let Some((prov, data)) = (unsafe { provider(cb) }) else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    let rel = unsafe { from_wide(data.FilePathName) };
    let entry = match prov.resolve(&rel) {
        Ok(e) => e,
        Err(c) => return hresult_of(c),
    };
    let info = PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: basic_info(&entry),
        ..Default::default()
    };
    let name = wide(&rel);
    match unsafe {
        PrjWritePlaceholderInfo(
            data.NamespaceVirtualizationContext,
            PCWSTR(name.as_ptr()),
            &info,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    } {
        Ok(()) => OK,
        Err(e) => {
            tracing::warn!("fs-projfs: writing a placeholder failed: {e}");
            e.code()
        }
    }
}

/// Hydrate: fetch a range of a file and hand it to ProjFS.
///
/// Chunked, because ProjFS may ask for an entire file in one callback and a
/// 3.4 GB request must not become a 3.4 GB allocation. Every chunk but the
/// file's last is a whole multiple of the volume's write alignment, which is
/// what `PrjWriteFileData` requires of a multi-part write.
unsafe extern "system" fn get_file_data(
    cb: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    let Some((prov, data)) = (unsafe { provider(cb) }) else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    let rel = unsafe { from_wide(data.FilePathName) };
    let entry = match prov.resolve(&rel) {
        Ok(e) => e,
        Err(c) => return hresult_of(c),
    };
    let fh = match prov.block_meta(prov.vfs.open(&entry.path)) {
        Ok(fh) => fh,
        Err(c) => return hresult_of(c),
    };
    let r = hydrate(prov, data, fh, byte_offset, length);
    prov.block(prov.vfs.close(fh));
    r
}

fn hydrate(
    prov: &Provider,
    data: &PRJ_CALLBACK_DATA,
    fh: u64,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    let ctx = data.NamespaceVirtualizationContext;
    let align = prov.write_alignment.load(Ordering::Relaxed).max(1);
    // Round the chunk UP to the alignment: a chunk that is a whole number of
    // alignment units keeps every write but the last one legal.
    let chunk = HYDRATE_CHUNK.div_ceil(align).saturating_mul(align).max(align);

    let Some(buf) = AlignedBuffer::new(ctx, chunk as usize) else {
        return HRESULT::from_win32(ERROR_IO_DEVICE.0);
    };
    let end = byte_offset.saturating_add(length as u64);
    let mut at = byte_offset;
    while at < end {
        let want = (end - at).min(chunk as u64) as u32;
        let bytes = match prov.block(prov.vfs.read_range(fh, at, want)) {
            Ok(b) => b,
            Err(c) => return hresult_of(c),
        };
        if bytes.len() != want as usize {
            // ProjFS asked for a definite range and will treat anything less as
            // a corrupt hydration. A file that shrank on the phone mid-read is
            // the honest cause; an error is the honest answer.
            tracing::warn!(
                at,
                want,
                got = bytes.len(),
                "fs-projfs: short read while hydrating"
            );
            return HRESULT::from_win32(ERROR_IO_DEVICE.0);
        }
        unsafe { buf.fill(&bytes) };
        if let Err(e) =
            unsafe { PrjWriteFileData(ctx, &data.DataStreamId, buf.ptr, at, bytes.len() as u32) }
        {
            tracing::warn!("fs-projfs: writing file data failed: {e}");
            return e.code();
        }
        at += bytes.len() as u64;
    }
    OK
}

/// A buffer from `PrjAllocateAlignedBuffer`, freed on drop.
///
/// `PrjWriteFileData` requires its buffer to meet the volume's alignment, which
/// an ordinary `Vec` does not guarantee. Owning it in a guard is what makes the
/// early returns in [`hydrate`] leak-free.
struct AlignedBuffer {
    ptr: *mut core::ffi::c_void,
    len: usize,
}

impl AlignedBuffer {
    fn new(ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, len: usize) -> Option<Self> {
        let ptr = unsafe { PrjAllocateAlignedBuffer(ctx, len) };
        (!ptr.is_null()).then_some(Self { ptr, len })
    }

    /// # Safety
    /// `src` must be no longer than the buffer.
    unsafe fn fill(&self, src: &[u8]) {
        debug_assert!(src.len() <= self.len);
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr as *mut u8, src.len()) };
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { PrjFreeAlignedBuffer(self.ptr) };
    }
}

/// Does this name exist? Asked before ProjFS commits to creating a placeholder.
///
/// Answering it is what keeps a miss cheap: without it, every probe for a file
/// that is not there costs a placeholder attempt.
unsafe extern "system" fn query_file_name(cb: *const PRJ_CALLBACK_DATA) -> HRESULT {
    let Some((prov, data)) = (unsafe { provider(cb) }) else {
        return HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
    };
    let rel = unsafe { from_wide(data.FilePathName) };
    match prov.resolve(&rel) {
        Ok(_) => OK,
        Err(code::NOENT) => HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0),
        Err(c) => hresult_of(c),
    }
}

/// Refuse every operation that would change the projection.
///
/// ProjFS has no mount-level read-only flag the way FUSE does, so this is what
/// makes the projection read-only in fact rather than by convention: returning
/// a failure from a `PRE_` notification is how a provider vetoes the operation
/// that triggered it. The `FILE_ATTRIBUTE_READONLY` on every placeholder is the
/// advisory half — it makes Explorer grey the commands out rather than offer
/// them and fail here.
unsafe extern "system" fn notification(
    _cb: *const PRJ_CALLBACK_DATA,
    _is_directory: bool,
    notification: PRJ_NOTIFICATION,
    _destination: PCWSTR,
    _params: *mut PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    match notification {
        PRJ_NOTIFICATION_PRE_DELETE
        | PRJ_NOTIFICATION_PRE_RENAME
        | PRJ_NOTIFICATION_PRE_SET_HARDLINK
        // "Convert to full" is the moment a placeholder would become a real,
        // writable file. Refusing it is what stops an edit in place.
        | PRJ_NOTIFICATION_FILE_PRE_CONVERT_TO_FULL => {
            HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
        }
        _ => OK,
    }
}

// ---------------------------------------------------------------------------
// Mount lifecycle
// ---------------------------------------------------------------------------

/// Where the phone's files appear.
///
/// Under `%LOCALAPPDATA%` because ProjFS hydrates content into the real
/// directory: it has to be a local NTFS path with room on it, which rules out
/// the roaming profile and a network home directory both.
pub(crate) fn mount_point() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Vortex").join("phone")
}

pub(crate) fn is_mounted() -> bool {
    instance_slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Start projecting the phone's storage. Returns the projection root.
pub(crate) async fn mount() -> Result<PathBuf, String> {
    let dir = mount_point();
    let rt = tokio::runtime::Handle::current();
    // `PrjStartVirtualizing` sets up a kernel filter and a thread pool; it does
    // not belong on an async thread.
    tokio::task::spawn_blocking(move || start(rt, dir))
        .await
        .map_err(|e| format!("mount task failed: {e}"))?
}

fn start(rt: tokio::runtime::Handle, dir: PathBuf) -> Result<PathBuf, String> {
    // Clear the projection before marking it. ProjFS serves hydrated content
    // from disk without asking us, so anything left by a previous session would
    // be served as current — see the module docs. Contents only, and only ever
    // our own directory under LOCALAPPDATA.
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            let _ = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let root = wide(&dir.to_string_lossy());
    // Marking is once per directory, not once per run: a directory that is
    // already our virtualization root comes back as an error we expect and
    // ignore, because the previous run left it correctly marked.
    if let Err(e) = unsafe {
        PrjMarkDirectoryAsPlaceholder(PCWSTR(root.as_ptr()), PCWSTR::null(), None, &INSTANCE_ID)
    } {
        tracing::debug!("fs-projfs: the root was already a placeholder ({e})");
    }

    let provider = Box::into_raw(Box::new(Provider {
        vfs: Arc::new(Vfs::new(LinkRemote)),
        rt,
        enums: Mutex::new(HashMap::new()),
        // 64 KiB until the real figure arrives, NOT 1: ProjFS may call back
        // the instant virtualization starts, which is before the
        // `PrjGetVirtualizationInstanceInfo` below has returned. Rounding
        // HYDRATE_CHUNK up to 64 KiB yields a chunk that is also a whole
        // multiple of 512 and 4096, so a hydration landing in that window is
        // legal whatever the volume turns out to want.
        write_alignment: AtomicU32::new(64 * 1024),
    }));

    let callbacks = PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_enumeration),
        GetDirectoryEnumerationCallback: Some(get_enumeration),
        EndDirectoryEnumerationCallback: Some(end_enumeration),
        GetPlaceholderInfoCallback: Some(get_placeholder_info),
        GetFileDataCallback: Some(get_file_data),
        QueryFileNameCallback: Some(query_file_name),
        NotificationCallback: Some(notification),
        CancelCommandCallback: None,
    };
    // One mapping over the whole tree: every `PRE_` operation that would modify
    // the projection has to reach `notification` for it to be refused. Bound
    // before `mappings` so it outlives the pointer taken into it.
    let notify_root = wide("");
    let mut mappings = [PRJ_NOTIFICATION_MAPPING {
        NotificationBitMask: PRJ_NOTIFY_TYPES(
            PRJ_NOTIFY_PRE_DELETE.0
                | PRJ_NOTIFY_PRE_RENAME.0
                | PRJ_NOTIFY_PRE_SET_HARDLINK.0
                | PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL.0,
        ),
        // The empty string is the virtualization root itself, so the mapping
        // covers all of it rather than one subtree. An empty string, NOT null:
        // that is what the API documents, and a null here would be a provider
        // with no veto at all — which fails open, as a writable projection.
        NotificationRoot: PCWSTR(notify_root.as_ptr()),
    }];
    let options = PRJ_STARTVIRTUALIZING_OPTIONS {
        Flags: PRJ_FLAG_NONE,
        PoolThreadCount: POOL_THREADS,
        ConcurrentThreadCount: POOL_THREADS,
        NotificationMappings: mappings.as_mut_ptr(),
        NotificationMappingsCount: mappings.len() as u32,
    };

    let ctx = match unsafe {
        PrjStartVirtualizing(
            PCWSTR(root.as_ptr()),
            &callbacks,
            Some(provider as *const core::ffi::c_void),
            Some(&options),
        )
    } {
        Ok(ctx) => ctx,
        Err(e) => {
            // Reclaim the provider: nothing will ever call back into it.
            drop(unsafe { Box::from_raw(provider) });
            // The likeliest cause by far, and one the user can act on. ProjFS
            // is an optional Windows feature, off by default on client SKUs.
            return Err(format!(
                "could not start projecting {}: {e}. If this says the request \
                 is not supported, enable the \"Windows Projected File System\" \
                 optional feature and restart.",
                dir.display()
            ));
        }
    };

    // The alignment every `PrjWriteFileData` has to respect. Asked for once,
    // now that there is an instance to ask about.
    let mut info = PRJ_VIRTUALIZATION_INSTANCE_INFO::default();
    if unsafe { PrjGetVirtualizationInstanceInfo(ctx, &mut info) }.is_ok() {
        // SAFETY: `provider` is live — virtualization started, so nothing has
        // freed it — and this runs before any callback can read the field.
        unsafe { &*provider }
            .write_alignment
            .store(info.WriteAlignment.max(1), Ordering::Relaxed);
    }

    if let Ok(mut g) = instance_slot().lock() {
        *g = Some(Instance {
            ctx,
            provider,
            root: dir.clone(),
        });
    }
    tracing::info!(path = %dir.display(), alignment = info.WriteAlignment, "fs-projfs: projecting");
    Ok(dir)
}

/// Stop projecting, if we are.
pub(crate) fn unmount() {
    let taken = instance_slot().lock().ok().and_then(|mut g| g.take());
    if let Some(inst) = taken {
        // Blocking: it waits for in-flight callbacks to finish, which is
        // precisely what makes reclaiming the provider afterwards safe.
        std::thread::spawn(move || {
            // Move the WHOLE `Instance`, not its fields. Rust 2021 closures
            // capture disjointly, so naming `inst.ctx` and `inst.provider`
            // would capture two raw pointers — neither of which is `Send` —
            // instead of the struct whose `unsafe impl Send` vouches for them.
            let inst = inst;
            unsafe { PrjStopVirtualizing(inst.ctx) };
            // SAFETY: `PrjStopVirtualizing` has returned, so no callback can
            // still hold this pointer.
            drop(unsafe { Box::from_raw(inst.provider as *mut Provider) });
            tracing::info!(path = %inst.root.display(), "fs-projfs: stopped");
        });
    }
}

/// Stop projecting on the process's way out.
///
/// Unlike a FUSE mount, an abandoned projection leaves nothing broken in the
/// filesystem — the directory is a real directory either way, and Windows drops
/// the virtualization when the process goes. So this only has to be tidy, not
/// urgent: stop the instance and let the leaked provider go with the process.
pub(crate) fn unmount_on_exit() {
    let taken = instance_slot().lock().ok().and_then(|mut g| g.take());
    if let Some(inst) = taken {
        unsafe { PrjStopVirtualizing(inst.ctx) };
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Only the parts that do not call into projectedfslib: everything ProjFS itself
// answers needs Windows, and the logic above it — the path walk, the cache, the
// read splitting — is tested once in `fs_vfs` against a fake peer.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_vfs::tests::{dir, file};

    #[test]
    fn every_protocol_code_reaches_the_user_as_itself() {
        assert_eq!(
            hresult_of(code::NOENT),
            HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
        );
        assert_eq!(
            hresult_of(code::ACCES),
            HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
        );
        assert_eq!(
            hresult_of(code::ROFS),
            HRESULT::from_win32(ERROR_WRITE_PROTECT.0)
        );
        assert_eq!(
            hresult_of(crate::fs_link::NO_LINK),
            HRESULT::from_win32(ERROR_HOST_DOWN.0),
            "an absent phone must read as 'host is down', not a disk error"
        );
        assert_eq!(
            hresult_of(9999),
            HRESULT::from_win32(ERROR_IO_DEVICE.0),
            "an unknown code is still an error"
        );
    }

    #[test]
    fn a_unix_timestamp_becomes_the_same_moment_in_filetime() {
        // The Unix epoch is 11,644,473,600 seconds after the FILETIME epoch.
        assert_eq!(filetime_of(0), 11_644_473_600 * 10_000_000);
        assert_eq!(
            filetime_of(1_700_000_000),
            (1_700_000_000 + 11_644_473_600) * 10_000_000
        );
    }

    #[test]
    fn a_nonsense_clock_does_not_overflow_the_mount() {
        // A phone set before 1601, and one set past the year 30000: both are
        // absurd, and neither may panic a filesystem callback.
        assert_eq!(filetime_of(-1_000_000_000_000), 0);
        assert_eq!(filetime_of(i64::MAX), i64::MAX);
        assert_eq!(filetime_of(i64::MIN), 0);
    }

    #[test]
    fn a_directory_and_a_file_carry_the_right_attributes() {
        let d = basic_info(&dir("DCIM", "/sdcard/DCIM"));
        assert!(d.IsDirectory);
        assert_eq!(d.FileSize, 0);
        assert_ne!(d.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
        assert_ne!(
            d.FileAttributes & FILE_ATTRIBUTE_READONLY.0,
            0,
            "the projection is read-only"
        );

        let f = basic_info(&file("a.txt", "/sdcard/a.txt", 5));
        assert!(!f.IsDirectory);
        assert_eq!(f.FileSize, 5);
        assert_eq!(f.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
        assert_ne!(f.FileAttributes & FILE_ATTRIBUTE_READONLY.0, 0);
        // One protocol timestamp, reported as all four — better than three
        // zeroes, which Explorer renders as 1601.
        assert_eq!(f.LastWriteTime, filetime_of(1_700_000_000));
        assert_eq!(f.CreationTime, f.LastWriteTime);
    }

    #[test]
    fn a_string_survives_the_round_trip_through_utf16() {
        for s in ["DCIM", "", "Ärger\\naïve", "日本語", "a b.txt"] {
            let w = wide(s);
            assert_eq!(w.last(), Some(&0), "must be NUL-terminated");
            assert_eq!(unsafe { from_wide(PCWSTR(w.as_ptr())) }, s);
        }
    }

    #[test]
    fn a_null_path_reads_as_the_root() {
        // ProjFS addresses the virtualization root with an empty string, and a
        // null is what several of its fields carry when unset. Both have to
        // mean "the root" rather than panic.
        assert_eq!(unsafe { from_wide(PCWSTR::null()) }, "");
    }

    #[test]
    fn the_hydrate_chunk_is_a_whole_number_of_alignment_units() {
        // The rule `PrjWriteFileData` imposes: every chunk but a file's last
        // must be a multiple of the volume's write alignment.
        for align in [1u32, 512, 4096, 64 * 1024, 2 * 1024 * 1024] {
            let chunk = HYDRATE_CHUNK.div_ceil(align).saturating_mul(align).max(align);
            assert_eq!(chunk % align, 0, "align={align}");
            assert!(chunk >= align, "align={align}");
            assert!(chunk >= HYDRATE_CHUNK.min(align), "align={align}");
        }
    }
}
