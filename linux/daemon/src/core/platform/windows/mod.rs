//! Windows implementations of the platform seam.
//!
//! Compiled only on Windows, so nothing here can break the Linux build. Each
//! stub names the concrete API it will call, so the remaining work is visible
//! as a checklist rather than as "port it".
//!
//! Verify with
//! `cargo check -p vortex-l3-daemon --lib --target x86_64-pc-windows-gnu`
//! (needs `rustup target add x86_64-pc-windows-gnu` — Arch's packaged rustc
//! ships no Windows std). The `-msvc` target cannot be cross-checked from
//! Linux: a C dependency's build script wants `lib.exe` and fails before
//! reaching our code. Running any of this needs a real Windows machine or VM —
//! BLE and toast activation cannot be exercised from Linux at all.

use std::path::PathBuf;

use super::{Autostart, BoxFuture, SessionControl, UserPaths};

pub mod ble;
pub mod input;
pub mod notify;

/// Join the multithreaded apartment on this thread, once.
///
/// WinRT activation fails with `CO_E_NOTINITIALIZED` on a thread that has not
/// initialized an apartment, and the daemon's threads are plain tokio workers
/// that never do. This compiles fine without it and then fails on the very
/// first call — the sort of thing only a real Windows run surfaces.
///
/// `S_FALSE` (already initialized) and `RPC_E_CHANGED_MODE` (this thread is
/// already in an STA — the UI thread, say) are both fine: in either case the
/// thread has an apartment, which is all we need.
fn ensure_winrt() {
    use std::cell::Cell;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    thread_local! {
        static JOINED: Cell<bool> = const { Cell::new(false) };
    }
    JOINED.with(|j| {
        if j.get() {
            return;
        }
        // SAFETY: no arguments to get wrong; the failure modes above are the
        // documented benign ones and everything else means WinRT is unusable
        // here, which the next call will report with real context.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        j.set(true);
    });
}


pub struct WindowsPaths;

/// Resolve a Windows known folder to a path.
///
/// This — not an environment variable — is the supported way to ask. Every one
/// of these folders can be REDIRECTED: OneDrive relocates Downloads and
/// Documents by default on a consumer machine, and a domain profile can move
/// AppData. `%USERPROFILE%\Downloads` is merely the common case, and getting it
/// wrong means writing received files into a folder the user never opens —
/// exactly the bug the Linux side already had with a hardcoded `~/Downloads`
/// on a French desktop.
///
/// `KF_FLAG_DONT_VERIFY` because a configured-but-missing folder is still the
/// user's stated intent: the receive path creates the directory anyway, and
/// verifying here would fail the lookup and send us to a fallback instead. Same
/// reasoning as the XDG side.
#[cfg(target_os = "windows")]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};

    // SAFETY: `id` is one of the FOLDERID_* constants, and the out-pointer is a
    // COM allocation we own. It is freed on BOTH paths below — including the
    // UTF-16 conversion failure — before anything is returned.
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None) }.ok()?;
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const std::ffi::c_void)) };
    Some(PathBuf::from(text.ok()?))
}

impl UserPaths for WindowsPaths {
    /// The user's real Downloads folder, wherever it has been moved to.
    fn downloads(&self) -> Option<PathBuf> {
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Downloads)
    }

    /// `%APPDATA%\Vortex` — roaming, so settings follow a domain profile.
    fn config(&self) -> Option<PathBuf> {
        Some(known_folder(&windows::Win32::UI::Shell::FOLDERID_RoamingAppData)?.join("Vortex"))
    }

    /// `%LOCALAPPDATA%\Vortex\Cache` — local, never roamed: the icon cache is
    /// machine-specific and would only bloat a roaming profile.
    fn cache(&self) -> Option<PathBuf> {
        Some(
            known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)?
                .join("Vortex")
                .join("Cache"),
        )
    }

    /// `%LOCALAPPDATA%\Vortex` — beside the cache, not inside it: a cache
    /// cleaner is entitled to delete the cache, and deleting the log of the run
    /// that just failed is the opposite of useful.
    fn logs(&self) -> Option<PathBuf> {
        Some(known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)?.join("Vortex"))
    }
}

/// Read `WTSINFOEXW.Data.WTSInfoExLevel1.SessionFlags` for this session.
///
/// Deliberately not cached: a stale "unlocked" would let proximity auto-lock
/// skip a lock it should have done.
fn query_session_locked() -> Option<bool> {
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
    };

    let mut buffer: windows::core::PWSTR = windows::core::PWSTR::null();
    let mut bytes: u32 = 0;
    // SAFETY: the out-params are ours; on success WTS allocates `buffer` and we
    // free it below on every path. The size check before the cast is what makes
    // the read safe — a shorter buffer would mean a different info level.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    };
    if ok.is_err() || buffer.is_null() {
        return None;
    }
    let flags = if (bytes as usize) >= std::mem::size_of::<WTSINFOEXW>() {
        let info = unsafe { &*(buffer.0 as *const WTSINFOEXW) };
        // Level 1 is the only level this call returns; anything else means the
        // OS handed back something we don't understand, so say "don't know".
        if info.Level == 1 {
            Some(unsafe { info.Data.WTSInfoExLevel1 }.SessionFlags)
        } else {
            None
        }
    } else {
        None
    };
    unsafe { WTSFreeMemory(buffer.0 as *mut std::ffi::c_void) };
    // WTS_SESSIONSTATE_LOCK / _UNLOCK are reported the right way round on
    // Windows 8 and later. (On Windows 7 they were inverted — a documented OS
    // bug. Vortex targets 10+, so this does not compensate for it; doing so
    // blindly would invert the answer on every supported version.)
    flags.map(|f| f == WTS_SESSIONSTATE_LOCK as i32)
}

pub struct WindowsSession;

impl SessionControl for WindowsSession {
    fn lock(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async move {
            // Inside the async block, not before it: a function returning a
            // future must not lock the screen just because someone built the
            // future. Nothing is held across an await here (there is none), so
            // the raw call is fine on any worker.
            //
            // SAFETY: no arguments, no out-params. Fails only if the calling
            // process lacks a visible window station (a service, say), which is
            // exactly what the error is for.
            unsafe { windows::Win32::System::Shutdown::LockWorkStation() }
                .map_err(|e| format!("LockWorkStation: {e}"))
        })
    }

    /// Windows has no programmatic unlock, by design — credentials must be
    /// presented to the LogonUI. Proximity auto-unlock is therefore Linux-only;
    /// [`SessionControl::can_unlock`] reports that so the UI can hide the
    /// setting instead of offering something that always fails.
    fn unlock(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Err("windows cannot unlock a session programmatically".to_string()) })
    }

    /// Query the session's lock flag directly.
    ///
    /// The obvious route — `WTSRegisterSessionNotification` and track
    /// `WTS_SESSION_LOCK`/`_UNLOCK` from process start — cannot answer before
    /// the first transition, which is the wrong answer for a daemon that starts
    /// while the screen is already locked. `WTSSessionInfoEx` reports the
    /// current flag instead, so the first call is as correct as the hundredth.
    ///
    /// `None` means "couldn't tell", never "unlocked": proximity auto-lock must
    /// not act on a guess.
    fn is_locked(&self) -> BoxFuture<Option<bool>> {
        Box::pin(async move { query_session_locked() })
    }

    fn can_unlock(&self) -> bool {
        false
    }
}

/// A NUL-terminated UTF-16 buffer, as every `*W` registry call wants.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A UTF-16 buffer as the BYTES `RegSetValueExW` wants.
///
/// `cbData` is a byte count, not a character count. Passing the `u16` length
/// writes half the string, and the truncation lands in the middle of a path —
/// which then fails at the next logon, not now.
fn utf16_as_bytes(v: &[u16]) -> &[u8] {
    // SAFETY: reinterpreting a u16 slice as bytes — same allocation, same
    // lifetime, length scaled. No alignment concern going wider to narrower.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// Run-at-login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
///
/// Chosen over the alternatives deliberately. A Startup-folder shortcut needs a
/// `.lnk` built through COM (`IShellLink`) for no gain, and Task Scheduler buys
/// delayed or elevated starts that Vortex does not want — it should come up with
/// the user's session, unelevated, like any other tray app.
///
/// Per-user (HKCU), never HKLM: the identity key this launches with lives in one
/// user's profile, so starting it for every account on the machine would have
/// them all fighting over a single pairing.
pub struct WindowsAutostart;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Vortex";

impl Autostart for WindowsAutostart {
    /// Whether we are registered to start AND the registration points at THIS
    /// binary.
    ///
    /// The path check matters: after a reinstall to a different directory the
    /// old value survives and silently launches nothing at the next logon.
    /// Reporting `false` there makes the settings toggle read off, and flipping
    /// it rewrites the path — so a stale entry is self-correcting rather than
    /// invisible.
    fn is_enabled(&self) -> bool {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
            REG_VALUE_TYPE,
        };
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let want = super::quoted_command(&exe);
        let key_w = wide(RUN_KEY);
        let val_w = wide(RUN_VALUE);

        let mut key = HKEY::default();
        // SAFETY: constant key path; `key` is ours and closed on every path.
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(key_w.as_ptr()),
                Some(0),
                KEY_READ,
                &mut key,
            )
        }
        .is_err()
        {
            return false;
        }

        let mut ty = REG_VALUE_TYPE::default();
        let mut bytes: u32 = 0;
        // Size first, then read — the documented two-step, since a registry
        // string has no length we are entitled to assume.
        let mut current = String::new();
        let mut ok = unsafe {
            RegQueryValueExW(
                key,
                windows::core::PCWSTR(val_w.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut bytes),
            )
        }
        .is_ok();
        if ok && bytes > 0 {
            let mut buf = vec![0u8; bytes as usize];
            ok = unsafe {
                RegQueryValueExW(
                    key,
                    windows::core::PCWSTR(val_w.as_ptr()),
                    None,
                    Some(&mut ty),
                    Some(buf.as_mut_ptr()),
                    Some(&mut bytes),
                )
            }
            .is_ok();
            if ok {
                // UTF-16 with a trailing NUL: drop it, or the comparison never
                // matches.
                let u16s: Vec<u16> = buf
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|c| *c != 0)
                    .collect();
                current = String::from_utf16_lossy(&u16s);
            }
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
        ok && current.eq_ignore_ascii_case(&want)
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
            KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
        };
        let key_w = wide(RUN_KEY);
        let val_w = wide(RUN_VALUE);
        let mut key = HKEY::default();
        // Create-or-open: the Run key exists on every normal install, but
        // creating covers a profile where it does not.
        // SAFETY: constant path; `key` is ours and closed below.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(key_w.as_ptr()),
                Some(0),
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .map_err(|e| format!("open Run key: {e}"))?;

        let result = if on {
            match std::env::current_exe() {
                Ok(exe) => {
                    let value = wide(&super::quoted_command(&exe));
                    unsafe {
                        RegSetValueExW(
                            key,
                            windows::core::PCWSTR(val_w.as_ptr()),
                            Some(0),
                            REG_SZ,
                            Some(utf16_as_bytes(&value)),
                        )
                    }
                    .ok()
                    .map_err(|e| format!("write Run value: {e}"))
                }
                Err(e) => Err(format!("current_exe: {e}")),
            }
        } else {
            // Deleting a value that is not there is success: this is a toggle,
            // and "off" is already the state the caller asked for.
            let _ = unsafe { RegDeleteValueW(key, windows::core::PCWSTR(val_w.as_ptr())) };
            Ok(())
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        result
    }
}
