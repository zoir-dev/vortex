//! Raising a window and putting the keyboard on it, straight through X11.
//!
//! Tauri's `set_focus()` only ASKS the WM, and under Wayland+XWayland Mutter's
//! focus-stealing prevention denies a focus request lacking a fresh
//! user-activation timestamp — the click that got us here went to gnome-shell
//! (the Super+V shortcut, the tray icon, a notification action), not to our
//! already-running process. The window then maps BEHIND whatever is on screen
//! and merely blinks in the dash. Going straight to X bypasses the WM policy.
//!
//! Asking ONCE is not enough, which is what made this flaky: `show()` maps the
//! window asynchronously and X refuses `SetInputFocus` on a window that is not
//! yet viewable, and Mutter can take focus back a beat after it is granted. So
//! ask, then VERIFY with `GetInputFocus`, and keep at it until X agrees.

use std::sync::atomic::{AtomicU32, Ordering};

/// Raise `matches`'s window above the stack and focus it, off the caller's
/// thread.
///
/// `matches` runs against each window's `WM_NAME` — a predicate rather than a
/// substring because our two toplevels are "Vortex" and "Vortex Clipboard", and
/// a `contains` test for the former would happily land on the latter.
///
/// `cache` holds the resolved X id between calls: the tree walk costs a
/// round-trip PER window, which is both slow and pointless for a window we only
/// ever hide (close is `hide()` + `prevent_close()`, so the id outlives it).
pub(crate) fn raise_and_focus(
    matches: fn(&str) -> bool,
    cache: &'static AtomicU32,
    tag: &'static str,
) {
    std::thread::spawn(move || {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, InputFocus,
            MapState, StackMode,
        };

        fn find(conn: &impl Connection, win: u32, matches: fn(&str) -> bool) -> Option<u32> {
            if let Ok(r) =
                conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
            {
                if let Ok(r) = r.reply() {
                    if !r.value.is_empty() && matches(&String::from_utf8_lossy(&r.value)) {
                        return Some(win);
                    }
                }
            }
            let tree = conn.query_tree(win).ok()?.reply().ok()?;
            for child in tree.children {
                if let Some(w) = find(conn, child, matches) {
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

        // The atom that actually moves a window on GNOME. See `activate`.
        let net_active_window = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);

        /// Ask the WINDOW MANAGER to activate the window, the EWMH way.
        ///
        /// This is the part that was missing. Mutter reparents every managed
        /// window into its own frame and owns the stacking order, so a client
        /// that calls `ConfigureWindow(ABOVE)` or `SetInputFocus` on its own
        /// window is simply overruled — which is why the window kept landing
        /// behind the terminal and GNOME posted "Vortex is ready" instead.
        ///
        /// `_NET_ACTIVE_WINDOW` is a request to the WM rather than an attempt
        /// to go around it, and the first field is what makes it work: source
        /// indication 2 means "a pager sent this". Requests from applications
        /// (source 1) are exactly what focus-stealing prevention exists to
        /// refuse; pager requests are treated as the user's own intent and are
        /// honoured. It is what `wmctrl -a` sends, and what a Qt app like
        /// Telegram ends up sending — hence the difference the user sees.
        fn activate(conn: &impl Connection, root: u32, win: u32, atom: Option<u32>) {
            let Some(atom) = atom else { return };
            let ev = ClientMessageEvent::new(32, win, atom, [2u32, 0, 0, 0, 0]);
            let _ = conn.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                ev,
            );
        }

        let mut win = cache.load(Ordering::Relaxed);
        if win == 0 {
            for _ in 0..25 {
                std::thread::sleep(std::time::Duration::from_millis(40));
                if let Some(w) = find(&conn, root, matches) {
                    win = w;
                    cache.store(w, Ordering::Relaxed);
                    break;
                }
            }
            if win == 0 {
                // Also the native-Wayland case: no X window to find, and the
                // compositor's own focus rules are all there is.
                tracing::debug!("{tag}: X window never appeared; focus not forced");
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
            // Ask the WM first — this is the one that works under Mutter.
            activate(&conn, root, win, net_active_window);
            // Then the direct calls, for the WMs that let a client do it
            // itself (and for a bare X session with no EWMH compositor at all).
            // Under Mutter these are no-ops, which is precisely why asking the
            // WM had to be added rather than these being tuned.
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
        tracing::debug!("{tag}: X never confirmed keyboard focus");
    });
}
