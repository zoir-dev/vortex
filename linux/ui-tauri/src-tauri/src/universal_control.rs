//! Universal Control — the laptop's cursor + keyboard cross the screen edge onto
//! the phone (Apple Mac↔iPad style), driving the phone's NATIVE cursor through
//! the existing uinput injector. Wayland-native: the `xdg-desktop-portal`
//! InputCapture portal places a pointer barrier on the chosen screen edge and,
//! once the cursor crosses it, delivers RELATIVE pointer/keyboard over libei
//! (EIS). We forward those as `P/B/W/E` lines to [`crate::mirror_inject`].
//!
//! Feel (proven via de-risk on GNOME 50): a 2 ms flush coalesces bursts so the
//! writer/socket never backs up (per-event flooding stuttered), 1:1 motion (no
//! "floaty" acceleration), and a return-to-laptop gesture — push back past the
//! entry edge and control snaps home, like crossing off a second monitor.
//!
//! The libei stream types are `!Send`, so the capture loop runs on a dedicated
//! thread with a current-thread Tokio runtime rather than `tokio::spawn`.
//!
//! The laptop cursor is hidden while captured by toggling `CursorHidden` on a
//! per-session `org.vortex.UniversalControl` D-Bus object; the GNOME extension
//! reacts with Mutter's `inhibit_cursor_visibility()` (the portal can't hide it).
//!
//! # Errors the user sees
//!
//! Every way this can fail is environmental — no portal, no adb, an edge someone
//! else holds — so the reason has to reach the settings switch, and in the user's
//! own language. Hence `<code>` or `<code>|<detail>` rather than an English
//! sentence: the UI translates the code (`settings.uc_err_<code>`) and appends
//! the detail as diagnostics. An error with no code passes through verbatim,
//! which is what the ones from deeper down (libei, zbus) do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ashpd::desktop::input_capture::{Barrier, Capabilities, InputCapture};
use futures::StreamExt;
use reis::ei::{self, button::ButtonState, keyboard::KeyState};
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::{EiConvertEventStream, EiEventStream};

/// A capture session is live (loop running). Prevents double-starts.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Set by [`uc_stop`]; the capture loop notices on its next flush tick, releases
/// the portal and exits.
static STOP: AtomicBool = AtomicBool::new(false);

/// Push back past the entry edge by this many delta units → return to laptop.
const RETURN_MARGIN: f32 = 60.0;

/// How far you have to keep pushing past the barrier before control crosses,
/// when you are moving SLOWLY — Apple's "push the cursor all the way through".
/// Without some such thing, every brush against the screen edge throws the
/// pointer onto the phone.
const PUSH_THROUGH: f32 = 10.0;

/// …and how far when you are moving fast. Distance alone reads as a wall, and
/// here it really is one: the compositor holds the laptop pointer still while
/// the push is measured, and unlike Apple we cannot show the pointer half onto
/// the other screen, so there is nothing to tell you the shove is working. But
/// a deliberate move at an edge is a FAST one, and an accidental graze is not,
/// so speed separates them better than distance ever did. Deskflow reaches the
/// same conclusion from the other side and gates on time instead (switchDelay,
/// switchDoubleTap) rather than on how hard you push.
const PUSH_THROUGH_FAST: f32 = 5.0;

/// Inward speed (units/sec) at or below which the full [`PUSH_THROUGH`] is
/// required, and at or above which only [`PUSH_THROUGH_FAST`] is.
///
/// Measured rather than guessed: with the window at 400..1500 the fastest
/// crossing took 9 ms and the median 259 ms, which says an ordinary deliberate
/// push was not being counted as fast at all — only a flick was. A push at the
/// edge is already a decision; it does not have to be a violent one.
///
/// The distances came down with it. Pushing slowly used to mean dragging the
/// mouse a long way into a pointer that would not move — the resistance is
/// meant to be felt, not leaned against.
const PUSH_SLOW: f32 = 40.0;
const PUSH_FAST: f32 = 500.0;

// NOTE. Do not put the phone on an edge something else already claims — an
// auto-hiding dock, a hot corner. Not because of these thresholds, but because
// the edge cannot be shared at all.
//
// A dock reveals itself from a pressure barrier of its own (dash-to-dock:
// pressure-threshold 100, show-delay 0.25s). Raising ours above 100 looks like
// it should let the dock win the gentle pushes, and it does nothing: the
// instant our barrier activates the compositor stops moving the pointer and
// routes the motion to us over libei, so the dock's counter freezes wherever it
// stood — near zero — and never advances however hard the user pushes. Measured:
// crossings landing in 110-250 ms at a threshold of 130, dock never appearing.
//
// Whoever captures first owns the edge, and we capture on contact.

/// How much of the motion has to be INTO the edge rather than along it.
///
/// This is what lets the push thresholds be as small as they are. The thing a
/// large threshold was really guarding against is a pointer sweeping ALONG the
/// edge — reaching for something in the corner — whose contact with the barrier
/// throws a burst of inward motion at us. Direction separates that from a
/// deliberate push far more cleanly than distance does, and unlike distance it
/// costs the deliberate push nothing. 0.6 allows a fairly diagonal approach and
/// still refuses anything that is mostly sideways.
const PUSH_INWARD_RATIO: f32 = 0.6;

/// Abandon a half-finished push after this long with no further progress. Kept
/// short on purpose: the laptop pointer is held still while we measure.
const PUSH_IDLE: Duration = Duration::from_millis(60);

/// How much of the chosen edge is armed.
///
/// A whole edge is the obvious thing and often the wrong one: anything else
/// that lives there — an auto-hiding dock, a hot corner — loses it entirely,
/// because the compositor stops moving the pointer the moment our barrier
/// activates and the other claimant's own pressure counter freezes at zero. A
/// short strip in one corner leaves the rest of the edge to its usual owner.
#[derive(Clone, Copy, PartialEq)]
enum Segment {
    /// The whole edge.
    Full,
    /// The low end: left for a horizontal edge, top for a vertical one.
    Start,
    /// The high end: right for a horizontal edge, bottom for a vertical one.
    End,
}

/// How long that strip is, in logical pixels — about a phone's width, which is
/// what it stands in for.
const SEGMENT_LEN: i32 = 400;

#[derive(Clone, Copy)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where the phone physically sits relative to the laptop (which screen edge is
/// the portal). Read from `~/.local/share/vortex/universal_control/placement`
/// ("left"|"right"|"top"|"bottom"); defaults to Right.
fn placement() -> (Edge, Segment) {
    let p = std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h).join(".local/share/vortex/universal_control/placement")
    });
    let s = p
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    parse_placement(s.trim())
}

/// `<edge>` or `<edge>-<end>`, e.g. "bottom" or "bottom-right".
fn parse_placement(s: &str) -> (Edge, Segment) {
    let s = s.to_lowercase();
    let (edge_s, end_s) = s.split_once('-').unwrap_or((s.as_str(), ""));
    let edge = match edge_s {
        "left" => Edge::Left,
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        _ => Edge::Right,
    };
    // "right"/"left" name the ends of a horizontal edge, "top"/"bottom" those of
    // a vertical one; both spellings map onto the same two ends.
    let seg = match end_s {
        "left" | "top" => Segment::Start,
        "right" | "bottom" => Segment::End,
        _ => Segment::Full,
    };
    (edge, seg)
}

/// Persist the phone placement (called by the settings UI).
#[tauri::command]
pub(crate) fn uc_set_placement(edge: String) -> Result<(), String> {
    let edge = edge.trim().to_lowercase();
    let (head, tail) = edge.split_once('-').unwrap_or((edge.as_str(), ""));
    if !matches!(head, "left" | "right" | "top" | "bottom")
        || !matches!(tail, "" | "left" | "right" | "top" | "bottom")
    {
        return Err(format!("bad placement: {edge}"));
    }
    let home = std::env::var_os("HOME").ok_or("no HOME")?;
    let dir = std::path::PathBuf::from(home).join(".local/share/vortex/universal_control");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("placement"), edge).map_err(|e| e.to_string())?;
    Ok(())
}

/// The current placement as a lowercase string (for the settings UI).
#[tauri::command]
pub(crate) fn uc_get_placement() -> String {
    let (edge, seg) = placement();
    let end = match (seg, edge) {
        (Segment::Full, _) => return edge_name(edge).to_string(),
        (Segment::Start, Edge::Top | Edge::Bottom) => "left",
        (Segment::Start, _) => "top",
        (Segment::End, Edge::Top | Edge::Bottom) => "right",
        (Segment::End, _) => "bottom",
    };
    format!("{}-{end}", edge_name(edge))
}

/// Where the switch's own position is remembered, next to the placement.
fn enabled_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".local/share/vortex/universal_control/enabled"))
}

/// Remember whether the user wants the edge armed, so a reboot or a quit does
/// not quietly turn the feature off. Only the intent is stored — whether it can
/// actually arm is decided again on the next launch.
fn remember_enabled(on: bool) {
    let Some(p) = enabled_path() else { return };
    if on {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, "1");
    } else {
        let _ = std::fs::remove_file(p);
    }
}

/// Arm the edge again at launch if that is how the user left it.
///
/// Deliberately does NOT require the injector: at login the phone is often not
/// reachable yet (no cable, adb not up), and the crossing itself brings the
/// injector up on demand — so demanding it here would turn "remember my switch"
/// into "remember it only when the phone happens to be plugged in".
pub(crate) fn restore(app: tauri::AppHandle) {
    if !enabled_path().is_some_and(|p| p.exists()) {
        return;
    }
    tracing::info!("universal-control: was left on — arming the edge again");
    // On Tauri's runtime, NOT the caller's thread: this runs from `setup`, where
    // the main thread has no reactor, and arming starts the cursor-hide publisher
    // with a plain `tokio::spawn` — which panics there, taking the rest of setup
    // (the clipboard hotkey, among others) down with it.
    //
    // Nothing to report to either: the window may not even exist yet. A failure
    // to arm still reaches the UI from inside the loop, as `vortex:uc-stopped`.
    tauri::async_runtime::spawn(async move {
        let _ = arm(app, false);
    });
}

/// Start Universal Control: bring up the injector and run the capture loop.
#[tauri::command]
pub(crate) async fn uc_start(app: tauri::AppHandle) -> Result<(), String> {
    // A start that lands while a stop is still unwinding must not turn into a
    // silent no-op: the old loop needs a tick plus a portal round-trip to see
    // STOP, and RUNNING is still true for all of it. Toggling off then straight
    // back on would otherwise leave the user with a dead switch and no error.
    if STOP.load(Ordering::SeqCst) {
        for _ in 0..50 {
            if !RUNNING.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let armed = arm(app, true);
    if armed.is_ok() {
        remember_enabled(true);
    }
    armed
}

/// Claim the runtime flags and run the capture loop on its own thread.
///
/// `require_injector` separates a switch flipped by hand — where an unreachable
/// phone is worth saying out loud immediately — from a restore at launch, where
/// it is not yet worth mentioning.
fn arm(app: tauri::AppHandle, require_injector: bool) -> Result<(), String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Ok(()); // already running
    }
    STOP.store(false, Ordering::SeqCst);
    // Cursor-hide D-Bus publisher runs on the MAIN runtime (reliable name
    // ownership — zbus on the capture thread's current-thread runtime didn't
    // acquire it). Started lazily here, lives for the app's lifetime.
    ensure_cursor_publisher();
    // The native-cursor path needs the uinput injector (adb/Shizuku). Without it
    // we'd fall back to the accessibility overlay — not wired yet, so require it.
    if require_injector && !crate::mirror_inject::active() && !crate::mirror_inject::start() {
        RUNNING.store(false, Ordering::SeqCst);
        return Err("no_injector".into());
    }
    // libei's stream is !Send → own thread + current-thread runtime.
    std::thread::spawn(move || {
        // Clears RUNNING even if the loop panics. Without it a single panic
        // leaves RUNNING stuck true, and every later uc_start() returns Ok
        // while doing nothing — Universal Control dead until the app restarts,
        // with nothing in the log to say why.
        struct RunningGuard;
        impl Drop for RunningGuard {
            fn drop(&mut self) {
                RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _guard = RunningGuard;

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!("universal-control: runtime: {e}");
                return;
            }
        };
        rt.block_on(async {
            if let Err(e) = capture_loop().await {
                tracing::warn!("universal-control: capture loop ended: {e}");
                // Everything that can go wrong in here goes wrong AFTER the
                // command has already answered Ok — no portal on this desktop, a
                // compositor that will not arm the barrier, adb dropping the
                // phone. Left unreported, the switch stays on over a feature
                // that is not running, which is the same thing to the user as
                // "it is broken and will not say why".
                let _ = tauri::Emitter::emit(&app, "vortex:uc-stopped", e.to_string());
                // …and stop remembering the switch: whatever went wrong will go
                // wrong again at the next launch, and arming into the same error
                // every login is worse than an off switch the user can flip.
                remember_enabled(false);
            }
        });
    });
    Ok(())
}

/// Stop Universal Control: the capture loop releases the portal and exits.
#[tauri::command]
pub(crate) fn uc_stop() {
    remember_enabled(false);
    STOP.store(true, Ordering::SeqCst);
}

/// Whether a capture session is currently running.
#[tauri::command]
pub(crate) fn uc_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

async fn capture_loop() -> Result<(), Box<dyn std::error::Error>> {
    let (edge, seg) = placement();
    // The phone's bounds. We only ever send relative deltas, so without them we
    // cannot tell "the cursor is against the far edge" from "it is halfway
    // across" — and returning to the laptop has to happen at the edge, the way
    // it does when you run the pointer off a second monitor.
    let phys = crate::mirror_inject::display_size().ok_or("no_display_size")?;
    // Re-derived at every crossing, because the phone can be rotated between
    // one and the next.
    let (mut pw, mut ph) = phys;
    tracing::info!("universal-control: phone physical bounds {}x{}", phys.0, phys.1);
    // Capturing input at a screen edge is the compositor's to grant, through the
    // input-capture portal — Wayland only, and only where the compositor has
    // implemented it: GNOME 45+, KDE Plasma 6.1+, Hyprland since July 2026.
    // Sway/wlroots has no such backend. Saying so is the difference between a
    // feature that is unavailable here and one that appears broken: the switch
    // goes on, the edge does nothing, and nothing anywhere says why.
    let ic = InputCapture::new()
        .await
        .map_err(|e| format!("no_portal|{e}"))?;
    let (session, _cap) = ic
        .create_session(
            &ashpd::WindowIdentifier::default(),
            Capabilities::Keyboard | Capabilities::Pointer,
        )
        .await
        .map_err(|e| format!("session_refused|{e}"))?;

    let fd = ic.connect_to_eis(&session).await?;
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    context.flush().ok();

    let mut event_stream = EiEventStream::new(context.clone())?;
    let resp = reis::tokio::ei_handshake(
        &mut event_stream,
        "vortex-uc",
        ei::handshake::ContextType::Receiver,
    )
    .await?;
    let mut ei_events = EiConvertEventStream::new(event_stream, resp);

    // Barrier on the chosen edge of every monitor zone (interior edges fail
    // harmlessly — only the outer one the phone sits past actually arms).
    let zones = ic.zones(&session).await?.response()?;
    let regions = zones.regions();
    // ONE barrier on the desktop's true OUTER edge in the placement direction —
    // NOT every monitor's edge (an interior monitor boundary would let the cursor
    // cross to the phone mid-desktop, freezing it between screens).
    let idx = match edge {
        Edge::Right => regions
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| r.x_offset() + r.width() as i32)
            .map(|(i, _)| i),
        Edge::Left => regions
            .iter()
            .enumerate()
            .min_by_key(|(_, r)| r.x_offset())
            .map(|(i, _)| i),
        Edge::Bottom => regions
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| r.y_offset() + r.height() as i32)
            .map(|(i, _)| i),
        Edge::Top => regions
            .iter()
            .enumerate()
            .min_by_key(|(_, r)| r.y_offset())
            .map(|(i, _)| i),
    }
    .unwrap_or(0);
    // No zones at all is reachable — a monitor hotplug or DPMS race can hand us
    // an empty set — and indexing it would panic out of the whole capture
    // thread, stranding RUNNING (see the guard in `uc_start`).
    let Some(r) = regions.get(idx) else {
        return Err("no_zones".into());
    };
    let (x, y) = (r.x_offset(), r.y_offset());
    let (w, h) = (r.width() as i32, r.height() as i32);
    let full = match edge {
        Edge::Top | Edge::Bottom => (x, w),
        Edge::Left | Edge::Right => (y, h),
    };
    let mut span = barrier_span(seg, full);
    let mut pos = barrier_pos(edge, (x, y, w, h), span);
    tracing::info!(
        "universal-control: barrier at {pos:?} ({} edge, {} of it)",
        edge_name(edge),
        match seg {
            Segment::Full => "all",
            Segment::Start => "the low end",
            Segment::End => "the high end",
        }
    );
    // A barrier the compositor refuses to arm is silent otherwise: we would log
    // "armed", then wait forever for an activation that cannot come.
    let mut set = ic
        .set_pointer_barriers(&session, &[Barrier::new(BARRIER_ID, pos)], zones.zone_set())
        .await?
        .response()?;
    // Not every compositor takes a partial edge. KWin only arms a barrier that
    // spans the WHOLE screen edge — xdg-desktop-portal-kde's
    // `inputcapturebarrier.cpp` rejects anything else with
    // `BetweenScreensOrDoesNotFill` (it requires `y1 == geometry.y() && y2 ==
    // geometry.bottom()`), so the short corner strip below is refused outright
    // and Universal Control never arms on Plasma. Mutter accepts it, which is
    // why this went unnoticed. Fall back to the full edge instead of giving up:
    // the corner-avoidance is a nicety, crossing at all is the feature.
    if !set.failed_barriers().is_empty() && seg != Segment::Full {
        tracing::warn!(
            "universal-control: compositor refused the partial {} barrier {pos:?}; \
             retrying across the whole edge",
            edge_name(edge)
        );
        span = full;
        pos = barrier_pos(edge, (x, y, w, h), span);
        set = ic
            .set_pointer_barriers(&session, &[Barrier::new(BARRIER_ID, pos)], zones.zone_set())
            .await?
            .response()?;
    }
    if !set.failed_barriers().is_empty() {
        return Err(format!("barrier_refused|{} {pos:?}", edge_name(edge)).into());
    }
    ic.enable(&session).await?;
    let mut activated = ic.receive_activated().await?;
    crate::mirror_inject::refresh_rotation();
    tracing::info!("universal-control: armed on {} edge", edge_name(edge));

    let mut active = false;
    let mut acc_dx = 0f32;
    let mut acc_dy = 0f32;
    // Dead-reckoned position of the phone's cursor, and how far the motion has
    // tried to push it back out through the edge it entered from.
    let (mut px, mut py) = (0f32, 0f32);
    let mut overpush = 0f32;
    // The sub-pixel remainder of dividing Android's acceleration curve out of
    // the motion we forward, and the gap Android will see between one delta and
    // the next — which is what the curve is a function of, so it moves only when
    // something is actually sent.
    let (mut send_x, mut send_y) = (0f32, 0f32);
    let mut last_motion = Instant::now();
    let mut entry: (f32, f32) = (0.0, 0.0);
    // Barrier touched, but control has not crossed yet: we are measuring how
    // hard the user is pushing into it, and how far they have slid along it.
    let mut pending = false;
    let mut push = 0f32;
    let mut slide = 0f32;
    let mut last_progress = Instant::now();
    // When the barrier was touched, i.e. when the current push began — the
    // denominator for how hard it is being pushed.
    let mut push_started = Instant::now();
    // Whether the compositor is holding our pointer, tracked SEPARATELY from
    // `active`/`pending`. Those describe what we intend; this describes what the
    // compositor is actually doing, and only a `release` that returned Ok clears
    // it. Conflating the two is how a failed release leaves the laptop frozen
    // with no state left that would make anything try again.
    let mut captured = false;
    let mut scroll_acc = 0f32;
    // Trackpad scrolling is done as a real finger on the touchscreen, not as a
    // wheel. Android 10 has no high-resolution wheel axis, so `REL_WHEEL` can
    // only ever move a whole detent — a ratchet, where the trackpad is offering
    // sub-pixel deltas. A synthesized drag scrolls by the pixel, works in every
    // app that scrolls by touch (i.e. all of them), and lifting while the finger
    // is still moving hands Android the velocity it needs for the fling. The
    // fling is the half that makes it feel like a Mac.
    let mut scroll_dx = 0f32;
    let mut scroll_dy = 0f32;
    /// Phone pixels of finger travel per logical pixel of trackpad scroll, at a
    /// slow, deliberate pace. Flat 2:1 made nudging the home screen one page
    /// along overshoot into a flick; flat 1:1 fixed that and left long scrolls
    /// feeling like work. Neither number exists — the speed does.
    const SCROLL_GAIN: f32 = 1.4;
    /// Below this hand speed (logical px/s) the gain stays at [`SCROLL_GAIN`],
    /// so a careful nudge is still a careful nudge.
    const SCROLL_ACCEL_KNEE: f32 = 250.0;
    /// Above the knee the gain climbs by one extra multiple per this much speed.
    const SCROLL_ACCEL_SPAN: f32 = 700.0;
    /// …and stops climbing here, so a fast flick cannot become uncontrollable.
    const SCROLL_ACCEL_MAX: f32 = 3.0;
    /// No scroll for this long ends the gesture: the finger lifts, and whatever
    /// speed it had becomes a fling.
    const SCROLL_LIFT: Duration = Duration::from_millis(70);
    /// How much of the gap to the target the finger closes each tick. Below 1
    /// it is a low-pass filter on the trackpad's own irregularity; too low and
    /// the finger lags behind the hand, and the fling at the end goes with it.
    const SCROLL_FOLLOW: f32 = 0.5;
    /// Gap between touch packets, i.e. ~120 Hz. The pointer flushes at 500 Hz,
    /// but a touch move is not a pointer move: every one of them becomes a
    /// MotionEvent that the app on the phone handles on its UI thread, and no
    /// real touchscreen reports anywhere near that fast. Sending at the flush
    /// rate buried the phone in events it could not drain, which looked exactly
    /// like the scroll seizing up.
    const SCROLL_TICK: Duration = Duration::from_millis(8);
    let mut last_touch = Instant::now();
    /// Touch slot for the scroll finger. Screen mirroring owns 0 and 1.
    const SCROLL_SLOT: u8 = 9;
    /// Fraction of the screen kept clear at each edge when planting the finger.
    /// Small on purpose: what is left is the travel a gesture gets before the
    /// finger has to be picked up and put down again, and that re-plant is
    /// visible on anything that snaps — the home screen jumps a page. 5% of a
    /// 1080-wide screen still leaves most of a screen-width of drag.
    const SCROLL_MARGIN: f32 = 0.05;
    // Where the scroll finger is, and where the scroll so far wants it to be,
    // both in the phone's current display space.
    let mut scroll_finger: Option<(f32, f32)> = None;
    let mut scroll_target = (0f32, 0f32);
    let mut last_scroll = Instant::now();
    // A real mouse wheel reports discrete AND smooth deltas for one turn. It
    // should stay a wheel, so the smooth half is ignored for a moment after a
    // detent rather than being drawn as a finger on top of it.
    let mut last_discrete = Instant::now() - Duration::from_secs(1);
    // The phone's rotation as of the current crossing, needed to undo Android's
    // rotation when placing a finger (see `touch_raw`).
    let mut rot = 0u32;
    let mut activation_id: Option<u32> = None;
    let mut last_input = Instant::now();
    // 2 ms (~500 Hz) — low latency, still coalesces bursts so the socket never
    // backs up (per-event flooding caused stutter).
    let mut flush = tokio::time::interval(Duration::from_millis(2));
    // Longest gap we allow in traffic to the phone while it holds the pointer.
    //
    // Android parks the Wi-Fi radio between packets, and the AP then buffers
    // ours until the phone's next wake — measured at 150–350 ms on an otherwise
    // idle link (vs ~2–13 ms awake), which is exactly what makes the cursor
    // stutter when adb rides TCP instead of USB. Real motion already keeps the
    // radio awake; this only fills the gaps when the hand pauses, so it costs
    // nothing while you are actually moving.
    const KEEPALIVE_GAP: Duration = Duration::from_millis(10);
    // Last time ANYTHING was written to the phone. Deliberately separate from
    // `last_motion`, which is the acceleration curve's `dt` — feeding keepalives
    // into that would corrupt the curve we divide out.
    let mut last_tx = Instant::now();

    loop {
        tokio::select! {
            a = activated.next() => match a {
                Some(act) => {
                    entry = act.cursor_position().unwrap_or((0.0, 0.0));
                    activation_id = act.activation_id();
                    // Touching the barrier only starts the measurement — the
                    // crossing itself happens in the flush tick, once the push
                    // is deliberate enough. Nothing is hidden and nothing is
                    // sent to the phone until then, so brushing the edge on the
                    // way to something else costs nothing.
                    pending = true;
                    push_started = Instant::now();
                    // Touching the edge is the cue to go and look: the answer is
                    // wanted a fraction of a second later, at the crossing, and
                    // fetching it there is what used to stall the pointer.
                    crate::mirror_inject::refresh_rotation();
                    captured = true;
                    if active {
                        // Re-activated without our release having landed: the
                        // phone still owns a cursor device nobody will remove.
                        crate::mirror_inject::send("V 0");
                    }
                    active = false;
                    push = 0.0;
                    slide = 0.0;
                    last_progress = Instant::now();
                    last_input = Instant::now();
                    acc_dx = 0.0;
                    acc_dy = 0.0;
                    tracing::info!("universal-control: barrier touched at {entry:?}");
                }
                None => break,
            },

            ev = ei_events.next() => {
                // A dead EI stream must NOT skip the cleanup below — see the
                // comment there. The compositor severs the EIS connection on a
                // libei protocol violation, and that is exactly the moment we
                // are still holding the pointer capture.
                let ev = match ev {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => {
                        tracing::warn!("universal-control: EI stream error: {e} → releasing");
                        break;
                    }
                    None => {
                        tracing::warn!("universal-control: EI stream ended → releasing");
                        break;
                    }
                };
                last_input = Instant::now();
                match ev {
                    EiEvent::SeatAdded(s) => {
                        s.seat.bind_capabilities(&[
                            DeviceCapability::Pointer,
                            DeviceCapability::Keyboard,
                            DeviceCapability::Button,
                            DeviceCapability::Scroll,
                        ]);
                        context.flush().ok();
                    }
                    EiEvent::PointerMotion(m) => {
                        acc_dx += m.dx;
                        acc_dy += m.dy;
                    }
                    // Buttons, scroll and keys only travel once control has
                    // actually crossed. The compositor captures ALL input the
                    // moment the barrier is touched, so without this guard a
                    // graze of the screen edge sends your clicks and keystrokes
                    // to the phone while the laptop gets nothing — which defeats
                    // the whole point of the push-through below.
                    EiEvent::Button(b) => {
                        if active {
                            if let Some(btn) = match b.button {
                                0x110 => Some(0),
                                0x111 => Some(1),
                                0x112 => Some(2),
                                _ => None,
                            } {
                                let v = if b.state == ButtonState::Press { 1 } else { 0 };
                                crate::mirror_inject::send(&format!("B {btn} {v}"));
                            }
                        }
                    }
                    // A real wheel stays a wheel: detents are what its apps
                    // expect, and there is nothing sub-detent to lose.
                    EiEvent::ScrollDiscrete(sd) => {
                        if active {
                            last_discrete = Instant::now();
                            // Accumulate the remainder: a stack reporting less
                            // than a full detent per event would otherwise
                            // integer-divide to zero and never scroll at all.
                            scroll_acc += sd.discrete_dy as f32 / 120.0;
                            let notches = scroll_acc.trunc();
                            scroll_acc -= notches;
                            let dy = -(notches as i32).clamp(-3, 3);
                            if dy != 0 {
                                crate::mirror_inject::send(&format!("W {dy}"));
                            }
                        }
                    }
                    // Smooth (two-finger) scrolling — the ONLY scroll axis a
                    // laptop trackpad reports. Accumulated here and drawn as a
                    // finger in the flush tick, for the same reason motion is:
                    // one packet per tick instead of one per event.
                    EiEvent::ScrollDelta(sd) => {
                        if !active {
                        } else if !crate::mirror_inject::touch_available() {
                            // No digitizer on this phone (UHID backend), so the
                            // drawn finger is not available. A wheel detent is
                            // coarse, but it is the difference between coarse
                            // scrolling and none.
                            scroll_acc += sd.dy / 15.0;
                            let notches = scroll_acc.trunc();
                            scroll_acc -= notches;
                            let dy = -(notches as i32).clamp(-3, 3);
                            if dy != 0 {
                                crate::mirror_inject::send(&format!("W {dy}"));
                            }
                        } else if last_discrete.elapsed() >= Duration::from_millis(200) {
                            scroll_dx += sd.dx;
                            scroll_dy += sd.dy;
                            // Timed on ARRIVAL, not on delivery: the finger has
                            // to lift when the user stops scrolling, which is
                            // not the same instant as the last packet we chose
                            // to send.
                            last_scroll = Instant::now();
                        }
                    }
                    EiEvent::KeyboardKey(k) => {
                        let v = if k.state == KeyState::Press { 1 } else { 0 };
                        // Esc (evdev 1) = manual return to the laptop (guaranteed
                        // escape, in case the edge return-gesture is awkward).
                        // Also cancels a half-finished push.
                        if k.key == 1 && v == 1 && (active || pending) {
                            tracing::info!("universal-control: Esc → return to laptop");
                            if active {
                                crate::mirror_inject::send("V 0");
                            }
                            match ic
                                .release(&session, activation_id, Some(return_pos(edge, entry)))
                                .await
                            {
                                Ok(()) => captured = false,
                                Err(e) => tracing::warn!("universal-control: release: {e}"),
                            }
                            active = false;
                            pending = false;
                            overpush = 0.0;
                            set_cursor(false);
                            continue;
                        }
                        if active {
                            // libei reports evdev keycodes — the injector speaks the same.
                            crate::mirror_inject::send(&format!("E {} {v}", k.key));
                        }
                    }
                    EiEvent::Disconnected(_) => {
                        tracing::warn!("universal-control: EIS disconnected → releasing");
                        break;
                    }
                    _ => {}
                }
            },

            _ = flush.tick() => {
                if STOP.load(Ordering::SeqCst) {
                    break; // cleanup below releases + disables + closes
                }
                let dx = acc_dx.round() as i32;
                let dy = acc_dy.round() as i32;
                if dx != 0 || dy != 0 {
                    acc_dx -= dx as f32;
                    acc_dy -= dy as f32;
                    if pending {
                        // Not across yet — measuring the push. Nothing reaches
                        // the phone until it commits, so a graze along the edge
                        // never disturbs it.
                        push += inward_delta(edge, dx, dy);
                        slide += along_delta(edge, dx, dy);
                        if inward_delta(edge, dx, dy) > 0.0 {
                            last_progress = Instant::now();
                        }
                        // Required push, eased between the two thresholds by
                        // how fast the edge is being pushed into.
                        let secs = push_started.elapsed().as_secs_f32().max(0.001);
                        let t = ((push / secs - PUSH_SLOW) / (PUSH_FAST - PUSH_SLOW))
                            .clamp(0.0, 1.0);
                        let needed = PUSH_THROUGH + (PUSH_THROUGH_FAST - PUSH_THROUGH) * t;
                        // …and it has to be a push, not a sweep past.
                        let deliberate = push >= needed && push >= slide.abs() * PUSH_INWARD_RATIO;
                        if deliberate && !crate::mirror_inject::active()
                            && !crate::mirror_inject::start()
                        {
                            // No injector, no cursor on the phone — crossing
                            // would strand the pointer on a screen that shows
                            // nothing. Rebuilding it HERE (and not only at
                            // uc_start) is what recovers from adb dropping out
                            // mid-session: pulling the cable, a Wi-Fi roam, or
                            // the mirror window closing and taking it with it.
                            // Zeroing the push lets the abandon timer below
                            // hand the pointer back instead of leaving the user
                            // straining against a barrier that will never give.
                            tracing::warn!(
                                "universal-control: injector unavailable — staying on the laptop"
                            );
                            push = 0.0;
                        } else if deliberate {
                            // Bounds are re-derived per crossing: the phone may
                            // have been rotated since the last one, and Android
                            // clamps the pointer to the ROTATED display.
                            rot = crate::mirror_inject::rotation_cached();
                            (pw, ph) = match rot {
                                1 | 3 => (phys.1, phys.0),
                                _ => phys,
                            };
                            let (tx, ty) = entry_point(edge, pw, ph, entry, span);
                            let (ox, oy, sx, sy) = home_vectors(edge, pw, ph, tx, ty);
                            tracing::info!(
                                "universal-control: pushed through → phone ({tx},{ty}) of {pw}x{ph}"
                            );
                            pending = false;
                            active = true;
                            overpush = 0.0;
                            set_cursor(true);
                            crate::mirror_inject::send(&format!("V 1 {ox} {oy} {sx} {sy}"));
                            px = tx as f32;
                            py = ty as f32;
                            // Parking the cursor IS motion as far as Android's
                            // velocity tracker is concerned, and last session's
                            // leftover fraction means nothing to this one.
                            last_motion = Instant::now();
                            send_x = 0.0;
                            send_y = 0.0;
                        }
                    } else if active {
                        // Send the delta that will COME OUT as (dx, dy) — not
                        // (dx, dy) itself. Android accelerates relative motion,
                        // by up to its full factor, so forwarding the trackpad
                        // untouched moves the phone's cursor 1.3–3× further than
                        // the laptop's went: the pointer runs away under the
                        // hand, and the reckoning below — which is all we know
                        // about where it is — falls behind by the difference.
                        // Dividing the curve out makes the crossing what it
                        // claims to be, one desktop continuing onto the next.
                        //
                        // The remainder carries: a slow drag divides down to
                        // fractions of a pixel, and rounding each tick away on
                        // its own would quietly lose most of the movement.
                        // Re-read per tick, not cached: the real curve arrives
                        // from the phone in the background, some way into the
                        // session.
                        let curve = crate::mirror_inject::pointer_curve();
                        let dt = last_motion.elapsed().as_secs_f32();
                        let want = (dx as f32).hypot(dy as f32);
                        let ratio = curve.undo(want, dt) / want.max(f32::EPSILON);
                        send_x += dx as f32 * ratio;
                        send_y += dy as f32 * ratio;
                        let (ex, ey) = (send_x.round(), send_y.round());
                        if ex != 0.0 || ey != 0.0 {
                            send_x -= ex;
                            send_y -= ey;
                            last_motion = Instant::now();
                            crate::mirror_inject::send(&format!("P {ex} {ey}", ex = ex as i32, ey = ey as i32));
                            last_tx = Instant::now();
                        }
                        // Dead reckoning, clamped exactly like Android clamps
                        // its own pointer, so `px`/`py` stay in step with what
                        // is on screen. In the movement we asked for, which is
                        // the movement that happens now the curve is undone.
                        let (nx, ny) = (px + dx as f32, py + dy as f32);
                        px = nx.clamp(0.0, (pw - 1) as f32);
                        py = ny.clamp(0.0, (ph - 1) as f32);
                        // Where the pointer would re-enter the laptop from here.
                        // Kept current for as long as the phone holds it, so a
                        // return lands level with where it left — the crossing
                        // is then symmetric, which is the whole illusion: walk
                        // down the phone and come back out lower, exactly as a
                        // second monitor would.
                        entry = laptop_point(edge, (px, py), (pw, ph), span, (x, y, w, h));
                        // Motion that Android threw away because the pointer is
                        // already against the edge it came in from = pushing
                        // back towards the laptop. Only a sustained push counts,
                        // so a jitter at the edge cannot bounce you out.
                        let over = match edge {
                            Edge::Right => -nx.min(0.0),
                            Edge::Left => (nx - (pw - 1) as f32).max(0.0),
                            Edge::Bottom => -ny.min(0.0),
                            Edge::Top => (ny - (ph - 1) as f32).max(0.0),
                        };
                        overpush = if over > 0.0 { overpush + over } else { 0.0 };
                        if overpush >= RETURN_MARGIN {
                            tracing::info!("universal-control: return → laptop (edge push)");
                            lift_scroll(&mut scroll_finger, SCROLL_SLOT);
                            crate::mirror_inject::send("V 0");
                            match ic
                                .release(&session, activation_id, Some(return_pos(edge, entry)))
                                .await
                            {
                                Ok(()) => captured = false,
                                Err(e) => tracing::warn!("universal-control: release: {e}"),
                            }
                            active = false;
                            overpush = 0.0;
                            set_cursor(false);
                        }
                    }
                }
                // Trackpad scroll, drawn as a finger on the touchscreen — and
                // PACED like one. Irregular delivery is the documented cause of
                // scroll jank on Android: the compositor resamples touch against
                // vsync, so samples that arrive in bursts, or with a gap where
                // the trackpad reported nothing, come out as stutter no matter
                // how correct the positions are. So the tick is fixed, a move
                // goes out on every one of them, and the scroll that arrived in
                // between moves a TARGET the finger walks towards rather than
                // being dumped onto the finger whole.
                if scroll_finger.is_some() && !active {
                    lift_scroll(&mut scroll_finger, SCROLL_SLOT);
                } else if active
                    && (scroll_finger.is_some() || scroll_dx != 0.0 || scroll_dy != 0.0)
                    && last_touch.elapsed() >= SCROLL_TICK
                {
                    let (mx, my) = (pw as f32 * SCROLL_MARGIN, ph as f32 * SCROLL_MARGIN);
                    let (lo_x, hi_x) = (mx, pw as f32 - mx);
                    let (lo_y, hi_y) = (my, ph as f32 - my);
                    let plant = |x: f32, y: f32| {
                        let (rx, ry) = touch_raw(rot, x, y, phys);
                        crate::mirror_inject::send(&format!("D {SCROLL_SLOT} {rx} {ry}"));
                    };
                    // Plant under the cursor — scrolling has to act on whatever
                    // is beneath the pointer, exactly as the wheel did — but
                    // pulled off the edges so the swipe has room to run.
                    if scroll_finger.is_none() {
                        let f = (px.clamp(lo_x, hi_x), py.clamp(lo_y, hi_y));
                        plant(f.0, f.1);
                        scroll_finger = Some(f);
                        scroll_target = f;
                    }
                    let (mut fx, mut fy) = scroll_finger.unwrap_or((px, py));
                    // Content follows the finger: scrolling down means the
                    // finger goes up, which is also why this needs no sign
                    // config — whichever way the trackpad is set up, the phone
                    // moves the same way the laptop would.
                    // Scroll acceleration, measured over the interval those
                    // deltas actually arrived in rather than assumed to be one
                    // tick: the gate only guarantees SCROLL_TICK has passed, not
                    // that exactly that much has.
                    let dt = last_touch.elapsed().as_secs_f32().max(0.001);
                    let speed = (scroll_dx * scroll_dx + scroll_dy * scroll_dy).sqrt() / dt;
                    let gain = SCROLL_GAIN
                        * (1.0 + (speed - SCROLL_ACCEL_KNEE).max(0.0) / SCROLL_ACCEL_SPAN)
                            .min(SCROLL_ACCEL_MAX);
                    scroll_target.0 -= scroll_dx * gain;
                    scroll_target.1 -= scroll_dy * gain;
                    scroll_dx = 0.0;
                    scroll_dy = 0.0;
                    fx += (scroll_target.0 - fx) * SCROLL_FOLLOW;
                    fy += (scroll_target.1 - fy) * SCROLL_FOLLOW;
                    // Out of screen to drag across. Clamping here is what made a
                    // long scroll appear to freeze: the finger sat against the
                    // margin, still down, and because scroll kept arriving it
                    // never timed out and lifted either. A hand does the obvious
                    // thing instead — pick up, put down on the far side, keep
                    // going. The target moves with it, so the travel that was
                    // still owed survives the hop.
                    if fx < lo_x || fx > hi_x || fy < lo_y || fy > hi_y {
                        let nx = if fx < lo_x {
                            hi_x
                        } else if fx > hi_x {
                            lo_x
                        } else {
                            fx
                        };
                        let ny = if fy < lo_y {
                            hi_y
                        } else if fy > hi_y {
                            lo_y
                        } else {
                            fy
                        };
                        scroll_target.0 += nx - fx;
                        scroll_target.1 += ny - fy;
                        fx = nx;
                        fy = ny;
                        lift_scroll(&mut scroll_finger, SCROLL_SLOT);
                        plant(fx, fy);
                    } else {
                        let (rx, ry) = touch_raw(rot, fx, fy, phys);
                        crate::mirror_inject::send(&format!("M {SCROLL_SLOT} {rx} {ry}"));
                    }
                    scroll_finger = Some((fx, fy));
                    last_touch = Instant::now();
                    // The gesture is over once the trackpad has gone quiet. The
                    // finger is deliberately NOT walked all the way onto the
                    // target first: it is still moving when it leaves, and that
                    // leftover speed is what Android turns into the fling.
                    if last_scroll.elapsed() >= SCROLL_LIFT {
                        lift_scroll(&mut scroll_finger, SCROLL_SLOT);
                    }
                }
                // The injector is shared with screen mirroring, which tears it
                // down on its own schedule (closing the mirror window kills it).
                // Without this we would sit "active" forwarding into a dead
                // socket: no cursor on the phone, none on the laptop either.
                if active && !crate::mirror_inject::active() {
                    tracing::warn!("universal-control: injector gone → returning to laptop");
                    match ic
                        .release(&session, activation_id, Some(return_pos(edge, entry)))
                        .await
                    {
                        Ok(()) => captured = false,
                        Err(e) => tracing::warn!("universal-control: release: {e}"),
                    }
                    active = false;
                    overpush = 0.0;
                    set_cursor(false);
                }
                // A push that stalls or turns away hands the pointer straight
                // back. This has to be quick: while we are measuring, the
                // compositor is holding the laptop pointer still, so anyone
                // merely running along the screen edge would feel it snag.
                // Releasing at entry+slide lets the pointer land where their
                // movement had actually taken it.
                if pending && last_progress.elapsed() >= PUSH_IDLE {
                    tracing::info!("universal-control: push abandoned ({push}) → laptop");
                    match ic
                        .release(
                            &session,
                            activation_id,
                            Some(abandon_pos(edge, entry, slide, (x, y, w, h))),
                        )
                        .await
                    {
                        Ok(()) => captured = false,
                        Err(e) => tracing::warn!("universal-control: release: {e}"),
                    }
                    pending = false;
                    set_cursor(false);
                }
                // Failsafe: still captured and nothing coming in → hand the
                // pointer back. Keyed on `captured`, NOT on `active`: the state
                // this has to rescue is precisely the one where our own flags
                // say we let go but the compositor never got the message, so a
                // failed release gets retried here every tick.
                if captured && last_input.elapsed() >= Duration::from_secs(6) {
                    tracing::info!("universal-control: idle release → laptop (at {px},{py})");
                    if active {
                        crate::mirror_inject::send("V 0");
                    }
                    match ic
                        .release(&session, activation_id, Some(return_pos(edge, entry)))
                        .await
                    {
                        Ok(()) => captured = false,
                        Err(e) => tracing::warn!("universal-control: release: {e}"),
                    }
                    active = false;
                    pending = false;
                    overpush = 0.0;
                    set_cursor(false);
                }
                // Wi-Fi power-save keepalive (see KEEPALIVE_GAP). Covers both
                // `active` (pointer on the phone) and `pending` (push being
                // measured) so the radio is already awake when the crossing
                // commits — otherwise the very first motion after crossing pays
                // the wake penalty, which is the jolt you feel on arrival.
                // Driven off the flush tick and gated on elapsed time, so it
                // cannot leak: the moment both flags clear, it stops.
                // An empty line is a no-op for the injector — `process_line`
                // switches on `line[0]`, and '\0' matches no command.
                if (active || pending) && last_tx.elapsed() >= KEEPALIVE_GAP {
                    crate::mirror_inject::send("");
                    last_tx = Instant::now();
                }
            },
        }
    }
    // Cleanup — runs on EVERY exit path, and it is not optional. `release` is a
    // portal D-Bus call, independent of the (possibly dead) EI socket, so it
    // still works after a disconnect. Leaving it out is what freezes the laptop:
    // the compositor keeps the pointer captured, so mouse AND keyboard stay dead
    // until the process is killed — deskflow #8559, the same failure we hit.
    // The touch device outlives the mouse (`V 0` does not touch it), so a scroll
    // finger left down here would stay down — the phone stuck mid-drag.
    lift_scroll(&mut scroll_finger, SCROLL_SLOT);
    if active {
        crate::mirror_inject::send("V 0");
    }
    // UNCONDITIONAL: guarding this on our own flags is exactly the bug it is
    // meant to prevent — a release that silently failed clears them while the
    // compositor is still holding the pointer, and then nothing here would try
    // again. Releasing when we are not captured is harmless.
    if let Err(e) = ic
        .release(&session, activation_id, Some(return_pos(edge, entry)))
        .await
    {
        tracing::debug!("universal-control: final release: {e}");
    }
    let _ = ic.disable(&session).await;
    let _ = session.close().await;
    set_cursor(false);
    Ok(())
}

/// Take the scroll finger off the screen, if one is down. Idempotent.
///
/// Lifting while the finger is still moving is deliberate: Android reads the
/// velocity of the last few moves and turns it into a fling, which is what makes
/// a flick of the trackpad coast the way it does on a phone (and on a Mac).
fn lift_scroll(finger: &mut Option<(f32, f32)>, slot: u8) {
    if finger.take().is_some() {
        crate::mirror_inject::send(&format!("U {slot}"));
    }
}

/// A point in the phone's CURRENT display space, as the normalized 0..=65535
/// pair the injected touchscreen speaks.
///
/// The touchscreen reports in the panel's own coordinates and Android rotates
/// them into the display's, so putting a finger where the cursor is standing
/// means undoing that rotation first. `nat` is the natural (unrotated) size;
/// the cases mirror Android's own `TouchInputMapper` transform, inverted.
fn touch_raw(rot: u32, x: f32, y: f32, nat: (i32, i32)) -> (u16, u16) {
    let (nw, nh) = (nat.0 as f32, nat.1 as f32);
    let (rx, ry) = match rot {
        1 => (nw - y, x),
        2 => (nw - x, nh - y),
        3 => (y, nh - x),
        _ => (x, y),
    };
    let n = |v: f32, span: f32| (v / span.max(1.0) * 65535.0).round().clamp(0.0, 65535.0) as u16;
    (n(rx, nw), n(ry, nh))
}

/// Slam-and-step vectors for the injector's `mouse_home`: which corner to clamp
/// into, and how far to walk out along the entry edge.
///
/// The corner is always one that lies ON the arriving edge, and the step only
/// ever runs parallel to that edge. That keeps the crossing axis — the one the
/// return gesture measures — exactly on the clamp.
///
/// The step itself is divided by the curve's saturated factor, because a
/// relative delta written straight after the slam reads to Android as infinite
/// speed and comes out multiplied by its full acceleration. Un-divided, a third
/// of the way down the phone landed at the bottom edge — which is what "the
/// entry point is roughly where you came in" used to mean here.
fn home_vectors(edge: Edge, pw: i32, ph: i32, tx: i32, ty: i32) -> (i32, i32, i32, i32) {
    // Far enough past any real display that the clamp is guaranteed.
    let far = (pw.max(ph) * 2).max(20000);
    let g = crate::mirror_inject::pointer_curve().saturated();
    let step = |v: i32| (v as f32 / g).round() as i32;
    let (tx, ty) = (step(tx), step(ty));
    match edge {
        // Arrives on the phone's left edge → clamp to the top-left, walk down.
        Edge::Right => (-far, -far, 0, ty),
        // Arrives on the right edge → clamp to the top-right, walk down.
        Edge::Left => (far, -far, 0, ty),
        // Arrives on the top edge → clamp to the top-left, walk right.
        Edge::Bottom => (-far, -far, tx, 0),
        // Arrives on the bottom edge → clamp to the bottom-left, walk right.
        Edge::Top => (-far, far, tx, 0),
    }
}

/// Motion component pointing INTO the phone (positive = further across).
fn inward_delta(edge: Edge, dx: i32, dy: i32) -> f32 {
    match edge {
        Edge::Right => dx as f32,
        Edge::Left => -dx as f32,
        Edge::Bottom => dy as f32,
        Edge::Top => -dy as f32,
    }
}

/// Motion component running ALONG the barrier.
fn along_delta(edge: Edge, dx: i32, dy: i32) -> f32 {
    match edge {
        Edge::Right | Edge::Left => dy as f32,
        Edge::Top | Edge::Bottom => dx as f32,
    }
}

/// Where to drop the laptop cursor when a push is abandoned: where the pointer
/// would have got to had we not been holding it still while we measured.
fn abandon_pos(
    edge: Edge,
    entry: (f32, f32),
    slide: f32,
    region: (i32, i32, i32, i32),
) -> (f64, f64) {
    let (rx, ry, rw, rh) = region;
    let moved = match edge {
        Edge::Right | Edge::Left => (
            entry.0,
            (entry.1 + slide).clamp(ry as f32, (ry + rh - 1) as f32),
        ),
        Edge::Top | Edge::Bottom => (
            (entry.0 + slide).clamp(rx as f32, (rx + rw - 1) as f32),
            entry.1,
        ),
    };
    return_pos(edge, moved)
}

/// The stretch of the edge to arm, as `(origin, length)` along it, given the
/// whole edge as the same pair.
///
/// A partial segment is clamped to HALF the edge: `SEGMENT_LEN` stands in for a
/// phone's width, and on a small or scaled display that constant can be most of
/// the edge — at which point "leave the rest to the dock" quietly stops being
/// true, which is the only reason a segment exists. Length stays at least 1
/// because a zero-length barrier is not a barrier.
/// Id for the single pointer barrier we register.
///
/// MUST NOT be 0. The InputCapture portal spec puts no constraint on the id, and
/// Mutter happily takes 0, but xdg-desktop-portal-kde rejects it out of hand —
/// `inputcapture.cpp` does `if (id == 0) { "Invalid barrier id"; failed; }`
/// *before* looking at the geometry at all. With id 0 the barrier is silently
/// refused on every KDE session, so Universal Control never arms and pushing at
/// the screen edge does nothing.
const BARRIER_ID: u32 = 1;

/// The barrier line for `span` along `edge` of the monitor rect `(x, y, w, h)`.
///
/// Split out of the caller so the KWin full-edge retry builds its line the same
/// way the first attempt did, rather than duplicating the four-way match.
fn barrier_pos(edge: Edge, rect: (i32, i32, i32, i32), span: (i32, i32)) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = rect;
    let (s0, sl) = span;
    match edge {
        Edge::Left => (x, s0, x, s0 + sl - 1),
        Edge::Right => (x + w, s0, x + w, s0 + sl - 1),
        Edge::Top => (s0, y, s0 + sl - 1, y),
        Edge::Bottom => (s0, y + h, s0 + sl - 1, y + h),
    }
}

fn barrier_span(seg: Segment, full: (i32, i32)) -> (i32, i32) {
    if seg == Segment::Full {
        return full;
    }
    let len = SEGMENT_LEN.min(full.1 / 2).max(1);
    match seg {
        Segment::Start => (full.0, len),
        _ => (full.0 + full.1 - len, len),
    }
}

/// Where on the phone the cursor arrives, given where along the ARMED span it
/// left the laptop. The crossing axis is pinned to the arriving edge; the other
/// keeps the same relative offset, so the pointer comes in level with where it
/// went out — as it would between two monitors of different sizes.
///
/// Measured against the armed span rather than the whole edge, so a short
/// corner strip still reaches the whole of the phone's edge — otherwise a
/// 400-pixel barrier on a 2560-pixel edge would only ever land in the last
/// sixth of the phone.
fn entry_point(edge: Edge, pw: i32, ph: i32, entry: (f32, f32), span: (i32, i32)) -> (i32, i32) {
    let (origin, len) = span;
    let frac = |v: f32| {
        if len <= 0 {
            0.5
        } else {
            ((v - origin as f32) / len as f32).clamp(0.0, 1.0)
        }
    };
    match edge {
        Edge::Right => (0, (frac(entry.1) * (ph - 1) as f32) as i32),
        Edge::Left => (pw - 1, (frac(entry.1) * (ph - 1) as f32) as i32),
        Edge::Bottom => ((frac(entry.0) * (pw - 1) as f32) as i32, 0),
        Edge::Top => ((frac(entry.0) * (pw - 1) as f32) as i32, ph - 1),
    }
}

/// The mirror of `entry_point`: where along the armed span the pointer would
/// re-enter the laptop, given where it currently is on the phone.
///
/// Same proportion, read the other way — a cursor 20% down the phone comes back
/// 20% down the armed span. The crossing axis sits on the barrier line itself;
/// `return_pos` then steps it just inside.
fn laptop_point(
    edge: Edge,
    p: (f32, f32),
    phone: (i32, i32),
    span: (i32, i32),
    rect: (i32, i32, i32, i32),
) -> (f32, f32) {
    let (pw, ph) = phone;
    let (origin, len) = span;
    let (rx, ry, rw, rh) = rect;
    let along = |v: f32, size: i32| {
        let f = if size > 1 {
            (v / (size - 1) as f32).clamp(0.0, 1.0)
        } else {
            0.5
        };
        origin as f32 + f * (len - 1).max(0) as f32
    };
    match edge {
        Edge::Right => ((rx + rw) as f32, along(p.1, ph)),
        Edge::Left => (rx as f32, along(p.1, ph)),
        Edge::Bottom => (along(p.0, pw), (ry + rh) as f32),
        Edge::Top => (along(p.0, pw), ry as f32),
    }
}

/// Where to drop the laptop cursor on return (just inside the barrier, at the
/// Y/X it left from).
fn return_pos(edge: Edge, entry: (f32, f32)) -> (f64, f64) {
    let (x, y) = (entry.0 as f64, entry.1 as f64);
    match edge {
        Edge::Right => (x - 2.0, y),
        Edge::Left => (x + 2.0, y),
        Edge::Top => (x, y + 2.0),
        Edge::Bottom => (x, y - 2.0),
    }
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    }
}

// ── Cursor-hide D-Bus (org.vortex.UniversalControl) ─────────────────────────
// The portal can't hide the laptop cursor; the GNOME extension does it with
// Mutter's inhibit_cursor_visibility(), watching `CursorHidden` here.

struct UcDbus {
    cursor_hidden: bool,
}

#[zbus::interface(name = "org.vortex.UniversalControl1")]
impl UcDbus {
    #[zbus(property)]
    async fn cursor_hidden(&self) -> bool {
        self.cursor_hidden
    }
}

/// Channel to the cursor-hide publisher (owns the D-Bus name on the MAIN runtime
/// for the app's lifetime). `set_cursor(bool)` is a non-blocking send from any
/// thread — the capture thread uses it without needing a runtime of its own.
static CURSOR_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<bool>> =
    std::sync::OnceLock::new();

/// Hide (true) / show (false) the laptop cursor via the GNOME extension. No-op
/// until [`ensure_cursor_publisher`] has run.
fn set_cursor(hidden: bool) {
    if let Some(tx) = CURSOR_TX.get() {
        let _ = tx.send(hidden);
    }
}

/// Start the cursor-hide D-Bus publisher on the CURRENT (main) Tokio runtime.
/// Idempotent. Owns `org.vortex.UniversalControl` (property `CursorHidden`) and
/// emits PropertiesChanged on each toggle; the GNOME extension reacts with
/// Mutter's `inhibit_cursor_visibility()`.
fn ensure_cursor_publisher() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    if CURSOR_TX.set(tx).is_err() {
        return; // already started
    }
    tokio::spawn(async move {
        let conn = match zbus::connection::Builder::session()
            .and_then(|b| b.name("org.vortex.UniversalControl"))
            .and_then(|b| {
                b.serve_at(
                    "/org/vortex/UniversalControl",
                    UcDbus { cursor_hidden: false },
                )
            }) {
            Ok(b) => match b.build().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("universal-control: cursor dbus build: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("universal-control: cursor dbus: {e}");
                return;
            }
        };
        let iface_ref = match conn
            .object_server()
            .interface::<_, UcDbus>("/org/vortex/UniversalControl")
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("universal-control: cursor dbus iface: {e}");
                return;
            }
        };
        tracing::info!("universal-control: cursor-hide D-Bus up (org.vortex.UniversalControl)");
        // Hold the connection for the app's lifetime.
        let _conn = conn;
        while let Some(hidden) = rx.recv().await {
            let mut iface = iface_ref.get_mut().await;
            if iface.cursor_hidden != hidden {
                iface.cursor_hidden = hidden;
                let _ = iface.cursor_hidden_changed(iface_ref.signal_emitter()).await;
                tracing::info!("universal-control: CursorHidden → {hidden} (emitted)");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{barrier_span, parse_placement, touch_raw, Edge, Segment, SEGMENT_LEN};

    /// Natural (portrait) panel of the phone this was developed against.
    const NAT: (i32, i32) = (1080, 2340);

    #[test]
    fn unrotated_maps_straight_through() {
        assert_eq!(touch_raw(0, 0.0, 0.0, NAT), (0, 0));
        assert_eq!(touch_raw(0, 1080.0, 2340.0, NAT), (65535, 65535));
    }

    /// Rotated 90° counter-clockwise the phone's top edge points left, so the
    /// panel's top-RIGHT corner is what the user sees at the top-left.
    #[test]
    fn ninety_degrees_lands_on_the_right_corner() {
        assert_eq!(touch_raw(1, 0.0, 0.0, NAT), (65535, 0));
        // …and the opposite corner of the landscape display (2340x1080) is the
        // panel's bottom-left.
        assert_eq!(touch_raw(1, 2340.0, 1080.0, NAT), (0, 65535));
    }

    #[test]
    fn two_seventy_is_the_other_way_round() {
        assert_eq!(touch_raw(3, 0.0, 0.0, NAT), (0, 65535));
        assert_eq!(touch_raw(3, 2340.0, 1080.0, NAT), (65535, 0));
    }

    #[test]
    fn upside_down_flips_both_axes() {
        assert_eq!(touch_raw(2, 0.0, 0.0, NAT), (65535, 65535));
        assert_eq!(touch_raw(2, 1080.0, 2340.0, NAT), (0, 0));
    }

    /// The centre is the centre whichever way up the phone is — the one point
    /// every transform has to agree on.
    #[test]
    fn centre_is_rotation_invariant() {
        for (rot, x, y) in [
            (0u32, 540.0, 1170.0),
            (1, 1170.0, 540.0),
            (2, 540.0, 1170.0),
            (3, 1170.0, 540.0),
        ] {
            let (rx, ry) = touch_raw(rot, x, y, NAT);
            assert!((rx as i32 - 32768).abs() <= 1, "rot {rot}: x {rx}");
            assert!((ry as i32 - 32768).abs() <= 1, "rot {rot}: y {ry}");
        }
    }

    /// A bare edge is the whole edge; `<edge>-<end>` names one end of it. The two
    /// spellings of an end — "right" on a horizontal edge, "bottom" on a vertical
    /// one — mean the same [`Segment`], because they are the same end.
    #[test]
    fn placement_reads_edge_and_end() {
        for (s, edge, seg) in [
            ("bottom", Edge::Bottom, Segment::Full),
            ("bottom-right", Edge::Bottom, Segment::End),
            ("bottom-left", Edge::Bottom, Segment::Start),
            ("right-bottom", Edge::Right, Segment::End),
            ("right-top", Edge::Right, Segment::Start),
            ("LEFT-Top", Edge::Left, Segment::Start),
        ] {
            let got = parse_placement(s);
            assert!(
                got.0 as u8 == edge as u8 && got.1 == seg,
                "{s} read as the wrong placement"
            );
        }
    }

    /// Garbage must land somewhere usable rather than panic: the file this comes
    /// from is on disk and can be edited, truncated or left over from an older
    /// version. An unreadable end means the whole edge, which is the safe default.
    #[test]
    fn nonsense_placement_falls_back() {
        assert!(parse_placement("").1 == Segment::Full);
        assert!(parse_placement("sideways").1 == Segment::Full);
        assert!(parse_placement("bottom-sideways").1 == Segment::Full);
        assert!(parse_placement("bottom-").1 == Segment::Full);
    }

    /// A full segment is the edge untouched; an end segment sits at the end it
    /// names, and the arithmetic has to land exactly on the far corner.
    #[test]
    fn segment_sits_at_the_end_it_names() {
        let full = (0, 2560);
        assert_eq!(barrier_span(Segment::Full, full), full);
        assert_eq!(barrier_span(Segment::Start, full), (0, SEGMENT_LEN));
        assert_eq!(
            barrier_span(Segment::End, full),
            (2560 - SEGMENT_LEN, SEGMENT_LEN)
        );
        // Offset origins (a second monitor) shift with the zone, not to zero.
        let off = (1920, 1080);
        assert_eq!(barrier_span(Segment::Start, off).0, 1920);
        let end = barrier_span(Segment::End, off);
        assert_eq!(end.0 + end.1, 1920 + 1080);
    }

    /// The clamp is the point of the segment: on an edge shorter than twice
    /// `SEGMENT_LEN` it must give up length rather than swallow the edge, or
    /// there is nothing left for the dock that made us pick a corner at all.
    #[test]
    fn segment_never_takes_more_than_half_the_edge() {
        for len in [1, 2, 17, 400, 799, 800, 801, 4096] {
            for seg in [Segment::Start, Segment::End] {
                let (o, l) = barrier_span(seg, (0, len));
                assert!(l >= 1, "len {len}: zero-length barrier");
                assert!(l <= len.max(1), "len {len}: longer than the edge");
                assert!(l * 2 <= len || l == 1, "len {len}: took more than half ({l})");
                assert!(o >= 0 && o + l <= len.max(1), "len {len}: {o}+{l} off the edge");
            }
        }
    }
}
