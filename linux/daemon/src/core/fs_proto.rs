//! Ranged filesystem protocol — the one primitive underneath both file
//! browsing and large-file transfer (design doc `docs/design/file-browsing.md`).
//!
//! # Why this exists
//!
//! Every file transfer in Vortex today buffers a whole file in memory on the
//! sending side: the phone stashes bytes in `ClipboardBlobStore` keyed by a
//! content token and the laptop pulls the lot in one go. That is what made an
//! 835 MB share an `OutOfMemoryError`, and it is why a 64 MB `MAX_FILE_BYTES`
//! cap exists at all. A file manager needs the same missing primitive for a
//! different reason — Explorer, Dolphin and every thumbnailer issue ranged
//! reads constantly. So:
//!
//! ```text
//! READ(handle, offset, len) -> bytes
//! ```
//!
//! buys both features at once, and the cap disappears as a side effect rather
//! than as a separate change.
//!
//! # Symmetry
//!
//! Unlike the original design sketch, this protocol is **bidirectional**: both
//! peers serve it and both consume it. The laptop browses the phone's storage,
//! and the phone browses the laptop's — same ops, same frames, same code paths.
//! Nothing here names a side. A "server" is whichever peer received the request.
//!
//! # Framing
//!
//! Four frame types ride the existing Noise-sealed app-data channel; `sub`
//! carries the op, so an unknown op is rejected without parsing a payload:
//!
//! | Frame | Payload |
//! |---|---|
//! | `FS_REQ`  | `sub` = [`op`], JSON request (or JSON + binary tail for WRITE) |
//! | `FS_META` | JSON [`FsReply`] — listing, stat, open result, write ack |
//! | `FS_DATA` | binary read result: `[id u32][offset u64][flags u8][bytes]` |
//! | `FS_ERR`  | JSON [`FsErr`] — a definite, immediate failure |
//!
//! Read results are binary rather than JSON on purpose: base64 would cost 33%
//! on the single hottest path in the protocol.
//!
//! Requests carry an `id` and are **pipelined, not serialised**. A file manager
//! stats everything in view at once; a request/response lock would feel broken.
//! Replies are correlated by `id` and may arrive out of order.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Op codes, carried in the frame's `sub` byte. Authoritative for routing — the
/// JSON payload is NOT self-tagged, so these must agree with the payload shape.
/// Mirrors Kotlin `FsOp`.
pub mod op {
    pub const LIST: u8 = 0x01;
    pub const STAT: u8 = 0x02;
    pub const OPEN: u8 = 0x03;
    pub const READ: u8 = 0x04;
    pub const WRITE: u8 = 0x05;
    pub const CLOSE: u8 = 0x06;
    pub const SETMETA: u8 = 0x07;
}

/// Error codes. Deliberately errno-shaped: both mount adapters (FUSE, ProjFS)
/// have to turn these back into OS errors, and inventing a private vocabulary
/// would mean two lossy translations instead of none.
///
/// Mirrors Kotlin `FsCode`.
pub mod code {
    /// No such file or directory.
    pub const NOENT: i32 = 2;
    /// Permission denied — including "outside every served root".
    pub const ACCES: i32 = 13;
    /// I/O error.
    pub const IO: i32 = 5;
    /// Bad handle: unknown, expired, or closed.
    pub const BADF: i32 = 9;
    /// Invalid argument (bad range, malformed path, oversized read).
    pub const INVAL: i32 = 22;
    /// Not supported — an op that is defined and wired but deliberately not
    /// implemented. Answered explicitly, never dropped: a stub that looks like
    /// a timeout is worse than an honest refusal, and a file manager blocked on
    /// a dead read is the worst outcome of all.
    pub const NOTSUP: i32 = 95;
    /// Is a directory (read attempted on one).
    pub const ISDIR: i32 = 21;
    /// Read-only: the path resolves under a root that does not allow writes.
    pub const ROFS: i32 = 30;
}

/// Bytes per `FS_READ`. Bounded so memory stays flat on both sides regardless
/// of file size — the consumer issues many ranged reads rather than one huge
/// one, which is the entire point of the exercise.
///
/// Sits inside `MAX_FRAME_PAYLOAD` (63 KiB) with room for the 13-byte
/// [`FS_DATA`](self) header and the AEAD tag.
pub const MAX_READ_LEN: u32 = 48 * 1024;

/// Entries per `FS_LIST` page. A 10,000-entry folder must not be one frame.
pub const LIST_PAGE: usize = 256;

/// Binary header on an `FS_DATA` payload: id(4) + offset(8) + flags(1).
pub const DATA_HEADER_LEN: usize = 13;

/// `FS_DATA` flag: this reply reaches end-of-file, so the consumer can stop
/// reading without a further round trip.
pub const FLAG_EOF: u8 = 0x01;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// List one page of a directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListReq {
    pub id: u32,
    pub path: String,
    /// Opaque resume point from the previous page's [`FsReply::List::cursor`].
    /// 0 starts at the beginning.
    #[serde(default)]
    pub cursor: u32,
}

/// Stat one path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatReq {
    pub id: u32,
    pub path: String,
}

/// Open a path and get a handle back.
///
/// Handles rather than paths for reads: resolving a path per read is a TOCTOU
/// problem, and under Android's SAF it is also slow. Open once, read many.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenReq {
    pub id: u32,
    pub path: String,
    /// Open for writing (creating or truncating as needed). Refused with
    /// [`code::ROFS`] unless the path resolves under a writable root.
    #[serde(default)]
    pub write: bool,
}

/// Read `len` bytes at `offset`. A short reply is normal (end of file, or the
/// server chose a smaller slice); it is not an error and not necessarily EOF —
/// check [`FLAG_EOF`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadReq {
    pub id: u32,
    pub handle: u64,
    pub offset: u64,
    pub len: u32,
}

/// Write at `offset`. The bytes ride a binary tail after the JSON header — see
/// [`encode_write`] — rather than inside it, for the same reason reads are
/// binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteReq {
    pub id: u32,
    pub handle: u64,
    pub offset: u64,
}

/// Release a handle. Best-effort: a server may drop handles on its own (process
/// death, idle expiry), so a `CLOSE` for an unknown handle is success, not
/// [`code::BADF`] — the consumer's intent is already satisfied.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloseReq {
    pub id: u32,
    pub handle: u64,
}

/// Set metadata / rename. Wired and answered, implementation optional per side.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetMetaReq {
    pub id: u32,
    pub path: String,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub mtime: Option<i64>,
    /// New *name* (not a path) within the same directory.
    #[serde(default)]
    pub rename_to: Option<String>,
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// One directory entry, or the result of a stat.
///
/// Deliberately minimal: a file manager needs name, kind, size and mtime to
/// render a row, and every extra field is bytes on a link that may be BLE.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsEntry {
    /// Display name. Base name for a listing; for a stat, the base name of the
    /// stat'd path. NOT an address — see [`FsEntry::path`].
    pub name: String,
    /// Opaque, server-defined token that addresses this entry in a later
    /// request.
    ///
    /// On a real filesystem this is the absolute path, but under Android's SAF
    /// it is a document URI — a name is simply not addressable there. So a
    /// consumer must send this back verbatim and must never construct a child
    /// address by joining [`FsEntry::name`] onto its parent.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: u64,
    /// Seconds since the Unix epoch, or 0 when the server cannot tell.
    #[serde(default)]
    pub mtime: i64,
    /// The server will refuse writes here. Advisory — used to grey out UI, not
    /// to enforce anything; enforcement is the server's job.
    #[serde(default)]
    pub readonly: bool,
}

/// A successful non-data reply, carried as JSON in an `FS_META` frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FsReply {
    List {
        id: u32,
        entries: Vec<FsEntry>,
        /// Resume point for the next page, or `None` when the listing is
        /// complete. `Some` always means "call again" — never a guess.
        #[serde(default)]
        cursor: Option<u32>,
    },
    Stat {
        id: u32,
        entry: FsEntry,
    },
    Open {
        id: u32,
        handle: u64,
        /// Size at open time, so a consumer can plan its reads in one round
        /// trip instead of open-then-stat.
        size: u64,
        #[serde(default)]
        readonly: bool,
    },
    Wrote {
        id: u32,
        bytes: u32,
    },
    /// Generic success for ops with nothing to report (`CLOSE`, `SETMETA`).
    Ok {
        id: u32,
    },
}

impl FsReply {
    /// The request this reply answers.
    pub fn id(&self) -> u32 {
        match self {
            FsReply::List { id, .. }
            | FsReply::Stat { id, .. }
            | FsReply::Open { id, .. }
            | FsReply::Wrote { id, .. }
            | FsReply::Ok { id } => *id,
        }
    }
}

/// A definite failure. Every failing op sends one of these — silence is never
/// an answer, because the far side cannot distinguish it from a lost frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsErr {
    pub id: u32,
    /// One of [`code`].
    pub code: i32,
    /// Short, human-readable context. Never contains a full path: paths are
    /// user data and this may be logged.
    #[serde(default)]
    pub msg: String,
}

impl FsErr {
    pub fn new(id: u32, code: i32, msg: impl Into<String>) -> Self {
        Self {
            id,
            code,
            msg: msg.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Binary framings
// ---------------------------------------------------------------------------

/// Build an `FS_DATA` payload: `[id u32 BE][offset u64 BE][flags u8][bytes]`.
pub fn encode_data(id: u32, offset: u64, eof: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DATA_HEADER_LEN + bytes.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.push(if eof { FLAG_EOF } else { 0 });
    out.extend_from_slice(bytes);
    out
}

/// An owned read result, so a consumer is not tied to the frame buffer's
/// lifetime. Mirrors Kotlin `FsData`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsData {
    pub id: u32,
    pub offset: u64,
    pub eof: bool,
    pub bytes: Vec<u8>,
}

/// Parse an `FS_DATA` payload into `(id, offset, eof, bytes)`.
pub fn decode_data(p: &[u8]) -> Option<(u32, u64, bool, &[u8])> {
    if p.len() < DATA_HEADER_LEN {
        return None;
    }
    let id = u32::from_be_bytes(p[0..4].try_into().ok()?);
    let offset = u64::from_be_bytes(p[4..12].try_into().ok()?);
    let eof = p[12] & FLAG_EOF != 0;
    Some((id, offset, eof, &p[DATA_HEADER_LEN..]))
}

/// Build an `FS_REQ`/`WRITE` payload: `[json_len u16 BE][json][bytes]`.
pub fn encode_write(req: &WriteReq, bytes: &[u8]) -> Vec<u8> {
    let json = serde_json::to_vec(req).unwrap_or_default();
    let mut out = Vec::with_capacity(2 + json.len() + bytes.len());
    out.extend_from_slice(&(json.len() as u16).to_be_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(bytes);
    out
}

/// Parse an `FS_REQ`/`WRITE` payload into its header and byte tail.
pub fn decode_write(p: &[u8]) -> Option<(WriteReq, &[u8])> {
    if p.len() < 2 {
        return None;
    }
    let n = u16::from_be_bytes([p[0], p[1]]) as usize;
    // `2 + n` cannot overflow (n is a u16) but can exceed the payload if the
    // frame was truncated — that must be a decode failure, not a panic.
    let end = 2usize.checked_add(n)?;
    if p.len() < end {
        return None;
    }
    let req: WriteReq = serde_json::from_slice(&p[2..end]).ok()?;
    Some((req, &p[end..]))
}

// ---------------------------------------------------------------------------
// Served roots
// ---------------------------------------------------------------------------

/// One served root and whether it accepts writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub writable: bool,
}

/// The set of paths this device serves to a paired peer, and the gate every
/// path-taking op passes through.
///
/// A phone that is paired is not thereby trusted with `~/.ssh` — pairing proves
/// identity, not authorisation. So the answer to "what may the peer see" is an
/// explicit allowlist rather than "whatever the daemon's user can read", and it
/// lives in a config file the user can widen to `/` deliberately if that is
/// what they want.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    roots: Vec<Root>,
}

impl Roots {
    pub fn new(roots: Vec<Root>) -> Self {
        Self { roots }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn list(&self) -> &[Root] {
        &self.roots
    }

    /// Resolve a peer-supplied path, or refuse it.
    ///
    /// Canonicalises before comparing, so `..` traversal and symlinks that
    /// point outside a root are rejected rather than merely discouraged. This
    /// is the only place a peer-supplied path becomes a real one; everything
    /// downstream may assume the result is inside a root.
    ///
    /// `for_write` additionally requires the matched root to be writable.
    pub fn resolve(&self, path: &str, for_write: bool) -> Result<PathBuf, i32> {
        if path.is_empty() {
            return Err(code::INVAL);
        }
        let requested = PathBuf::from(path);
        if !requested.is_absolute() {
            return Err(code::INVAL);
        }
        // Canonicalise the deepest existing ancestor, then re-append the rest.
        // A write may legitimately target a path that does not exist yet, so
        // canonicalising the full path would refuse every file creation — but
        // the *existing* prefix is what a symlink escape would have to go
        // through, so checking that is sufficient.
        let (existing, tail) = deepest_existing(&requested);
        let canon = existing.canonicalize().map_err(|_| code::NOENT)?;
        let full = if tail.as_os_str().is_empty() {
            canon.clone()
        } else {
            canon.join(&tail)
        };
        for root in &self.roots {
            let Ok(croot) = root.path.canonicalize() else {
                continue;
            };
            if !full.starts_with(&croot) {
                continue;
            }
            if for_write && !root.writable {
                return Err(code::ROFS);
            }
            return Ok(full);
        }
        // Deliberately ACCES and not NOENT: "outside every root" is a policy
        // refusal, and reporting NOENT would let a peer probe for the existence
        // of paths it is not allowed to see.
        Err(code::ACCES)
    }

    /// Whether writes are allowed anywhere. Used to advertise capability.
    pub fn any_writable(&self) -> bool {
        self.roots.iter().any(|r| r.writable)
    }
}

/// Split `p` into (deepest existing ancestor, remaining tail).
///
/// Components are collected and joined at the end rather than prepended as we
/// go: `PathBuf::push("")` appends a separator, so building the tail
/// incrementally produced `out.bin/` for a single missing component. `resolve`
/// still accepted that — the prefix check passes — but the trailing slash means
/// "directory" to the OS, so every attempt to create a file failed with
/// `NotADirectory` well after the path had been blessed.
fn deepest_existing(p: &Path) -> (PathBuf, PathBuf) {
    fn joined(parts: &[std::ffi::OsString]) -> PathBuf {
        let mut tail = PathBuf::new();
        for part in parts.iter().rev() {
            tail.push(part);
        }
        tail
    }
    let mut base = p.to_path_buf();
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if base.exists() {
            return (base, joined(&parts));
        }
        let Some(name) = base.file_name().map(|n| n.to_os_string()) else {
            // Walked off the top without finding anything that exists.
            return (base, joined(&parts));
        };
        parts.push(name);
        if !base.pop() {
            return (base, joined(&parts));
        }
    }
}

/// Parse the roots config. One path per line; `#` comments; blank lines
/// ignored. An optional `ro `/`rw ` prefix sets writability (default `ro`).
///
/// Kept this dumb on purpose — the user was promised a file they could edit by
/// hand, and a hand-edited TOML/JSON that fails to parse would silently serve
/// nothing.
pub fn parse_roots(text: &str) -> Roots {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Tokenise rather than strip a prefix: `strip_prefix("rw ")` fails on a
        // bare "rw" line (the trailing space is already trimmed), which then
        // fell through to the default arm and served a root literally named
        // "rw". A path containing spaces still works — only an exact `rw`/`ro`
        // first token is treated as a flag.
        let mut parts = line.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        let remainder = parts.next().unwrap_or("").trim();
        let (writable, rest) = match first {
            "rw" => (true, remainder),
            "ro" => (false, remainder),
            _ => (false, line),
        };
        if rest.is_empty() {
            continue;
        }
        out.push(Root {
            path: PathBuf::from(rest),
            writable,
        });
    }
    Roots::new(out)
}

/// The default config file written on first run.
pub fn default_roots_file(home: &Path) -> String {
    format!(
        "# Folders this device serves to a paired phone.\n\
         #\n\
         # One path per line. Prefix with \"rw \" to allow writes (so the phone\n\
         # can upload into it), or \"ro \" for read-only. Default is read-only.\n\
         #\n\
         # Set this to \"rw /\" to serve the whole filesystem. Nothing here can\n\
         # exceed your own user's permissions, but note that a paired phone is\n\
         # then able to read anything you can — including SSH keys and browser\n\
         # profiles. Pairing proves which device it is, not that it should see\n\
         # everything.\n\
         #\n\
         # Paths are canonicalised before use, so \"..\" and symlinks that point\n\
         # outside a root are refused.\n\
         rw {}\n",
        home.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_round_trips_including_an_empty_tail() {
        let enc = encode_data(7, 4096, true, b"hello");
        let (id, off, eof, bytes) = decode_data(&enc).expect("decodes");
        assert_eq!((id, off, eof, bytes), (7, 4096, true, b"hello".as_slice()));

        // A zero-length read at EOF is a normal reply, not a malformed frame.
        let enc = encode_data(1, 0, true, b"");
        let (_, _, eof, bytes) = decode_data(&enc).expect("decodes");
        assert!(eof && bytes.is_empty());
    }

    #[test]
    fn a_truncated_data_frame_decodes_to_none_rather_than_panicking() {
        assert!(decode_data(&[]).is_none());
        assert!(decode_data(&[0u8; DATA_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn write_round_trips_and_a_lying_length_is_refused() {
        let req = WriteReq {
            id: 3,
            handle: 9,
            offset: 100,
        };
        let enc = encode_write(&req, b"payload");
        let (got, bytes) = decode_write(&enc).expect("decodes");
        assert_eq!(got, req);
        assert_eq!(bytes, b"payload");

        // A header length past the end of the buffer must not panic or read
        // out of bounds — a peer can send anything.
        let mut bad = enc.clone();
        bad[0] = 0xff;
        bad[1] = 0xff;
        assert!(decode_write(&bad).is_none());
    }

    #[test]
    fn roots_parse_with_comments_and_write_prefixes() {
        let r = parse_roots(
            "# comment\n\
             \n\
             rw /home/u\n\
             ro /srv/media\n\
             /plain/is/readonly\n\
             rw \n",
        );
        assert_eq!(r.list().len(), 3);
        assert!(r.list()[0].writable);
        assert!(!r.list()[1].writable);
        assert!(!r.list()[2].writable);
        assert!(r.any_writable());
    }

    #[test]
    fn resolve_refuses_traversal_relative_paths_and_unserved_roots() {
        let tmp = std::env::temp_dir().join("vortex-fsproto-test-a");
        let inside = tmp.join("inside");
        std::fs::create_dir_all(&inside).expect("mkdir");
        let roots = Roots::new(vec![Root {
            path: tmp.clone(),
            writable: false,
        }]);

        assert!(roots.resolve(inside.to_str().unwrap(), false).is_ok());
        // Relative paths are never accepted.
        assert_eq!(roots.resolve("inside", false), Err(code::INVAL));
        assert_eq!(roots.resolve("", false), Err(code::INVAL));
        // Traversal out of the root canonicalises away and is then unserved.
        let escape = format!("{}/../../etc", inside.display());
        assert_eq!(roots.resolve(&escape, false), Err(code::ACCES));
        // A read-only root refuses writes with ROFS, not a generic error, so
        // the far side can tell "not allowed" from "broken".
        assert_eq!(
            roots.resolve(inside.to_str().unwrap(), true),
            Err(code::ROFS)
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_allows_a_not_yet_existing_file_under_a_writable_root() {
        let tmp = std::env::temp_dir().join("vortex-fsproto-test-b");
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let roots = Roots::new(vec![Root {
            path: tmp.clone(),
            writable: true,
        }]);

        // An upload targets a path that does not exist yet; canonicalising the
        // whole path would refuse every file creation.
        let fresh = tmp.join("subdir-does-not-exist").join("new.bin");
        let got = roots.resolve(fresh.to_str().unwrap(), true);
        assert!(got.is_ok(), "creation under a writable root must resolve");

        // ...but the escape check still applies to the non-existent tail.
        let escape = tmp.join("..").join("elsewhere").join("new.bin");
        assert_eq!(roots.resolve(escape.to_str().unwrap(), true), Err(code::ACCES));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_empty_root_set_serves_nothing() {
        let roots = Roots::default();
        assert!(roots.is_empty());
        assert_eq!(roots.resolve("/etc/passwd", false), Err(code::ACCES));
    }

    #[test]
    fn read_len_fits_a_frame_with_room_for_the_header_and_tag() {
        let ceiling = super::super::ble::frame::MAX_FRAME_PAYLOAD;
        assert!(MAX_READ_LEN as usize + DATA_HEADER_LEN + 16 <= ceiling);
    }
}
