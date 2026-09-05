//! Showing the main window — in FRONT of whatever else is on screen.
//!
//! Every path that opens the window (tray, notification action, a second
//! launch from the dash, `--sms`) used to be `show()` + `set_focus()`, which
//! on GNOME leaves the window BEHIND a maximised terminal: we run on XWayland
//! and Mutter refuses a focus request that carries no user-activation
//! timestamp — the click landed on the panel, not on us. So the polite request
//! goes out first, and [`x11_focus`] then raises and focuses the window for
//! real, the same way the clipboard popup already had to.
//!
//! [`x11_focus`]: crate::x11_focus

use std::sync::atomic::AtomicU32;

use tauri::Manager;

/// The main window's X id, resolved once. It is only ever hidden (close is
/// `hide()` + `prevent_close()`), so the id stays valid for the process.
static MAIN_XID: AtomicU32 = AtomicU32::new(0);

/// Show the main window and bring it to the front. Safe from any thread.
pub(crate) fn present_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        present(&w);
    }
}

/// Show `w` and bring it to the front. Only the main window gets the X11
/// treatment — it is the one this module caches an id for.
pub(crate) fn present(w: &tauri::WebviewWindow) {
    let win = w.clone();
    let _ = w.run_on_main_thread(move || show_and_raise(&win));
}

/// Tray-click behaviour: hide the window when it is already the window you are
/// looking at, show and raise it otherwise.
///
/// Visibility alone is the wrong test — a window buried under a full-screen
/// terminal is "visible", and hiding it there is exactly the bug this is meant
/// to fix (you click to see it, it disappears). Focus is the honest question.
///
/// The whole decision runs on the main thread: the caller is the tray's D-Bus
/// task, where `is_visible()`'s round-trip would stall the menu.
pub(crate) fn toggle_main(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let win = w.clone();
    let _ = w.run_on_main_thread(move || {
        if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
            let _ = win.hide();
        } else {
            show_and_raise(&win);
        }
    });
}

/// Main thread only. `raise_and_focus` does its waiting on a thread of its own,
/// so this returns immediately.
///
/// NO `set_focus()` on X11, deliberately. That is the call GNOME answers with
/// "Vortex is ready" in the message tray instead of raising us: a focus request
/// with no user-activation timestamp is treated as focus stealing, and the
/// notification IS the refusal. Going straight to X (below) both raises the
/// window and keeps that notification from ever being posted.
fn show_and_raise(w: &tauri::WebviewWindow) {
    let _ = w.show();
    let _ = w.unminimize();
    // Ask the shell extension first — it is the only thing that actually works
    // on a Wayland session.
    //
    // The app cannot raise itself there. Wayland gives ordinary clients no
    // "raise me"; the sanctioned route is an xdg-activation token, and a token
    // is issued for a user action delivered TO the app — a tray click goes to
    // the shell, and the appindicator protocol carries no token to pass on.
    //
    // Going through X11 does not rescue it either. This process runs on
    // XWayland, but `_NET_ACTIVE_WINDOW` only orders the X stack, and on a
    // Wayland desktop Vortex is typically the ONLY X client — measured here:
    // one entry in `_NET_CLIENT_LIST`, our own. Every window it needs to come
    // in front of is a native Wayland one that EWMH cannot address, so Mutter
    // answers with its focus-stealing policy: the window stays put and the user
    // gets a "Vortex is ready" notification instead.
    //
    // The extension runs INSIDE Mutter, where `activate()` is the compositor
    // raising a window rather than a client asking it to.
    if activate_via_shell() {
        return;
    }
    // No extension (not GNOME, or it is disabled): fall back to the X11 route,
    // which is the right one on a real X session, and to `set_focus` otherwise.
    if on_x11() {
        crate::x11_focus::raise_and_focus(|t| t.trim() == "Vortex", &MAIN_XID, "main window");
    } else {
        let _ = w.set_focus();
    }
}

/// The app's WM_CLASS — matches `StartupWMClass` in the .desktop entries.
const WM_CLASS: &str = "vortex-ui-tauri";

/// Ask the Vortex GNOME extension to raise our window. `false` when the
/// extension is not there, which is a normal state, not an error.
fn activate_via_shell() -> bool {
    let out = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.vortex.Shell",
            "--object-path",
            "/org/vortex/Shell",
            "--method",
            "org.vortex.Shell1.ActivateWindow",
            WM_CLASS,
        ])
        .output();
    match out {
        // gdbus prints the return tuple: `(true,)` when a window was raised.
        Ok(o) if o.status.success() => {
            let ok = String::from_utf8_lossy(&o.stdout).contains("true");
            if !ok {
                tracing::debug!("shell extension found no window to activate");
            }
            ok
        }
        _ => false,
    }
}

/// Whether our toplevel is an X window — the app's own launcher pins
/// `GDK_BACKEND=x11` (WebKitGTK under Wayland has its own troubles), and a
/// session with no Wayland display at all is X11 by definition.
fn on_x11() -> bool {
    match std::env::var("GDK_BACKEND") {
        Ok(v) => v.split(',').any(|b| b == "x11"),
        Err(_) => std::env::var_os("WAYLAND_DISPLAY").is_none(),
    }
}
