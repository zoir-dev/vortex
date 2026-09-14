//! The Linux mount adapter: the phone's storage as a FUSE filesystem.
//!
//! Everything between this and the wire — caching, the path walk, pipelined
//! reads — is [`crate::fs_vfs`]; this file is only the translation between that
//! and the kernel's FUSE protocol. Its Windows counterpart is
//! [`crate::fs_projfs`].
//!
//! # Why FUSE and not the WebDAV gateway
//!
//! The design doc sequenced WebDAV ahead of a native VFS because one gateway
//! serves both operating systems. On Linux it buys nothing FUSE does not:
//! GVFS/KIO mount `davs://` in *their* process, so only their own file dialogs
//! see the files — `cp`, `mpv` and every non-KIO program do not. A FUSE mount
//! is a real path in the filesystem with no ceiling.
//!
//! # Concurrency, which is the load-bearing design decision
//!
//! FUSE hands us one request at a time on one thread. Answering each one
//! inline — issue the request, block on the phone's reply, return — would make
//! the mount as slow as the round trip *times* the number of operations, and a
//! file manager stats every visible file at once. So every operation is
//! immediately handed to the async runtime and its `Reply` object (which is
//! `Send`, deliberately) is answered from there. The session thread does
//! nothing but parse and dispatch.
//!
//! That is also what makes the kernel's own readahead work for us: a sequential
//! reader triggers several `read` calls at once, and because we never block,
//! they overlap on the wire instead of queueing.
//!
//! (ProjFS is the opposite: it runs a thread pool and expects a provider to
//! block one of its threads. Same requirement — do not serialise — reached from
//! opposite directions.)

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileType, FopenFlags, Generation, INodeNo, KernelConfig, MountOption,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use vortex_l3_daemon::core::fs_proto::{self as p, code};

use crate::fs_vfs::{FsRemote, LinkRemote, Vfs, ATTR_TTL, MAX_INFLIGHT, ROOT_ADDR};

/// The mount root. FUSE fixes this at 1; the peer's synthetic root lives here.
const ROOT_INO: u64 = 1;

// ---------------------------------------------------------------------------
// Inode identity
// ---------------------------------------------------------------------------

/// Inode numbers ↔ peer addresses.
///
/// A peer address is an opaque token, not a path, so an inode number cannot be
/// derived from one and the mapping has to be remembered. Numbers are **never
/// recycled**: a file manager holds inode numbers across a refresh and reusing
/// one would silently show it the wrong file. The table therefore only grows,
/// which is fine at a few dozen bytes per entry for a session's browsing.
#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, String>,
    by_addr: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut t = Self {
            next: ROOT_INO + 1,
            ..Default::default()
        };
        t.by_ino.insert(ROOT_INO, ROOT_ADDR.to_string());
        t.by_addr.insert(ROOT_ADDR.to_string(), ROOT_INO);
        t
    }

    fn intern(&mut self, addr: &str) -> u64 {
        if let Some(&ino) = self.by_addr.get(addr) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, addr.to_string());
        self.by_addr.insert(addr.to_string(), ino);
        ino
    }

    fn addr(&self, ino: u64) -> Option<&str> {
        self.by_ino.get(&ino).map(String::as_str)
    }
}

struct PhoneFs<R: FsRemote> {
    vfs: Arc<Vfs<R>>,
    inodes: Arc<Mutex<Inodes>>,
    rt: tokio::runtime::Handle,
}

/// Intern an address, outside any `await`.
fn intern(inodes: &Mutex<Inodes>, addr: &str) -> u64 {
    inodes.lock().map(|mut t| t.intern(addr)).unwrap_or(ROOT_INO)
}

fn addr_of(inodes: &Mutex<Inodes>, ino: u64) -> Option<String> {
    inodes
        .lock()
        .ok()
        .and_then(|t| t.addr(ino).map(str::to_string))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// A protocol entry as a kernel `stat`.
///
/// Permissions are fixed rather than reported by the peer: the mount is
/// read-only until design doc §8 step 5 lands, and a writable-looking mode bit
/// would only get a copy half-way through before the phone refused it. `nlink`
/// of 2 for a directory is the usual lie (`.` and `..`) — the real subdirectory
/// count would cost a listing per stat.
fn attr_of(ino: u64, e: &p::FsEntry, uid: u32, gid: u32) -> FileAttr {
    let mtime = mtime_of(e.mtime);
    FileAttr {
        ino: INodeNo(ino),
        size: if e.is_dir { 0 } else { e.size },
        blocks: e.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: if e.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: if e.is_dir { 0o555 } else { 0o444 },
        nlink: if e.is_dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Seconds since the epoch as a `SystemTime`, tolerating the 0 the protocol
/// uses for "the peer cannot tell" and the negative values a badly-set phone
/// clock can produce.
fn mtime_of(secs: i64) -> SystemTime {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// A protocol code as an errno.
///
/// The reason [`code`] is errno-shaped in the first place: this is meant to be
/// a rename, not a translation. What the file manager shows the user comes
/// straight from here, so [`crate::fs_link::NO_LINK`] mapping to `EHOSTDOWN`
/// ("Host is down") rather than a generic I/O error is the difference between
/// an accurate message and a puzzling one.
fn errno_of(c: i32) -> Errno {
    match c {
        code::NOENT => Errno::ENOENT,
        code::ACCES => Errno::EACCES,
        code::BADF => Errno::EBADF,
        code::INVAL => Errno::EINVAL,
        code::NOTSUP => Errno::ENOTSUP,
        code::ISDIR => Errno::EISDIR,
        code::ROFS => Errno::EROFS,
        crate::fs_link::NO_LINK => Errno::EHOSTDOWN,
        // Includes `code::IO`, and anything a future peer invents.
        _ => Errno::EIO,
    }
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

/// Hand `body` the runtime and let it answer whenever the phone does.
///
/// Every operation goes through here, which is what keeps the FUSE session
/// thread free to dispatch the next one. Nothing waits on the result: the
/// `Reply` carries the request id, so the answer finds its way back on its own.
macro_rules! detach {
    ($fs:expr, |$vfs:ident, $inodes:ident| $body:block) => {{
        let $vfs = $fs.vfs.clone();
        let $inodes = $fs.inodes.clone();
        $fs.rt.spawn(async move { $body });
    }};
}

impl<R: FsRemote> fuser::Filesystem for PhoneFs<R> {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Readahead is the one design-doc §7 item the kernel implements for us:
        // it turns a sequential reader into several overlapping `read` calls,
        // and because we never block one, they overlap on the wire too. Ask for
        // as much as it will give (it clamps and reports what it took).
        let readahead = config.set_max_readahead(1024 * 1024).unwrap_or_else(|max| {
            let _ = config.set_max_readahead(max);
            max
        });
        // Background requests are how many of those may be outstanding. Ours
        // are answered off-thread, so a deeper queue costs nothing here.
        let _ = config.set_max_background(MAX_INFLIGHT as u16 * 2);
        tracing::info!(readahead, "fs-mount: kernel session up");
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        // Names arrive from the kernel as bytes and reach us over JSON, so a
        // name that is not UTF-8 cannot round-trip in the first place; matching
        // lossily just makes it miss rather than panic.
        let name = name.to_string_lossy().to_string();
        let (uid, gid) = (req.uid(), req.gid());
        let parent = parent.0;
        detach!(self, |vfs, inodes| {
            let Some(parent) = addr_of(&inodes, parent) else {
                return reply.error(Errno::ENOENT);
            };
            match vfs.lookup_child(&parent, &name).await {
                Ok(e) => {
                    let ino = intern(&inodes, &e.path);
                    reply.entry(&ATTR_TTL, &attr_of(ino, &e, uid, gid), Generation(0))
                }
                Err(c) => reply.error(errno_of(c)),
            }
        });
    }

    fn getattr(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        let (uid, gid) = (req.uid(), req.gid());
        let ino = ino.0;
        detach!(self, |vfs, inodes| {
            let Some(addr) = addr_of(&inodes, ino) else {
                return reply.error(Errno::ENOENT);
            };
            match vfs.entry_of(&addr).await {
                Ok(e) => reply.attr(&ATTR_TTL, &attr_of(ino, &e, uid, gid)),
                Err(c) => reply.error(errno_of(c)),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        detach!(self, |vfs, inodes| {
            let Some(addr) = addr_of(&inodes, ino) else {
                return reply.error(Errno::ENOENT);
            };
            let entries = match vfs.listing(&addr).await {
                Ok(v) => v,
                Err(c) => return reply.error(errno_of(c)),
            };
            // `.` and `..` occupy the first two slots so the rest of the indices
            // line up with the listing. `..` points at this directory rather
            // than the parent: the parent is not knowable from an opaque
            // address, and no caller resolves `..` through us — the kernel
            // remembers the path it walked.
            for (i, (child, kind, name)) in std::iter::once((ino, FileType::Directory, ".".into()))
                .chain(std::iter::once((ino, FileType::Directory, "..".into())))
                .chain(entries.iter().map(|e| {
                    (
                        intern(&inodes, &e.path),
                        if e.is_dir {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        },
                        e.name.clone(),
                    )
                }))
                .enumerate()
                .skip(offset as usize)
            {
                // The offset we hand back is where to RESUME, hence i + 1.
                if reply.add(INodeNo(child), i as u64 + 1, kind, &name) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // The kernel enforces `ro` at the mount before reaching us, so this is
        // belt-and-braces for a caller that got here another way.
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        let ino = ino.0;
        detach!(self, |vfs, inodes| {
            let Some(addr) = addr_of(&inodes, ino) else {
                return reply.error(Errno::ENOENT);
            };
            match vfs.open(&addr).await {
                // No flags: keeping the page cache is what lets the kernel serve
                // a re-read without us, and read ahead of the reader.
                Ok(fh) => reply.opened(fuser::FileHandle(fh), FopenFlags::empty()),
                Err(c) => reply.error(errno_of(c)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let fh = fh.0;
        detach!(self, |vfs, _inodes| {
            match vfs.read_range(fh, offset, size).await {
                Ok(bytes) => reply.data(&bytes),
                Err(c) => reply.error(errno_of(c)),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fh = fh.0;
        detach!(self, |vfs, _inodes| {
            // Answer the kernel first: `close()` cannot fail from here and the
            // caller should not wait on a phone round trip to return from it.
            reply.ok();
            vfs.close(fh).await;
        });
    }

    // ── Ops answered without asking the phone ────────────────────────────
    //
    // Left unimplemented, each of these answers `ENOSYS`, which the kernel
    // handles but fuser logs as "[Not Implemented]" — a warning per call in the
    // app's log, several per file opened. Answering them here costs nothing and
    // the answers are all knowable locally.

    /// Nothing to flush: the mount is read-only, and a read has no state on the
    /// peer beyond the handle `release` will close.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// No extended attributes, and no way to have any: the protocol carries a
    /// name, a kind, a size and an mtime, and nothing else. `ENODATA` is the
    /// answer for "this attribute is not set", which is the truth for every
    /// name that could be asked about.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENODATA);
    }

    /// An empty list, not an error — `cp -a` and Dolphin both ask, and a failure
    /// here reads to them as a file they could not fully inspect.
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        // The two-call protocol: size 0 asks how much room to allocate.
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Zeroed on purpose. There is no protocol op for free space, and a made
        // up figure would be a lie a file manager acts on. Zero free on a
        // read-only mount is at least the truth: nothing can be written here.
        // Revisit with design doc §8 step 5, which is when a real number starts
        // to matter.
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }
}

// ---------------------------------------------------------------------------
// Mount lifecycle
// ---------------------------------------------------------------------------

/// The live session. Dropping it unmounts, which is what cleans up on a normal
/// app exit.
static SESSION: OnceLock<Mutex<Option<fuser::BackgroundSession>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<fuser::BackgroundSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// Where the phone's files appear.
///
/// Under `XDG_RUNTIME_DIR` because the session lifetime is exactly right: the
/// directory goes away at logout, so a crashed app cannot leave a stale mount
/// point in the user's home.
pub(crate) fn mount_point() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("vortex").join("phone")
}

pub(crate) fn is_mounted() -> bool {
    session_slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Mount the phone's storage. Returns the mount point.
pub(crate) async fn mount() -> Result<PathBuf, String> {
    let dir = mount_point();
    let rt = tokio::runtime::Handle::current();
    // `Session::new` runs `fusermount3` and waits for the kernel's INIT, so it
    // does not belong on an async thread.
    tokio::task::spawn_blocking(move || {
        let bg = spawn_session(rt, &dir, LinkRemote)?;
        if let Ok(mut g) = session_slot().lock() {
            *g = Some(bg);
        }
        tracing::info!(path = %dir.display(), "fs-mount: mounted");
        Ok(dir)
    })
    .await
    .map_err(|e| format!("mount task failed: {e}"))?
}

/// Mount `remote` at `dir` and start serving it.
///
/// Generic over the remote so the integration test at the bottom of this file
/// can mount its fake peer for real — kernel, session thread and all — which is
/// the only way to check that what we tell the kernel is what a program reading
/// the mount actually sees.
fn spawn_session<R: FsRemote>(
    rt: tokio::runtime::Handle,
    dir: &Path,
    remote: R,
) -> Result<fuser::BackgroundSession, String> {
    // FIRST, before touching the path at all: a previous run that died without
    // unmounting (a crash, a SIGKILL, the installer restarting the app) leaves
    // the mount in the table with no server behind it, and every syscall on it
    // answers ENOTCONN. That includes the `stat` inside `create_dir_all`, which
    // therefore fails with EEXIST — the directory is there, it just cannot be
    // looked at. Clearing the corpse first is what makes a remount work.
    //
    // Only ever our own private path under XDG_RUNTIME_DIR, and a no-op when
    // nothing is mounted there.
    let _ = std::process::Command::new("fusermount3")
        .args(["-quz", &dir.to_string_lossy()])
        .stderr(std::process::Stdio::null())
        .status();
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let fs = PhoneFs {
        vfs: Arc::new(Vfs::new(remote)),
        inodes: Arc::new(Mutex::new(Inodes::new())),
        rt,
    };
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        // What `mount` and `df` call it.
        MountOption::FSName("vortex".into()),
        MountOption::Subtype("vortex".into()),
        // Read-only until design doc §8 step 5. Enforced by the kernel, so a
        // write is refused without a round trip to the phone.
        MountOption::RO,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoExec,
        // Let the KERNEL do permission checks, from the mode bits we report.
        // Without it the kernel asks us (`access`) on every path walk, which is
        // a round trip through the session thread to answer "yes" — the mount
        // is visible to its owner alone and read-only, so there is nothing for
        // us to decide that the mode bits do not already say.
        MountOption::DefaultPermissions,
    ];
    // One session thread is enough: it only parses a request and hands it to
    // the runtime, so it is never the thing that is busy.
    fuser::Session::new(fs, dir, &config)
        .map_err(|e| format!("mounting {} failed: {e}", dir.display()))?
        .spawn()
        .map_err(|e| format!("session thread failed: {e}"))
}

/// Unmount, if mounted.
pub(crate) fn unmount() {
    let taken = session_slot().lock().ok().and_then(|mut g| g.take());
    if let Some(bg) = taken {
        // `umount_and_join` waits for the session loop to finish, which needs
        // the kernel to have released the mount — a blocking call, so keep it
        // off the async threads.
        std::thread::spawn(move || match bg.umount_and_join() {
            Ok(()) => tracing::info!("fs-mount: unmounted"),
            Err(e) => tracing::warn!("fs-mount: unmount failed: {e}"),
        });
    }
}

/// Detach the mount on the process's way out.
///
/// A FUSE mount whose server process has died is not gone — it stays in the
/// mount table answering `ENOTCONN`, which makes `df` error and leaves a broken
/// entry in every file manager. So the deliberate-quit path detaches it first.
///
/// `fusermount3 -z` rather than [`unmount`] because this runs microseconds
/// before `exit()`: the lazy form returns immediately and lets the kernel finish
/// when the last user of the mount goes away, where waiting for the session
/// thread to join would simply be killed half-way.
pub(crate) fn unmount_on_exit() {
    if !is_mounted() {
        return;
    }
    let dir = mount_point();
    let _ = std::process::Command::new("fusermount3")
        .args(["-quz", &dir.to_string_lossy()])
        .stderr(std::process::Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_vfs::tests::{big_bytes, dir, file, FakePeer};

    #[test]
    fn a_directory_and_a_file_translate_to_the_right_stat() {
        let d = attr_of(7, &dir("DCIM", "/sdcard/DCIM"), 1000, 1000);
        assert_eq!(d.kind, FileType::Directory);
        assert_eq!(d.perm, 0o555);
        assert_eq!(d.ino, INodeNo(7));
        let f = attr_of(8, &file("a.txt", "/sdcard/a.txt", 5), 1000, 1000);
        assert_eq!(f.kind, FileType::RegularFile);
        assert_eq!(f.perm, 0o444, "the mount is read-only");
        assert_eq!(f.size, 5);
        assert_eq!(f.blocks, 1);
        assert_eq!(
            f.mtime,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn an_unknown_mtime_is_the_epoch_not_a_panic() {
        assert_eq!(mtime_of(0), SystemTime::UNIX_EPOCH);
        // A phone with a clock set before 1970 must not take the mount down.
        assert!(mtime_of(-86_400) < SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn every_protocol_code_reaches_the_user_as_itself() {
        assert_eq!(errno_of(code::NOENT), Errno::ENOENT);
        assert_eq!(errno_of(code::ACCES), Errno::EACCES);
        assert_eq!(errno_of(code::ROFS), Errno::EROFS);
        assert_eq!(errno_of(code::ISDIR), Errno::EISDIR);
        assert_eq!(errno_of(crate::fs_link::NO_LINK), Errno::EHOSTDOWN);
        assert_eq!(errno_of(code::IO), Errno::EIO);
        assert_eq!(
            errno_of(9999),
            Errno::EIO,
            "an unknown code is still an error"
        );
    }

    #[test]
    fn an_inode_is_stable_and_never_reused() {
        let t = Mutex::new(Inodes::new());
        let first = intern(&t, "/sdcard/a.txt");
        assert_eq!(intern(&t, "/sdcard/a.txt"), first, "same address, same ino");
        assert_ne!(intern(&t, "/sdcard/DCIM"), first);
        assert_ne!(first, ROOT_INO);
        assert_eq!(addr_of(&t, ROOT_INO).as_deref(), Some(ROOT_ADDR));
        assert_eq!(addr_of(&t, first).as_deref(), Some("/sdcard/a.txt"));
        assert_eq!(addr_of(&t, 9999), None, "an unknown ino resolves to nothing");
    }

    /// The whole thing, for real: a kernel mount over the fake peer, driven by
    /// ordinary `std::fs` calls.
    ///
    /// `#[ignore]` because it needs `/dev/fuse` and `fusermount3`, which a
    /// container or a build box may not have, and a test that cannot run there
    /// should not look like a failure. Run it with:
    ///
    /// ```text
    /// cargo test --lib fs_fuse -- --ignored --nocapture
    /// ```
    ///
    /// Multi-threaded on purpose: the syscalls block a thread while the FUSE
    /// operations they trigger are answered on another.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs /dev/fuse and fusermount3"]
    async fn a_real_mount_answers_ordinary_file_calls() {
        let peer = Arc::new(FakePeer::new());
        let dir = std::env::temp_dir().join(format!("vortex-fuse-{}", std::process::id()));
        let session = spawn_session(tokio::runtime::Handle::current(), &dir, peer.clone())
            .expect("mount failed — is /dev/fuse available?");

        let at = dir.clone();
        let seen = tokio::task::spawn_blocking(move || {
            let mut names: Vec<String> = std::fs::read_dir(&at)
                .expect("read_dir")
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            let small = std::fs::read_to_string(at.join("a.txt")).expect("read a.txt");
            let big = std::fs::read(at.join("DCIM").join("big.bin")).expect("read big.bin");
            let meta = std::fs::metadata(at.join("DCIM")).expect("stat DCIM");
            // Writes must be refused by the kernel, without reaching the peer.
            let write = std::fs::write(at.join("nope.txt"), b"x");
            (names, small, big, meta.is_dir(), write.is_err())
        })
        .await
        .unwrap();

        let (names, small, big, dcim_is_dir, write_refused) = seen;
        assert_eq!(names, ["DCIM", "a.txt"]);
        assert_eq!(small, "hello");
        assert!(dcim_is_dir);
        assert!(write_refused, "the mount must be read-only");
        assert_eq!(big, big_bytes(), "100 KiB came back wrong through the kernel");

        tokio::task::spawn_blocking(move || {
            let _ = session.umount_and_join();
            let _ = std::fs::remove_dir(&dir);
        })
        .await
        .unwrap();
    }
}
