//! What this machine can and cannot do, in one place, plus a report the user
//! can paste into an issue.
//!
//! The project's own spec has called for this since the start (docs3 §10 calls
//! Diagnostics "the most important" panel) and the reason is sharper now that
//! the beta is public: almost every feature here depends on something outside
//! the app — a BLE adapter, a portal, a GNOME extension, adb, a GStreamer
//! plugin — and when one is missing the feature simply does not happen. The
//! audit's own top theme was "failures with no way to observe them".
//!
//! CONTRIBUTING.md currently asks a reporter to type their distro, desktop and
//! ROM by hand. Half of that never arrives, and the half that does is the half
//! we could have read ourselves.
//!
//! Everything here is READ-ONLY and cheap: no probing that changes state, no
//! blocking call that could hang the UI thread for long.

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct Check {
    /// Stable id, so the UI can translate the label rather than show this.
    pub id: String,
    /// "ok" | "warn" | "fail" | "unknown"
    pub level: String,
    /// What we actually found — a version, a path, an error. Shown as-is.
    pub detail: String,
}

#[derive(Serialize, Clone)]
pub struct Diagnostics {
    pub app_version: String,
    pub checks: Vec<Check>,
    pub log_path: String,
}

fn check(id: &str, level: &str, detail: impl Into<String>) -> Check {
    Check { id: id.into(), level: level.into(), detail: detail.into() }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

/// First matching line of /etc/os-release, without quotes.
fn distro() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn cmd_ok(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Does anyone own this well-known name on the session bus?
fn bus_name_owned(name: &str) -> bool {
    cmd_ok(
        "gdbus",
        &[
            "call", "--session", "--dest", "org.freedesktop.DBus",
            "--object-path", "/org/freedesktop/DBus",
            "--method", "org.freedesktop.DBus.NameHasOwner", name,
        ],
    )
    .is_some_and(|s| s.contains("true"))
}

#[tauri::command]
pub(crate) fn diagnostics() -> Diagnostics {
    let mut checks = Vec::new();

    // ── environment ──────────────────────────────────────────────────────
    checks.push(check("distro", "ok", distro()));
    let session = env_or("XDG_SESSION_TYPE", "unknown");
    let desktop = env_or("XDG_CURRENT_DESKTOP", "unknown");
    checks.push(check("desktop", "ok", format!("{desktop} ({session})")));

    // ── Bluetooth ────────────────────────────────────────────────────────
    // The BLE link is how the two devices find each other at all; without an
    // adapter nothing else in the app can work.
    let bt = match cmd_ok("bluetoothctl", &["show"]) {
        Some(s) if s.contains("Powered: yes") => check("bluetooth", "ok", "adapter powered"),
        Some(s) if s.contains("Powered: no") => {
            check("bluetooth", "fail", "adapter present but powered off")
        }
        Some(_) => check("bluetooth", "warn", "adapter state unclear"),
        None => check("bluetooth", "fail", "no adapter (bluetoothctl found none)"),
    };
    checks.push(bt);

    // A powered adapter is not the same as a working one. BlueZ will accept
    // StartDiscovery and then never discover — `Discovering` stays false and
    // no advertising report ever arrives — and every symptom of that looks
    // exactly like a phone that is switched off. The scan loop counts the
    // rounds this happens on; say so here, with the fix, because the fix
    // (power the adapter off and on) drops the user's audio and so has to be
    // their decision rather than something the app does behind them.
    // BlueZ-specific: the counter is kept by the BlueZ discovery loop, and
    // WinRT's scanner reports nothing equivalent. No counter means no wedge to
    // report, which is the honest answer rather than a fabricated zero.
    #[cfg(target_os = "linux")]
    let wedged = crate::ble::not_discovering_rounds();
    #[cfg(not(target_os = "linux"))]
    let wedged = 0u32;
    if wedged > 0 {
        checks.push(check(
            "bluetooth_discovery",
            "fail",
            format!(
                "adapter is powered but not discovering ({wedged} scan round(s)) — \
                 turn Bluetooth off and on to clear it"
            ),
        ));
    } else {
        checks.push(check("bluetooth_discovery", "ok", "scanning works"));
    }

    // ── the phone link ───────────────────────────────────────────────────
    let paired = crate::ipc::get_peer_states().len();
    checks.push(if paired > 0 {
        check("paired", "ok", format!("{paired} device(s)"))
    } else {
        check("paired", "warn", "no phone paired yet")
    });

    // ── screen features ──────────────────────────────────────────────────
    // adb is what Universal Control and the second screen ride on. It is
    // optional — everything else works without it — so a miss is a warning.
    let adb = match cmd_ok("adb", &["devices"]) {
        None => check("adb", "warn", "adb not installed (screen features need it)"),
        Some(out) => {
            let n = out.lines().skip(1).filter(|l| l.trim().ends_with("device")).count();
            if n > 0 {
                check("adb", "ok", format!("{n} device(s) attached"))
            } else {
                check("adb", "warn", "installed, no device attached")
            }
        }
    };
    checks.push(adb);
    checks.push(if crate::mirror_inject::has_transport() {
        check("injector", "ok", "input transport up")
    } else {
        check("injector", "warn", "no input transport (cursor sharing idle)")
    });

    // ── the live pill ────────────────────────────────────────────────────
    // Owning the name means the extension is not just installed but LOADED by
    // the running shell — which is the distinction that actually matters, and
    // the one a user cannot see for themselves.
    checks.push(if !desktop.to_lowercase().contains("gnome") {
        check("gnome_extension", "warn", "not GNOME — pill unavailable")
    } else if bus_name_owned("org.vortex.Shell1") {
        check("gnome_extension", "ok", "loaded in the shell")
    } else {
        check("gnome_extension", "warn", "not loaded — log out and back in once")
    });

    // ── keyring ──────────────────────────────────────────────────────────
    // Keys live in the Secret Service. Without it pairing cannot be stored,
    // which the user meets as "it forgets my phone every restart".
    checks.push(if bus_name_owned("org.freedesktop.secrets") {
        check("keyring", "ok", "Secret Service available")
    } else {
        check("keyring", "fail", "no Secret Service (pairing cannot be saved)")
    });

    // ── mirroring codecs ─────────────────────────────────────────────────
    // Loaded at runtime, so a missing plugin is invisible until the moment
    // someone tries to mirror and gets nothing.
    // Whichever H.264 encoder the cast pipeline will actually pick. Asking for
    // x264enc alone reported a failure on a machine that casts fine through
    // openh264enc — and a check that cries wolf is worse than no check.
    let encoder = ["x264enc", "openh264enc"]
        .into_iter()
        .find(|e| cmd_ok("gst-inspect-1.0", &[e]).is_some());
    checks.push(match encoder {
        Some(e) => check("gstreamer", "ok", format!("{e} present")),
        None => check(
            "gstreamer",
            "warn",
            "no H.264 encoder (x264enc or openh264enc) — screen sharing cannot encode",
        ),
    });

    Diagnostics {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        checks,
        log_path: crate::applog::log_path().display().to_string(),
    }
}

/// The same findings as text, for pasting into an issue.
///
/// Deliberately carries no identifiers: no hostname, no SSID, no MAC, no peer
/// key. A bug report should cost the reporter nothing to send.
#[tauri::command]
pub(crate) fn diagnostics_report() -> String {
    let d = diagnostics();
    let mut s = format!("Vortex {}\n", d.app_version);
    for c in &d.checks {
        let mark = match c.level.as_str() {
            "ok" => "ok  ",
            "warn" => "warn",
            "fail" => "FAIL",
            _ => "?   ",
        };
        s.push_str(&format!("{mark}  {:<16} {}\n", c.id, c.detail));
    }
    s.push_str(&format!("\nlog: {}\n", d.log_path));
    s
}
