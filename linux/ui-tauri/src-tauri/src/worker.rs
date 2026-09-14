//! Worker thread: owns the BLE adapter, identity, peer store, and drives
//! the UiCmd channel loop. Emits Tauri events for every state change so
//! the Vue layer can render reactively. Split out of lib.rs.

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, State};

use vortex_l3_daemon::core::identity::Platform;
use vortex_l3_daemon::core::storage::peers::PeerStore;
#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::storage::peers_secret_service::SecretServicePeerStore;
// The secure-storage backend, chosen at compile time. Same two traits either
// way — Secret Service over D-Bus, or Credential Manager via CredWriteW.
#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::storage::secret_service::SecretServiceIdentityStore;
#[cfg(target_os = "windows")]
use vortex_l3_daemon::core::storage::windows_credentials::{
    WindowsIdentityStore, WindowsPeerStore,
};
use vortex_l3_daemon::core::storage::{load_or_generate, IdentityStore, InMemoryIdentityStore};

#[cfg(target_os = "linux")]
use crate::ble::run_ble_persistent_loop;
use crate::call::spawn_consumer as spawn_call_consumer;
use crate::call_log::spawn_consumer as spawn_call_log_consumer;
use crate::contacts::spawn_consumer as spawn_contacts_consumer;
use crate::ipc::{emit_peers, CmdChannel, IdentityInfo, TrustedPeerDto, UiCmd};
use crate::lan::{self, load_last_peer_ip, try_lan_reconnect};
use crate::live_activity::spawn_consumer as spawn_live_consumer;
use crate::sms::{self, spawn_consumer as spawn_sms_consumer};
use crate::{
    notifications, worker_ctx, BLE_RETRY_NUDGE, CALL_MIRROR_TX, CALL_WRITER, SYNC_NUDGE,
};
use crate::cmd_pairing;
// The UI commands that need a radio or an audio device.
#[cfg(target_os = "linux")]
use crate::{cmd_earbuds, earbuds};

#[tauri::command]
pub fn start_scan(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::Scan).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn refresh_state(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::RefreshState).map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn start_screen_mirror(
    state: State<'_, CmdChannel>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<(), String> {
    state
        .0
        .send(UiCmd::StartMirror { width, height, fps, bitrate })
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn stop_screen_mirror(state: State<'_, CmdChannel>) -> Result<(), String> {
    state.0.send(UiCmd::StopMirror).map_err(|e| e.to_string())
}

/// Phone-screen mirroring needs the GStreamer pipeline and the GTK window that
/// hosts its sink, both Linux-gated — so there is no handler for `StartMirror`
/// off Linux and the worker would drop it.
///
/// The failure mode this replaces is the bad one: these used to send the command
/// and return `Ok(())`, so the UI's mirror button reported success and then
/// nothing happened at all. Same contract as every other unsupported command
/// (see `platform_unsupported`) — say so, and let the UI show it.
#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub fn start_screen_mirror(
    _state: State<'_, CmdChannel>,
    _width: u32,
    _height: u32,
    _fps: u32,
    _bitrate: u32,
) -> Result<(), String> {
    Err(crate::platform_unsupported::UNSUPPORTED.to_string())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub fn stop_screen_mirror(_state: State<'_, CmdChannel>) -> Result<(), String> {
    Err(crate::platform_unsupported::UNSUPPORTED.to_string())
}

// --------------------------------------------------------------------------
// Worker — owns the BLE adapter, identity, peer store, and drives the
// channel loop. Emits Tauri events for every state change so the Vue
// layer can render reactively.
// --------------------------------------------------------------------------

/// Retry a startup step that can legitimately fail because something else on
/// the machine is not ready yet, instead of giving up on the whole worker.
///
/// Every use of this replaced a bare `return`, and that `return` was the bug:
/// the worker exits, `cmd_rx` is dropped, and from then on every command the UI
/// sends fails with "sending on a closed channel" — while the tray, the window
/// and the settings carry on working, so the app looks perfectly healthy with a
/// phone that simply reads "Offline" forever. Nothing retried, and nothing told
/// the user; the fix was to restart the app, if you thought of it.
///
/// The causes are all transient. The autostart entry fires eight seconds into
/// the session, which is a guess rather than a guarantee: on a cold boot
/// bluetoothd may still be starting, a keyring on an autologin session may not
/// be unlocked yet, a Bluetooth dongle may not be plugged in yet.
///
/// Backs off 2s → 30s for [`STARTUP_RETRY_WINDOW_SECS`] — long enough to
/// outlast a slow boot or someone typing their keyring password — and only then
/// calls it fatal, out loud.
// Every user of this is a BlueZ or Secret Service startup step, all of which
// are Linux-gated — so off Linux it is an unused macro rather than dead weight.
#[cfg(target_os = "linux")]
macro_rules! retry_startup {
    ($app:expr, $what:expr, $call:expr) => {{
        let started = std::time::Instant::now();
        let mut delay = std::time::Duration::from_secs(2);
        loop {
            match $call {
                Ok(v) => break Some(v),
                Err(err) => {
                    let waited = started.elapsed().as_secs();
                    if waited >= STARTUP_RETRY_WINDOW_SECS {
                        tracing::error!("FATAL: {} unavailable after {waited}s: {err}", $what);
                        let _ = $app.emit(
                            "vortex:fatal",
                            format!(
                                "{} is unavailable ({err}). Vortex cannot reach your phone until \
                                 this is fixed.",
                                $what
                            ),
                        );
                        break None;
                    }
                    tracing::warn!("{} not ready ({err}) — retrying in {delay:?}", $what);
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
    }};
}

/// How long [`retry_startup`] keeps trying before calling a step fatal.
const STARTUP_RETRY_WINDOW_SECS: u64 = 300;

pub(crate) fn run_worker(app: AppHandle, cmd_rx: Receiver<UiCmd>) {
    // Prime the LAN fast-path from disk so the first heartbeat after a restart
    // reuses the last-known phone IP instead of guessing the gateway.
    load_last_peer_ip();
    let rt = tokio::runtime::Builder::new_multi_thread()
        // 8 threads instead of 2: sync secret-service D-Bus calls
        // (peer_store.list/save/load_counter) block their executor
        // thread, and right after a re-pair the heartbeat and BLE
        // persistent loops can both hit them simultaneously. With
        // only 2 worker threads, that wedges the whole runtime —
        // timers stop firing and the whole UI goes silent. 8 leaves
        // plenty of headroom; spawn_blocking around the sync calls
        // would be the cleaner long-term fix.
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("tokio runtime");
    // Hand the BLE link back on SIGTERM/SIGINT too.
    //
    // Tauri's `RunEvent::Exit` covers a tray quit, but a session logout, a
    // `systemctl --user stop`, or a plain `kill` sends a signal that ends the
    // process without it — and that is the common path, because this app is
    // started from an autostart entry and dies with the session. Without the
    // teardown BlueZ keeps the GATT connection, the phone goes on believing a
    // peer is attached and stops advertising discoverably, and the next login's
    // instance scans for something it will never see.
    #[cfg(target_os = "linux")]
    rt.spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("shutdown signals unavailable; BLE teardown on exit is best-effort");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        tracing::info!("caught a shutdown signal — releasing the phone link");
        crate::mirror_inject::stop();
        crate::ble::shutdown_link_blocking();
        // The signal is ours now, so the default disposition never runs: exit
        // explicitly, or the process would sit here with nothing to stop it.
        std::process::exit(0);
    });

    rt.block_on(async move {
        // Identity store: Secret Service is mandatory per the V1
        // security baseline ("if secure storage is unavailable, V1
        // MUST stop and show an error"). We surface a vortex:fatal
        // event so the UI can render a banner instead of silently
        // downgrading to an in-memory identity that would persist
        // nothing across restarts.
        #[cfg(target_os = "windows")]
        let id_store: Box<dyn IdentityStore> = Box::new(WindowsIdentityStore);
        // Credential Manager needs no connection and cannot be "locked", so
        // there is no unavailable case to report and no keyring to unlock — the
        // fallback below is a Secret Service concern.
        #[cfg(target_os = "linux")]
        let id_store: Box<dyn IdentityStore> = match SecretServiceIdentityStore::new() {
            Ok(s) => Box::new(s),
            Err(err) => {
                tracing::error!("FATAL: secret-service unavailable ({err}); cannot start");
                let _ = app.emit(
                    "vortex:fatal",
                    format!("Secure storage unavailable: {err}. Unlock your keyring and restart Vortex."),
                );
                // Honour the long-standing dev escape hatch for hermetic
                // test environments (CI without a session keyring).
                if std::env::var("VORTEX_INSECURE").as_deref() == Ok("1") {
                    tracing::warn!("VORTEX_INSECURE=1 — falling back to in-memory identity (dev only)");
                    Box::new(InMemoryIdentityStore::new())
                } else {
                    return;
                }
            }
        };
        // The platform byte in the identity record is what the phone shows as
        // the peer's device class, so it must name the machine we are actually
        // running on.
        #[cfg(target_os = "linux")]
        let platform = Platform::Linux;
        #[cfg(target_os = "windows")]
        let platform = Platform::Windows;
        let identity = match load_or_generate(&*id_store, platform) {
            Ok(id) => id,
            Err(err) => {
                tracing::error!("FATAL: identity init failed: {err}");
                return;
            }
        };
        let _ = app.emit("vortex:identity", IdentityInfo { ready: true });

        // Peer store.
        #[cfg(target_os = "windows")]
        let peer_store: Arc<dyn PeerStore> = Arc::new(WindowsPeerStore);
        #[cfg(target_os = "linux")]
        let peer_store: Arc<dyn PeerStore> =
            match retry_startup!(app, "Secure storage", SecretServicePeerStore::new()) {
                Some(s) => Arc::new(s),
                None => {
                    let _ = app.emit::<Vec<TrustedPeerDto>>("vortex:peers", Vec::new());
                    return;
                }
            };
        emit_peers(&app, peer_store.clone()).await;
        let trusted = peer_store.list().unwrap_or_default();
        let _have_trust = !trusted.is_empty();
        // Point the phone-specific caches at the trusted peer before any
        // session exists, so the SMS/contacts/call-log pages render from cache
        // at startup exactly as they did when those files were global. Only
        // when there is exactly one peer: with several, "which phone's data"
        // has no answer until a session picks one (BLE IK sets it), and
        // guessing would show the wrong phone's messages.
        if let [only] = trusted.as_slice() {
            crate::arbiter::claim(&only.peer_static_pub);
        }

        // BLE adapter. BlueZ-specific: a `Session`, a default adapter, and a
        // pairing agent registered for the worker's lifetime. The Windows BLE
        // path needs none of this — WinRT resolves the radio per call — so it
        // is gated as a block rather than abstracted.
        //
        // BlueZ is very often not ready yet at this point. The autostart entry
        // fires eight seconds into the session, which is a guess, not a
        // guarantee: on a cold boot bluetoothd may still be coming up, and with
        // a USB dongle the adapter appears whenever it is plugged in. Both used
        // to `return` straight out of the worker — and everything below here,
        // the BLE loop, the LAN heartbeat and the proximity watcher, never
        // started. The tray, the window and the settings all kept working, so
        // the app looked perfectly healthy with a phone that was simply
        // "Offline" forever, until the user thought to restart it.
        #[cfg(target_os = "linux")]
        let Some(session) = retry_startup!(app, "Bluetooth service", bluer::Session::new().await)
        else {
            return;
        };
        #[cfg(target_os = "linux")]
        let Some(adapter) =
            retry_startup!(app, "Bluetooth adapter", session.default_adapter().await)
        else {
            return;
        };
        #[cfg(target_os = "linux")]
        let _ = adapter.set_powered(true).await;

        // ----- BlueZ pairing agent (BT bond, Just Works) -----
        // We register a NoInputNoOutput agent so `device.pair()` in
        // `do_pair` can complete Just Works bonding without any PIN
        // dialog on this side. Only `request_authorization` is wired up
        // (auto-accept); `request_confirmation` is intentionally left
        // unset — supplying it would push BlueZ into DisplayYesNo
        // capability and trigger numeric-comparison flows. The bond is
        // safe under Just Works because by the time `do_pair` calls
        // `device.pair()` the peer is already authenticated via
        // Noise+SAS (our app-layer MITM defence runs *before* the BT
        // bond — see do_pair). The `AgentHandle` MUST be kept alive for
        // the worker's lifetime; dropping it unregisters the agent and
        // BlueZ falls back to its default (which on a typical desktop
        // session would prompt the user).
        #[cfg(target_os = "linux")]
        let _agent_handle = match session
            .register_agent(bluer::agent::Agent {
                request_default: true,
                request_authorization: Some(Box::new(|_req| {
                    Box::pin(async move { Ok(()) })
                })),
                authorize_service: Some(Box::new(|_req| {
                    Box::pin(async move { Ok(()) })
                })),
                request_confirmation: Some(Box::new(|_req| {
                    Box::pin(async move { Ok(()) })
                })),
                request_passkey: Some(Box::new(|_req| {
                    Box::pin(async move { Ok(0) })
                })),
                request_pin_code: Some(Box::new(|_req| {
                    Box::pin(async move { Ok("0000".into()) })
                })),
                ..Default::default()
            })
            .await
        {
            Ok(h) => {
                tracing::info!("BlueZ pairing agent registered (Just Works)");
                Some(h)
            }
            Err(e) => {
                // Non-fatal: pairing still works at the Noise layer; only
                // the BT-level bond is skipped, which means the persistent
                // BLE loop will continue to chase rotating RPAs by scan
                // (current pre-bond behaviour).
                tracing::warn!("BlueZ agent register failed: {e}; bonding disabled this session");
                None
            }
        };

        // ----- Earbuds-switch orchestrator + media follow (Phases 1–3) -----
        // The whole audio/earbuds wiring (orchestrator + race-for-first-
        // success sender, smart-follow watcher, media runtime, resume
        // watcher, switch-state bridge) lives in earbuds::setup_audio.
        // The whole audio/earbuds wiring is Linux-only; elsewhere the
        // heartbeat gets an empty `AudioServices` and the features are absent.
        #[cfg(target_os = "linux")]
        let earbuds::AudioSetup {
            session_writers,
            ble_audio_writers,
            switch_orchestrator,
            media_watch,
            media_in_call,
            media_store,
        } = earbuds::setup_audio(&app, &adapter, peer_store.clone()).await;
        // Tracks the most recent call_phase seen from the phone so
        // try_lan_reconnect reacts only on transitions (e.g. null →
        // ringing), not every steady-state heartbeat.
        let last_call_phase: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        // Continuous auto-reconnect / refresh loop. Each pass does a
        // fresh IK + ping/pong + app-state exchange (~150ms locally).
        // The Mutex guards against overlapping reconnects when the
        // user pokes a manual action while we're in the middle of one.
        let auto_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Tracks when any path last completed a LAN reconnect. The
        // mDNS wake-up uses this as a cooldown gate: mdns-sd re-resolves
        // `_vortex._tcp` every couple of seconds (TTL refresh /
        // re-announce) even while we're already connected, and without
        // this guard each resolve fired a fresh full TCP+IK handshake —
        // 13 in ~27 s in one observed run. That storm burned the trust
        // counter and, by hammering libsecret/BlueZ D-Bus over and over,
        // wedged the executor (the very hazard the spawn_blocking note
        // below guards against). The 12 s heartbeat still refreshes the
        // link; mDNS now only pounces when the phone has actually been
        // gone longer than the cooldown.
        let last_reconnect_at: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        // Two gates, matching the heartbeat's own cadence (lan.rs): with
        // the BLE link live it carries liveness, state pushes and the
        // ~200 ms call signal, so LAN only keeps the cached-IP path warm;
        // with BLE down LAN is the sole liveness path and must stay brisk.
        const MDNS_COOLDOWN_BLE_LIVE: Duration = Duration::from_secs(240);
        const MDNS_COOLDOWN_LAN_ONLY: Duration = Duration::from_secs(12);

        // Event-driven push: a local state change (laptop charging flip or a
        // meaningful battery-level delta) fires this Notify to wake the
        // heartbeat loop immediately instead of waiting out the 12/45 s tick.
        // The periodic tick stays as a liveness floor + safety net for any
        // missed wake. This is the universal "state changed → sync now"
        // primitive — future fields just call `sync_nudge.notify_one()`.
        let sync_nudge = Arc::new(tokio::sync::Notify::new());
        // Publish the nudge so Tauri commands (e.g. the smart-switch toggle)
        // can wake the heartbeat to push the new state immediately.
        let _ = SYNC_NUDGE.set(sync_nudge.clone());
        // Pending phone-shared image token (LAN-pulled via bulk-sync).
        let _ = crate::PENDING_IMAGE_TOKEN.set(std::sync::Mutex::new(None));
        // …and the queue of instant-share files awaiting their LAN pull.
        let _ = crate::PENDING_FILE_OFFERS.set(std::sync::Mutex::new(std::collections::VecDeque::new()));

        // BLE-side twin: the LAN heartbeat fires this on its down→up edge so
        // the BLE presence wait retries the moment the phone shows up on the
        // network (the cross-transport presence hint).
        let ble_retry_nudge = Arc::new(tokio::sync::Notify::new());
        let _ = BLE_RETRY_NUDGE.set(ble_retry_nudge.clone());

        // BLE state-push channel: the persistent BLE listener forwards a
        // peer STATE frame (battery/charging) here as (peer_pub, AppState);
        // a consumer task applies it to the UI instantly — the same Vue
        // event + tray refresh a LAN heartbeat produces, but in ~200 ms over
        // the already-open BLE link instead of a fresh TCP+IK reconnect.
        let ble_state_tx =
            crate::lan_state::spawn_state_consumer(app.clone(), peer_store.clone());

        // BLE notification-mirror channel: the persistent listener forwards
        // a decoded NotificationMirror here; a consumer pops it as a desktop
        // notification via org.freedesktop.Notifications. Content is not
        // logged (privacy) beyond the app label.
        // BLE live-activity channel: the persistent listener forwards decoded
        // LiveActivity updates here; this consumer drives the top-bar tray
        // "pill" — on Linux `set_title` shows a text label next to the tray
        // icon (libappindicator label), updated in place as the ETA changes.
        // Live-activity style. Content not logged beyond the app label.
        // Call-card actions: the GNOME extension's in-call-pill buttons
        // (Mute/Speaker/End) call CallAction on the live-activity D-Bus
        // interface → this channel → the call consumer → the phone.
        let (call_action_tx, call_action_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        // The extension's pill buttons all arrive on one channel, and not all
        // of them are about a call: the transfer pill's Cancel is handled here
        // and never reaches the phone. Interposed rather than handled further
        // down so the call path keeps seeing only call verbs.
        let (pill_action_tx, mut pill_action_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        {
            let call_action_tx = call_action_tx.clone();
            tokio::spawn(async move {
                while let Some(verb) = pill_action_rx.recv().await {
                    if crate::transfers::handle_pill_action(&verb) {
                        continue;
                    }
                    let _ = call_action_tx.send(verb);
                }
            });
        }
        let ble_live_tx = spawn_live_consumer(app.clone(), pill_action_tx).await;

        // BLE call-mirror channel: the listener forwards CALL frames here; the
        // consumer drives the laptop's call banner (ringing → Accept/Decline)
        // and in-call pill (caller + live duration → Mute/End). The writer
        // handle carries the user's banner clicks back to the phone.
        let (ble_call_tx, ble_call_writer) =
            spawn_call_consumer(app.clone(), ble_live_tx.clone(), call_action_rx).await;
        // Expose the call-control writer globally so the `dial` command can
        // place a laptop-initiated call over the live BLE link.
        let _ = CALL_WRITER.set(ble_call_writer.clone());

        // BLE contacts-mirror channel: the listener forwards CONTACTS chunks
        // here; the consumer reassembles, caches, and emits to the Contacts page.
        let ble_contacts_tx = spawn_contacts_consumer(app.clone()).await;

        // BLE call-log-mirror channel: same pattern for the Recents page.
        let ble_call_log_tx = spawn_call_log_consumer(app.clone()).await;

        // BLE SMS-mirror channel: same pattern for the Messages page.
        let ble_sms_tx = spawn_sms_consumer(app.clone()).await;
        // BLE on-demand SMS-thread channel: a single conversation's page (the
        // Messages page's infinite scroll), MERGED into the open thread instead
        // of replacing the recent list.
        let ble_sms_thread_tx = sms::spawn_thread_consumer(app.clone()).await;
        // Publish the call-mirror sender so the LAN heartbeat can feed a peer
        // AppState's `call` into the same consumer (additive LAN path).
        let _ = CALL_MIRROR_TX.set(ble_call_tx.clone());

        // BLE browsing-handoff channel: the listener forwards HANDOFF frames
        // here; the consumer opens a shared page (Share) or shows a "continue
        // from phone" pill (live read) the user clicks to open.
        let ble_handoff_tx = crate::handoff::spawn_consumer(app.clone(), ble_live_tx.clone());
        // Publish it so a peer AppState's `handoff` (the LAN backstop) feeds the
        // same consumer alongside the dedicated BLE HANDOFF frame.
        let _ = crate::handoff::HANDOFF_TX.set(ble_handoff_tx.clone());

        // Notes bidirectional sync: a generic sealed-frame writer (filled on
        // connect) + the raw-frame channel the listener forwards NOTES_SYNC
        // into. All merge/protocol logic lives in notes.rs.
        let ble_sealed_writer: Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let _ = crate::BLE_SEALED_WRITER.set(ble_sealed_writer.clone());
        let ble_notes_tx = crate::notes::spawn_sync(app.clone(), ble_sealed_writer.clone());
        // The generic additive-frame channel is single-consumer and notes used to
        // own it. Put the peer-handoff dispatcher in front: it takes the frames it
        // handles and forwards the rest to notes unchanged.
        // Ranged filesystem, both ways: serves the phone's FS_REQs against
        // this laptop's roots, and correlates replies to requests we issue.
        // Shares the same sealed writer — the ops are just frames.
        crate::fs_link::init(ble_sealed_writer.clone());
        // Wi-Fi is preferred for the same frames; this hands it the
        // credentials a TCP+IK session needs (design doc §6).
        crate::fs_lan::init(identity.clone(), peer_store.clone());
        // Drains accepted phone file offers by streaming each one to disk.
        crate::fs_pull::spawn();
        let ble_raw_tx = crate::peer_handoff::spawn_dispatcher(
            app.clone(),
            peer_store.clone(),
            ble_notes_tx,
        );
        crate::notes::spawn_reminders(); // desktop due-date reminders

        // BLE app-icon channel: the listener forwards ICON chunks here; a
        // consumer reassembles each app's PNG and caches it to disk so
        // mirrored notifications can show the real app logo.
        let ble_icon_tx = notifications::spawn_icon_consumer();

        // Phone→laptop notification mirror + laptop→phone capture +
        // dismiss/action sync — see notifications::spawn_subsystem.
        let (ble_notif_tx, ble_notif_writer) = notifications::spawn_subsystem(app.clone());

        // Clipboard sync (P2): phone→laptop receive (set clipboard + history)
        // + laptop→phone send (the watcher queues; this drains via the writer).
        let (ble_clipboard_tx, ble_clipboard_writer, ble_clipboard_image_tx, ble_clipboard_image_writer) =
            crate::clipboard_sync::spawn_clipboard_sync(app.clone());
        // Phone image/file-offer → stash + nudge the LAN heartbeat to pull it.
        let ble_clipboard_offer_tx = crate::clipboard_sync::spawn_image_offer_consumer();
        // File-transfer pills (incoming pull + outgoing push) + receive-consent
        // action router. Self-contained — only needs the live-activity channel.
        crate::worker_transfers::wire_transfer_indicators(ble_live_tx.clone());
        // Wi-Fi Direct: the phone offered a P2P group → join it + pull fast.
        {
            let app = app.clone();
            vortex_l3_daemon::core::wifi_direct::set_hook(Box::new(move |ssid, pass| {
                crate::lan_wifi_direct::on_wifi_direct_offer(app.clone(), ssid, pass);
            }));
        }

        // The one place the platform difference in the heartbeat's handles
        // lives: populated on Linux, an empty stand-in elsewhere. A closure so
        // both the heartbeat and the mDNS-wake path below build a fresh bundle
        // without repeating the cfg.
        #[cfg(target_os = "linux")]
        let audio_services = || lan::AudioServices {
            switch_orchestrator: Some(switch_orchestrator.clone()),
            session_writers: Some(session_writers.clone()),
            media_store: Some(media_store.clone()),
            shared_adapter: Some(adapter.clone()),
            media_watch: Some(media_watch.clone()),
            media_in_call: Some(media_in_call.clone()),
        };
        #[cfg(not(target_os = "linux"))]
        let audio_services = || lan::AudioServices;

        // (1) Heartbeat: tick every 12 s, OR immediately when notified.
        //     Either way, a single Mutex serializes with the mDNS-wake
        //     and manual paths.
        lan::spawn_heartbeat(
            app.clone(),
            identity.clone(),
            peer_store.clone(),
            auto_lock.clone(),
            last_call_phase.clone(),
            audio_services(),
            last_reconnect_at.clone(),
            sync_nudge.clone(),
            #[cfg(target_os = "linux")]
            ble_audio_writers.clone(),
        );

        // Power watcher: edge-detect the laptop's charging flag + battery
        // level from sysfs every 2 s and, on a real change, nudge the
        // heartbeat to push the new state to the phone immediately. Cheap
        // (two tiny file reads) and fully portable — no UPower dependency.
        lan::spawn_power_watcher(sync_nudge.clone());

        // Locked-hint watcher: pushes fresh state to the phone the moment
        // the lock screen flips (remote command or local Super+L), so the
        // phone's lock icon doesn't sit stale until the next beat.
        #[cfg(target_os = "linux")]
        lan::spawn_locked_watch(sync_nudge.clone());

        // Proximity auto-lock/unlock (both toggles opt-in, Settings page).
        // Presence comes from the BLE-audio session map and needs a BlueZ
        // adapter to act on, so it rides with the Linux BLE path for now — the
        // `SessionControl` half of it (lock, and "can we unlock?") is already
        // portable.
        #[cfg(target_os = "linux")]
        crate::proximity::spawn_proximity_watch(
            ble_audio_writers.clone(),
            adapter.clone(),
            peer_store.clone(),
        );

        // Clipboard history watcher (P1) — polls wl-paste, persists to
        // ~/.cache/vortex/clipboard, feeds the Super+V popup.
        crate::clipboard::spawn_clipboard_watcher(app.clone());

        // Pin BLE discovery to the LE transport ONCE, up front. bluer
        // reads the stored discovery filter every time it (re)starts a
        // discovery session, and it only sends the filter on the FIRST
        // StartDiscovery of a shared session — so any per-scan attempt to
        // change it loses a race when a scan is already active (returns
        // DiscoveryActive). Storing LE here, before any scanner spawns,
        // guarantees every session (presence reconnect, UI pair scan,
        // even two that race at boot) is LE-only. Why it matters: a
        // *general* (dual-mode) discovery runs a fixed ~10.24 s BR/EDR
        // Inquiry that hogs the single radio and starved every LE connect
        // to ~10 s (root-caused via btmon). We never use BR/EDR discovery
        // for Vortex; the earbuds picker re-asserts Auto for its own scan.
        #[cfg(target_os = "linux")]
        if let Err(e) = adapter
            .set_discovery_filter(bluer::DiscoveryFilter {
                transport: bluer::DiscoveryTransport::Le,
                ..Default::default()
            })
            .await
        {
            tracing::warn!("could not pin LE-only discovery filter at startup: {e}");
        } else {
            tracing::info!("BLE discovery pinned to LE-only transport (no BR/EDR inquiry)");
        }

        // (1b) BLE persistent listener (P2.13). Independent of the LAN
        //      reconnect loop above — owns its own GATT connection so
        //      AUDIO_OP frames (notably call-start) reach us in ~200 ms
        //      instead of waiting for the 12 s LAN heartbeat. Runs only
        //      when a trusted peer exists; restarts itself on disconnect.
        // The portable BLE link, over the platform seam. Same job as the BlueZ
        // loop below — connect, IK, publish writers, pump the event stream —
        // without any BlueZ specifics. See `ble_portable` for why both exist.
        #[cfg(not(target_os = "linux"))]
        {
            let central = vortex_l3_daemon::core::platform::windows::ble::central();
            let identity_c = identity.clone();
            let peer_store_c = peer_store.clone();
            let sinks = crate::ble_portable::BleSinks {
                state: ble_state_tx.clone(),
                notif: ble_notif_tx.clone(),
                live: ble_live_tx.clone(),
                icon: ble_icon_tx.clone(),
                call: ble_call_tx.clone(),
                contacts: ble_contacts_tx.clone(),
                call_log: ble_call_log_tx.clone(),
                sms: ble_sms_tx.clone(),
                sms_thread: ble_sms_thread_tx.clone(),
                clipboard: ble_clipboard_tx.clone(),
                clipboard_image: ble_clipboard_image_tx.clone(),
                clipboard_offer: ble_clipboard_offer_tx.clone(),
                handoff: ble_handoff_tx.clone(),
                // The dispatcher's input, not notes' — peer-handoff frames are
                // taken in front and the rest forwarded on, so the seam loop has
                // to enter at the same point the BlueZ loop does.
                raw: ble_raw_tx.clone(),
            };
            let writers = crate::ble_portable::BleWriterSlots {
                notif: ble_notif_writer.clone(),
                clipboard: ble_clipboard_writer.clone(),
                clipboard_image: ble_clipboard_image_writer.clone(),
                call: ble_call_writer.clone(),
                sealed: ble_sealed_writer.clone(),
            };
            let nudge = ble_retry_nudge.clone();
            tokio::spawn(async move {
                crate::ble_portable::run_portable_ble_loop(
                    central,
                    identity_c,
                    peer_store_c,
                    sinks,
                    writers,
                    nudge,
                )
                .await;
            });
        }

        // The BlueZ loop: everything the seam version above does, plus the
        // BlueZ-specific self-healing (adapter power-cycle, `remove_device` to
        // force a re-resolve, last-RPA learning) that has no counterpart off
        // Linux.
        #[cfg(target_os = "linux")]
        {
            let ble_adapter = adapter.clone();
            let ble_identity = identity.clone();
            let ble_peer_store = peer_store.clone();
            let ble_orch = switch_orchestrator.clone();
            let ble_media = media_store.clone();
            let ble_writers = ble_audio_writers.clone();
            let ble_state_tx = ble_state_tx.clone();
            let ble_notif_tx = ble_notif_tx.clone();
            let ble_live_tx = ble_live_tx.clone();
            let ble_icon_tx = ble_icon_tx.clone();
            let ble_call_tx = ble_call_tx.clone();
            let ble_contacts_tx = ble_contacts_tx.clone();
            let ble_call_log_tx = ble_call_log_tx.clone();
            let ble_sms_tx = ble_sms_tx.clone();
            let ble_sms_thread_tx = ble_sms_thread_tx.clone();
            let ble_clipboard_tx = ble_clipboard_tx.clone();
            let ble_clipboard_image_tx = ble_clipboard_image_tx.clone();
            let ble_clipboard_offer_tx = ble_clipboard_offer_tx.clone();
            let ble_handoff_tx = ble_handoff_tx.clone();
            let ble_raw_tx = ble_raw_tx.clone();
            let ble_notif_writer = ble_notif_writer.clone();
            let ble_clipboard_writer = ble_clipboard_writer.clone();
            let ble_clipboard_image_writer = ble_clipboard_image_writer.clone();
            let ble_call_writer = ble_call_writer.clone();
            let ble_sealed_writer = ble_sealed_writer.clone();
            let ble_nudge = ble_retry_nudge.clone();
            tokio::spawn(async move {
                run_ble_persistent_loop(
                    ble_adapter,
                    ble_identity,
                    ble_peer_store,
                    ble_orch,
                    ble_media,
                    ble_writers,
                    ble_state_tx,
                    ble_notif_tx,
                    ble_live_tx,
                    ble_icon_tx,
                    ble_call_tx,
                    ble_contacts_tx,
                    ble_call_log_tx,
                    ble_sms_tx,
                    ble_sms_thread_tx,
                    ble_clipboard_tx,
                    ble_clipboard_image_tx,
                    ble_clipboard_offer_tx,
                    ble_handoff_tx,
                    ble_raw_tx,
                    ble_notif_writer,
                    ble_clipboard_writer,
                    ble_clipboard_image_writer,
                    ble_call_writer,
                    ble_sealed_writer,
                    ble_nudge,
                )
                .await;
            });
        }

        // (2) Event-driven wake-up: a long-lived mDNS browse fires the
        //     moment the phone announces itself (e.g. just woke up,
        //     just connected to Wi-Fi). We pounce on the first matching
        //     resolve so the user doesn't wait the full 12 s tick.
        //     Cheap: the auto_lock prevents overlap with the heartbeat.
        if let Ok(mut mdns_rx) =
            vortex_l3_daemon::core::lan::discovery::watch_candidates()
        {
            let auto_app = app.clone();
            let auto_identity = identity.clone();
            let auto_peer_store = peer_store.clone();
            let auto_lock_clone = auto_lock.clone();
            let auto_last_phase = last_call_phase.clone();
            #[cfg(target_os = "linux")]
            let auto_orch = switch_orchestrator.clone();
            #[cfg(target_os = "linux")]
            let auto_writers = session_writers.clone();
            #[cfg(target_os = "linux")]
            let auto_media = media_store.clone();
            #[cfg(target_os = "linux")]
            let auto_media_watch = media_watch.clone();
            #[cfg(target_os = "linux")]
            let auto_media_in_call = media_in_call.clone();
            #[cfg(target_os = "linux")]
            let auto_adapter = adapter.clone();
            #[cfg(target_os = "linux")]
            let auto_ble_writers = ble_audio_writers.clone();
            let mdns_last_reconnect = last_reconnect_at.clone();
            tokio::spawn(async move {
                while let Some(_cand) = mdns_rx.recv().await {
                    // Debounce: try-lock so multiple resolves within
                    // one cycle collapse into a single reconnect.
                    let g = match auto_lock_clone.try_lock() {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    #[cfg(target_os = "linux")]
                    let ble_live = !auto_ble_writers.lock().await.is_empty();
                    #[cfg(not(target_os = "linux"))]
                    let ble_live = crate::ble_portable::link_is_up();
                    // Cooldown: mdns-sd re-resolves the service every few
                    // seconds even while we're already connected, so this
                    // gate — not the resolve rate — is what decides how
                    // often we handshake. It has to track the heartbeat's
                    // OWN adaptive cadence, because both are answering the
                    // same question: how stale may the LAN link get?
                    //
                    // A flat 10 s gate ignored that. While BLE was live the
                    // heartbeat backed off to 240 s (BLE carries liveness),
                    // but mDNS still forced a full TCP+IK every ~10 s
                    // anyway: 267 handshakes in two hours on 2026-08-24,
                    // essentially all of them redundant, each one burning a
                    // trust counter and hammering libsecret/BlueZ D-Bus —
                    // the very load the auto_lock note warns wedges the
                    // executor.
                    //
                    // Gating on the last SUCCESSFUL sync keeps the useful
                    // half: when the phone has genuinely been away the gate
                    // is long expired, so its reappearance still reconnects
                    // on the first resolve.
                    {
                        let cooldown = if ble_live {
                            MDNS_COOLDOWN_BLE_LIVE
                        } else {
                            MDNS_COOLDOWN_LAN_ONLY
                        };
                        let last = mdns_last_reconnect.lock().await;
                        if let Some(t) = *last {
                            if t.elapsed() < cooldown {
                                drop(g);
                                continue;
                            }
                        }
                    }
                    let have_trust = {
                        let store = auto_peer_store.clone();
                        tokio::task::spawn_blocking(move || {
                            !store.list().unwrap_or_default().is_empty()
                        })
                        .await
                        .unwrap_or(false)
                    };
                    if have_trust {
                        tracing::info!("mDNS wake-up: triggering immediate reconnect");
                        #[cfg(target_os = "linux")]
                        let ble_live = !auto_ble_writers.lock().await.is_empty();
                        #[cfg(not(target_os = "linux"))]
                        let ble_live = false;
                        let outcome = try_lan_reconnect(
                            &auto_app,
                            &auto_identity,
                            auto_peer_store.clone(),
                            Some(auto_last_phase.clone()),
                            ble_live,
                            {
                                #[cfg(target_os = "linux")]
                                let a = lan::AudioServices {
                                    switch_orchestrator: Some(auto_orch.clone()),
                                    session_writers: Some(auto_writers.clone()),
                                    media_store: Some(auto_media.clone()),
                                    shared_adapter: Some(auto_adapter.clone()),
                                    media_watch: Some(auto_media_watch.clone()),
                                    media_in_call: Some(auto_media_in_call.clone()),
                                };
                                #[cfg(not(target_os = "linux"))]
                                let a = lan::AudioServices;
                                a
                            },
                        )
                        .await;
                        // Success only — see the matching note in the
                        // heartbeat. A failed attempt must not refresh the
                        // staleness gate, or a phone that is merely
                        // unreachable would keep pushing back the reconnect
                        // that is supposed to catch it coming back.
                        if matches!(outcome, Ok(Some(_))) {
                            *mdns_last_reconnect.lock().await =
                                Some(tokio::time::Instant::now());
                        }
                    }
                    drop(g);
                }
            });
        }

        // ----- Command loop -----
        // Handle to the most recent in-flight pairable scan. A pairing
        // connect needs a quiet radio (an active discovery contends with
        // connection establishment and dragged the connect out to ~10 s,
        // mirroring the reconnect case), so when the user taps Pair we
        // abort+await this first.
        let ctx = worker_ctx::WorkerCtx {
            app: app.clone(),
            #[cfg(target_os = "linux")]
            adapter: adapter.clone(),
            identity: identity.clone(),
            peer_store: peer_store.clone(),
            #[cfg(target_os = "linux")]
            switch_orchestrator: switch_orchestrator.clone(),
            #[cfg(target_os = "linux")]
            session_writers: session_writers.clone(),
        };
        let mut active_scan: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let cmd = match cmd_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(c) => c,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            // Thin dispatcher: each arm delegates to its feature's command
            // handler (cmd_pairing / cmd_earbuds / mirror) so run_worker stays
            // small as features are added.
            match cmd {
                #[cfg(target_os = "linux")]
                UiCmd::Scan => cmd_pairing::scan(&ctx, &mut active_scan),
                #[cfg(target_os = "linux")]
                UiCmd::Pair(addr_str) => cmd_pairing::pair(&ctx, addr_str, &mut active_scan).await,
                #[cfg(target_os = "linux")]
                UiCmd::ForgetPeer(hex_str) => cmd_pairing::forget_peer(&ctx, hex_str).await,
                #[cfg(target_os = "linux")]
                UiCmd::ForgetAll => cmd_pairing::forget_all(&ctx).await,
                // `switch_peer` runs a discovery, so it needs the BlueZ
                // adapter; the other two are a peer-store read and an arbiter
                // flip, which work anywhere.
                #[cfg(target_os = "linux")]
                UiCmd::SwitchPeer => cmd_pairing::switch_peer(&ctx),
                UiCmd::CancelSwitch => cmd_pairing::cancel_switch(&ctx).await,
                UiCmd::ActivatePeer(hex_str) => {
                    cmd_pairing::activate_peer(&ctx, hex_str).await
                }
                #[cfg(target_os = "linux")]
                UiCmd::RefreshState => cmd_earbuds::refresh_state(&ctx).await,
                #[cfg(target_os = "linux")]
                UiCmd::RefreshLocalEarbuds => cmd_earbuds::refresh_local_earbuds(&ctx).await,
                #[cfg(target_os = "linux")]
                UiCmd::RequestEarbudsSwitch { peer_static_pub, mac } => {
                    cmd_earbuds::request_switch(&ctx, peer_static_pub, mac).await
                }
                #[cfg(target_os = "linux")]
                UiCmd::SendEarbudsClaim { peer_static_pub, mac } => {
                    cmd_earbuds::send_claim(&ctx, peer_static_pub, mac).await
                }
                #[cfg(target_os = "linux")]
                UiCmd::ToggleEarbuds => cmd_earbuds::toggle_earbuds(&ctx).await,
                #[cfg(target_os = "linux")]
                UiCmd::StartMirror { width, height, fps, bitrate } => {
                    crate::mirror::handle_start_cmd(&ctx, width, height, fps, bitrate).await
                }
                #[cfg(target_os = "linux")]
                UiCmd::StopMirror => crate::mirror::handle_stop_cmd(),
                // The mirror commands cannot be constructed off Linux (the UI
                // paths that send them are gated too), but the enum is shared.
                // The commands whose handlers need a radio, an audio device or a
                // capture pipeline. The UI paths that send them are gated too,
                // so reaching here means something new started sending one —
                // logged rather than silently ignored.
                // Pairing over the seam: scan for a pairable phone, then the
                // same XX handshake and trust save the Linux path uses. The
                // address the UI sends is ignored — see `pair_by_scan`.
                // Forgetting a peer needs no radio: a trust-store delete, a
                // cache purge, and a best-effort LAN revoke so the phone drops
                // us too. These were Linux-only purely because the module they
                // live in was, which left a Windows user with a paired-forever
                // phone and no way back — the trust record is in Credential
                // Manager, so there is no folder to delete either.
                #[cfg(not(target_os = "linux"))]
                UiCmd::ForgetPeer(hex_str) => cmd_pairing::forget_peer(&ctx, hex_str).await,
                #[cfg(not(target_os = "linux"))]
                UiCmd::ForgetAll => cmd_pairing::forget_all(&ctx).await,
                // THE thing that unblocks the whole UI off Linux, and the
                // reason the radar stayed empty even once `Scan` had a handler.
                //
                // `emit_peers` runs once at worker startup (above), but Tauri
                // events are not replayed: the webview registers its listeners
                // in `initConnectionStore()`, and anything emitted before that
                // is gone. The frontend's `peersLoaded` flips only on a
                // `vortex:peers` event, and `runScanLoop` refuses to start
                // until it is true — so a lost startup emit means no scan is
                // ever requested, no matter how well discovery works.
                //
                // Linux never noticed because this command re-emits peers, and
                // Vue calls it on every mount and webview reload. That makes it
                // the repair for the lost first emit, not just a refresh. Off
                // Linux it was dropped, so `peersLoaded` stayed false for the
                // life of the process.
                //
                // Identity and peers only: the rest of the Linux handler is
                // earbuds and switch state, which need a radio and an audio
                // device. The UI treats those events as optional; `vortex:peers`
                // is the one it gates on.
                #[cfg(not(target_os = "linux"))]
                UiCmd::RefreshState => {
                    let _ = ctx.app.emit("vortex:identity", IdentityInfo { ready: true });
                    emit_peers(&ctx.app, ctx.peer_store.clone()).await;
                }
                // The radar the Pair button lives on. Without this arm the
                // Windows pairing screen has nothing to click, so the handler
                // below is unreachable — see `scan_for_ui`.
                #[cfg(not(target_os = "linux"))]
                UiCmd::Scan => crate::ble_portable::scan_for_ui(
                    &ctx.app,
                    vortex_l3_daemon::core::platform::windows::ble::central(),
                    &mut active_scan,
                ),
                #[cfg(not(target_os = "linux"))]
                UiCmd::Pair(_addr) => {
                    // Quiet the radio first, for the reason the BlueZ path
                    // documents below: the radar scan and the pairing scan are
                    // two watchers competing for one radio, and `pair_by_scan`
                    // starts with a scan of its own.
                    if let Some(h) = active_scan.take() {
                        h.abort();
                        let _ = h.await;
                    }
                    let central = vortex_l3_daemon::core::platform::windows::ble::central();
                    let result = crate::ble_portable::pair_by_scan(
                        &ctx.app,
                        central,
                        &ctx.identity,
                        ctx.peer_store.clone(),
                    )
                    .await;
                    // `vortex:pairing_result` — the SAME event the BlueZ path
                    // emits, and the only one the overlay listens for. It had
                    // been emitting `vortex:pairing_error`, which nothing
                    // subscribes to: a pairing that fully succeeded left the
                    // approve screen up forever, on top of the rest of the UI,
                    // while the phone was already synced underneath it. Both
                    // outcomes have to be reported, not just the failure.
                    match result {
                        Ok(()) => {
                            let _ = ctx.app.emit(
                                "vortex:pairing_result",
                                crate::ipc::PairingResultDto::Ok {
                                    ok: true,
                                    message: "trust persisted".to_string(),
                                },
                            );
                            emit_peers(&ctx.app, ctx.peer_store.clone()).await;
                        }
                        Err(e) => {
                            tracing::warn!("pairing failed: {e}");
                            let _ = ctx.app.emit(
                                "vortex:pairing_result",
                                crate::ipc::PairingResultDto::Err { ok: false, error: e },
                            );
                        }
                    }
                }
                // Naming the command matters: the frontend is one shared
                // bundle, so anything it polls lands here, and a nameless warn
                // repeated every few seconds says only "something is missing".
                #[cfg(not(target_os = "linux"))]
                other => tracing::warn!(cmd = ?other, "UI command has no handler on this platform; ignoring"),
            }
        }
    });
}
