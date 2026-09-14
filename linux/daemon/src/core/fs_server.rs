//! Serves the ranged-filesystem protocol against this machine's real files.
//!
//! The counterpart of [`crate::core::fs_proto`]: that module is the wire format
//! and the policy gate, this one does the I/O. Split so the gate is testable
//! without touching a disk, and so the Android side can mirror the wire format
//! without mirroring any of this.
//!
//! Everything here is synchronous and expected to run on a blocking thread —
//! reads are bounded to [`fs_proto::MAX_READ_LEN`], so no single call is long,
//! but a stalled network filesystem must not block an async executor.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::fs_proto::{self as p, code, FsEntry, FsErr, FsReply};

/// An idle handle is dropped after this long.
///
/// A consumer that dies mid-copy (a file manager killed, a mount unmounted)
/// never sends `CLOSE`, and an open file descriptor per abandoned read would
/// eventually exhaust the process. Reopening transparently is cheap; leaking is
/// not.
const HANDLE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent open handles. Bounded so a peer cannot exhaust our descriptors by
/// opening in a loop and never closing.
const MAX_HANDLES: usize = 64;

struct Handle {
    file: File,
    writable: bool,
    last_used: Instant,
}

/// Open handles for one peer.
///
/// Per-peer rather than global: with several paired phones, one peer's handle
/// ids must not address another's files. The multi-peer work made "whose
/// statement is this" a load-bearing question everywhere else in the codebase,
/// and a handle table is no different.
#[derive(Default)]
pub struct FsHandles {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    open: HashMap<u64, Handle>,
}

impl FsHandles {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, h: Handle) -> Result<u64, i32> {
        let mut g = self.inner.lock().map_err(|_| code::IO)?;
        prune(&mut g);
        if g.open.len() >= MAX_HANDLES {
            return Err(code::IO);
        }
        // Start at 1 so 0 is never a valid handle — it is the value a buggy
        // consumer is most likely to send by accident.
        g.next = g.next.wrapping_add(1).max(1);
        let id = g.next;
        g.open.insert(id, h);
        Ok(id)
    }

    /// Number of live handles. Diagnostics only.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.open.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn with<T>(&self, id: u64, f: impl FnOnce(&mut Handle) -> Result<T, i32>) -> Result<T, i32> {
        let mut g = self.inner.lock().map_err(|_| code::IO)?;
        prune(&mut g);
        let h = g.open.get_mut(&id).ok_or(code::BADF)?;
        h.last_used = Instant::now();
        f(h)
    }

    fn remove(&self, id: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.open.remove(&id);
        }
    }

    /// Drop every handle — called when a peer's session ends, so a reconnect
    /// starts from a clean table rather than inheriting stale ids.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.open.clear();
        }
    }
}

fn prune(g: &mut Inner) {
    let now = Instant::now();
    g.open
        .retain(|_, h| now.duration_since(h.last_used) < HANDLE_IDLE_TIMEOUT);
}

/// What a served op produced. The caller turns this into frames — this module
/// deliberately knows nothing about framing or transports.
pub enum Served {
    /// Send as `FS_META`.
    Meta(FsReply),
    /// Send as `FS_DATA` — already includes its binary header.
    Data(Vec<u8>),
    /// Send as `FS_ERR`.
    Err(FsErr),
}

impl Served {
    fn err(id: u32, c: i32, msg: &str) -> Self {
        Served::Err(FsErr::new(id, c, msg))
    }
}

/// Serve one request.
///
/// `roots` is the policy gate; every path-taking op resolves through it, and a
/// path outside every root is refused before any I/O happens.
pub fn serve(
    roots: &p::Roots,
    handles: &FsHandles,
    op: u8,
    payload: &[u8],
) -> Served {
    match op {
        p::op::LIST => match serde_json::from_slice::<p::ListReq>(payload) {
            Ok(r) => do_list(roots, &r),
            Err(_) => Served::err(0, code::INVAL, "malformed LIST"),
        },
        p::op::STAT => match serde_json::from_slice::<p::StatReq>(payload) {
            Ok(r) => do_stat(roots, &r),
            Err(_) => Served::err(0, code::INVAL, "malformed STAT"),
        },
        p::op::OPEN => match serde_json::from_slice::<p::OpenReq>(payload) {
            Ok(r) => do_open(roots, handles, &r),
            Err(_) => Served::err(0, code::INVAL, "malformed OPEN"),
        },
        p::op::READ => match serde_json::from_slice::<p::ReadReq>(payload) {
            Ok(r) => do_read(handles, &r),
            Err(_) => Served::err(0, code::INVAL, "malformed READ"),
        },
        p::op::WRITE => match p::decode_write(payload) {
            Some((r, bytes)) => do_write(handles, &r, bytes),
            None => Served::err(0, code::INVAL, "malformed WRITE"),
        },
        p::op::CLOSE => match serde_json::from_slice::<p::CloseReq>(payload) {
            Ok(r) => {
                handles.remove(r.handle);
                // Not BADF for an unknown handle: we expire handles ourselves,
                // so "already gone" is exactly the state the caller wanted.
                Served::Meta(FsReply::Ok { id: r.id })
            }
            Err(_) => Served::err(0, code::INVAL, "malformed CLOSE"),
        },
        p::op::SETMETA => match serde_json::from_slice::<p::SetMetaReq>(payload) {
            Ok(r) => do_setmeta(roots, &r),
            Err(_) => Served::err(0, code::INVAL, "malformed SETMETA"),
        },
        other => {
            // Answer, do not drop. An unimplemented op that behaves like a
            // timeout hangs the far side's file manager.
            tracing::debug!("fs: unsupported op 0x{other:02x}");
            Served::err(0, code::NOTSUP, "unsupported op")
        }
    }
}

fn do_list(roots: &p::Roots, r: &p::ListReq) -> Served {
    // The empty path is the synthetic root: it lists the served roots
    // themselves, so a peer can discover what it may see without being told
    // the paths out of band.
    if r.path == "/" || r.path.is_empty() {
        if roots.list().len() != 1 {
            let entries = roots
                .list()
                .iter()
                .map(|root| FsEntry {
                    name: root.path.to_string_lossy().to_string(),
                    path: root.path.to_string_lossy().to_string(),
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    readonly: !root.writable,
                })
                .collect();
            return Served::Meta(FsReply::List {
                id: r.id,
                entries,
                cursor: None,
            });
        }
        // With exactly one root, a synthetic level above it would be a folder
        // the user has to click through every time for no information.
    }
    // Which path to actually list. The single-root case above deliberately does
    // NOT interpose a synthetic level, so the root's own path has to be
    // substituted for the empty request here. Falling through with the empty
    // path reached `resolve("")`, which is INVAL — so a peer asking for the
    // root of a one-root device got "path refused" and could not browse at all.
    // That is the DEFAULT configuration, and it is what the phone's browser hit
    // on its first run.
    let requested: String = if (r.path == "/" || r.path.is_empty()) && roots.list().len() == 1 {
        roots.list()[0].path.to_string_lossy().to_string()
    } else {
        r.path.clone()
    };
    let path = match resolve_or(roots, &requested, false, r.id) {
        Ok(p) => p,
        Err(s) => return s,
    };
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return Served::Err(FsErr::new(r.id, io_code(&e), "read_dir failed")),
    };
    // Stable order so pagination is coherent: `read_dir` order is unspecified
    // and can differ between calls, which would make a cursor meaningless.
    let mut names: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
    names.sort();

    let start = r.cursor as usize;
    let end = (start + p::LIST_PAGE).min(names.len());
    let mut entries = Vec::with_capacity(end.saturating_sub(start));
    for name in &names[start.min(names.len())..end] {
        let full = path.join(name);
        // A single unreadable entry must not fail the whole page — a folder
        // with one broken symlink would otherwise be unlistable.
        let md = match std::fs::symlink_metadata(&full) {
            Ok(md) => md,
            Err(_) => continue,
        };
        entries.push(FsEntry {
            name: name.to_string_lossy().to_string(),
            // The absolute path IS the address on a real filesystem, but the
            // consumer must not assume that — it joins nothing itself.
            path: full.to_string_lossy().to_string(),
            is_dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            mtime: mtime_secs(&md),
            readonly: md.permissions().readonly(),
        });
    }
    Served::Meta(FsReply::List {
        id: r.id,
        entries,
        cursor: if end < names.len() {
            Some(end as u32)
        } else {
            None
        },
    })
}

fn do_stat(roots: &p::Roots, r: &p::StatReq) -> Served {
    let path = match resolve_or(roots, &r.path, false, r.id) {
        Ok(p) => p,
        Err(s) => return s,
    };
    let md = match std::fs::metadata(&path) {
        Ok(md) => md,
        Err(e) => return Served::Err(FsErr::new(r.id, io_code(&e), "stat failed")),
    };
    Served::Meta(FsReply::Stat {
        id: r.id,
        entry: FsEntry {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: path.to_string_lossy().to_string(),
            is_dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            mtime: mtime_secs(&md),
            readonly: md.permissions().readonly(),
        },
    })
}

fn do_open(roots: &p::Roots, handles: &FsHandles, r: &p::OpenReq) -> Served {
    let path = match resolve_or(roots, &r.path, r.write, r.id) {
        Ok(p) => p,
        Err(s) => return s,
    };
    if path.is_dir() {
        return Served::err(r.id, code::ISDIR, "open on a directory");
    }
    let file = if r.write {
        // Create the parent chain: an upload into a folder the peer names is
        // the whole point of a writable root, and requiring the directory to
        // pre-exist would make that fail for no good reason.
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Served::Err(FsErr::new(r.id, io_code(&e), "mkdir failed"));
            }
        }
        // NOT truncating: writes are ranged, so a consumer may legitimately
        // fill a file out of order. Truncation is the caller's business.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    } else {
        File::open(&path)
    };
    let file = match file {
        Ok(f) => f,
        Err(e) => return Served::Err(FsErr::new(r.id, io_code(&e), "open failed")),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let readonly = !r.write;
    match handles.insert(Handle {
        file,
        writable: r.write,
        last_used: Instant::now(),
    }) {
        Ok(handle) => Served::Meta(FsReply::Open {
            id: r.id,
            handle,
            size,
            readonly,
        }),
        Err(c) => Served::Err(FsErr::new(r.id, c, "too many open handles")),
    }
}

fn do_read(handles: &FsHandles, r: &p::ReadReq) -> Served {
    if r.len == 0 || r.len > p::MAX_READ_LEN {
        return Served::err(r.id, code::INVAL, "read length out of range");
    }
    let out = handles.with(r.handle, |h| {
        h.file.seek(SeekFrom::Start(r.offset)).map_err(|e| io_code(&e))?;
        let mut buf = vec![0u8; r.len as usize];
        let mut got = 0usize;
        // Loop: a single `read` is allowed to return short for reasons that
        // have nothing to do with EOF, and a consumer that treated every short
        // read as EOF would silently truncate files.
        while got < buf.len() {
            match h.file.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_code(&e)),
            }
        }
        buf.truncate(got);
        let size = h.file.metadata().map(|m| m.len()).unwrap_or(0);
        let eof = r.offset.saturating_add(got as u64) >= size;
        Ok((buf, eof))
    });
    match out {
        Ok((buf, eof)) => Served::Data(p::encode_data(r.id, r.offset, eof, &buf)),
        Err(c) => Served::Err(FsErr::new(r.id, c, "read failed")),
    }
}

fn do_write(handles: &FsHandles, r: &p::WriteReq, bytes: &[u8]) -> Served {
    let out = handles.with(r.handle, |h| {
        if !h.writable {
            return Err(code::ROFS);
        }
        h.file.seek(SeekFrom::Start(r.offset)).map_err(|e| io_code(&e))?;
        h.file.write_all(bytes).map_err(|e| io_code(&e))?;
        Ok(bytes.len() as u32)
    });
    match out {
        Ok(n) => Served::Meta(FsReply::Wrote { id: r.id, bytes: n }),
        Err(c) => Served::Err(FsErr::new(r.id, c, "write failed")),
    }
}

fn do_setmeta(roots: &p::Roots, r: &p::SetMetaReq) -> Served {
    let path = match resolve_or(roots, &r.path, true, r.id) {
        Ok(p) => p,
        Err(s) => return s,
    };
    if let Some(name) = &r.rename_to {
        // A rename target is a NAME, not a path: accepting a path would let a
        // peer move a file out of its root using the destination instead of
        // the source, which the source-side gate above would never see.
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || *name == ".."
            || *name == "."
        {
            return Served::err(r.id, code::INVAL, "rename_to must be a bare name");
        }
        let Some(parent) = path.parent() else {
            return Served::err(r.id, code::INVAL, "no parent");
        };
        let dest = parent.join(name);
        // Re-gate the destination: same root, and writable.
        if let Err(c) = roots.resolve(&dest.to_string_lossy(), true) {
            return Served::Err(FsErr::new(r.id, c, "rename destination refused"));
        }
        if let Err(e) = std::fs::rename(&path, &dest) {
            return Served::Err(FsErr::new(r.id, io_code(&e), "rename failed"));
        }
    }
    if r.mtime.is_some() {
        // Honest refusal rather than a silent no-op: a consumer that believes
        // it set an mtime will cache against a value that never changed.
        return Served::err(r.id, code::NOTSUP, "mtime not supported");
    }
    Served::Meta(FsReply::Ok { id: r.id })
}

fn resolve_or(
    roots: &p::Roots,
    path: &str,
    for_write: bool,
    id: u32,
) -> Result<PathBuf, Served> {
    roots
        .resolve(path, for_write)
        // The path is NOT included in the message: it is user data and this
        // goes to the log.
        .map_err(|c| Served::Err(FsErr::new(id, c, "path refused")))
}

fn io_code(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => code::NOENT,
        K::PermissionDenied => code::ACCES,
        K::InvalidInput | K::InvalidData => code::INVAL,
        K::IsADirectory => code::ISDIR,
        _ => code::IO,
    }
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Load the served roots, writing the documented default on first run.
///
/// Uses the platform seam's config root, so this is `~/.config/vortex` on Linux
/// and `%APPDATA%\Vortex` on Windows without a second code path.
pub fn load_roots() -> p::Roots {
    let Some(dir) = crate::core::platform::paths().config() else {
        tracing::warn!("fs: no config dir; serving nothing");
        return p::Roots::default();
    };
    let path = dir.join("fs-roots.conf");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let roots = p::parse_roots(&text);
        tracing::info!("fs: serving {} root(s)", roots.list().len());
        return roots;
    }
    // First run: write the default so the file exists to be edited. Without a
    // home directory there is nothing sensible to serve, so serve nothing
    // rather than guess.
    let Some(home) = home_dir() else {
        tracing::warn!("fs: no home dir; serving nothing");
        return p::Roots::default();
    };
    let text = p::default_roots_file(&home);
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &text)) {
        tracing::warn!("fs: couldn't write {}: {e}", path.display());
    } else {
        tracing::info!("fs: wrote default roots config to {}", path.display());
    }
    p::parse_roots(&text)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Now, in seconds since the epoch. Used by callers building `FsEntry`s for
/// synthetic paths.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fs_proto::Root;

    /// The empty path against a ONE-root device must list that root.
    ///
    /// It used to answer INVAL: the single-root branch skips the synthetic
    /// listing (rightly — a level with one entry is a click for nothing) but
    /// then resolved the still-empty path. One root is the default config, so
    /// the default device could not be browsed at all.
    #[test]
    fn empty_path_lists_the_only_root() {
        let dir = scratch("one-root");
        std::fs::write(dir.join("a.txt"), b"hi").expect("write");
        let roots = p::Roots::new(vec![Root {
            path: dir.clone(),
            writable: false,
        }]);
        let handles = FsHandles::new();
        let req = serde_json::to_vec(&serde_json::json!({"id": 1, "path": "", "cursor": 0}))
            .expect("json");
        match serve(&roots, &handles, p::op::LIST, &req) {
            Served::Meta(FsReply::List { entries, .. }) => {
                assert!(
                    entries.iter().any(|e| e.name == "a.txt"),
                    "expected the root's contents, got {entries:?}"
                );
            }
            Served::Err(e) => panic!("expected a listing, got error {}: {}", e.code, e.msg),
            _ => panic!("expected a listing"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vortex-fsserver-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn rw_roots(dir: &PathBuf) -> p::Roots {
        p::Roots::new(vec![Root {
            path: dir.clone(),
            writable: true,
        }])
    }

    #[test]
    fn open_read_reports_eof_and_survives_a_bounded_range() {
        let dir = scratch("read");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"0123456789").expect("write");
        let roots = rw_roots(&dir);
        let handles = FsHandles::new();

        let open = serde_json::to_vec(&p::OpenReq {
            id: 1,
            path: file.to_string_lossy().to_string(),
            write: false,
        })
        .unwrap();
        let handle = match serve(&roots, &handles, p::op::OPEN, &open) {
            Served::Meta(FsReply::Open { handle, size, .. }) => {
                assert_eq!(size, 10);
                handle
            }
            _ => panic!("open failed"),
        };
        assert_ne!(handle, 0, "0 must never be a valid handle");

        // A mid-file read is not EOF...
        let req = serde_json::to_vec(&p::ReadReq {
            id: 2,
            handle,
            offset: 0,
            len: 4,
        })
        .unwrap();
        match serve(&roots, &handles, p::op::READ, &req) {
            Served::Data(d) => {
                let (id, off, eof, bytes) = p::decode_data(&d).expect("decodes");
                assert_eq!((id, off, eof, bytes), (2, 0, false, b"0123".as_slice()));
            }
            _ => panic!("read failed"),
        }
        // ...and a read that reaches the end is.
        let req = serde_json::to_vec(&p::ReadReq {
            id: 3,
            handle,
            offset: 6,
            len: 100,
        })
        .unwrap();
        match serve(&roots, &handles, p::op::READ, &req) {
            Served::Data(d) => {
                let (_, _, eof, bytes) = p::decode_data(&d).expect("decodes");
                assert!(eof);
                assert_eq!(bytes, b"6789");
            }
            _ => panic!("read failed"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_past_max_len_is_refused_rather_than_silently_clamped() {
        let dir = scratch("maxlen");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"x").expect("write");
        let roots = rw_roots(&dir);
        let handles = FsHandles::new();
        let open = serde_json::to_vec(&p::OpenReq {
            id: 1,
            path: file.to_string_lossy().to_string(),
            write: false,
        })
        .unwrap();
        let handle = match serve(&roots, &handles, p::op::OPEN, &open) {
            Served::Meta(FsReply::Open { handle, .. }) => handle,
            _ => panic!("open"),
        };
        let req = serde_json::to_vec(&p::ReadReq {
            id: 2,
            handle,
            offset: 0,
            len: p::MAX_READ_LEN + 1,
        })
        .unwrap();
        // Clamping would make the reply's length silently disagree with the
        // request, and a consumer computing offsets from what it asked for
        // would corrupt the file.
        match serve(&roots, &handles, p::op::READ, &req) {
            Served::Err(e) => assert_eq!(e.code, code::INVAL),
            _ => panic!("expected INVAL"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranged_writes_can_arrive_out_of_order() {
        let dir = scratch("write");
        let file = dir.join("out.bin");
        let roots = rw_roots(&dir);
        let handles = FsHandles::new();
        let open = serde_json::to_vec(&p::OpenReq {
            id: 1,
            path: file.to_string_lossy().to_string(),
            write: true,
        })
        .unwrap();
        let handle = match serve(&roots, &handles, p::op::OPEN, &open) {
            Served::Meta(FsReply::Open { handle, .. }) => handle,
            _ => panic!("open for write failed"),
        };
        // Second half first: a pull that parallelises ranges must not depend on
        // arrival order.
        for (off, data) in [(4u64, b"DEFG".as_slice()), (0u64, b"ABCD".as_slice())] {
            let pl = p::encode_write(
                &p::WriteReq {
                    id: 2,
                    handle,
                    offset: off,
                },
                data,
            );
            match serve(&roots, &handles, p::op::WRITE, &pl) {
                Served::Meta(FsReply::Wrote { bytes, .. }) => assert_eq!(bytes, 4),
                _ => panic!("write failed"),
            }
        }
        assert_eq!(std::fs::read(&file).expect("read back"), b"ABCDDEFG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_root_refuses_open_for_write_with_rofs() {
        let dir = scratch("ro");
        let roots = p::Roots::new(vec![Root {
            path: dir.clone(),
            writable: false,
        }]);
        let handles = FsHandles::new();
        let open = serde_json::to_vec(&p::OpenReq {
            id: 1,
            path: dir.join("nope.bin").to_string_lossy().to_string(),
            write: true,
        })
        .unwrap();
        match serve(&roots, &handles, p::op::OPEN, &open) {
            Served::Err(e) => assert_eq!(e.code, code::ROFS),
            _ => panic!("expected ROFS"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_paginates_in_a_stable_order() {
        let dir = scratch("list");
        for i in 0..(p::LIST_PAGE + 10) {
            std::fs::write(dir.join(format!("f{i:05}")), b"").expect("write");
        }
        let roots = rw_roots(&dir);
        let handles = FsHandles::new();

        let mut seen = Vec::new();
        let mut cursor = 0u32;
        loop {
            let req = serde_json::to_vec(&p::ListReq {
                id: 1,
                path: dir.to_string_lossy().to_string(),
                cursor,
            })
            .unwrap();
            match serve(&roots, &handles, p::op::LIST, &req) {
                Served::Meta(FsReply::List { entries, cursor: c, .. }) => {
                    seen.extend(entries.into_iter().map(|e| e.name));
                    match c {
                        Some(next) => cursor = next,
                        None => break,
                    }
                }
                _ => panic!("list failed"),
            }
        }
        assert_eq!(seen.len(), p::LIST_PAGE + 10);
        // No duplicates and no gaps: an unstable order would produce both.
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "pagination lost or repeated entries");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_op_is_answered_notsup_rather_than_dropped() {
        let roots = p::Roots::default();
        let handles = FsHandles::new();
        match serve(&roots, &handles, 0xEE, b"{}") {
            Served::Err(e) => assert_eq!(e.code, code::NOTSUP),
            _ => panic!("expected NOTSUP"),
        }
    }

    #[test]
    fn a_stale_handle_is_badf_not_a_panic() {
        let handles = FsHandles::new();
        let roots = p::Roots::default();
        let req = serde_json::to_vec(&p::ReadReq {
            id: 1,
            handle: 12345,
            offset: 0,
            len: 16,
        })
        .unwrap();
        match serve(&roots, &handles, p::op::READ, &req) {
            Served::Err(e) => assert_eq!(e.code, code::BADF),
            _ => panic!("expected BADF"),
        }
    }

    #[test]
    fn close_of_an_unknown_handle_succeeds() {
        let handles = FsHandles::new();
        let roots = p::Roots::default();
        let req = serde_json::to_vec(&p::CloseReq { id: 1, handle: 99 }).unwrap();
        // We expire handles ourselves, so "already gone" is the caller's goal.
        match serve(&roots, &handles, p::op::CLOSE, &req) {
            Served::Meta(FsReply::Ok { id }) => assert_eq!(id, 1),
            _ => panic!("close should succeed"),
        }
    }

    #[test]
    fn rename_to_a_path_rather_than_a_name_is_refused() {
        let dir = scratch("rename");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").expect("write");
        let roots = rw_roots(&dir);
        let handles = FsHandles::new();
        // A destination path would escape the gate applied to the source.
        let req = serde_json::to_vec(&p::SetMetaReq {
            id: 1,
            path: file.to_string_lossy().to_string(),
            mtime: None,
            rename_to: Some("../../evil.txt".into()),
        })
        .unwrap();
        match serve(&roots, &handles, p::op::SETMETA, &req) {
            Served::Err(e) => assert_eq!(e.code, code::INVAL),
            _ => panic!("expected INVAL"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
