//! Clipboard-history popup WINDOW management — split out of `clipboard.rs`.
//! Owns the frameless Super+V popup: pre-warm/show/hide, focus-loss auto-hide,
//! and the narrow↔wide resize for the detail pane. No coupling to the clipboard
//! store/sync internals — purely Tauri window plumbing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tauri::{AppHandle, Manager};

// One window, macOS-clipboard style: opens NARROW (list only) and WIDENS to
// reveal a right-hand detail pane when the selected entry needs it (image /
// long text), narrowing back otherwise. +2 for the card's 1px border each side.
const LIST_W: f64 = 462.0;
const PREVIEW_W: f64 = 440.0;
const WIDE_W: f64 = LIST_W + PREVIEW_W;

// Height is a share of the MONITOR, not a constant: a launcher panel that is
// 600px on a 1440-logical-tall display reads as a dialog, not as a panel. The
// clamp keeps it sane on a netbook and on a 5K.
const H_FRACTION: f64 = 0.62;
const H_MIN: f64 = 560.0;
const H_MAX: f64 = 920.0;
/// Fraction of the LEFTOVER vertical space that goes above the panel. Below
/// 0.5 it sits a little high, the way macOS launchers do — dead-centre looks
/// like it sank.
const TOP_BIAS: f64 = 0.38;

// Hide only when the window has truly lost focus. A short delayed re-check
// tolerates any transient blur the compositor emits.
static LIST_FOCUSED: AtomicBool = AtomicBool::new(false);

/// Height chosen for the current monitor (f64 bits; 0 = not computed yet), so
/// the detail-pane resize keeps the height it was shown at.
static POPUP_H: AtomicU64 = AtomicU64::new(0);

/// The popup's X window id, resolved once and reused. The window is hidden on
/// dismiss, never destroyed, so the id is stable for the process's life.
static CLIP_XID: AtomicU32 = AtomicU32::new(0);

fn popup_h() -> f64 {
    match POPUP_H.load(Ordering::Relaxed) {
        0 => H_MIN,
        bits => f64::from_bits(bits),
    }
}

fn hide_popup(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("clipboard") {
        let _ = w.hide();
    }
}

/// Size + top-left corner for the monitor the POINTER is on — the panel should
/// open on the screen the user is looking at, which `center()` (the window's
/// last monitor) gets wrong the moment a second display is attached.
fn panel_geometry(app: &AppHandle) -> (f64, f64, f64) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(m) = monitor else {
        return (H_MIN, 0.0, 0.0);
    };
    let scale = m.scale_factor();
    let logical_h = m.size().height as f64 / scale;
    let logical_w = m.size().width as f64 / scale;
    let origin_x = m.position().x as f64 / scale;
    let origin_y = m.position().y as f64 / scale;
    // Never taller than the screen with room to breathe, whatever the clamp says.
    let height = (logical_h * H_FRACTION)
        .clamp(H_MIN, H_MAX)
        .min(logical_h - 80.0);
    let x = origin_x + (logical_w - LIST_W) / 2.0;
    let y = origin_y + (logical_h - height) * TOP_BIAS;
    (height, x, y)
}

/// Put the keyboard on the popup via a direct X11 `SetInputFocus`. Tauri's
/// `set_focus()` only ASKS the WM, but under Wayland+XWayland Mutter's
/// focus-stealing prevention denies a focus request lacking a fresh
/// user-activation timestamp — and the Super+V press went to gnome-shell, not to
/// our already-running process. Without real focus the search box never gets the
/// caret AND the window never emits `Focused(false)`, so click-outside can't
/// auto-hide it. Going straight to X bypasses the WM policy.
///
/// Asking ONCE is not enough, which is what made this flaky: `show()` maps the
/// window asynchronously and X refuses `SetInputFocus` on a window that is not
/// yet viewable, and Mutter can take focus back a beat after it is granted. So
/// ask, then VERIFY with `GetInputFocus`, and keep at it until X agrees.
/// Nothing to force where there is no X server: the popup is a normal top-level
/// window and the platform's own focus rules apply. The Linux version exists
/// because Mutter grants focus and then takes it back a beat later.
#[cfg(not(target_os = "linux"))]
fn force_x11_focus() {}

#[cfg(target_os = "linux")]
fn force_x11_focus() {
    std::thread::spawn(|| {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            AtomEnum, ConfigureWindowAux, ConnectionExt, InputFocus, MapState, StackMode,
        };

        fn find(conn: &impl Connection, win: u32, target: &str) -> Option<u32> {
            if let Ok(r) =
                conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
            {
                if let Ok(r) = r.reply() {
                    if !r.value.is_empty()
                        && String::from_utf8_lossy(&r.value).contains(target)
                    {
                        return Some(win);
                    }
                }
            }
            let tree = conn.query_tree(win).ok()?.reply().ok()?;
            for child in tree.children {
                if let Some(w) = find(conn, child, target) {
                    return Some(w);
                }
            }
            None
        }

        /// X may report focus on a DESCENDANT of our toplevel (WebKitGTK nests
        /// its own X windows), which is still our window having the keyboard.
        fn owns(conn: &impl Connection, ancestor: u32, mut win: u32) -> bool {
            for _ in 0..8 {
                if win == ancestor {
                    return true;
                }
                let Some(parent) = conn
                    .query_tree(win)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.parent)
                else {
                    return false;
                };
                if parent == 0 || parent == win {
                    return false;
                }
                win = parent;
            }
            false
        }

        let Ok((conn, screen)) = x11rb::connect(None) else {
            return;
        };
        let root = conn.setup().roots[screen].root;

        // Resolve the id once: the tree walk costs a round-trip PER window, so
        // paying it on every open is both slow and pointless for a window we
        // only ever hide.
        let mut win = CLIP_XID.load(Ordering::Relaxed);
        if win == 0 {
            for _ in 0..25 {
                std::thread::sleep(std::time::Duration::from_millis(40));
                if let Some(w) = find(&conn, root, "Vortex Clipboard") {
                    win = w;
                    CLIP_XID.store(w, Ordering::Relaxed);
                    break;
                }
            }
            if win == 0 {
                tracing::warn!("clipboard popup: X window never appeared; focus not forced");
                return;
            }
        }

        for attempt in 0..40 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let viewable = conn
                .get_window_attributes(win)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|a| a.map_state == MapState::VIEWABLE)
                .unwrap_or(false);
            if !viewable {
                continue; // still mapping — SetInputFocus would be a BadMatch
            }
            let _ = conn
                .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
            let _ = conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
            let _ = conn.flush();
            let focused = conn
                .get_input_focus()
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.focus)
                .unwrap_or(0);
            if focused != 0 && owns(&conn, win, focused) {
                return; // X confirms we hold the keyboard
            }
        }
        tracing::warn!("clipboard popup: X never confirmed keyboard focus");
    });
}

fn schedule_group_hide(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(160));
        if !LIST_FOCUSED.load(Ordering::SeqCst) {
            hide_popup(&app);
        }
    });
}

/// Build the popup window. `visible` false is the pre-warm path — the window
/// and its webview exist, the page has booted, nothing is on screen.
fn build(app: &AppHandle, visible: bool) -> bool {
    match tauri::WebviewWindowBuilder::new(
        app,
        "clipboard",
        tauri::WebviewUrl::App("index.html#/clipboard".into()),
    )
    .title("Vortex Clipboard")
    .inner_size(LIST_W, popup_h())
    // Pin the floor at list-width so the compositor honours the narrow request
    // when the detail pane closes (it won't clamp up to content).
    .min_inner_size(LIST_W, H_MIN)
    .max_inner_size(WIDE_W, H_MAX)
    .decorations(false)
    // Transparent WINDOW so the page's rounded-corner card cuts cleanly; the
    // card paints a SOLID background, so there's no see-through.
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(visible)
    .focused(visible)
    .center()
    .build()
    {
        Ok(win) => {
            tracing::info!(visible, "clipboard popup: created");
            let app2 = app.clone();
            win.on_window_event(move |ev| {
                if let tauri::WindowEvent::Focused(f) = ev {
                    LIST_FOCUSED.store(*f, Ordering::SeqCst);
                    if !*f {
                        schedule_group_hide(&app2);
                    }
                }
            });
            true
        }
        Err(e) => {
            tracing::error!("clipboard popup: build failed: {e}");
            false
        }
    }
}

/// Build the popup hidden, at startup, so the FIRST Super+V is a show — not a
/// window build plus a webview boot plus a Vue mount while the user waits. The
/// launcher process itself costs ~50 ms (measured); everything else the first
/// open used to pay for happens here instead.
pub(crate) fn prewarm(app: &AppHandle) {
    if app.get_webview_window("clipboard").is_some() {
        return;
    }
    build(app, false);
}

/// Show the frameless clipboard-history popup. Reuses the window across opens
/// (hide on dismiss, show here); re-arms the webview via a direct eval so a
/// just-copied item appears on first open (WebKitGTK pauses hidden-webview JS,
/// so an event emit right after show() would race the webview waking). When the
/// eval lands before the page has mounted, the page's own `onMounted` re-arms —
/// so a never-yet-shown pre-warmed window is covered either way.
pub(crate) fn show_clipboard_window(app: &AppHandle) {
    let (h, x, y) = panel_geometry(app);
    POPUP_H.store(h.to_bits(), Ordering::Relaxed);

    if app.get_webview_window("clipboard").is_none() && !build(app, true) {
        return;
    }
    let Some(w) = app.get_webview_window("clipboard") else {
        return;
    };
    let _ = w.unminimize();
    let _ = w.set_always_on_top(true);
    let _ = w.set_size(tauri::LogicalSize::new(LIST_W, h));
    let _ = w.set_position(tauri::LogicalPosition::new(x, y));
    let _ = w.show();
    let _ = w.set_focus();
    // Mutter would otherwise deny the focus request: the Super+V press went to
    // gnome-shell, not to us. See `x11_focus`.
    crate::x11_focus::raise_and_focus(
        |t| t.contains("Vortex Clipboard"),
        &CLIP_XID,
        "clipboard popup",
    );
    let _ = w.eval("window.__vortexRearm && window.__vortexRearm()");
    tracing::info!("clipboard popup: shown");
}

#[tauri::command]
pub fn clipboard_hide(app: AppHandle) {
    hide_popup(&app);
}

/// Widen the popup to reveal the right-hand detail pane, or narrow it back to
/// list-only. The list calls this as the selection changes — `visible` is true
/// for entries that need the extra room (image / long text). Same window
/// throughout, so keyboard focus is never disturbed.
#[tauri::command]
pub fn clipboard_set_preview(app: AppHandle, visible: bool) {
    if let Some(w) = app.get_webview_window("clipboard") {
        let width = if visible { WIDE_W } else { LIST_W };
        let _ = w.set_size(tauri::LogicalSize::new(width, popup_h()));
    }
}
