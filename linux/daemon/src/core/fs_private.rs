//! Owner-only filesystem helpers for the on-disk mirror caches.
//!
//! Everything under the cache root carries phone-private data (SMS bodies,
//! contacts, call history, app icons), so the directory and every file in it
//! must be readable by this user and nobody else — including repairing
//! permissions left behind by older builds that wrote with the default umask.
//!
//! # The two platforms do not offer the same guarantee
//!
//! On Unix this is exact: 0700 on the directory, 0600 on each file, set
//! explicitly rather than left to the umask.
//!
//! On Windows there is no mode to set. A file under the user's profile inherits
//! that profile's ACL, which already excludes other standard users — but grants
//! `Administrators` and `SYSTEM`. That is weaker than 0600 (where root is the
//! only equivalent) and it is *inherited*, so it holds only as long as the path
//! really is inside the profile. Tightening it means writing an explicit DACL
//! with `SetNamedSecurityInfoW`; until that exists, [`write_private`] on Windows
//! is "as private as the user's profile" and no more. Callers storing anything
//! stronger than mirror data must not rely on it. (Identity and peer keys do
//! not: they live in Secret Service / Credential Manager via
//! [`crate::core::storage`], never here.)

use std::fs;
use std::io;
use std::path::Path;

/// Create `dir` (and parents) owner-only. If it already exists, tighten it —
/// this repairs caches written by older builds.
#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // `recursive(true)` applies the mode only to dirs it creates; an existing
    // dir keeps its old (possibly world-readable) mode — fix it explicitly.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Create `dir` (and parents), inheriting the user profile's ACL.
///
/// TODO: `SetNamedSecurityInfoW` with an explicit owner-only DACL and
/// `PROTECTED_DACL_SECURITY_INFORMATION` to stop inheritance. See the module
/// docs for what is and isn't guaranteed until then.
#[cfg(windows)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Write `bytes` to `path` with mode 0600, creating the parent dir 0700.
/// Truncates an existing file and tightens its mode too.
#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // mode(0o600) only applies on create; an existing file keeps its mode.
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)
}

/// Write `bytes` to `path`, creating the parent dir, both inheriting the user
/// profile's ACL. See the module docs: this is weaker than the Unix path.
#[cfg(windows)]
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh per-test directory under the system temp dir.
    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("vortex-fsp-{tag}-{}", std::process::id()))
    }

    /// Holds on both platforms: the write lands, parents are created, and a
    /// second write replaces rather than appends.
    #[test]
    fn writes_through_missing_parents_and_replaces_content() {
        let base = scratch("rw");
        let file = base.join("nested").join("data.json");
        write_private(&file, b"old").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"old");
        write_private(&file, b"new").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"new");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(p: &Path) -> u32 {
            fs::metadata(p).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn dir_and_file_are_owner_only() {
            let base = scratch("modes");
            let dir = base.join("nested");
            let file = dir.join("data.json");
            write_private(&file, b"x").unwrap();
            assert_eq!(mode_of(&dir), 0o700);
            assert_eq!(mode_of(&file), 0o600);
            let _ = fs::remove_dir_all(&base);
        }

        #[test]
        fn repairs_existing_loose_permissions() {
            let base = scratch("fix");
            fs::create_dir_all(&base).unwrap();
            fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
            let file = base.join("data.json");
            fs::write(&file, b"old").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

            write_private(&file, b"new").unwrap();
            assert_eq!(mode_of(&base), 0o700);
            assert_eq!(mode_of(&file), 0o600);
            assert_eq!(fs::read(&file).unwrap(), b"new");
            let _ = fs::remove_dir_all(&base);
        }
    }
}
