//! Pointer/keyboard capture for Universal Control — the Windows half of
//! [`InputCapture`].
//!
//! This is the one subsystem that is genuinely *easier* here than on Linux,
//! where it needs the `xdg-desktop-portal` InputCapture portal plus libei and
//! only works on a compositor that implements both. Windows has no pointer
//! barrier, but a low-level hook plus `ClipCursor` does the same job on every
//! desktop, with no portal and no per-compositor behaviour.
//!
//! # How the illusion works
//!
//! 1. A `WH_MOUSE_LL` hook watches every pointer move. While disarmed it only
//!    looks for the cursor reaching the armed screen edge.
//! 2. On contact, `ClipCursor` pins the cursor to a 1×1 rectangle and the hook
//!    starts returning 1 — which SWALLOWS the event, so the local desktop stops
//!    seeing input at all.
//! 3. Because a clipped cursor cannot move, absolute positions would stop
//!    changing and relative motion would die with them. So each event is
//!    differenced against the pin point and the cursor is re-centred there.
//!    That is the same trick a first-person game uses for mouse-look, and it is
//!    what keeps deltas flowing indefinitely in one direction.
//! 4. `WH_KEYBOARD_LL` does the same for keys once armed, translating VK codes
//!    through [`crate::core::platform::vk_to_evdev`] because the phone speaks
//!    evdev.
//!
//! # Why a thread with a message loop
//!
//! `SetWindowsHookExW` for the low-level hooks delivers callbacks *on the thread
//! that installed them*, and only while that thread pumps messages — a hook
//! installed on a thread that never calls `GetMessageW` simply never fires, and
//! Windows silently un-hooks a callback that takes too long. So the hooks get a
//! dedicated thread whose only job is the pump, and the trait methods talk to it
//! through a channel. Third time this shape appears in the port: the
//! advertisement watcher and the toast thread have the same constraint.
//!
//! # Untested
//!
//! Never run. Only the keycode translation has tests, and those live with the
//! table. Everything here is type-checked against the Win32 metadata and no
//! more.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

use crate::core::platform::vk_to_evdev::vk_to_evdev;
use crate::core::platform::{BoxFuture, Edge, InputCapture, InputEvent};

/// True once the cursor has crossed the armed edge and we own the input.
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// Armed edge, as a discriminant the hook callback can read without a lock.
/// -1 = not armed.
static ARMED_EDGE: AtomicI32 = AtomicI32::new(-1);
/// The point the cursor is pinned to and re-centred on while capturing.
static PIN: (AtomicI32, AtomicI32) = (AtomicI32::new(0), AtomicI32::new(0));

/// Where captured events go. Set by [`InputCapture::arm`].
fn sink() -> &'static Mutex<Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>> {
    static S: OnceLock<Mutex<Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>>> =
        OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn emit(ev: InputEvent) {
    if let Ok(g) = sink().lock() {
        if let Some(tx) = g.as_ref() {
            // A closed channel means the consumer went away; the release path
            // handles the teardown, so drop the event quietly.
            let _ = tx.send(ev);
        }
    }
}

fn edge_code(edge: Edge) -> i32 {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

/// Has the cursor reached the armed edge?
///
/// One pixel of tolerance, not zero: the cursor stops at `width - 1`, and a
/// fast flick can land a sample a pixel short of the boundary.
fn at_armed_edge(x: i32, y: i32) -> bool {
    // SAFETY: GetSystemMetrics takes an index and returns an int.
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    match ARMED_EDGE.load(Ordering::Relaxed) {
        0 => x <= 0,
        1 => x >= w - 1,
        2 => y <= 0,
        3 => y >= h - 1,
        _ => false,
    }
}

/// Pin the cursor to a 1×1 rect at its current position and take the input.
fn begin_capture(x: i32, y: i32) {
    PIN.0.store(x, Ordering::Relaxed);
    PIN.1.store(y, Ordering::Relaxed);
    let rect = RECT {
        left: x,
        top: y,
        right: x + 1,
        bottom: y + 1,
    };
    // SAFETY: a valid rect on the desktop. Failure just means the cursor is not
    // pinned; the hook still swallows events, so control still transfers.
    let _ = unsafe { ClipCursor(Some(&rect)) };
    CAPTURING.store(true, Ordering::Relaxed);
    tracing::info!(x, y, "universal control: captured at the screen edge");
}

fn end_capture() {
    CAPTURING.store(false, Ordering::Relaxed);
    // SAFETY: None releases the clip, which is the documented way to restore
    // free cursor movement.
    let _ = unsafe { ClipCursor(None) };
}

/// `WH_MOUSE_LL` callback. Returning 1 swallows the event.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A negative code means "not ours to inspect"; the docs require passing it
    // straight on without looking at the payload.
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    let msg = wparam.0 as u32;

    if !CAPTURING.load(Ordering::Relaxed) {
        // Disarmed, or armed but not yet touched: only watch for the edge.
        if ARMED_EDGE.load(Ordering::Relaxed) >= 0
            && msg == WM_MOUSEMOVE
            && at_armed_edge(info.pt.x, info.pt.y)
        {
            begin_capture(info.pt.x, info.pt.y);
            return LRESULT(1);
        }
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let (px, py) = (PIN.0.load(Ordering::Relaxed), PIN.1.load(Ordering::Relaxed));
    match msg {
        WM_MOUSEMOVE => {
            let dx = (info.pt.x - px) as f64;
            let dy = (info.pt.y - py) as f64;
            if dx != 0.0 || dy != 0.0 {
                emit(InputEvent::Motion { dx, dy });
                // Re-centre so the NEXT event is measured from the pin again.
                // Without this the cursor sits against the clip edge and the
                // difference stops growing, which reads as the pointer having
                // stopped even though the user is still moving the mouse.
                let _ = SetCursorPos(px, py);
            }
        }
        WM_LBUTTONDOWN => emit(InputEvent::Button { button: 1, pressed: true }),
        WM_LBUTTONUP => emit(InputEvent::Button { button: 1, pressed: false }),
        WM_MBUTTONDOWN => emit(InputEvent::Button { button: 2, pressed: true }),
        WM_MBUTTONUP => emit(InputEvent::Button { button: 2, pressed: false }),
        WM_RBUTTONDOWN => emit(InputEvent::Button { button: 3, pressed: true }),
        WM_RBUTTONUP => emit(InputEvent::Button { button: 3, pressed: false }),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // The delta is in the HIGH word of mouseData, signed, in units of
            // WHEEL_DELTA (120) per notch — the trait wants notches.
            let raw = ((info.mouseData >> 16) & 0xFFFF) as i16;
            let notches = raw as f64 / 120.0;
            if msg == WM_MOUSEWHEEL {
                emit(InputEvent::Scroll { dx: 0.0, dy: notches });
            } else {
                emit(InputEvent::Scroll { dx: notches, dy: 0.0 });
            }
        }
        _ => {}
    }
    // Swallow everything while we own the input: the local desktop must not
    // also act on clicks meant for the phone.
    LRESULT(1)
}

/// `WH_KEYBOARD_LL` callback.
unsafe extern "system" fn key_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 || !CAPTURING.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let msg = wparam.0 as u32;
    let pressed = match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return CallNextHookEx(None, code, wparam, lparam),
    };
    match vk_to_evdev(info.vkCode as u16) {
        Some(keycode) => {
            emit(InputEvent::Key { keycode, pressed });
            LRESULT(1)
        }
        // Unmapped: pass it to the local desktop rather than swallowing a key
        // that would then reach neither machine.
        None => CallNextHookEx(None, code, wparam, lparam),
    }
}

enum Cmd {
    Arm(Edge),
    Release,
}

fn commands() -> &'static Mutex<Option<std::sync::mpsc::Sender<Cmd>>> {
    static TX: OnceLock<Mutex<Option<std::sync::mpsc::Sender<Cmd>>>> = OnceLock::new();
    TX.get_or_init(|| Mutex::new(None))
}

/// The hook thread: installs both hooks, then does nothing but pump messages.
///
/// The pump IS the work. Commands are handled between messages via
/// `try_recv`, because a blocking `recv` here would stop the pump and the hooks
/// with it.
fn hook_thread(rx: std::sync::mpsc::Receiver<Cmd>) {
    // SAFETY: a low-level hook takes no module handle and no thread id (0 =
    // global). Failure leaves `HHOOK` null, which we check.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) };
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), None, 0) };
    let (mouse, keyboard) = match (mouse, keyboard) {
        (Ok(m), Ok(k)) => (m, k),
        _ => {
            tracing::error!("universal control: could not install the input hooks");
            return;
        }
    };

    let mut msg = MSG::default();
    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Arm(edge) => ARMED_EDGE.store(edge_code(edge), Ordering::Relaxed),
                Cmd::Release => {
                    ARMED_EDGE.store(-1, Ordering::Relaxed);
                    end_capture();
                }
            }
        }
        // A 1-shot timer would be tidier, but GetMessageW blocks until SOME
        // message arrives and the hooks generate a constant stream while the
        // user is at the machine — so commands are picked up promptly in
        // practice, and the loop never spins when the machine is idle.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: both handles came from SetWindowsHookExW and are unhooked once.
    unsafe {
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
    }
}

fn send(cmd: Cmd) -> Result<(), String> {
    let mut guard = commands().lock().map_err(|_| "hook channel poisoned")?;
    if guard.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("vortex-input-hooks".into())
            .spawn(move || hook_thread(rx))
            .map_err(|e| format!("spawn hook thread: {e}"))?;
        *guard = Some(tx);
    }
    guard
        .as_ref()
        .expect("just set")
        .send(cmd)
        .map_err(|_| "input hook thread is gone".to_string())
}

pub struct WindowsInputCapture;

impl InputCapture for WindowsInputCapture {
    fn arm(
        &self,
        edge: Edge,
        tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    ) -> BoxFuture<Result<(), String>> {
        Box::pin(async move {
            if let Ok(mut g) = sink().lock() {
                *g = Some(tx);
            }
            send(Cmd::Arm(edge))
        })
    }

    fn release(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async move {
            let r = send(Cmd::Release);
            // Drop the sink AFTER telling the thread to stop, so an event
            // already in flight still has somewhere to go.
            if let Ok(mut g) = sink().lock() {
                *g = None;
            }
            r
        })
    }

    /// Not implemented, and not obviously needed.
    ///
    /// On Wayland the cursor keeps being drawn where the portal left it, which
    /// is why the Linux path asks Mutter to hide it. Here the cursor is clipped
    /// to a 1×1 rect and every move is swallowed, so it does not travel and
    /// there is nothing following the user's hand across the screen. The
    /// alternatives are all global and stateful (`SetSystemCursor` replaces the
    /// system arrow for every process and must be restored, including after a
    /// crash), which is a bad trade for hiding a stationary pointer.
    fn hide_cursor(&self, _hidden: bool) -> bool {
        false
    }
}

/// The cursor's current position, for the caller that wants to restore it after
/// a session. `None` if the desktop will not say.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    // SAFETY: `p` is ours; the call only writes it.
    if unsafe { GetCursorPos(&mut p) }.is_ok() {
        Some((p.x, p.y))
    } else {
        None
    }
}
