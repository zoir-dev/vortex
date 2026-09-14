//! File logging for the desktop app.
//!
//! Why this exists: `install_linux.sh` (and the autostart .desktop entry)
//! launch the binary detached, with stdout and stderr going nowhere. So the
//! normal way a user runs Vortex produces NO log at all — which made a real
//! user report ("the call pill stayed on Calling…") impossible to diagnose from
//! the laptop side: the only evidence available was the phone's logcat. Asking
//! the user to re-launch from a terminal to reproduce a sporadic bug is not a
//! diagnosis path.
//!
//! So the app always tees its own tracing output to
//! `$XDG_STATE_HOME/vortex/vortex.log` (defaulting to `~/.local/state`),
//! keeping one previous file. Stderr still gets everything, so running from a
//! terminal is unchanged.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Rotate once the live log passes this; one previous file is kept, so the
/// pair costs at most twice this on disk.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// How many writes between size checks — `metadata()` on every log line would
/// be a syscall per line for no benefit.
const SIZE_CHECK_EVERY: u64 = 512;

pub fn log_dir() -> PathBuf {
    // XDG state is the right home for a log on Linux — not cache, which a
    // cleaner may delete, and deleting the log of the run that just failed is
    // the opposite of useful.
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        base.unwrap_or_else(std::env::temp_dir).join("vortex")
    }
    // Elsewhere, through the seam. Neither `XDG_STATE_HOME` nor `HOME` is set
    // on Windows, so this fell all the way through to the temp directory —
    // where a disk cleanup is entitled to remove exactly the evidence a first
    // run on an untested platform exists to leave. The seam answers
    // `%LOCALAPPDATA%\Vortex`, beside the app's other state.
    #[cfg(not(target_os = "linux"))]
    {
        vortex_l3_daemon::core::platform::paths()
            .logs()
            .unwrap_or_else(|| std::env::temp_dir().join("vortex"))
    }
}

pub fn log_path() -> PathBuf {
    log_dir().join("vortex.log")
}

/// Move the current log aside if it has grown past [`MAX_LOG_BYTES`].
fn rotate_if_large(path: &std::path::Path) {
    let too_big = std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    if too_big {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// Writes every line to stderr AND to the log file. A failing file write is
/// swallowed on purpose: losing the log must never take the app down, and the
/// terminal copy still carries everything.
struct Tee {
    file: &'static Mutex<Option<File>>,
    writes: &'static AtomicU64,
    path: &'static PathBuf,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut guard) = self.file.lock() {
            // Periodic rotation: reopen after moving the file aside, so a
            // long-lived instance can't grow the log without bound.
            if self.writes.fetch_add(1, Ordering::Relaxed) % SIZE_CHECK_EVERY == 0 {
                let big = guard.as_ref().and_then(|f| f.metadata().ok())
                    .map(|m| m.len() > MAX_LOG_BYTES)
                    .unwrap_or(false);
                if big {
                    *guard = None;
                    rotate_if_large(self.path);
                    *guard = OpenOptions::new().create(true).append(true).open(self.path).ok();
                }
            }
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(buf);
            }
        }
        io::stderr().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
        io::stderr().flush()
    }
}

/// Install the tracing subscriber. Returns the log file's path when file
/// logging is active, so startup can say where to look.
pub fn init() -> Option<PathBuf> {
    static FILE: Mutex<Option<File>> = Mutex::new(None);
    static WRITES: AtomicU64 = AtomicU64::new(0);
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

    let path = PATH.get_or_init(log_path);
    let opened = std::fs::create_dir_all(log_dir()).is_ok() && {
        rotate_if_large(path);
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => {
                if let Ok(mut g) = FILE.lock() {
                    *g = Some(f);
                }
                true
            }
            Err(_) => false,
        }
    };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(move || Tee { file: &FILE, writes: &WRITES, path: PATH.get_or_init(log_path) })
        .init();

    opened.then(|| path.clone())
}
