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

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use tauri::WindowEvent;
use tokio::sync::oneshot;

use vortex_l3_daemon::core::pairing::handshake::LocalDecision;

mod ble;
mod call;
mod call_log;
mod camera;
mod clipboard;
mod clipboard_hotkey;
mod clipboard_window;
mod clipboard_sync;
mod transfers;
mod transfers_out;
mod worker_transfers;
mod worker_ctx;
mod cmd_pairing;
mod cmd_earbuds;
mod share;
mod file_consent;
mod contacts;
mod desktop_apps;
mod earbuds;
mod handoff;
mod ipc;
mod lan;
mod laptop_cast;
mod lan_wifi_direct;
mod lan_state;
mod live_activity;
mod media_remote;
mod mirror;
mod mirror_inject;
mod mirror_window;
mod notes;
mod notifications;
mod pairing;
mod proximity;
mod ring;
mod sms;
mod tray;
mod universal_control;
mod virtual_display;
mod voice_settings;
mod worker;

// Re-exports for items that moved out of lib.rs in the module split, so
// existing `crate::Item` references across the feature modules keep
// compiling unchanged.
pub(crate) use call::{
    CallWriter, CALL_CONTROL_SEQ, CALL_MIRROR_TX, CALL_WRITER, PENDING_CALL_CONTROL,
};
pub(crate) use clipboard_sync::{ClipboardImageWriter, ClipboardWriter};
pub(crate) use ipc::{app_state_to_dto, emit_peers, CmdChannel, UiCmd};
pub(crate) use notifications::{NotifWriter, ACTIVE_CHAT};

/// Generic laptop→phone sealed-frame writer: `(frame_ty, payload)` → an AEAD-
/// sealed BLE frame. The BLE persistent loop fills the holder on connect; any
/// feature (e.g. notes) sends through it without its own transport plumbing.
pub(crate) type SealedWriter = Arc<
    dyn Fn(
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
/// `(token, name, mime, transfer_id)` — the id ties each queued file to its
/// row in the transfer panel for live progress + completion.
pub(crate) static PENDING_FILE_OFFERS: std::sync::OnceLock<
    Mutex<std::collections::VecDeque<(String, String, String, u64)>>,
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

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCmd>();
    // Tray heartbeat: the 5-second local-earbuds rescan used to live in the
    // webview (a Vue setInterval) — but WebKit throttles hidden-window timers,
    // so with the window closed the tray battery/owner rows froze until the
    // phone's next heartbeat. Driven from here instead: same worker path,
    // independent of window visibility.
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
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                    tauri::async_runtime::spawn(async move { call::send_sms(number, body).await });
                }
            } else if let Some(pos) = argv.iter().position(|a| a == "--sms") {
                // Voice assistant "message <name>" → open that contact's thread.
                if let Some(number) = argv.get(pos + 1).cloned() {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.emit("vortex:open-sms", serde_json::json!({ "number": number }));
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
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
                camera::set_camera_request(true);
            } else if argv.iter().any(|a| a == "--camera-stop") {
                camera::set_camera_request(false);
            } else if argv.iter().any(|a| a == "--mirror-stop") {
                if let Some(ch) = app.try_state::<ipc::CmdChannel>() {
                    let _ = ch.0.send(ipc::UiCmd::StopMirror);
                }
            } else if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
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
                use tauri::Manager;
                let special = std::env::args()
                    .any(|a| a == "--hidden" || a == "--clipboard" || a == "--share");
                if !special {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
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
            universal_control::restore(app.handle().clone());

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
            worker::start_scan,
            worker::refresh_state,
            ipc::get_peer_states,
            worker::start_screen_mirror,
            worker::stop_screen_mirror,
            pairing::start_pair,
            pairing::pair_decision,
            pairing::forget_peer,
            pairing::forget_all,
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
            laptop_cast::set_extend_mode,
            laptop_cast::get_extend_mode,
            universal_control::uc_set_placement,
            universal_control::uc_get_placement,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Vortex Tauri");
}
