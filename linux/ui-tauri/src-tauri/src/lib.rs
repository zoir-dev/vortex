//! Tauri backend for the Vortex Linux UI.
//!
//! Architecture mirrors the eframe binary `vortex-l3-ui`:
//!   - one tokio runtime + worker thread
//!   - commands push UiCmd onto an mpsc channel
//!   - worker emits WorkerEvent which we forward as Tauri events
//!
//! All protocol logic stays inside `vortex_l3_daemon` — this layer is
//! pure glue. Feature code lives one-file-per-feature in the submodules;
//! this file is just the composition root: module declarations, the
//! cross-cutting statics, the re-export block, and `run()`.

// A port in progress: on Windows the Linux-only paths are gated out, which
// leaves ~110 helpers, constants and statics unreachable there. They are all
// still live on Linux, so deleting or per-item-gating them would be churn that
// has to be undone as the Windows side fills in. Silence them as a group
// instead, and REMOVE this once the port stops moving — otherwise it hides
// genuinely dead Windows code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_variables, unused_imports))]

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use tauri::WindowEvent;
use tokio::sync::oneshot;

use vortex_l3_daemon::core::pairing::handshake::LocalDecision;

mod applog;
// The BLE transport and everything that drives it. Gated together with the
// persistent loop in `worker`: `BleCentral`/`GattLink` make the protocol side
// portable, but this orchestration is still BlueZ-shaped. See the TODO at the
// loop's spawn site.
#[cfg(target_os = "linux")]
mod ble;
// The same link, over the seam. Used where there is no BlueZ loop.
#[cfg(not(target_os = "linux"))]
mod ble_portable;
mod call;
mod call_log;
// ── Linux-only subsystems ─────────────────────────────────────────────────
// Screen mirror / cast / camera (GStreamer + GTK3 + the ScreenCast portal) and
// the earbuds audio hand-off (PulseAudio + BlueZ) have no Windows
// implementation. Gated as whole modules rather than cfg'd internally: there is
// no partial version of "decode H.264 into a GTK widget", and pretending
// otherwise would leave a Windows build full of functions that always error.
//
// The features simply do not exist on Windows for now. Everything else — pair,
// reconnect, notifications, clipboard, file transfer, Universal Control — does.
#[cfg(target_os = "linux")]
mod camera;
mod capture_ledger;
// Elsewhere, the same command names resolve to the "unsupported" module, so the
// `generate_handler!` list below stays single-sourced.
#[cfg(not(target_os = "linux"))]
mod platform_unsupported;
#[cfg(not(target_os = "linux"))]
use platform_unsupported as camera;
#[cfg(not(target_os = "linux"))]
use platform_unsupported as proximity;
#[cfg(not(target_os = "linux"))]
use platform_unsupported as earbuds;
#[cfg(not(target_os = "linux"))]
use platform_unsupported as laptop_cast;
mod clipboard;
mod clipboard_hotkey;
mod clipboard_window;
mod clipboard_sync;
mod phone_files;
mod transfers;
mod transfers_out;
mod worker_transfers;
mod worker_ctx;
// Available everywhere: ForgetPeer / ForgetAll need no radio — they are a trust
// store delete, a cache purge and a LAN revoke, all of which work on any
// platform. Only Scan and Pair inside it are Linux-gated, since those two are
// the ones that hold a BlueZ adapter.
mod cmd_pairing;
#[cfg(target_os = "linux")]
mod cmd_earbuds;
mod send_to_phone;
mod share;
mod file_consent;
mod fs_cli;
mod fs_lan;
// The phone's storage as a real filesystem. `fs_mount` is the facade and
// `fs_vfs` the OS-independent half; the adapter under them is per-OS — FUSE on
// Linux, ProjFS on Windows (design doc §8 step 6).
mod fs_mount;
mod fs_vfs;
#[cfg(target_os = "linux")]
mod fs_fuse;
#[cfg(target_os = "windows")]
mod fs_projfs;
mod fs_pull;
mod contacts;
mod desktop_apps;
mod diagnostics;
mod dnd;
mod first_run;
#[cfg(target_os = "linux")]
mod earbuds;
mod handoff;
mod ipc;
mod lan;
#[cfg(target_os = "linux")]
mod laptop_cast;
mod lan_wifi_direct;
mod lan_state;
mod live_activity;
mod media_remote;
#[cfg(target_os = "linux")]
mod mirror;
mod arbiter;
// NOT gated with the rest of the mirror: this is the laptop→phone injection
// path — an adb-forwarded socket to a uinput helper ON THE PHONE. Pure std plus
// `adb`, no GStreamer and no GTK, and Universal Control sends through it too.
mod mirror_inject;
mod peer_cache;
mod peer_handoff;
/// The laptop's end of the ranged-filesystem protocol (serves and consumes).
mod fs_link;
#[cfg(target_os = "linux")]
mod mirror_window;
mod notes;
mod notifications;
mod notify;
mod presence;
mod pairing;
#[cfg(target_os = "linux")]
mod proximity;
mod ring;
mod sms;
mod tray;
// The two tray implementations behind it — see `tray.rs` for why there are two.
#[cfg(target_os = "linux")]
mod tray_ksni;
#[cfg(not(target_os = "linux"))]
mod tray_tauri;
mod universal_control;
#[cfg(target_os = "linux")]
mod virtual_display;
mod voice_settings;
mod window;
// Explorer's "Share via Vortex", the counterpart of the Nautilus extension and
// Dolphin ServiceMenu that install_linux.sh writes.
#[cfg(target_os = "windows")]
mod win_shell;
mod worker;
mod x11_focus;

// Re-exports for items that moved out of lib.rs in the module split, so
// existing `crate::Item` references across the feature modules keep
// compiling unchanged.
pub(crate) use call::{
    CallWriter, CALL_CONTROL_SEQ, CALL_MIRROR_TX, CALL_WRITER, PENDING_CALL_CONTROL,
};
pub(crate) use clipboard_sync::{ClipboardImageWriter, ClipboardWriter};
pub(crate) use ipc::{app_state_to_dto, emit_peers, CmdChannel, UiCmd};
pub(crate) use notifications::{NotifWriter, ACTIVE_CHAT};

/// Generic laptop→phone sealed-frame writer: `(frame_ty, sub, payload)` → an
/// AEAD-sealed BLE frame. The BLE persistent loop fills the holder on connect;
/// any feature (e.g. notes) sends through it without its own transport plumbing.
///
/// `sub` is exposed because the filesystem ops carry their op there — it has
/// always been in the wire format, this writer just used to hardcode it to 0.
/// Pass 0 for frame types that do not use it.
pub(crate) type SealedWriter = Arc<
    dyn Fn(
            u8,
            u8,
            Vec<u8>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// The live smart-audio-follow watcher, published once at worker start so
/// the `set_smart_switch_enabled` / `get_smart_switch_enabled` Tauri
/// commands can flip its `enabled` flag from the Settings UI. The watcher
/// itself lives inside the worker task; this is just a handle to its
/// AtomicBool. `OnceLock` because it's set exactly once and read-only
/// thereafter (the AtomicBool inside is what mutates).
/// Linux-only: the watcher is MPRIS, so there is nothing to hold a handle to
/// elsewhere. Its readers are the earbuds module (gated with it) and the
/// smart-switch setting adopted from the phone's heartbeat, which skips the
/// adoption rather than inventing a local value.
#[cfg(target_os = "linux")]
pub(crate) static MEDIA_WATCH: std::sync::OnceLock<
    Arc<vortex_l3_daemon::core::media_watch::MediaWatch>,
> = std::sync::OnceLock::new();

/// Heartbeat early-wake handle, published once at worker start so Tauri
/// commands can push a just-changed state immediately instead of waiting
/// out the periodic tick.
pub(crate) static SYNC_NUDGE: std::sync::OnceLock<Arc<tokio::sync::Notify>> =
    std::sync::OnceLock::new();

/// BLE presence-wait early-wake handle (the cross-transport hint,
/// continuity-style): the LAN heartbeat fires this on its down→up edge —
/// "the phone just appeared on the network" — so the BLE persistent loop
/// retries its direct connect immediately instead of waiting out the
/// passive monitor / its scan backoff.
pub(crate) static BLE_RETRY_NUDGE: std::sync::OnceLock<Arc<tokio::sync::Notify>> =
    std::sync::OnceLock::new();

/// The BLE session's generic sealed-frame writer, published so command
/// handlers outside the BLE loop can send a frame to the CURRENTLY CONNECTED
/// peer.
///
/// Deliberately "the connected peer", not an arbitrary one: it is a handle on
/// the live session's cipher state. That is exactly what
/// `PeerHandoff.RELEASE` needs — at the moment a switch is confirmed the live
/// link is still the peer being displaced (we have not connected to the
/// replacement yet), so this reaches the right device. If that ordering ever
/// changes, the RELEASE send in cmd_pairing has to change with it.
pub(crate) static BLE_SEALED_WRITER: std::sync::OnceLock<
    Arc<tokio::sync::Mutex<Option<SealedWriter>>>,
> = std::sync::OnceLock::new();

/// Token of a phone-shared clipboard image waiting to be pulled over LAN.
/// Set by the BLE image-offer consumer (which also nudges the heartbeat),
/// added to the next bulk-sync request, and cleared once the LAN fetch
/// delivers the image. `None` = nothing pending.
pub(crate) static PENDING_IMAGE_TOKEN: std::sync::OnceLock<Mutex<Option<String>>> =
    std::sync::OnceLock::new();

/// Queue of phone-shared FILES (instant-share style) waiting to be pulled over LAN.
/// Each entry is `(token, name, mime)`. The offer consumer pushes; the bulk-sync
/// pulls the FRONT one per round (and nudges again if more remain), then pops it
/// on delivery. Distinct from [`PENDING_IMAGE_TOKEN`] (clipboard images) so file
/// transfer and clipboard-image sync don't clobber each other.
/// `(token, name, mime, transfer_id, kind)` — the id ties each queued file to
/// its row in the transfer panel for live progress + completion; `kind` is the
/// offer's (`"screenshot"` / `"photo"` / empty), carried through to the save so
/// a capture lands in its subfolder and gets its notification.
pub(crate) static PENDING_FILE_OFFERS: std::sync::OnceLock<
    Mutex<std::collections::VecDeque<(String, String, String, u64, String)>>,
> = std::sync::OnceLock::new();

/// Holds the oneshot sender that `do_pair` is currently awaiting on,
/// keyed implicitly by "the active pairing session" (there is at most
/// one at a time — start_pair early-rejects when another pair is in
/// flight via `pairingPeer` on the UI side).
///
/// The `pair_decision` Tauri command takes the sender out of this slot
/// and fires it with the user's choice. If the slot is empty (no
/// pairing in flight, or the closure already moved on), the command
/// is a no-op — clicking Approve a second time after a successful
/// pair, or after timeout, must not panic the worker.
#[derive(Default)]
pub(crate) struct PairDecisionState(pub(crate) Mutex<Option<oneshot::Sender<LocalDecision>>>);

// --------------------------------------------------------------------------
// Tauri entrypoint
// --------------------------------------------------------------------------

/// `RUST_LOG` if set, else `info`.
fn log_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// Tee log lines to stderr AND the log file.
///
/// stderr is best-effort on purpose: a Windows GUI binary has none, and a
/// desktop-launched Linux app has it pointed at `/dev/null`. A failed write
/// there must never cost us the line in the file.
struct Tee(std::fs::File);

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stderr());
        self.0.flush()
    }
}

/// Start logging to a FILE — on every platform — while still writing stderr for
/// whoever is watching a terminal.
///
/// A file is not a Windows nicety. Neither platform reliably has a console:
/// a Windows GUI binary has none at all, and on Linux the app is started from a
/// `.desktop` autostart entry, which sends stdout and stderr to `/dev/null`. So
/// the installed Linux app kept no record of anything, and every diagnosis
/// began by asking someone to kill it and relaunch it by hand with `RUST_LOG`
/// set — which loses exactly the run that misbehaved. (The doc comment this
/// replaces claimed the journal picked it up; that is only true under a systemd
/// user unit, which is not how this is installed.)
///
/// `~/.cache/vortex/vortex.log`, or `%LOCALAPPDATA%\Vortex\vortex.log`, with
/// the previous run kept as `vortex.log.1` — one restart of history, because
/// the interesting run is often the one before the one you thought to look at.
/// Falls back to stderr alone if the file cannot be opened, which is no worse
/// than before.
fn init_logging() {
    use std::io::Write;

    // A launch WITH arguments is a forwarder: single-instance hands the argv to
    // the already-running app and this process exits seconds later. It must not
    // touch the log file, because rolling it aside pulls the running app's file
    // out from under its open handle — after two such invocations the real
    // app's output is going to an unlinked inode nobody can read. Found the
    // hard way: two `--fs-ls` runs in a row destroyed the very log they were
    // supposed to be inspected in.
    //
    // The cost is that a FIRST launch carrying arguments keeps no file log for
    // that session. That is the rare case (autostart and the desktop entry both
    // launch bare) and it is recoverable by restarting, whereas losing the
    // running app's log is not.
    let forwarding = std::env::args().len() > 1;
    let path = vortex_l3_daemon::core::platform::paths()
        .logs()
        .filter(|_| !forwarding)
        .map(|dir| {
            let _ = std::fs::create_dir_all(&dir);
            dir.join("vortex.log")
        });

    let file = path.as_ref().and_then(|p| {
        // Roll the previous run aside rather than appending: a fresh file per
        // launch is what makes "what did THIS run do" answerable at a glance.
        if p.exists() {
            let _ = std::fs::rename(p, p.with_extension("log.1"));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
            .ok()
    });

    match file {
        Some(f) => {
            // A closure is the `MakeWriter` that needs no extra dependency;
            // cloned handles share the file offset, so lines from different
            // threads append rather than overwrite. ANSI off — colour escapes
            // in a file make it unreadable in Notepad.
            let writer = move || Tee(f.try_clone().expect("clone log handle"));
            tracing_subscriber::fmt()
                .with_env_filter(log_filter())
                .with_writer(writer)
                .with_ansi(false)
                .init();
            if let Some(p) = path.as_ref() {
                tracing::info!("logging to {}", p.display());
                // Named on stderr too, so someone watching a terminal knows
                // where the file is without reading this function.
                let _ = writeln!(std::io::stderr(), "vortex: logging to {}", p.display());
            }
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(log_filter())
                .with_ansi(false)
                .init();
            tracing::warn!("could not open a log file; logging to stderr only");
        }
    }
}

/// Route panics into the log.
///
/// The default hook prints to stderr, which on a Windows GUI binary goes
/// nowhere at all — a thread that panics simply stops, leaving a log that ends
/// mid-startup with no reason given. That is the single most confusing failure
/// a first run can produce, so panics go where the rest of the diagnosis is.
///
/// Keeps the default hook too: on Linux stderr IS the journal.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `info.location()` is where it happened; the payload is the message.
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        tracing::error!(location = %where_, "PANIC: {what}");
        default(info);
    }));
}

pub fn run() {
    // Logs go to stderr AND to a file — the packaged launch paths discard
    // stderr, so without the file a normal user's run leaves no trace at all.
    match applog::init() {
        Some(p) => tracing::info!("logging to {}", p.display()),
        None => tracing::warn!("could not open the log file; stderr only"),
    }
    // Panics too. The default hook prints to stderr, which on a Windows GUI
    // binary goes nowhere at all — a thread that panics simply stops, leaving a
    // log that ends mid-startup with no reason given.
    log_panics();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "vortex starting");
    // Explorer's "Share via Vortex". Re-registered every start rather than
    // once, because the command has this exe's full path in it and Explorer
    // will not go looking for a binary that moved. Cheap, and it means an
    // unzipped standalone .exe gets the menu entry without an installer —
    // which is the same reason the toast AUMID shortcut registers itself.
    #[cfg(target_os = "windows")]
    win_shell::register_share_verb();

    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCmd>();
    // Tray heartbeat: the 5-second local-earbuds rescan used to live in the
    // webview (a Vue setInterval) — but WebKit throttles hidden-window timers,
    // so with the window closed the tray battery/owner rows froze until the
    // phone's next heartbeat. Driven from here instead: same worker path,
    // independent of window visibility.
    // Linux only: what it drives is a BlueZ earbuds rescan, so off Linux it is
    // a command with no handler arriving every 5 s forever — 28 warn lines in
    // the first Windows log, and the noise that hid the real fault.
    #[cfg(target_os = "linux")]
    {
        let hb_tx = cmd_tx.clone();
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_secs(5));
            if hb_tx.send(UiCmd::RefreshLocalEarbuds).is_err() {
                break; // worker gone — app is shutting down
            }
        });
    }
    let cmd_channel = CmdChannel(cmd_tx);

    tauri::Builder::default()
        // Single instance: a second launch (e.g. the GNOME clipboard
        // shortcut firing `vortex-ui-tauri --clipboard`) forwards its
        // argv here and exits — it must NEVER start a second BLE/LAN
        // stack. Registered FIRST so it wins before any other setup.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            use tauri::{Emitter, Manager};
            tracing::info!(?argv, "single-instance: second launch forwarded");
            if let Some(pos) = argv.iter().position(|a| a == "--share") {
                // Nautilus "Share via Vortex" → push these files to the phone.
                let paths: Vec<String> = argv[pos + 1..].to_vec();
                share::handle_share(app, paths);
            } else if let Some(pos) = argv.iter().position(|a| a == "--call") {
                // Voice assistant "call <name>" → dial via the phone.
                if let Some(number) = argv.get(pos + 1).cloned() {
                    tauri::async_runtime::spawn(async move { call::dial(number).await });
                }
            } else if argv.iter().any(|a| a == "--call-answer") {
                // Voice assistant: answer the ringing call.
                tauri::async_runtime::spawn(async move { call::call_accept().await });
            } else if argv.iter().any(|a| a == "--call-decline") {
                // Voice assistant: decline the ringing call.
                tauri::async_runtime::spawn(async move { call::call_decline().await });
            } else if let Some(pos) = argv.iter().position(|a| a == "--sms-send") {
                // Voice assistant "send <body> to <name>" → send the SMS the same
                // way the UI does (CALL_CONTROL send_sms to the phone). `--sms-send`
                // checked before `--sms` since the flag string is a prefix.
                if let (Some(number), Some(body)) =
                    (argv.get(pos + 1).cloned(), argv.get(pos + 2).cloned())
                {
                    // Surface the open thread too, so the sent bubble is visible.
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.emit("vortex:open-sms", serde_json::json!({ "number": number }));
                        window::present(&w);
                    }
                    tauri::async_runtime::spawn(async move { call::send_sms(number, body).await });
                }
            } else if let Some(pos) = argv.iter().position(|a| a == "--sms") {
                // Voice assistant "message <name>" → open that contact's thread.
                if let Some(number) = argv.get(pos + 1).cloned() {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.emit("vortex:open-sms", serde_json::json!({ "number": number }));
                        window::present(&w);
                    }
                }
            } else if fs_cli::dispatch(&argv) {
                // `--fs-ls` / `--fs-stat` / `--fs-get`: drive the filesystem
                // client over whatever session is already up. Same rationale as
                // `--mirror` below — without a mount adapter or any browsing UI
                // yet, this is the only way to exercise the path at all.
            } else if argv.iter().any(|a| a == "--clipboard") {
                clipboard_window::show_clipboard_window(app);
            } else if argv.iter().any(|a| a == "--mirror") {
                // Same request the home screen's "Share screen" button makes.
                // Having it on the command line means the mirror can be driven
                // from a script or a shortcut without opening the window — and
                // it is the only way to exercise the whole path unattended.
                if let Some(ch) = app.try_state::<ipc::CmdChannel>() {
                    let _ = ch.0.send(ipc::UiCmd::StartMirror {
                        width: 720,
                        height: 1560,
                        fps: 60,
                        bitrate: 10_000_000,
                    });
                }
            } else if argv.iter().any(|a| a == "--camera") {
                // Continuity camera on/off, the same request the "use phone as
                // webcam" toggle makes. Same reason as `--mirror`: it is the
                // only way to exercise the path without a hand on the UI.
                if let Err(e) = camera::set_camera_request(true) {
                    tracing::warn!("--camera: {e}");
                }
            } else if argv.iter().any(|a| a == "--camera-stop") {
                let _ = camera::set_camera_request(false);
            } else if argv.iter().any(|a| a == "--mirror-stop") {
                if let Some(ch) = app.try_state::<ipc::CmdChannel>() {
                    let _ = ch.0.send(ipc::UiCmd::StopMirror);
                }
            } else {
                window::present_main(app);
            }
        }))
        // Window persistence: remember only the POSITION across launches, so
        // the window always opens at the standard config size (680×580) and is
        // never restored maximized/oversized — only where the user left it.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(tauri_plugin_window_state::StateFlags::POSITION)
                .build(),
        )
        .manage(cmd_channel)
        .manage(PairDecisionState::default())
        .setup(move |app| {
            let handle = app.handle().clone();
            // We move cmd_rx into the worker thread. Re-bind via Option to
            // satisfy `setup`'s FnOnce signature.
            let rx_holder: Arc<Mutex<Option<Receiver<UiCmd>>>> =
                Arc::new(Mutex::new(Some(cmd_rx)));
            let rx_holder_for_thread = rx_holder.clone();
            thread::spawn(move || {
                let rx = rx_holder_for_thread.lock().unwrap().take().unwrap();
                worker::run_worker(handle, rx);
            });

            // System tray (Telegram-style) — see tray::setup.
            tray::setup(app)?;

            // The window starts hidden (tauri.conf visible:false). Reveal it for
            // a normal launch, but NOT when autostarted with `--hidden` (the app
            // lives in the tray and keeps the BLE/LAN link up in the background —
            // no need to pop a window on every boot). --clipboard/--share open
            // their own surfaces, handled below / by single-instance.
            {
                let special = std::env::args()
                    .any(|a| a == "--hidden" || a == "--clipboard" || a == "--share");
                if !special {
                    window::present_main(app.handle());
                }
            }

            // Clipboard-history shortcut is fixed at <Super>v now (the
            // configurable Settings field was removed). Re-register it on every
            // launch — set_clipboard_hotkey rebuilds the command from the
            // current executable into a fixed gsettings slot (idempotent, no
            // duplicates), so the popup works out of the box AND self-heals the
            // path after a move/reinstall (a stale binding to an old build path
            // would otherwise launch nothing). gsettings blocks, so off-thread.
            thread::spawn(|| {
                let _ = clipboard_hotkey::set_clipboard_hotkey("<Super>v".to_string());
            });

            // Universal Control is the one switch that used to forget itself: it
            // lives entirely in this process, so a reboot or a quit left the edge
            // unarmed with the switch showing off. Put it back the way it was.
            #[cfg(target_os = "linux")]
            universal_control::ensure_bt_hid();
            // Per-user setup a package cannot do for us (autostart entry,
            // enabling the GNOME extension). Idempotent, so it also repairs an
            // install whose files were removed by hand.
            first_run::ensure();
            universal_control::restore(app.handle().clone());
            // Watch this desktop's own Do Not Disturb switch, so flipping it in
            // GNOME's menu silences the phone too.
            dnd::spawn_watcher();

            // The popup when THIS launch came from the GNOME shortcut (the
            // app wasn't running yet). The history WATCHER spawns inside
            // run_worker — it needs the worker's tokio runtime.
            if std::env::args().any(|a| a == "--clipboard") {
                clipboard_window::show_clipboard_window(app.handle());
            } else {
                // Otherwise build it hidden NOW, so the first Super+V of the
                // session is a show rather than a window build + webview boot +
                // Vue mount with the user watching. Deferred a moment so it
                // doesn't compete with the main window's own first paint.
                let h = app.handle().clone();
                thread::spawn(move || {
                    thread::sleep(std::time::Duration::from_secs(3));
                    let inner = h.clone();
                    let _ = h.run_on_main_thread(move || {
                        clipboard_window::prewarm(&inner);
                    });
                });
            }

            Ok(())
        })
        // Telegram-style: close → hide the window, keep the daemon
        // running in the background (BLE/LAN listeners stay up so
        // call-handoff and reconnect still work without the UI open).
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            diagnostics::diagnostics,
            phone_files::browse_phone,
            phone_files::stop_browsing_phone,
            phone_files::fetch_phone_file,
            send_to_phone::send_to_phone,
            diagnostics::diagnostics_report,
            worker::start_scan,
            worker::refresh_state,
            ipc::get_peer_states,
            ipc::host_platform,
            worker::start_screen_mirror,
            worker::stop_screen_mirror,
            pairing::start_pair,
            pairing::pair_decision,
            pairing::forget_peer,
            pairing::forget_all,
            pairing::switch_peer,
            pairing::cancel_switch,
            pairing::activate_peer,
            earbuds::refresh_local_earbuds,
            earbuds::open_bluetooth_settings,
            earbuds::scan_bluetooth_devices,
            earbuds::save_earbuds,
            earbuds::clear_earbuds,
            earbuds::get_saved_earbuds,
            earbuds::request_earbuds_switch,
            earbuds::send_earbuds_claim,
            earbuds::set_smart_switch_enabled,
            earbuds::get_smart_switch_enabled,
            notifications::set_notif_mirror_show,
            notifications::get_notif_mirror_show,
            notifications::set_notif_mirror_send,
            notifications::get_notif_mirror_send,
            proximity::get_proximity_settings,
            proximity::set_proximity_settings,
            clipboard::clipboard_history,
            clipboard::clipboard_capture_now,
            clipboard_sync::set_clipboard_sync,
            clipboard_sync::get_clipboard_sync,
            file_consent::set_file_auto_accept,
            file_consent::get_file_auto_accept,
            clipboard::clipboard_get,
            clipboard_window::clipboard_set_preview,
            clipboard::clipboard_select,
            clipboard::clipboard_pin,
            clipboard::clipboard_delete,
            clipboard_window::clipboard_hide,
            contacts::get_contacts,
            call_log::get_call_log,
            call_log::get_call_log_history,
            sms::get_sms,
            sms::get_sms_history,
            notifications::set_active_chat,
            call::dial,
            call::send_sms,
            call::mark_sms_read,
            call::load_sms_thread,
            camera::set_camera_request,
            camera::set_camera_facing,
            ring::ring_phone,
            notes::get_notes,
            notes::upsert_note,
            notes::toggle_todo,
            notes::delete_note,
            voice_settings::set_voice_lang,
            universal_control::uc_start,
            universal_control::uc_stop,
            universal_control::uc_running,
            universal_control::uc_set_placement,
            universal_control::uc_get_placement,
            fs_mount::open_phone_files,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Vortex Tauri")
        .run(|_app, event| {
            // Clean up what outlives this process.
            //
            // `app.exit(0)` from the tray went straight out with no hook, so
            // every quit left the phone's injector running, an `adb forward`
            // bound, an orphaned `adb shell` child, and an in-flight Wi-Fi
            // Direct join that only the phone's own 60 s teardown would undo.
            // None of it is ours to leave behind on the user's machine or on
            // their phone.
            if matches!(event, tauri::RunEvent::Exit) {
                tracing::info!("shutting down — releasing the injector and adb forward");
                crate::mirror_inject::stop();
                crate::laptop_cast::dispatch_request(false, None);
                // And hand the BLE link back. BlueZ owns the connection
                // independently of us, so without this it survives the process
                // — leaving the phone believing a peer is still attached, and
                // the next run scanning for an advertisement it will therefore
                // never send.
                #[cfg(target_os = "linux")]
                crate::ble::shutdown_link_blocking();
            }
        });
}
