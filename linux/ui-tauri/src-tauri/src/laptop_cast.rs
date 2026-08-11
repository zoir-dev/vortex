//! Screen-mirror SENDER (laptop → phone, view-only): capture the laptop screen
//! and stream it to the phone, which shows it in a viewer. The mirror image of
//! [`crate::mirror`] (which RECEIVES the phone's screen).
//!
//! Pipeline: the xdg-desktop-portal **ScreenCast** portal (Wayland-native, pops
//! the "share your screen" consent on the laptop) hands us a PipeWire node + fd;
//! GStreamer captures it (`pipewiresrc`), scales to 720p and encodes HEVC with
//! NVENC; an `appsink` hands each H.265 access unit to the daemon's
//! [`mirror_tcp::MirrorTcpSealer`], which seals it (ChaCha20-Poly1305, same wire
//! as the phone→laptop path) and serves it on [`mirror_tcp::LAPTOP_VIDEO_PORT`].
//! The phone connects out to it and decodes with MediaCodec.
//!
//! Crypto: the media key is derived from the live session's IK handshake hash
//! via [`mirror_udp::derive_laptop_media_key`] — a DISTINCT key from the
//! phone→laptop direction so the two streams never reuse a nonce. Both peers
//! derive it identically; nothing key-related goes on the wire.
//!
//! Trigger: the PHONE starts this (its "view laptop screen" button) — the portal
//! consent still appears on the laptop, but the user initiates from the phone.
//! One cast at a time; [`start`] replaces any prior session.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use ashpd::WindowIdentifier;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use rand::RngCore;
use tokio::sync::mpsc;

use vortex_l3_daemon::core::appstate::LaptopCast;
use vortex_l3_daemon::core::mirror_tcp;

/// Live cast handle: dropping/taking the stop sender ends the session (the cast
/// task selects on it). `None` = no cast running.
static CAST: Mutex<Option<CastHandle>> = Mutex::new(None);

/// The current cast's offer (ip, port, hex key) for the laptop's outgoing
/// AppState, so the phone knows where to dial. `None` when not casting.
static CAST_OFFER: Mutex<Option<LaptopCast>> = Mutex::new(None);

/// Why the last cast attempt failed, for the phone to show and act on.
///
/// Without this the phone cannot tell "starting…" from "never going to work":
/// it re-asserts `laptop_mirror_req` on every heartbeat, we fail again, and the
/// only trace is a WARN in a log the user is not reading. Its request stays
/// latched, `requestView` early-returns on `requestActive`, and further taps do
/// nothing — the UI wedges until the app is force-stopped. The laptop already
/// knows the exact reason and has a sealed channel to say it on, so it does.
static CAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// The failure reason to ship in the next AppState push, if any.
pub(crate) fn current_error() -> Option<String> {
    CAST_ERROR.lock().ok().and_then(|g| g.clone())
}

fn set_error(msg: Option<String>) {
    if let Ok(mut g) = CAST_ERROR.lock() {
        *g = msg;
    }
}

/// Push the sealed stream to the phone, and tear the cast down if that gives up.
///
/// `run_tcp_video_client` returns either because `au_rx` closed (the normal stop
/// path) or because it exhausted its ~60 s of connect retries — the phone's
/// viewer never came up, or went away for good. The pipeline and the portal
/// session live in a DIFFERENT task, so nothing used to act on that: the capture
/// kept running with nobody watching, and for an extend cast KWin kept the
/// virtual output in the desktop layout — a phantom 1920x1080 screen with no
/// viewer behind it, which windows can be dragged into and lost. Observed
/// exactly that after a viewer was closed: `kscreen-doctor` still listed
/// `Virtual-virtual-xdp-kde-…` at 2058,0 until the whole app was killed.
///
/// Give-up on the transport therefore has to mean give-up on the cast — which
/// also releases the compositor's output and, via `CAST_ERROR`, tells the phone
/// why instead of leaving it on a black screen.
fn spawn_video_sender(
    phone_ip: std::net::IpAddr,
    key: [u8; 32],
    au_rx: mpsc::Receiver<Vec<u8>>,
) {
    tokio::spawn(async move {
        mirror_tcp::run_tcp_video_client(phone_ip, key, au_rx).await;
        // On the normal stop path `stop()` has already taken CAST, so this is
        // only reached with a live handle when the transport gave up by itself.
        if CAST.lock().map(|g| g.is_some()).unwrap_or(false) {
            tracing::warn!("laptop-cast: phone viewer unreachable — stopping the cast");
            set_error(Some("the phone's viewer stopped responding".to_string()));
            stop();
        }
    });
}

/// Edge-tracker for the phone's `laptop_mirror_req` level: we act only on the
/// false→true (start) and true→false (stop) transitions, ignoring the repeats
/// that arrive on every heartbeat.
static REQ_WANTED: AtomicBool = AtomicBool::new(false);

/// Consecutive `req == false` heartbeats seen while a cast is wanted. The phone
/// advertises `laptop_mirror_req` over BOTH BLE and LAN, sent at slightly
/// different times — right after the user taps, a STALE pre-tap snapshot
/// (req=false) can still arrive over the other transport and would otherwise
/// stop→restart the cast (new key → AEAD mismatch → black). We only honour a
/// stop after several consecutive falses; a genuine "close viewer" yields a
/// sustained false, while the startup race's lone stale false is outvoted by
/// the real req=true arriving on the other transport.
static REQ_FALSE_MISSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Falses needed before we actually stop (≈ a few heartbeats of confirmed off).
const REQ_FALSE_LIMIT: u32 = 3;

struct CastHandle {
    /// Fire to tear the cast down (pipeline → Null, portal session closed).
    stop_tx: tokio::sync::oneshot::Sender<()>,
}

/// True while a laptop→phone cast is live.
pub fn active() -> bool {
    CAST.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// The active cast offer for the laptop's outgoing AppState (`laptop_cast`).
pub fn current_offer() -> Option<LaptopCast> {
    CAST_OFFER.lock().ok().and_then(|g| g.clone())
}

/// Drive the cast from the phone's `laptop_mirror_req` level (called on every
/// inbound phone AppState, over LAN + BLE-STATE). Starts a fresh cast on the
/// false→true edge (random key, our LAN IP, portal consent) and stops on
/// true→false. Idempotent across the repeated heartbeats in between.
///
/// `extend` is the phone's choice of screen kind, carried alongside the request
/// (`None` from a phone that does not offer the choice — then the laptop's own
/// preference decides, as it did before the phone could ask).
pub fn dispatch_request(req: bool, extend: Option<bool>) {
    // Any real request resets the stop-debounce: a single stale `false` between
    // genuine `true`s must not count toward a stop.
    if req {
        REQ_FALSE_MISSES.store(0, Ordering::SeqCst);
    } else if REQ_WANTED.load(Ordering::SeqCst) {
        // Casting but saw a `false`: only stop once it's CONFIRMED (sustained),
        // not on a lone stale snapshot from the other transport.
        if REQ_FALSE_MISSES.fetch_add(1, Ordering::SeqCst) + 1 < REQ_FALSE_LIMIT {
            return;
        }
    }
    if req && !REQ_WANTED.swap(true, Ordering::SeqCst) {
        // Rising edge. We DIAL the phone (it's the video server — only
        // laptop→phone connections survive real networks), so we need the
        // phone's IP from the live LAN session. Without it we can't reach the
        // viewer; bail and let a later heartbeat (with an IP) retry.
        let Some(phone_ip) = (match crate::lan::LAST_GOOD_PEER_IP.lock() {
            Ok(g) => *g,
            Err(_) => None,
        }) else {
            tracing::warn!("laptop-cast: no known phone IP yet — not starting");
            REQ_WANTED.store(false, Ordering::SeqCst);
            return;
        };
        // Fresh random media key (never derived/reused). The offer just carries
        // the key (+ port) so the phone opens the stream; the phone is the
        // server now, so no laptop IP is needed.
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        if let Ok(mut g) = CAST_OFFER.lock() {
            *g = Some(LaptopCast {
                ip: String::new(), // unused — the laptop dials the phone
                port: mirror_tcp::LAPTOP_VIDEO_PORT,
                key: hex::encode(key), // key material — logged nowhere
            });
        }
        // A fresh attempt is not the previous attempt's failure: clear the
        // reason so the phone isn't shown a stale one while this one runs.
        set_error(None);
        tokio::spawn(async move {
            if let Err(e) = start(phone_ip, key, extend).await {
                tracing::warn!("laptop-cast: start failed: {e}");
                if let Ok(mut g) = CAST_OFFER.lock() {
                    *g = None;
                }
                set_error(Some(e));
                REQ_WANTED.store(false, Ordering::SeqCst);
            }
        });
    } else if !req && REQ_WANTED.swap(false, Ordering::SeqCst) {
        // Confirmed falling edge: the phone closed the viewer → release capture.
        REQ_FALSE_MISSES.store(0, Ordering::SeqCst);
        stop();
        if let Ok(mut g) = CAST_OFFER.lock() {
            *g = None;
        }
        // The phone has stopped asking, so it has either seen the reason or no
        // longer cares. Keeping it would re-report an old failure against the
        // next request the moment it is made.
        set_error(None);
    }
}

/// Start casting the laptop screen to the phone with media key `key` (a fresh
/// random per-cast secret the caller also ships to the phone over the Noise-
/// sealed AppState). Pops the ScreenCast consent and serves the sealed HEVC
/// stream on [`mirror_tcp::LAPTOP_VIDEO_PORT`]. Replaces any prior cast. Returns
/// once the portal + pipeline are up (or an error if the user cancels consent /
/// capture can't start).
/// `extend` is the kind of screen asked for: `Some(true)` a new monitor,
/// `Some(false)` a view of an existing one, `None` "whoever is asking did not
/// say" — which falls back to the laptop's own saved preference.
pub async fn start(
    phone_ip: std::net::IpAddr,
    key: [u8; 32],
    extend: Option<bool>,
) -> Result<(), String> {
    stop();

    // Extend mode swaps the SOURCE, nothing else: instead of a view of a screen
    // that already exists we ask for a brand-new monitor and capture that.
    // Everything downstream — encode, seal, transport, the phone's viewer — is
    // identical, which is the whole reason this fits here rather than in a
    // module of its own.
    if extend.unwrap_or_else(extend_enabled) {
        // Mutter first: it is the tuned path (and it rides its own cursor
        // overlay, because Mutter will not composite a pointer into a virtual
        // monitor). But `org.gnome.Mutter.ScreenCast` is GNOME's private API, so
        // on any other compositor it fails instantly with ServiceUnknown — and
        // the ScreenCast portal's `Virtual` source is the cross-desktop
        // equivalent, which KWin implements (its portal advertises it in
        // AvailableSourceTypes). Falling back keeps "second screen" working off
        // GNOME instead of failing with nothing on screen to say why.
        match start_extend(phone_ip, key).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "laptop-cast: Mutter virtual monitor unavailable ({e}); \
                     trying the ScreenCast portal's Virtual source instead"
                );
                return start_portal(phone_ip, key, SourceType::Virtual).await;
            }
        }
    }
    start_portal(phone_ip, key, SourceType::Monitor).await
}

/// Capture `source` through the ScreenCast portal and serve it sealed on
/// [`mirror_tcp::LAPTOP_VIDEO_PORT`].
///
/// `SourceType::Monitor` is a view of a screen that already exists (mirror);
/// `SourceType::Virtual` asks the compositor to materialise a NEW one (extend).
/// Everything after the source selection is identical, which is why both kinds
/// share this body.
async fn start_portal(
    phone_ip: std::net::IpAddr,
    key: [u8; 32],
    source: SourceType,
) -> Result<(), String> {
    // ---- Portal: open a ScreenCast session and get the PipeWire node + fd. ----
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("portal connect: {e}"))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|e| format!("portal session: {e}"))?;
    proxy
        .select_sources(
            &session,
            CursorMode::Embedded,
            source.into(),
            false,
            None,
            PersistMode::DoNot,
        )
        .await
        .map_err(|e| format!("portal select_sources: {e}"))?;
    let streams = proxy
        .start(&session, &WindowIdentifier::default())
        .await
        .map_err(|e| format!("portal start (consent declined?): {e}"))?
        .response()
        .map_err(|e| format!("portal start response: {e}"))?;
    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| "portal returned no stream".to_string())?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|e| format!("portal open_pipe_wire_remote: {e}"))?;
    tracing::info!(node_id, size = ?stream.size(), "laptop-cast: portal stream ready");

    // ---- GStreamer: capture → scale 720p → NVENC HEVC → appsink. ----
    if let Err(e) = gst::init() {
        return Err(format!("gst init: {e}"));
    }
    let raw_fd = fd.as_raw_fd();
    // CPU H.264 (x264enc), NOT a GPU encoder. On this hybrid laptop the GNOME
    // compositor + the portal capture run on the INTEL iGPU; feeding those
    // Intel PipeWire frames into NVENC (the NVIDIA dGPU) forces a cross-GPU
    // DMA-BUF import that FAULTS the compositor (it crashed gnome-shell → logged
    // the user out). Forcing `video/x-raw,format=I420` after `videoconvert`
    // pins the frames to SYSTEM MEMORY (no DMA-BUF reaches the encoder), so the
    // encode never touches a GPU context the compositor owns. `videorate` caps
    // a high-refresh panel to 30 fps — plenty for a screen view, light on CPU.
    let desc = format!(
        "pipewiresrc fd={raw_fd} path={node_id} do-timestamp=true keepalive-time=1000 ! \
         videorate ! videoconvert ! videoscale ! \
         video/x-raw,format=I420,width=1280,height=720,framerate=30/1 ! \
         x264enc tune=zerolatency speed-preset=veryfast bitrate=4000 key-int-max=30 ! \
         h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=vsink emit-signals=false max-buffers=3 drop=true sync=false"
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| format!("build pipeline: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline downcast".to_string())?;

    // appsink → bounded channel of sealed-ready access units. The callback runs
    // on a GStreamer streaming thread (sync), so it `try_send`s and drops on a
    // full queue rather than blocking the encoder — keeps the stream live.
    let (au_tx, au_rx) = mpsc::channel::<Vec<u8>>(8);
    let appsink = pipeline
        .by_name("vsink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| "appsink missing".to_string())?;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    if let Ok(map) = buf.map_readable() {
                        // Drop if the network can't keep up — never block here.
                        let _ = au_tx.try_send(map.as_slice().to_vec());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Push the sealed stream to the phone (we dial it — the phone is the server).
    spawn_video_sender(phone_ip, key, au_rx);

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("pipeline play: {e}"))?;
    tracing::info!("laptop-cast: capturing + serving on {}", mirror_tcp::LAPTOP_VIDEO_PORT);

    // Own proxy/session/fd/pipeline for the cast's lifetime in one task; drop
    // them all (closing the portal + capture) when stopped or the bus errors.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut g = CAST.lock().map_err(|_| "cast lock".to_string())?;
        *g = Some(CastHandle { stop_tx });
    }
    let bus = pipeline.bus();
    tokio::spawn(async move {
        // Keep these alive until teardown. `fd` closing does end the PipeWire
        // stream, but the SESSION must be closed explicitly — ashpd 0.9's
        // `Session` has `close()` and no `Drop` that calls it, so merely dropping
        // it leaves the portal session open until our whole D-Bus connection goes
        // away. For an extend cast that means the compositor keeps the virtual
        // output: a phantom 1920x1080 screen stayed in the KDE display layout
        // after the cast stopped, and only disappeared when the app was killed.
        // See the explicit `close()` at the end of this task.
        let _keep = (proxy, fd);
        let mut stop_rx = stop_rx;
        loop {
            // Drain any pending bus messages WITHOUT blocking the async runtime
            // (`pop()` is non-blocking; the old `timed_pop` blocked a tokio
            // worker thread for 250ms a spin). Stop on a fatal error / EOS.
            let mut fatal = false;
            if let Some(bus) = &bus {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            tracing::warn!(
                                src = ?msg.src().map(|s| s.name()),
                                debug = ?e.debug(),
                                "laptop-cast: pipeline error: {}",
                                e.error()
                            );
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
            // Wait for the next poll tick OR the user stop, whichever first.
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        if let Ok(mut g) = CAST.lock() {
            *g = None;
        }
        // Clear the offer so a pipeline error/EOS (not just a user stop) makes
        // the phone see a sustained `laptop_cast = None` → it closes its viewer
        // → its request drops → our `dispatch_request(false)` falling edge then
        // resets REQ_WANTED cleanly. We deliberately do NOT reset REQ_WANTED here:
        // doing so while the phone still wants the cast would immediately re-arm
        // a NEW cast (new key) under the still-open old viewer → AEAD mismatch.
        if let Ok(mut g) = CAST_OFFER.lock() {
            *g = None;
        }
        // Hand the session back to the compositor. Pipeline-to-Null stops the
        // capture but leaves the SOURCE allocated — for `SourceType::Virtual`
        // that is a whole output still sitting in the user's display layout,
        // which windows can be dragged into and lost.
        if let Err(e) = session.close().await {
            tracing::warn!("laptop-cast: portal session close failed: {e}");
        }
        tracing::info!("laptop-cast: stopped (capture + portal session closed)");
    });

    Ok(())
}

/// Size of the monitor extend mode creates. Landscape: the phone's viewer is
/// locked to landscape, and 720 logical pixels of width is uncomfortably narrow
/// for desktop windows anyway. 1560x720 is the phone's own screen turned on its
/// side (2340x1080 in exactly this ratio), so the picture fills it edge to edge
/// with no letterboxing.
///
/// The caps force this at the source, so it really is the monitor's resolution
/// and not a scale applied afterwards.
const EXTEND_W: u32 = 1560;
const EXTEND_H: u32 = 720;

fn extend_flag_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".local/share/vortex/laptop_cast/extend"))
}

/// Whether the phone should be given a NEW screen rather than a copy of this one.
pub(crate) fn extend_enabled() -> bool {
    extend_flag_path().is_some_and(|p| p.exists())
}

/// Choose between mirroring this screen and extending onto a new one. Takes
/// effect on the next cast — switching mid-cast would mean tearing the viewer
/// down and re-keying it.
#[tauri::command]
pub(crate) fn set_extend_mode(on: bool) -> Result<(), String> {
    let p = extend_flag_path().ok_or("no HOME")?;
    if on {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, b"1").map_err(|e| e.to_string())?;
    } else {
        let _ = std::fs::remove_file(&p);
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn get_extend_mode() -> bool {
    extend_enabled()
}

/// Cast a NEW monitor to the phone (see [`crate::virtual_display`]).
///
/// Differs from the mirror path in two ways only: the frames come from a
/// PipeWire node on our own connection (no portal, so no consent dialog and no
/// remote fd), and the caps sit immediately after the source — with anything
/// scalable in between, the size would not propagate back and Mutter would pick
/// the monitor's resolution itself.
async fn start_extend(phone_ip: std::net::IpAddr, key: [u8; 32]) -> Result<(), String> {
    let monitor = crate::virtual_display::create().await?;
    let node_id = monitor.node_id;

    if let Err(e) = gst::init() {
        return Err(format!("gst init: {e}"));
    }
    // Same encoder reasoning as the mirror path: CPU x264 into system memory,
    // never a GPU context the compositor owns.
    // The cursor is drawn here rather than by the compositor: Mutter refuses to
    // composite one into a virtual monitor without killing the session, so the
    // overlay rides the pointer instead (starts invisible until we know where
    // it is).
    // Dropped from the pipeline entirely if the artwork can't be staged — an
    // overlay with no image to load refuses to start, and losing the pointer is
    // a far smaller loss than losing the screen.
    let cursor_stage = match crate::virtual_display::stage_cursor_image() {
        Some(p) => format!(
            "gdkpixbufoverlay name=cursor location=\"{}\" alpha=0 ! ",
            p.display()
        ),
        None => {
            tracing::warn!("laptop-cast: no cursor artwork — extending without a pointer");
            String::new()
        }
    };
    let desc = format!(
        "pipewiresrc path={node_id} do-timestamp=true keepalive-time=1000 ! \
         video/x-raw,width={EXTEND_W},height={EXTEND_H} ! \
         videorate ! videoconvert ! \
         video/x-raw,format=I420,framerate=30/1 ! \
         {cursor_stage}\
         x264enc tune=zerolatency speed-preset=veryfast bitrate=4000 key-int-max=30 ! \
         h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=vsink emit-signals=false max-buffers=3 drop=true sync=false"
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| format!("build pipeline: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline downcast".to_string())?;

    let (au_tx, au_rx) = mpsc::channel::<Vec<u8>>(8);
    let appsink = pipeline
        .by_name("vsink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| "appsink missing".to_string())?;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    if let Ok(map) = buf.map_readable() {
                        let _ = au_tx.try_send(map.as_slice().to_vec());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    spawn_video_sender(phone_ip, key, au_rx);
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("pipeline play: {e}"))?;
    tracing::info!(
        "laptop-cast: extending onto a new {EXTEND_W}x{EXTEND_H} monitor, serving on {}",
        mirror_tcp::LAPTOP_VIDEO_PORT
    );

    let cursor_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    if let Some(overlay) = pipeline.by_name("cursor") {
        crate::virtual_display::spawn_cursor_overlay(overlay, cursor_alive.clone());
    }

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut g = CAST.lock().map_err(|_| "cast lock".to_string())?;
        *g = Some(CastHandle { stop_tx });
    }
    let bus = pipeline.bus();
    tokio::spawn(async move {
        let mut stop_rx = stop_rx;
        loop {
            let mut fatal = false;
            if let Some(bus) = &bus {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            tracing::warn!(
                                src = ?msg.src().map(|s| s.name()),
                                debug = ?e.debug(),
                                "laptop-cast: pipeline error: {}",
                                e.error()
                            );
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
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
            }
        }
        cursor_alive.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = pipeline.set_state(gst::State::Null);
        // Take the monitor away before anything else: windows left on it need
        // somewhere to go, and the shell only moves them once it is gone.
        monitor.stop().await;
        if let Ok(mut g) = CAST.lock() {
            *g = None;
        }
        if let Ok(mut g) = CAST_OFFER.lock() {
            *g = None;
        }
        tracing::info!("laptop-cast: stopped (extra monitor removed)");
    });

    Ok(())
}

/// Stop the live cast (if any): the task tears down the pipeline and releases
/// the portal/capture. Idempotent.
pub fn stop() {
    let taken = CAST.lock().ok().and_then(|mut g| g.take());
    if let Some(h) = taken {
        let _ = h.stop_tx.send(());
    }
}
