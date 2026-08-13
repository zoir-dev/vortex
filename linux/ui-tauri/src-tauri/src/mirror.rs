//! Screen-mirror RECEIVER (laptop): decode the phone's HEVC (H.265) video and
//! show it in the mirror window.
//!
//! All networking + crypto live in the daemon crate
//! (`core::mirror_session` for the TCP control plane, `core::mirror_udp` for the
//! sealed UDP video). This module is just the GStreamer half: it takes a channel
//! of decrypted, reassembled HEVC access units and pumps them into an `appsrc`,
//! plus the orchestration that wires a session together. Ported from the
//! upstream `ecosystem` `mirror_runtime.rs` (the raw-TCP receiver there is
//! replaced by the daemon's sealed transport here).
//!
//! Decoder backends, best-first: NVIDIA `nvh265dec`, VA-API `vah265dec`, else
//! software `avdec_h265`. All three end in `xvimagesink`, drawn into the window
//! `mirror_window` owns. XVideo rather than GL is a measured choice — see that
//! module for the numbers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use vortex_l3_daemon::core::crypto::x25519::X25519SecBytes;
use vortex_l3_daemon::core::mirror_session::{start_mirror_session, MirrorHandle, MirrorStart};
use vortex_l3_daemon::core::{mirror_tcp, mirror_udp};

/// The live control-session handle, kept alive for the stream's lifetime (its
/// channel senders must not drop or the session loops shut down). M3's X11 input
/// loop pushes into `input_tx`; `stop_mirror` takes + closes it.
static MIRROR_HANDLE: std::sync::Mutex<Option<MirrorHandle>> = std::sync::Mutex::new(None);

/// The live TCP video receiver task. Held so it can be ABORTED on teardown.
///
/// Without this it outlives its session: the receiver retries the connect for
/// as long as the phone's video server is closed (it only opens after the user
/// taps "Start now" on the consent dialog), so a second Start — a double click,
/// or the address-retry pass — leaves the first receiver still looping. Both
/// then connect the instant the phone's server opens, each holding a media key
/// derived from its OWN session's IK handshake hash. The phone encrypts for the
/// session it honoured (it debounces the duplicate START, see Android
/// `VortexStack.kt`), so the other receiver fails to open frame 0
/// ("AEAD open failed counter=0"), closes the socket, and takes the whole
/// stream down with it — leaving the window spinning on a mirror that had in
/// fact connected.
static VIDEO_RX_TASK: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(None);

/// Abort any previous video receiver and remember the new one.
fn set_video_rx_task(task: tokio::task::JoinHandle<()>) {
    if let Ok(mut g) = VIDEO_RX_TASK.lock() {
        if let Some(prev) = g.take() {
            prev.abort();
        }
        *g = Some(task);
    }
}

/// Abort the live video receiver, if any.
fn abort_video_rx_task() {
    let prev = VIDEO_RX_TASK.lock().ok().and_then(|mut g| g.take());
    if let Some(t) = prev {
        t.abort();
    }
}

/// The live GStreamer pipeline. Held so a UI "Stop sharing" (and `cleanup`) can
/// set it to Null without relying on the bus EOS cascade. It must reach Null
/// BEFORE the mirror window goes away — the sink is drawing into that window.
static MIRROR_PIPELINE: std::sync::Mutex<Option<gst::Pipeline>> = std::sync::Mutex::new(None);

/// The phone's display name, set when a mirror starts so the window and its
/// header can carry it.
static MIRROR_TITLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Sender clone of the live session's input channel, published while a mirror
/// is up so the window's input handlers (which run on the GTK thread, far from
/// `spawn_mirror`) can push laptop→phone packets into the sealed control plane.
/// Cleared on `stop_mirror`.
static INPUT_TX: std::sync::Mutex<Option<mpsc::Sender<Vec<u8>>>> = std::sync::Mutex::new(None);

/// True while the left mouse button is held down inside the mirror window — a
/// press starts a touch, moves extend it, release ends it. Reset at session
/// start. Single mirror at a time, so a process-wide flag is enough.
static POINTER_DOWN: AtomicBool = AtomicBool::new(false);

/// Wheel / two-finger scroll is replayed on the phone as a CONTINUOUS finger
/// drag (so it rides the same smooth gesture pump as a real drag). These hold
/// the synthetic finger between scroll notches; an idle-watcher thread lifts the
/// finger once the wheel goes quiet. `SCROLL_SERIAL` bumps on every notch so the
/// watcher can detect "no scroll for a while" without clock math.
static SCROLL_ACTIVE: AtomicBool = AtomicBool::new(false);
/// The synthetic finger's current position (normalized), accumulated across
/// notches on BOTH axes — vertical and horizontal, the latter driving
/// home-screen page swipes.
static SCROLL_FINGER_X: AtomicI32 = AtomicI32::new(0);
static SCROLL_FINGER_Y: AtomicI32 = AtomicI32::new(0);
/// Ensures the scroll idle-watcher thread is spawned exactly once.
static SCROLL_WATCHER: AtomicBool = AtomicBool::new(false);

/// Ctrl held (tracked from key events) → wheel becomes pinch-zoom, not scroll.
static CTRL_HELD: AtomicBool = AtomicBool::new(false);
/// Two-finger pinch-zoom state (Ctrl+wheel). The fingers sit above/below a fixed
/// centre; `PINCH_SPREAD` (their half-distance) grows to zoom in, shrinks to zoom
/// out. The same idle-watcher emits the MOVEs and lifts both fingers.
static PINCH_ACTIVE: AtomicBool = AtomicBool::new(false);
static PINCH_CX: AtomicI32 = AtomicI32::new(0);
static PINCH_CY: AtomicI32 = AtomicI32::new(0);
static PINCH_SPREAD: AtomicI32 = AtomicI32::new(0);
/// Initial half-spread between the two pinch fingers, and the change per notch.
const PINCH_INIT: i32 = 5_000;
const PINCH_NOTCH: i32 = 1_600;
const PINCH_MIN: i32 = 800;
const PINCH_MAX: i32 = 30_000;

/// Normalized units the finger travels per discrete vertical wheel notch.
/// High-res touchpads fire MANY of these per swipe, so keep it small or
/// scrolling races; the scroll-drag accumulates them and the watcher emits
/// real-touch MOVEs at a steady rate. Positive = finger DOWN. Bumped ~1.6× from
/// the original 950 so each wheel notch covers more screen (snappier scrolling).
const NOTCH_STEP: i32 = 1_500;

/// Larger per-notch step for HORIZONTAL notches: a home-screen page
/// flip needs the finger to cross a big fraction of the screen within one
/// scroll-drag burst, so each horizontal notch travels further. Positive =
/// finger RIGHT.
const NOTCH_STEP_X: i32 = 6_000;

/// laptop→phone input packet types (5-byte `[type][x_hi][x_lo][y_hi][y_lo]`,
/// x/y normalized to 0..=65535 of the video frame). The phone un-normalizes to
/// its real resolution and injects via its AccessibilityService. Touch is
/// DOWN→MOVE…→UP (a tap is DOWN+UP at one point); nav buttons ignore x/y.
mod input_proto {
    pub const DOWN: u8 = 0;
    pub const MOVE: u8 = 1;
    pub const UP: u8 = 2;
    pub const BACK: u8 = 10;
    #[allow(dead_code)]
    pub const HOME: u8 = 11;
    #[allow(dead_code)]
    pub const RECENTS: u8 = 12;
}

/// Scale one `mouse-scroll` delta into finger travel. GDK sends ±1.0 for a full
/// wheel notch and small fractions for high-resolution touchpad scrolling, so a
/// whole notch maps to exactly the tuned discrete step (`notch`) and anything
/// finer scales down from it. The clamp matters: unclamped, a single coarse
/// delta could fling the finger across the whole screen.
fn scroll_step(delta: f64, notch: i32) -> i32 {
    (delta.clamp(-1.0, 1.0) * notch as f64) as i32
}

/// Enqueue a 5-byte input packet onto the live session's sealed control plane.
/// `try_send` (drop-on-full) is deliberate: the channel is bounded and MOVE
/// floods are coalescible — losing an intermediate move never breaks a gesture
/// (DOWN/UP are rare and always make it through).
fn send_input(ty: u8, nx: u16, ny: u16) {
    // Prefer the real-touch uinput injector (scrcpy-style: low latency, true
    // multitouch, bypasses MIUI's INJECT_EVENTS block). Fall back to the
    // AccessibilityService control plane when adb/the injector isn't available.
    // The injector is only the better path if it can actually draw a finger.
    // Where it fell back to UHID there is no digitizer, and taps sent here would
    // vanish — the accessibility plane is worse, but it is not nothing.
    if crate::mirror_inject::active() && crate::mirror_inject::touch_available() {
        let cmd = match ty {
            input_proto::DOWN => format!("D 0 {nx} {ny}"),
            input_proto::MOVE => format!("M 0 {nx} {ny}"),
            input_proto::UP => "U 0".to_string(),
            input_proto::BACK => "K back".to_string(),
            input_proto::HOME => "K home".to_string(),
            input_proto::RECENTS => "K recents".to_string(),
            _ => return,
        };
        crate::mirror_inject::send(&cmd);
        return;
    }
    let pkt = vec![ty, (nx >> 8) as u8, nx as u8, (ny >> 8) as u8, ny as u8];
    if let Ok(g) = INPUT_TX.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.try_send(pkt);
        }
    }
}

/// Mouse and keyboard from the mirror window, already normalized to the video
/// frame (0..=65535 on both axes — `mirror_window` owns the widget-to-frame
/// mapping, including the letterbox margins). These used to arrive as
/// GstNavigation events from the video sink; XVideo draws into a window GTK
/// owns, so the events now come straight off the widget, which is both fewer
/// layers and the only way to read a scroll wheel accurately.

/// Left button down/up. A press ends any inertial scroll or pinch first — you
/// cannot be tapping and flinging at the same moment.
pub(crate) fn on_button(down: bool, nx: u16, ny: u16) {
    if down {
        flush_scroll();
        flush_pinch();
        POINTER_DOWN.store(true, Ordering::Relaxed);
        send_input(input_proto::DOWN, nx, ny);
    } else if POINTER_DOWN.swap(false, Ordering::Relaxed) {
        send_input(input_proto::UP, nx, ny);
    }
}

/// Pointer motion. Only meaningful while the button is held — a phone has no
/// hover, so a moving cursor with no finger down is nothing to send.
pub(crate) fn on_motion(nx: u16, ny: u16) {
    if POINTER_DOWN.load(Ordering::Relaxed) {
        send_input(input_proto::MOVE, nx, ny);
    }
}

/// Wheel / two-finger scroll. `dx`/`dy` are GDK's deltas: ±1.0 per wheel notch,
/// smaller fractions from a high-resolution touchpad. GDK's +y means "scrolled
/// DOWN" and the finger has to travel the other way to move the content that
/// way, hence the negation; x keeps GDK's sign. Ctrl turns the wheel into a
/// pinch, the way every desktop app does zoom.
pub(crate) fn on_scroll(nx: u16, ny: u16, dx: f64, dy: f64) {
    if CTRL_HELD.load(Ordering::Relaxed) {
        if dy != 0.0 {
            feed_pinch(nx, ny, if dy < 0.0 { 1 } else { -1 });
        }
        return;
    }
    if dx != 0.0 || dy != 0.0 {
        feed_scroll(nx, ny, scroll_step(dx, NOTCH_STEP_X), scroll_step(-dy, NOTCH_STEP));
    }
}

/// A key, named the X11 way ("a", "A", "Return", "BackSpace", "Escape", ...) —
/// which is exactly what `gdk::keys::Key::name()` gives us.
pub(crate) fn on_key(down: bool, key: &str) {
    if key == "Control_L" || key == "Control_R" {
        CTRL_HELD.store(down, Ordering::Relaxed); // wheel → pinch-zoom
    } else if down {
        inject_key(key);
    }
}

/// Linux key codes for letters a..z (QWERTY positional, NOT alphabetical).
const LETTER_CODES: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];
/// Linux key codes for digits 0..9.
const DIGIT_CODES: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
const KEY_LEFTSHIFT: u16 = 42;

/// Map a GstNavigation key name (X11 keysym / character) to a Linux key code and
/// whether Shift is needed. Covers letters, digits, common punctuation and the
/// editing/navigation keys — enough for real typing into phone text fields.
fn key_to_linux(key: &str) -> Option<(u16, bool)> {
    if key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some((
                LETTER_CODES[(ch.to_ascii_lowercase() as u8 - b'a') as usize],
                ch.is_ascii_uppercase(),
            ));
        }
        if ch.is_ascii_digit() {
            return Some((DIGIT_CODES[(ch as u8 - b'0') as usize], false));
        }
        return match ch {
            ' ' => Some((57, false)),
            '.' => Some((52, false)),
            ',' => Some((51, false)),
            '-' => Some((12, false)),
            '=' => Some((13, false)),
            '/' => Some((53, false)),
            ';' => Some((39, false)),
            '\'' => Some((40, false)),
            '@' => Some((3, true)),
            '!' => Some((2, true)),
            '?' => Some((53, true)),
            ':' => Some((39, true)),
            '_' => Some((12, true)),
            _ => None,
        };
    }
    match key {
        "space" => Some((57, false)),
        "Return" | "KP_Enter" => Some((28, false)),
        "BackSpace" => Some((14, false)),
        "Tab" => Some((15, false)),
        "Left" => Some((105, false)),
        "Right" => Some((106, false)),
        "Up" => Some((103, false)),
        "Down" => Some((108, false)),
        "period" => Some((52, false)),
        "comma" => Some((51, false)),
        "minus" => Some((12, false)),
        _ => None,
    }
}

/// Inject a keystroke. Esc maps to Android Back. Real keys ride the uinput
/// injector (a complete down/up, Shift-wrapped) — only available there; with the
/// accessibility fallback only Esc→Back works (no text injection).
fn inject_key(key: &str) {
    if key == "Escape" {
        send_input(input_proto::BACK, 0, 0);
        return;
    }
    if !crate::mirror_inject::active() {
        return;
    }
    if let Some((code, shift)) = key_to_linux(key) {
        if shift {
            crate::mirror_inject::send(&format!("E {KEY_LEFTSHIFT} 1"));
        }
        crate::mirror_inject::send(&format!("E {code} 1"));
        crate::mirror_inject::send(&format!("E {code} 0"));
        if shift {
            crate::mirror_inject::send(&format!("E {KEY_LEFTSHIFT} 0"));
        }
    }
}

/// Feed one scroll increment into the synthetic scroll-drag. The first
/// increment presses a finger at the cursor; each one slides it by
/// `(step_x, step_y)` (already-signed normalized units, +x = RIGHT, +y = DOWN);
/// the idle-watcher lifts it once scrolling stops. The phone replays this as one
/// continuous drag — vertical = scroll, horizontal = home-screen page swipe.
fn feed_scroll(nx: u16, ny: u16, step_x: i32, step_y: i32) {
    ensure_scroll_watcher();
    if !SCROLL_ACTIVE.swap(true, Ordering::Relaxed) {
        tracing::info!(nx, ny, "mirror: scroll-drag started");
        SCROLL_FINGER_X.store(nx as i32, Ordering::Relaxed);
        SCROLL_FINGER_Y.store(ny as i32, Ordering::Relaxed);
        send_input(input_proto::DOWN, nx, ny);
    }
    // Only ACCUMULATE the target here — a high-res touchpad fires notches far
    // faster than we should write to the adb pipe. The watcher emits real-touch
    // MOVEs from this position at a steady ~60Hz, so the pipe never floods (the
    // cause of the stutter) and the motion stays smooth.
    let new_x = (SCROLL_FINGER_X.load(Ordering::Relaxed) + step_x).clamp(0, 65_535);
    let new_y = (SCROLL_FINGER_Y.load(Ordering::Relaxed) + step_y).clamp(0, 65_535);
    SCROLL_FINGER_X.store(new_x, Ordering::Relaxed);
    SCROLL_FINGER_Y.store(new_y, Ordering::Relaxed);
}

/// Lift the synthetic scroll finger now (called when scrolling goes idle, a real
/// click begins, or the session stops). No-op if no scroll drag is active.
fn flush_scroll() {
    if SCROLL_ACTIVE.swap(false, Ordering::Relaxed) {
        let fx = SCROLL_FINGER_X.load(Ordering::Relaxed).clamp(0, 65_535) as u16;
        let fy = SCROLL_FINGER_Y.load(Ordering::Relaxed).clamp(0, 65_535) as u16;
        send_input(input_proto::UP, fx, fy);
    }
}

/// Feed one Ctrl+wheel notch into a two-finger pinch-zoom. `dir` +1 = zoom in
/// (fingers spread apart), -1 = zoom out. Real multitouch (uinput) only.
fn feed_pinch(cx: u16, cy: u16, dir: i32) {
    if !crate::mirror_inject::active() {
        return;
    }
    ensure_scroll_watcher();
    if !PINCH_ACTIVE.swap(true, Ordering::Relaxed) {
        flush_scroll(); // never pinch + scroll at once
        PINCH_CX.store(cx as i32, Ordering::Relaxed);
        PINCH_CY.store(cy as i32, Ordering::Relaxed);
        PINCH_SPREAD.store(PINCH_INIT, Ordering::Relaxed);
        let top = (cy as i32 - PINCH_INIT).clamp(0, 65_535);
        let bot = (cy as i32 + PINCH_INIT).clamp(0, 65_535);
        crate::mirror_inject::send(&format!("D 0 {cx} {top}"));
        crate::mirror_inject::send(&format!("D 1 {cx} {bot}"));
    }
    let s = (PINCH_SPREAD.load(Ordering::Relaxed) + dir * PINCH_NOTCH).clamp(PINCH_MIN, PINCH_MAX);
    PINCH_SPREAD.store(s, Ordering::Relaxed);
}

/// Lift both pinch fingers (idle, click, or session stop). No-op if inactive.
fn flush_pinch() {
    if PINCH_ACTIVE.swap(false, Ordering::Relaxed) {
        crate::mirror_inject::send("U 0");
        crate::mirror_inject::send("U 1");
    }
}

/// Spawn (once) the watcher that ends a scroll drag after the wheel goes quiet
/// (~120ms with no new notch), giving the phone a clean finger-up so its fling
/// settles. Sampling `SCROLL_SERIAL` avoids any clock arithmetic.
fn ensure_scroll_watcher() {
    if SCROLL_WATCHER.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        // ~80ms of quiet before lifting: short enough that the finger lifts while
        // the recent motion's velocity is still fresh, so Android computes a real
        // FLING (inertial scroll) — and the app renders the fling itself instead
        // of us dragging it frame-by-frame, which is smoother AND lighter.
        // ~190ms: long enough that a continuous touchpad/wheel scroll stays ONE
        // drag (no lift/re-press breaks between notch bursts — those read as
        // "uzilish"/stutter), short enough to stay responsive at the end.
        const IDLE_TICKS: u32 = 12;
        let mut last_x = -1i32;
        let mut last_y = -1i32;
        let mut last_spread = i32::MIN;
        let mut idle = 0u32;
        loop {
            // ~60Hz: steady emission rate decoupled from the touchpad's notch
            // rate, so the adb pipe never backs up.
            std::thread::sleep(Duration::from_millis(16));
            if SCROLL_ACTIVE.load(Ordering::Relaxed) {
                let tx = SCROLL_FINGER_X.load(Ordering::Relaxed);
                let ty = SCROLL_FINGER_Y.load(Ordering::Relaxed);
                if tx != last_x || ty != last_y {
                    send_input(input_proto::MOVE, tx.clamp(0, 65_535) as u16, ty.clamp(0, 65_535) as u16);
                    last_x = tx;
                    last_y = ty;
                    idle = 0;
                } else {
                    idle += 1;
                    if idle >= IDLE_TICKS {
                        flush_scroll();
                        idle = 0;
                    }
                }
            } else if PINCH_ACTIVE.load(Ordering::Relaxed) {
                let sp = PINCH_SPREAD.load(Ordering::Relaxed);
                if sp != last_spread {
                    let cx = PINCH_CX.load(Ordering::Relaxed);
                    let cy = PINCH_CY.load(Ordering::Relaxed);
                    let top = (cy - sp).clamp(0, 65_535);
                    let bot = (cy + sp).clamp(0, 65_535);
                    crate::mirror_inject::send(&format!("M 0 {cx} {top}"));
                    crate::mirror_inject::send(&format!("M 1 {cx} {bot}"));
                    last_spread = sp;
                    idle = 0;
                } else {
                    idle += 1;
                    if idle >= IDLE_TICKS {
                        flush_pinch();
                        idle = 0;
                    }
                }
            } else {
                last_x = -1;
                last_y = -1;
                last_spread = i32::MIN;
                idle = 0;
            }
        }
    });
}

/// The pacing grid, retuned to what the phone can SUSTAIN.
///
/// "Sustain" is the operative word, learned by measuring: a first attempt moved
/// the grid to each window's average rate, and it chased — 48, then 24, then 40
/// — turning the grid itself into a source of unevenness. Jitter went from
/// 17 ms to 114 ms and the worst gap from 65 ms to 1310 ms. A grid above what
/// the phone reliably delivers is the whole problem, because every frame the
/// display waits for and does not get is a visible hitch.
///
/// So: judge by the SLOWEST of the recent windows, drop to it at once, and rise
/// only when every window in the history agrees. Falling behind costs a
/// duplicated frame nobody sees; running ahead costs a stutter everybody does.
const PACE_LADDER: [i32; 7] = [24, 30, 40, 48, 60, 90, 120];

/// How many measurement windows must agree before the grid is allowed UP.
const PACE_RAISE_WINDOWS: usize = 3;

fn pace_grid_for(rate: f64) -> i32 {
    // The nearest rung at or below the delivery rate, so the grid never asks for
    // frames the phone is not sending. Never below the slowest rung: under ~24
    // the picture is choppy regardless and duplicating is the lesser evil.
    *PACE_LADDER
        .iter()
        .rev()
        .find(|&&g| (g as f64) <= rate + 2.0)
        .unwrap_or(&PACE_LADDER[0])
}

/// Decide the next grid from the recent window rates. `None` = leave it alone.
fn next_pace_grid(current: i32, history: &[f64]) -> Option<i32> {
    let slowest = history.iter().cloned().fold(f64::INFINITY, f64::min);
    let want = pace_grid_for(slowest);
    if want < current {
        return Some(want); // fall back immediately
    }
    if want > current
        && history.len() >= PACE_RAISE_WINDOWS
        && history.iter().all(|&r| pace_grid_for(r) >= want)
    {
        return Some(want); // every window agrees there is headroom
    }
    None
}

/// Retune the live pipeline's pacing grid. A capsfilter accepts new caps while
/// PLAYING; the renegotiation ripples back through `videorate`, which starts
/// resampling onto the new grid.
fn set_pace_grid(pipeline: &gst::Pipeline, fps: i32) {
    let Some(pace) = pipeline.by_name("pacecaps") else { return };
    let caps = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(fps, 1))
        .build();
    pace.set_property("caps", &caps);
    tracing::info!(fps, "mirror: pacing grid retuned to the phone's delivery rate");
}

/// Log how EVEN the displayed frames are, not just how many. Smoothness is a
/// property of the gaps between frames: 30 fps arriving every 33 ms looks
/// smooth, 30 fps arriving in bursts of five looks like stutter, and a plain
/// frame counter cannot tell those apart — which is why tuning this by counting
/// frames kept going in circles. Reports mean gap, its standard deviation and
/// the worst gap over each window.
fn attach_cadence_probe(pipeline: &gst::Pipeline) {
    let Some(vsink) = pipeline.by_name("vsink") else { return };
    let Some(pad) = vsink.static_pad("sink") else { return };
    let state = std::sync::Mutex::new((None::<std::time::Instant>, Vec::<f64>::new()));
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        let now = std::time::Instant::now();
        if let Ok(mut g) = state.lock() {
            if let Some(prev) = g.0.replace(now) {
                g.1.push(now.duration_since(prev).as_secs_f64() * 1000.0);
            }
            if g.1.len() >= 150 {
                let gaps = std::mem::take(&mut g.1);
                let n = gaps.len() as f64;
                let mean = gaps.iter().sum::<f64>() / n;
                let sd = (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / n).sqrt();
                let worst = gaps.iter().cloned().fold(0.0_f64, f64::max);
                // Debug, not info: at 60 fps this fires every couple of
                // seconds, which is a tuning instrument, not something a
                // shipped app should be writing to the user's journal.
                tracing::debug!(
                    fps = format!("{:.1}", 1000.0 / mean),
                    gap_ms = format!("{mean:.1}"),
                    jitter_ms = format!("{sd:.1}"),
                    worst_ms = format!("{worst:.0}"),
                    "mirror: display cadence"
                );
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// True if the box has any DRM render node. We scan for `renderD*` rather than
/// assuming `renderD128`: on a hybrid / multi-GPU machine the usable node can be
/// `renderD129` (or higher), and hardcoding 128 would wrongly fall back to slow
/// software decode. The `vah265dec` element find below is the real gate.
fn has_drm_render_node() -> bool {
    std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
        .unwrap_or(false)
}

/// Pick the best available HEVC (H.265) decoder backend.
pub fn detect_decoder_backend() -> &'static str {
    let _ = gst::init();
    if std::process::Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && gst::ElementFactory::find("nvh265dec").is_some()
    {
        return "nvdec";
    }
    if has_drm_render_node() && gst::ElementFactory::find("vah265dec").is_some() {
        return "vaapi";
    }
    "software"
}

/// Pre-decoder queue: non-leaky 4 buffers (absorbs IDR bursts without dropping
/// reference frames). Post-decoder queue: leaky=downstream 1 buffer (always show
/// the freshest frame). `h265parse config-interval=-1` re-injects VPS/SPS/PPS before
/// every IDR so a viewer that joined mid-stream recovers at the next keyframe.
///
/// Every backend ends in `xvimagesink`, rendered into the mirror window's own X
/// window (see `mirror_window`). That choice is measured, not assumed: on this
/// hybrid-GPU laptop the GL sinks sustain 14-18 fps at 720x1560 while XVideo
/// sustains 119-135 with nothing dropped, whatever the decoder in front of it.
/// The reason is that the GL context lives on the Intel display GPU, so every
/// frame the NVIDIA decoder produces crosses the bus — and even a plain CPU
/// upload into that context was 21 fps. So: no GL in this pipeline at all, and
/// no scaling either. XVideo scales into the window for free in hardware, which
/// is why the caps here stay at the decoded size.
fn pipeline_string_for_backend(backend: &str, _disp_w: u32, _disp_h: u32) -> String {
    // The grid is 30, not 60. Measured on this phone (Helio G80), scrcpy itself
    // sustains 29-40 fps at a QUARTER of our pixel count, so 60 was never on the
    // table; asking for it only produced an uneven 30.
    // Retime onto an even 30 fps grid and let the sink honour it. The phone
    // delivers frames in bursts — measured at the sink, showing each one the
    // moment it arrived gave a mean gap of 20-44 ms with a standard deviation
    // of 38-153 ms and single gaps up to 1504 ms, which is what "it freezes"
    // actually was: plenty of frames, delivered in clumps. Pacing them costs at
    // most one frame of latency and took the same stream to 17-18 ms deviation
    // with a worst gap of 65 ms.
    //
    // VORTEX_MIRROR_PACE=0 turns it off for anyone who would rather have the
    // lowest possible touch latency than an even picture.
    let paced = !std::env::var("VORTEX_MIRROR_PACE").is_ok_and(|v| v == "0");
    let sink = if paced {
        "videorate ! capsfilter name=pacecaps caps=video/x-raw,framerate=30/1 ! \
         gtksink name=vsink sync=true force-aspect-ratio=true"
    } else {
        "gtksink name=vsink sync=false force-aspect-ratio=true"
    };
    let sink = &sink;
    match backend {
        "nvdec" => format!(
            "appsrc name=src is-live=true format=time block=true max-bytes=1048576 do-timestamp=true \
             caps=video/x-h265,stream-format=byte-stream,alignment=au ! \
             queue max-size-buffers=4 max-size-bytes=4194304 max-size-time=0 ! \
             h265parse name=parser config-interval=-1 ! \
             nvh265dec ! videoconvert ! \
             queue name=postq leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! \
             {sink}"
        ),
        "vaapi" => format!(
            "appsrc name=src is-live=true format=time block=true max-bytes=1048576 do-timestamp=true \
             caps=video/x-h265,stream-format=byte-stream,alignment=au ! \
             queue max-size-buffers=4 max-size-bytes=4194304 max-size-time=0 ! \
             h265parse name=parser config-interval=-1 ! \
             vah265dec ! videoconvert ! \
             queue name=postq leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! \
             {sink}"
        ),
        _ => format!(
            "appsrc name=src is-live=true format=time block=true max-bytes=1048576 do-timestamp=true \
             caps=video/x-h265,stream-format=byte-stream,alignment=au ! \
             queue max-size-buffers=4 max-size-bytes=4194304 max-size-time=0 ! \
             h265parse name=parser config-interval=-1 ! \
             avdec_h265 max-threads=4 ! videoconvert ! \
             queue name=postq leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! \
             {sink}"
        ),
    }
}

/// A phone-sized window, scrcpy-style — in LOGICAL pixels, which is the only
/// unit that answers "how big does it look".
///
/// Two things had this wrong. It sized from the TALLEST connected monitor
/// (a 5120×2880 desktop panel here) and it used Tauri's PHYSICAL pixels while
/// GTK sizes windows logically — a third larger at 1.33× scaling. Between them
/// the window came out taller than the screen; the WM then clamped its height
/// but not its width, and a window wider than the video's aspect is exactly
/// where the black side bars came from.
///
/// So: the monitor the app is actually on, converted to logical pixels, and
/// capped three ways — never taller than the stream, never more than 65% of the
/// screen, and never more than 900 px, which is about the size a real phone
/// occupies on a desk. Aspect is preserved here and locked by the window's
/// geometry hints, so resizing can never reintroduce the bars.
fn window_size(app: &AppHandle, vw: u32, vh: u32) -> (u32, u32) {
    use tauri::Manager;
    let logical_h = app
        .get_webview_window("main")
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
        .map(|m| m.size().height as f64 / m.scale_factor())
        .filter(|h| *h > 0.0)
        .unwrap_or(900.0);
    let max_h = (logical_h * 0.65).min(900.0).min(vh as f64).max(240.0);
    let scale = max_h / vh as f64;
    let mut w = (vw as f64 * scale).round() as u32;
    let mut h = max_h.round() as u32;
    // Even dimensions (some sinks/scalers dislike odd sizes).
    w &= !1;
    h &= !1;
    (w.max(2), h.max(2))
}

/// Synchronously tear down the input + session side-effects when the mirror
/// window is closed by the user (X button / WM). Without this, closing the GL
/// window stopped only the GStreamer pipeline and LEAKED the uinput injector,
/// the adb forward and the control session.
fn cleanup_session() {
    flush_scroll();
    flush_pinch();
    crate::mirror_inject::stop();
    if let Ok(mut g) = INPUT_TX.lock() {
        *g = None;
    }
    POINTER_DOWN.store(false, Ordering::Relaxed);
    SCROLL_ACTIVE.store(false, Ordering::Relaxed);
    // Pipeline to Null FIRST, then the window: the sink must stop rendering
    // before the widget it draws into is destroyed.
    stop_pipeline();
    crate::mirror_window::close();
    // Dropping the handle ends the writer/reader loops (its channel senders go
    // away), which closes the phone-side session.
    if let Ok(mut g) = MIRROR_HANDLE.lock() {
        *g = None;
    }
}

/// Close-button / WM-close entry point for the mirror window. The teardown is
/// async (it sends STOP to the phone), so the GTK handler just fires it off and
/// returns — the window is destroyed by [`stop_mirror`] once the pipeline is
/// safely at Null.
pub(crate) fn request_stop() {
    tauri::async_runtime::spawn(async {
        stop_mirror().await;
    });
}

/// Set the live pipeline to Null (closing glimagesink's window) and forget it.
/// Idempotent — `take()` means a second caller (bus EOS, UI stop, cleanup) is a
/// no-op.
fn stop_pipeline() {
    if let Some(p) = MIRROR_PIPELINE.lock().ok().and_then(|mut g| g.take()) {
        let _ = p.set_state(gst::State::Null);
    }
}

/// Build + start the GStreamer pipeline; returns `(pipeline, appsrc)`. A bus
/// thread tears the pipeline down on EOS/Error (and treats the user closing the
/// window as a clean stop).
fn spawn_gstreamer_player(
    app: &AppHandle,
    backend: &str,
    vw: u32,
    vh: u32,
) -> Result<(gst::Pipeline, gst_app::AppSrc), String> {
    gst::init().map_err(|e| format!("gst init: {e}"))?;

    // Decode at the source size and let XVideo do the one scale into the window.
    // The pipeline used to rescale to the window size first, so every frame was
    // resampled twice — pure waste, and it cost sharpness whenever the window
    // was smaller than the stream.
    let (disp_w, disp_h) = (vw, vh);
    tracing::info!(vw, vh, "mirror: decoding at source size, XVideo scales to fit");
    let pipeline_str = pipeline_string_for_backend(backend, disp_w, disp_h);
    let element = gst::parse::launch(&pipeline_str).map_err(|e| format!("gst parse: {e}"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline downcast".to_string())?;
    let appsrc = pipeline
        .by_name("src")
        .ok_or_else(|| "appsrc not found".to_string())?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "appsrc cast".to_string())?;
    appsrc.set_is_live(true);
    appsrc.set_do_timestamp(true);
    appsrc.set_block(false);
    appsrc.set_format(gst::Format::Time);
    appsrc.set_max_bytes(4 * 1024 * 1024);

    attach_cadence_probe(&pipeline);

    // Put the sink's widget in our window, which is still showing the logo.
    // Done while the pipeline is below Playing: a gtk sink whose widget has no
    // parent when it starts makes a toplevel of its own.
    crate::mirror_window::attach_video(pipeline.clone());

    let bus = pipeline.bus().ok_or_else(|| "gst bus unavailable".to_string())?;
    let app_bus = app.clone();
    let pipeline_bus = pipeline.clone();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    let s = err.error().to_string();
                    let m = if s.contains("Quit requested") {
                        "mirror window closed by user".to_string()
                    } else {
                        format!("gst error: {} ({})", err.error(), err.debug().unwrap_or_default())
                    };
                    tracing::warn!("mirror: bus ERROR → {m} — tearing down");
                    let _ = app_bus.emit("mirror-player", serde_json::json!({ "message": m }));
                    let _ = pipeline_bus.set_state(gst::State::Null);
                    cleanup_session();
                    break;
                }
                MessageView::Eos(..) => {
                    tracing::warn!("mirror: bus EOS — tearing down");
                    let _ = app_bus.emit("mirror-player", serde_json::json!({ "message": "gst EOS" }));
                    let _ = pipeline_bus.set_state(gst::State::Null);
                    cleanup_session();
                    break;
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("set Playing: {e:?}"))?;
    // Publish the pipeline so a UI "Stop sharing" / cleanup can close the window
    // directly (the window's own X is unreliable under fractional scaling).
    if let Ok(mut g) = MIRROR_PIPELINE.lock() {
        *g = Some(pipeline.clone());
    }
    Ok((pipeline, appsrc))
}

/// Spawn the player task: build the pipeline and pump access units from `au_rx`
/// into `appsrc` until the channel closes (session ended) or the pipeline dies.
fn start_player(
    app: AppHandle,
    backend: &'static str,
    vw: u32,
    vh: u32,
    mut au_rx: mpsc::Receiver<Vec<u8>>,
) {
    tracing::info!(backend, "mirror: starting GStreamer player");
    tokio::spawn(async move {
        let (pipeline, appsrc) = match spawn_gstreamer_player(&app, backend, vw, vh) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("mirror: GStreamer start FAILED: {e}");
                let _ = app.emit("mirror-player", serde_json::json!({ "message": format!("gst start failed: {e}") }));
                return;
            }
        };
        tracing::info!("mirror: GStreamer pipeline Playing — window should open");
        let _ = app.emit("mirror-player", serde_json::json!({ "message": "mirror window opening" }));
        let mut first = true;
        // Arrival-rate tracking for the pacing grid. Measured HERE, before the
        // pipeline, because this is the phone's true delivery rate — after
        // `videorate` every frame is on the grid by definition, so measuring
        // there would only ever confirm the grid to itself.
        let mut window_start = std::time::Instant::now();
        let mut window_frames = 0u32;
        let mut grid = 30i32;
        let mut rates: Vec<f64> = Vec::new();
        while let Some(au) = au_rx.recv().await {
            if first {
                first = false;
                tracing::info!(bytes = au.len(), "mirror: first AU → appsrc");
                // There is a picture now — crossfade the window off the logo.
                crate::mirror_window::show_video();
                window_start = std::time::Instant::now();
            }
            window_frames += 1;
            let elapsed = window_start.elapsed().as_secs_f64();
            if elapsed >= 4.0 {
                let rate = window_frames as f64 / elapsed;
                window_frames = 0;
                window_start = std::time::Instant::now();
                rates.push(rate);
                if rates.len() > PACE_RAISE_WINDOWS {
                    rates.remove(0);
                }
                if let Some(want) = next_pace_grid(grid, &rates) {
                    grid = want;
                    set_pace_grid(&pipeline, grid);
                }
            }
            let mut buffer = match gst::Buffer::with_size(au.len()) {
                Ok(b) => b,
                Err(_) => break,
            };
            if let Some(bm) = buffer.get_mut() {
                if let Ok(mut map) = bm.map_writable() {
                    map.as_mut_slice().copy_from_slice(&au);
                }
            }
            if appsrc.push_buffer(buffer).is_err() {
                break;
            }
        }
        let _ = appsrc.end_of_stream();
        let _ = pipeline.set_state(gst::State::Null);
        let _ = app.emit("mirror-player", serde_json::json!({ "message": "mirror stopped" }));
    });
}

/// Orchestrate a full laptop-side mirror session:
///  1. bind a local UDP socket for the sealed video,
///  2. open the dedicated Noise-IK control session to the phone (`start_mirror_
///     session`) carrying the START params (incl. our UDP port),
///  3. derive the UDP media key from the IK handshake hash,
///  4. run the UDP receiver (decrypt + reassemble → access units),
///  5. feed the GStreamer player.
///
/// The peer IP + identity/PRS come from the worker (LAN discovery). Returns once
/// the session is established (streaming proceeds in spawned tasks).
#[allow(clippy::too_many_arguments)]
pub async fn spawn_mirror(
    app: AppHandle,
    phone_addr: SocketAddr,
    static_priv: &X25519SecBytes,
    peer_pub: &[u8; 32],
    prs: &[u8; 32],
    local_counter: u64,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<(), String> {
    // Bind via socket2 so we can grow SO_RCVBUF before handing off to tokio: the
    // phone fires all fragments of a frame back-to-back, so a whole IDR (tens of
    // KB → tens of packets) lands as one burst. A small default buffer drops the
    // tail of each burst → incomplete frames. 4 MB (kernel rmem_max) holds many
    // frames' worth, killing the burst loss.
    // Open the control session (START/STOP/input + keepalive). Video no longer
    // rides UDP, so `udp_port` is vestigial — the phone serves video on its own
    // fixed TCP port and the laptop connects out to it.
    let start = MirrorStart { w: width, h: height, fps, bitrate, udp_port: 0 };
    let handle =
        start_mirror_session(phone_addr, static_priv, peer_pub, prs, local_counter, start).await?;

    // TCP video data-plane (reliable + ordered, like scrcpy): connect OUT to the
    // phone's video server and stream sealed access units. TCP never loses a
    // frame, so there are no UDP-style burst-loss freezes — and no hole-punch is
    // needed (the laptop is the connecting side; its firewall allows that).
    let key = mirror_udp::derive_media_key(&handle.handshake_hash);
    let (au_tx, au_rx) = mpsc::channel::<Vec<u8>>(8);
    // Tracked + cancellable: a receiver from a previous session would otherwise
    // still be retrying its connect with a stale media key. See VIDEO_RX_TASK.
    set_video_rx_task(tokio::spawn(mirror_tcp::run_tcp_video_receiver(
        phone_addr.ip(),
        key,
        au_tx,
        Some(handle.keyframe_tx.clone()),
    )));

    let backend = detect_decoder_backend();
    start_player(app, backend, width, height, au_rx);

    // Bring up the real-touch injector (scrcpy-style uinput over adb) off the
    // async runtime — push + connect-retries take ~1-2s and must not block a
    // tokio worker. Until it's up, input rides the accessibility fallback.
    std::thread::spawn(|| {
        crate::mirror_inject::start();
    });

    // Publish the input channel BEFORE storing the handle so the navigation
    // probe can start pushing the moment the window accepts events.
    if let Ok(mut g) = INPUT_TX.lock() {
        *g = Some(handle.input_tx.clone());
    }
    // Keep the control session alive for the stream's lifetime (the input probe
    // reaches its `input_tx` via INPUT_TX; `stop_mirror` closes it).
    if let Ok(mut g) = MIRROR_HANDLE.lock() {
        *g = Some(handle);
    }
    Ok(())
}

/// Tear down the active mirror session (sends STOP to the phone + drops the
/// session, which ends the writer/reader loops and the UDP receiver).
pub async fn stop_mirror() {
    flush_scroll();
    flush_pinch();
    crate::mirror_inject::stop();
    if let Ok(mut g) = INPUT_TX.lock() {
        *g = None;
    }
    POINTER_DOWN.store(false, Ordering::Relaxed);
    SCROLL_ACTIVE.store(false, Ordering::Relaxed);
    // Kill the video receiver before the decoder: it must not outlive this
    // session and reconnect later with a now-stale media key.
    abort_video_rx_task();
    // Decoder first, then the window it renders into.
    stop_pipeline();
    crate::mirror_window::close();
    let handle = MIRROR_HANDLE.lock().ok().and_then(|mut g| g.take());
    if let Some(h) = handle {
        h.stop().await;
    }
}

/// `UiCmd::StartMirror` handler — resolve the phone's CURRENT LAN address and
/// open a mirror session. Split out of `run_worker`.
///
/// Address resolution is `lan::resolve_peer_addr`, NOT a raw read of the
/// cached IP: the cache goes stale whenever the phone's DHCP lease moves
/// (Wi-Fi rejoin, router restart), and blindly dialing it made the mirror
/// "sometimes just not start" on the very same network. The resolver probes
/// the cached IP first (2 s), falls back to a fresh mDNS browse, then the
/// probed gateway (hotspot) — and if the session STILL fails on an address
/// that answered TCP, we force one fresh rediscovery and retry before giving
/// up, emitting a `mirror-player` message either way so the UI never fails
/// silently.
/// The frame rate to ask the phone for: its own panel's refresh rate, since a
/// capture cannot carry frames the display never draws. Asking for more wastes
/// encoder budget on frames that will not exist; asking for a fixed 60 caps a
/// 120 Hz phone at half its panel. Falls back to the caller's number when the
/// phone has not reported one (older build), and never exceeds 120 — beyond
/// that the encoder and the link cost more than the eye gets back.
fn requested_fps(fallback: u32) -> u32 {
    match crate::lan::PEER_DISPLAY_HZ.load(Ordering::Relaxed) {
        0 => fallback,
        hz => hz.min(120),
    }
}

pub(crate) async fn handle_start_cmd(
    ctx: &crate::worker_ctx::WorkerCtx,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) {
    let fps = requested_fps(fps);
    tracing::info!(fps, "mirror: frame rate requested from the phone");
    // Tear down any prior session first so repeated clicks don't stack up
    // receivers / leave a half-open phone server.
    stop_mirror().await;
    let peer = ctx.peer_store.list().ok().and_then(|l| l.into_iter().next());
    let Some(peer) = peer else {
        tracing::warn!("start mirror: no trusted peer");
        return;
    };
    let title = peer.peer_name.clone().unwrap_or_else(|| "Phone".to_string());
    if let Ok(mut g) = MIRROR_TITLE.lock() {
        *g = Some(title.clone());
    }
    // Put the window up NOW, showing the logo and a spinner. Resolving the
    // phone's address and running the handshake takes a second or two, and
    // until this the click produced nothing visible at all. Sized with the same
    // fit as the video, so nothing jumps when the picture arrives.
    let (win_w, win_h) = window_size(&ctx.app, width, height);
    crate::mirror_window::open(title, width, height, win_w as i32, win_h as i32);
    let counter = ctx.peer_store.load_counter(&peer.peer_static_pub).unwrap_or(0);
    let app_c = ctx.app.clone();
    let identity_c = ctx.identity.clone();
    tokio::spawn(async move {
        let mut tried: Option<SocketAddr> = None;
        let mut last_err: Option<String> = None;
        for fresh in [false, true] {
            let Some(addr) = crate::lan::resolve_peer_addr(fresh).await else {
                break;
            };
            // The retry pass only helps on a DIFFERENT address — re-dialing
            // the one that just failed would just fail again.
            if tried == Some(addr) {
                continue;
            }
            tried = Some(addr);
            tracing::info!(%addr, fresh, "start mirror → opening session");
            match spawn_mirror(
                app_c.clone(),
                addr,
                &identity_c.static_priv.0,
                &peer.peer_static_pub,
                &peer.prs,
                counter,
                width,
                height,
                fps,
                bitrate,
            )
            .await
            {
                Ok(()) => return,
                Err(e) => {
                    // An address can pass the TCP probe yet fail the session —
                    // e.g. the old lease now belongs to another device. Round 2
                    // skips the cache and rediscovers from scratch.
                    tracing::warn!(%addr, "start mirror failed: {e}{}", if fresh { "" } else { " — rediscovering + retrying" });
                    last_err = Some(e);
                }
            }
        }
        let msg = match last_err {
            Some(e) => format!("mirror failed: {e}"),
            None => "phone not reachable on LAN".to_string(),
        };
        // Take the window down with the attempt. It was opened optimistically
        // at click time, and nothing else closes it on this path — so a failed
        // start used to leave a window spinning on "Connecting…" forever, which
        // reads exactly like a mirror that froze.
        crate::mirror_window::close();
        tracing::warn!("start mirror: {msg}");
        let _ = app_c.emit("mirror-player", serde_json::json!({ "message": msg }));
    });
}

/// `UiCmd::StopMirror` handler — fire-and-forget teardown.
pub(crate) fn handle_stop_cmd() {
    tokio::spawn(async {
        stop_mirror().await;
    });
}
