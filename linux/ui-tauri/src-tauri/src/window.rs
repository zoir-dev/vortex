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

/// Size the main window to the content the webview just measured, then reveal it.
///
/// The window is built `visible: false` and stays hidden until this runs, so
/// the user never sees the wrong size: the layout happens off-screen, gets
/// measured, and the window is shown once at the size that fits. A fixed width
/// in `tauri.conf.json` cannot do that — the content's real width depends on
/// the display's scale factor, the UI font and the locale (German and Russian
/// labels are materially wider than English), none of which are known at build
/// time.
///
/// `width`/`height` are CSS pixels, which are logical pixels — the same units
/// `LogicalSize` takes — so the scale factor needs no conversion here.
///
/// Clamped on both ends: never below the configured minimum (a measurement
/// that comes back absurdly small must not produce an unusable window) and
/// never past the monitor's usable area (a long unwrapped line must not push
/// the window off-screen or behind the panel).
#[tauri::command]
pub(crate) fn fit_main_window(
    app: tauri::AppHandle,
    width_ratio: f64,
    height_ratio: f64,
    reason: Option<String>,
    probe: Option<String>,
) -> Result<bool, String> {
    let Some(w) = app.get_webview_window("main") else {
        return Err("no main window".into());
    };

    // The monitor's usable box. `monitor.size()` is PHYSICAL, so divide by the
    // scale factor to land back in the logical units LogicalSize wants.
    let (max_w, max_h) = match w.current_monitor() {
        Ok(Some(m)) => {
            let sf = m.scale_factor();
            let sz = m.size();
            // 0.92 rather than 1.0: leave room for the shell's panel/dock and a
            // window border, which the monitor size does not account for.
            (
                (sz.width as f64 / sf) * 0.92,
                (sz.height as f64 / sf) * 0.92,
            )
        }
        // No monitor info (headless, race at startup): trust the measurement
        // rather than refuse to size at all.
        _ => (f64::MAX, f64::MAX),
    };

    // Mirrors tauri.conf.json's minWidth/minHeight.
    const MIN_W: f64 = 560.0;
    const MIN_H: f64 = 600.0;

    // GROW ONLY, never shrink.
    //
    // The measurement is a lower bound on what the content needs, not a
    // statement about what the window should be: a webview that reports early
    // (or reports a viewport the compositor has not laid out yet) comes back
    // far too small, and honouring that shrinks the window to the minimum —
    // which is precisely the bug this guard exists to stop. It also means a
    // window the user widened by hand is never clawed back.
    let cur = w
        .inner_size()
        .ok()
        .and_then(|sz| w.scale_factor().ok().map(|sf| (sz.width as f64 / sf, sz.height as f64 / sf)))
        .unwrap_or((0.0, 0.0));

    // The webview measured in CSS pixels, which are not this side's logical
    // pixels on a scaled display, so it sends how much MORE room it needs
    // rather than an absolute size. Applied to the width we actually have, the
    // units cancel.
    let target_w = (cur.0 * width_ratio).max(MIN_W).max(cur.0).min(max_w);
    let target_h = (cur.1 * height_ratio).max(MIN_H).max(cur.1).min(max_h);

    // Nothing to do — don't churn the window (or move it) for a no-op. Also the
    // caller's stop condition: it re-measures until this says "no".
    if (target_w - cur.0).abs() < 1.0 && (target_h - cur.1).abs() < 1.0 {
        tracing::info!(
            want_w = target_w,
            cur_w = cur.0,
            probe = probe.as_deref().unwrap_or(""),
            "window fit: already wide enough"
        );
        return Ok(false);
    }

    let win = w.clone();
    let _ = w.run_on_main_thread(move || {
        let _ = win.set_size(tauri::LogicalSize::new(target_w, target_h));
        // Re-centre: the window was centred at the old size, so growing it
        // from the top-left would drift it off centre (and possibly off-screen).
        let _ = win.center();
    });
    // Debug, not info: this fires a few times per launch while the layout
    // converges, and says nothing a working app needs to report. It is kept
    // because the `probe` string is what made this diagnosable at all — the
    // measurements disagreed with each other for a long time, and reading them
    // side by side is what settled it. `RUST_LOG=vortex_ui_tauri_lib::window=debug`.
    tracing::debug!(
        target_w,
        target_h,
        reason = reason.as_deref().unwrap_or("boot"),
        probe = probe.as_deref().unwrap_or(""),
        "main window grown to fit its content"
    );

    Ok(true)
}

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
