//! Continuity Camera (phone → laptop): use the phone's camera as a laptop
//! webcam. Dials the phone's `CAMERA_VIDEO_PORT`, decrypts the sealed H.264
//! stream (reusing `mirror_tcp`), decodes it on the CPU (no GPU — same reason
//! the screen cast avoids NVENC on this hybrid box), and writes the frames into
//! a v4l2loopback device (`/dev/video42`) that any app — Zoom, Meet, the
//! browser's getUserMedia — sees as a regular webcam.
//!
//! Triggered from the laptop ("use phone as webcam"): the phone starts its
//! camera, advertises `{port, key}` in its AppState, and we dial it (the phone
//! is unreachable inbound on real networks, but laptop→phone always works — the
//! same direction the phone→laptop screen mirror uses).

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;

use vortex_l3_daemon::core::appstate::CameraOffer;
use vortex_l3_daemon::core::mirror_tcp;

/// True while the laptop WANTS the phone camera (the user toggled "use phone as
/// webcam"). Read by the AppState builder → `camera_req`; the phone streams only
/// while it's set. The actual v4l2 pipe starts when the phone's offer arrives.
pub static CAMERA_WANTED: AtomicBool = AtomicBool::new(false);

/// Consecutive failed pipeline starts, and how many we tolerate before
/// releasing the phone's camera. See the dispatch handler.
static START_FAILS: AtomicU32 = AtomicU32::new(0);
const START_FAIL_LIMIT: u32 = 3;

/// "A camera session is engaged." The phone's offer arrives over BOTH BLE and
/// LAN at nearly the same instant; without an atomic gate both dispatchers race
/// past `!active()` and call `start()` twice (each tears the other down). This
/// compare-exchange flag lets exactly ONE of them start the pipe.
static ENGAGED: AtomicBool = AtomicBool::new(false);

/// Which phone lens to request — "front" (selfie, the natural webcam default)
/// or "back". Sent to the phone in `camera_facing`; it re-opens the camera when
/// this changes mid-stream.
static CAMERA_FACING: Mutex<&'static str> = Mutex::new("front");

/// Read by the laptop's outgoing AppState builder (BLE + LAN).
pub fn camera_wanted() -> bool {
    CAMERA_WANTED.load(Ordering::SeqCst)
}

/// Read by the AppState builder → `camera_facing`.
pub fn camera_facing() -> String {
    CAMERA_FACING.lock().map(|g| g.to_string()).unwrap_or_default()
}

/// Tauri command: the UI toggles "use phone as webcam".
#[tauri::command]
pub(crate) fn set_camera_request(on: bool) -> Result<(), String> {
    // Check the loopback device BEFORE asking the phone for its camera.
    //
    // It used to be checked only deep inside `start()`, whose failure was a
    // `tracing::warn!` and nothing else: `CAMERA_WANTED` stayed true, so the
    // phone kept `camera_req=true`, kept its camera open with the indicator lit,
    // and every heartbeat dialled again — while the laptop showed nothing and
    // said nothing. On a fresh Fedora, where the module has to be loaded by
    // hand, that is the DEFAULT experience of this feature.
    //
    // The message names the exact modprobe line. Loading the module is the
    // user's to do, not ours: it is a system-wide kernel module, and vortex does
    // not install those behind anyone's back.
    if on {
        if let Err(e) = resolve_v4l2_device() {
            tracing::warn!("continuity-camera: refusing to start — {e}");
            return Err(e);
        }
    }
    tracing::info!(on, "continuity-camera: request toggled");
    set_wanted(on);
    Ok(())
}

/// Tauri command: flip the phone lens (front ↔ back). The phone re-opens the
/// camera; our v4l2 pipe survives the brief reconnect.
#[tauri::command]
pub(crate) fn set_camera_facing(facing: String) {
    let f = if facing == "back" { "back" } else { "front" };
    if let Ok(mut g) = CAMERA_FACING.lock() {
        *g = f;
    }
    tracing::info!(facing = f, "continuity-camera: lens flipped");
    if let Some(n) = crate::SYNC_NUDGE.get() {
        n.notify_waiters();
    }
}

/// Flip the "want phone camera" request (the Tauri command / UI toggle). On
/// `false` we also tear down any live pipe immediately.
pub fn set_wanted(on: bool) {
    CAMERA_WANTED.store(on, Ordering::SeqCst);
    if !on {
        ENGAGED.store(false, Ordering::SeqCst);
        stop();
    }
    if let Some(n) = crate::SYNC_NUDGE.get() {
        n.notify_waiters(); // ship camera_req to the phone now
    }
}

/// Consecutive offer-less AppStates seen while piping. The phone advertises
/// `camera_offer` over BOTH BLE and LAN; right after a toggle a STALE pre-toggle
/// snapshot (offer=None) can arrive over the other transport and would otherwise
/// stop→restart the pipe (thrash). Only stop after a sustained absence.
static OFFER_MISSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const OFFER_MISS_LIMIT: u32 = 4;

/// Rotation the live pipe was built with. A lens flip changes the phone's
/// `rot`; we rebuild the pipe so the new lens shows upright.
static CURRENT_ROT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// React to the phone's `camera_offer` (carried on each AppState). When it
/// appears and we want the camera, dial the phone + start the v4l2 pipe; when it
/// stays gone (debounced), stop. `phone_ip` comes from the live LAN session.
pub fn dispatch_offer(offer: &Option<CameraOffer>, phone_ip: Option<IpAddr>) {
    match offer {
        Some(o) => {
            OFFER_MISSES.store(0, Ordering::SeqCst);
            // A lens flip changes `rot` while engaged → tear down so the pipe
            // rebuilds with the new rotation on the next dispatch.
            if ENGAGED.load(Ordering::SeqCst)
                && o.rot as u32 != CURRENT_ROT.load(Ordering::SeqCst)
            {
                ENGAGED.store(false, Ordering::SeqCst);
                stop();
            }
            // Win the start race against the other transport: only the dispatcher
            // that flips ENGAGED false→true starts the pipe.
            if camera_wanted()
                && ENGAGED
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                let (Some(ip), Some(key)) = (phone_ip, hex_to_key(&o.key)) else {
                    ENGAGED.store(false, Ordering::SeqCst);
                    return;
                };
                CURRENT_ROT.store(o.rot as u32, Ordering::SeqCst);
                if let Err(e) = start(ip, key, o.rot) {
                    tracing::warn!("continuity-camera: start failed: {e}");
                    ENGAGED.store(false, Ordering::SeqCst);
                    // Repeated failure means it is not going to work this
                    // session. Stop asking, so the phone can close its camera
                    // instead of holding it open for a stream nobody receives.
                    if START_FAILS.fetch_add(1, Ordering::SeqCst) + 1 >= START_FAIL_LIMIT {
                        START_FAILS.store(0, Ordering::SeqCst);
                        tracing::warn!(
                            "continuity-camera: giving up after {START_FAIL_LIMIT} failed starts \
                             — releasing the phone's camera"
                        );
                        set_wanted(false);
                    }
                } else {
                    START_FAILS.store(0, Ordering::SeqCst);
                }
            }
        }
        None => {
            if ENGAGED.load(Ordering::SeqCst)
                && OFFER_MISSES.fetch_add(1, Ordering::SeqCst) + 1 >= OFFER_MISS_LIMIT
            {
                OFFER_MISSES.store(0, Ordering::SeqCst);
                ENGAGED.store(false, Ordering::SeqCst);
                stop();
            }
        }
    }
}

fn hex_to_key(hex_str: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    Some(k)
}

/// The v4l2loopback device the phone camera is written to. Created once with
/// `modprobe v4l2loopback video_nr=42 card_label="Vortex Camera" exclusive_caps=1`.
/// This fixed node is only a *fallback* — [`resolve_v4l2_device`] prefers to
/// discover the loopback by its card label so a different `video_nr` (or a
/// machine where 42 is taken by a real camera) still works.
const V4L2_DEVICE: &str = "/dev/video42";

/// The `card_label` we create the v4l2loopback node with. We match on this to
/// find our node regardless of its number.
const V4L2_CARD_LABEL: &str = "Vortex Camera";

/// Find the v4l2loopback node that carries our "Vortex Camera" label instead of
/// trusting a fixed number. v4l2loopback writes the card label to
/// `/sys/class/video4linux/videoN/name`; we scan for ours. Falls back to the
/// conventional [`V4L2_DEVICE`] when present, else returns a clear, actionable
/// error — so the camera doesn't silently fail on a machine where the module
/// isn't loaded or used a different `video_nr`.
fn resolve_v4l2_device() -> Result<String, String> {
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        for entry in entries.flatten() {
            let name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            if name.trim() == V4L2_CARD_LABEL {
                let dev = format!("/dev/{}", entry.file_name().to_string_lossy());
                if std::path::Path::new(&dev).exists() {
                    return Ok(dev);
                }
            }
        }
    }
    if std::path::Path::new(V4L2_DEVICE).exists() {
        return Ok(V4L2_DEVICE.to_string());
    }
    Err(format!(
        "Vortex Camera (v4l2loopback) not found. Load the module with:\n  \
         sudo modprobe v4l2loopback video_nr=42 card_label=\"{V4L2_CARD_LABEL}\" exclusive_caps=1"
    ))
}

static CAM: Mutex<Option<CamHandle>> = Mutex::new(None);

struct CamHandle {
    /// Fire to tear the camera feed down (pipeline → Null, receiver dropped).
    stop_tx: tokio::sync::oneshot::Sender<()>,
}

/// True while the phone camera is being piped into the v4l2 device.
pub fn active() -> bool {
    CAM.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Start piping the phone camera into the v4l2 webcam. `key` is the media key
/// the phone sealed the stream with (carried in its AppState offer). Replaces
/// any prior feed. Returns once the pipeline + dialer are up.
pub fn start(phone_ip: IpAddr, key: [u8; 32], rot: u16) -> Result<(), String> {
    stop();
    gst::init().map_err(|e| format!("gst init: {e}"))?;
    // Discover our loopback node by label (fail fast with a clear hint if the
    // v4l2loopback module isn't loaded) rather than assuming /dev/video42.
    let v4l2_device = resolve_v4l2_device()?;

    // Rotate to upright by the phone's sensor orientation (videoflip, before the
    // scale/cap so the dimensions are right). 90/270 swap W/H → the webcam is
    // portrait when the phone is held portrait, which is the upright view.
    let flip = match rot {
        90 => "videoflip method=clockwise ! ",
        180 => "videoflip method=rotate-180 ! ",
        270 => "videoflip method=counterclockwise ! ",
        _ => "",
    };

    // CPU H.264 decode → YUY2 (the format webcam consumers expect) → v4l2sink.
    // 720p30 SW-decode is light and avoids any GPU context the compositor owns.
    let desc = format!(
        "appsrc name=src is-live=true format=time do-timestamp=true \
         max-bytes=4194304 caps=video/x-h264,stream-format=byte-stream,alignment=au ! \
         h264parse ! avdec_h264 ! videoconvert ! {flip}videoscale add-borders=true ! videorate ! \
         video/x-raw,format=YUY2,width=1280,height=720,framerate=30/1 ! \
         v4l2sink device={v4l2_device} sync=false"
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| format!("build pipeline: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline downcast".to_string())?;
    let appsrc = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| "appsrc missing".to_string())?;
    appsrc.set_is_live(true);
    appsrc.set_format(gst::Format::Time);
    appsrc.set_max_bytes(4 * 1024 * 1024);

    // Dial the phone's camera server; decrypted access units land on this channel.
    let (au_tx, mut au_rx) = mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(mirror_tcp::run_tcp_video_receiver_on(
        phone_ip,
        mirror_tcp::CAMERA_VIDEO_PORT,
        key,
        au_tx,
        // The webcam path has no control plane to ask for an IDR on; a dropped
        // frame simply resolves at the next scheduled keyframe.
        None,
    ));

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("pipeline play: {e}"))?;
    tracing::info!(%phone_ip, "continuity-camera: piping phone camera → {v4l2_device}");

    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let bus = pipeline.bus();
    tokio::spawn(async move {
        let mut first = true;
        loop {
            // Drain any fatal pipeline message without blocking the runtime.
            let mut fatal = false;
            if let Some(bus) = &bus {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            tracing::warn!("continuity-camera: pipeline error: {}", e.error());
                            fatal = true;
                        }
                        gst::MessageView::Eos(_) => fatal = true,
                        _ => {}
                    }
                }
            }
            if fatal {
                break;
            }
            tokio::select! {
                _ = &mut stop_rx => break,
                au = au_rx.recv() => {
                    match au {
                        Some(au) => {
                            if first {
                                tracing::info!("continuity-camera: first frame → webcam");
                                first = false;
                            }
                            let buffer = gst::Buffer::from_slice(au);
                            if appsrc.push_buffer(buffer).is_err() {
                                break;
                            }
                        }
                        None => break, // phone closed the stream
                    }
                }
            }
        }
        let _ = appsrc.end_of_stream();
        let _ = pipeline.set_state(gst::State::Null);
        if let Ok(mut g) = CAM.lock() {
            *g = None;
        }
        // Release engagement so a later offer can re-start cleanly (e.g. after a
        // pipeline error/EOS, not just a user stop).
        ENGAGED.store(false, Ordering::SeqCst);
        tracing::info!("continuity-camera: stopped");
    });

    if let Ok(mut g) = CAM.lock() {
        *g = Some(CamHandle { stop_tx });
    }
    Ok(())
}

/// Stop the camera feed (if any). Idempotent.
pub fn stop() {
    let taken = CAM.lock().ok().and_then(|mut g| g.take());
    if let Some(h) = taken {
        let _ = h.stop_tx.send(());
    }
}
