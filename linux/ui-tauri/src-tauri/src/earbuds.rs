//! Earbuds + smart-switch Tauri commands (save/scan/switch/claim, the
//! smart audio-follow toggle). Split out of lib.rs.

use std::sync::Arc;

use tauri::{Emitter, State};

use crate::ipc::switch_state_dto;
use crate::{CmdChannel, UiCmd, MEDIA_WATCH, SYNC_NUDGE};

/// Auto-persist a peer's earbuds into our own store so the card pins on this
/// (e.g. freshly-paired) device too — the buds appear on every device once one
/// of them knows the pair, instead of only the side that picked them. Acts only
/// when the peer reports them CONNECTED with a real address AND we have nothing
/// saved: it never overwrites the user's own pick. No feedback loop — once
/// saved we report the same (addr, name) back, and the peer's own no-saved
/// guard makes that a no-op.
pub(crate) fn persist_peer_earbuds(state: &vortex_l3_daemon::core::appstate::AppState) {
    let Some(buds) = state.earbuds.as_ref() else { return };
    if !buds.connected || buds.address.is_empty() {
        return;
    }
    if vortex_l3_daemon::core::earbuds_store::load().is_some() {
        return;
    }
    let saved = vortex_l3_daemon::core::earbuds_store::SavedEarbuds {
        address: buds.address.clone(),
        name: buds.name.clone(),
    };
    match vortex_l3_daemon::core::earbuds_store::save(&saved) {
        Ok(()) => tracing::info!(name = %buds.name, "auto-saved peer earbuds (card pinned locally)"),
        Err(e) => tracing::warn!("auto-save peer earbuds failed: {e}"),
    }
}

#[tauri::command]
pub fn refresh_local_earbuds(state: State<'_, CmdChannel>) -> Result<(), String> {
    state
        .0
        .send(UiCmd::RefreshLocalEarbuds)
        .map_err(|e| e.to_string())
}

/// Initiator entry for the earbuds-switch flow (Phase 1). Failure to
/// queue the command — not orchestrator-level rejection — is
/// returned synchronously; the actual flow result arrives later on
/// the `vortex:switch_state` event.
#[tauri::command]
pub fn request_earbuds_switch(
    peer_static_pub: String,
    mac: String,
    state: State<'_, CmdChannel>,
) -> Result<(), String> {
    state
        .0
        .send(UiCmd::RequestEarbudsSwitch { peer_static_pub, mac })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn send_earbuds_claim(
    peer_static_pub: String,
    mac: String,
    state: State<'_, CmdChannel>,
) -> Result<(), String> {
    state
        .0
        .send(UiCmd::SendEarbudsClaim { peer_static_pub, mac })
        .map_err(|e| e.to_string())
}

/// In-app Bluetooth device picker — kicks off a short BlueZ scan and
/// returns the list of known devices (paired + previously-seen +
/// freshly-discovered). The Vue layer renders these in the "+ Add
/// earbuds" modal so the user never has to leave the app.
#[tauri::command]
pub async fn scan_bluetooth_devices() -> Result<Vec<vortex_l3_daemon::core::earbuds::BluetoothDevice>, String> {
    let session = bluer::Session::new()
        .await
        .map_err(|e| format!("bluer session: {e}"))?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| format!("bluer adapter: {e}"))?;
    let _ = adapter.set_powered(true).await;
    // 4s discovery window is enough to surface most nearby audio
    // peripherals in pairing mode without making the modal feel slow.
    vortex_l3_daemon::core::earbuds::start_brief_discovery(
        &adapter,
        std::time::Duration::from_secs(4),
    )
    .await;
    Ok(vortex_l3_daemon::core::earbuds::list_known_devices(&adapter).await)
}

#[tauri::command]
pub async fn save_earbuds(address: String, name: String) -> Result<(), String> {
    // Persist the choice first — UI updates instantly off the saved
    // entry regardless of whether BlueZ is reachable. The actual
    // connect is best-effort: many users pick a pair they already
    // have streaming through some other host, and we still want the
    // card populated so the switch-to-Linux button has somewhere to
    // aim.
    vortex_l3_daemon::core::earbuds_store::save(
        &vortex_l3_daemon::core::earbuds_store::SavedEarbuds {
            address: address.clone(),
            name,
        },
    )
    .map_err(|e| format!("save earbuds: {e}"))?;
    // Auto-connect in the background so picking an earbud "just
    // works" without making the user open KDE Bluetooth settings.
    // Spawned (not awaited) — pairing-class buds can take 3–4 s to
    // accept the A2DP profile and we don't want the Vue command to
    // hang. Errors are logged; the UI shows the connected state via
    // the periodic local_earbuds refresh.
    tokio::spawn(async move {
        match bluer::Session::new().await {
            Ok(session) => match session.default_adapter().await {
                Ok(adapter) => {
                    if let Err(e) = vortex_l3_daemon::core::audio_switch::connect_audio(
                        &adapter, &address,
                    )
                    .await
                    {
                        tracing::warn!(%address, "save_earbuds: auto-connect failed: {e}");
                    } else {
                        tracing::info!(%address, "save_earbuds: auto-connect ok");
                    }
                }
                Err(e) => tracing::warn!("save_earbuds: adapter unavailable: {e}"),
            },
            Err(e) => tracing::warn!("save_earbuds: bluer session: {e}"),
        }
    });
    Ok(())
}

#[tauri::command]
pub fn clear_earbuds() -> Result<(), String> {
    vortex_l3_daemon::core::earbuds_store::clear()
        .map_err(|e| format!("clear earbuds: {e}"))
}

/// Read the saved earbuds row from disk. The Vue side needs the MAC
/// address to drive switch flows — the EarbudsSnapshot pushed via
/// vortex:local_earbuds intentionally omits the MAC (so the card
/// doesn't leak it into the webview when it doesn't need to), so we
/// expose a direct read for the swap click path.
#[tauri::command]
pub fn get_saved_earbuds() -> Option<vortex_l3_daemon::core::earbuds_store::SavedEarbuds> {
    vortex_l3_daemon::core::earbuds_store::load()
}

/// Launch the user's system Bluetooth settings so they can pair or
/// unpair an audio device. We don't programmatically run BT pairing
/// because that requires privileged D-Bus access on most desktops
/// and an intrusive `ACTION_REQUEST_DISCOVERABLE` flow on Android —
/// the system UI is the right surface for this.
#[tauri::command]
pub fn open_bluetooth_settings() -> Result<(), String> {
    let candidates: &[&[&str]] = &[
        &["gnome-control-center", "bluetooth"],
        &["blueberry"],
        &["blueman-manager"],
        &["systemsettings5", "bluetooth"],
        &["plasma-settings", "kcm_bluetooth"],
        &["xdg-open", "bluetooth://"],
    ];
    for argv in candidates {
        let (cmd, args) = argv.split_first().unwrap();
        if std::process::Command::new(cmd)
            .args(args.iter())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }
    Err("no Bluetooth settings launcher found".into())
}

/// Settings toggle: enable/disable smart audio-follow. A SHARED setting —
/// stamped with the current time and persisted, then synced to the phone
/// last-writer-wins via the AppState heartbeat. When off, the laptop never
/// auto-grabs on a local play-edge (manual tray switch still works).
#[tauri::command]
pub fn set_smart_switch_enabled(enabled: bool) {
    use std::sync::atomic::Ordering;
    if let Some(mw) = MEDIA_WATCH.get() {
        // Stamp now, but force strictly-greater than our current ts so a
        // local toggle always wins over our own previous value (even two
        // in the same second) and propagates.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ts = now.max(mw.enabled_changed_at.load(Ordering::Relaxed) + 1);
        mw.apply_setting(enabled, ts);
        // Wake the heartbeat so the new value reaches the phone now (~1 s)
        // rather than on the next periodic tick.
        if let Some(n) = SYNC_NUDGE.get() {
            n.notify_one();
        }
    }
}

/// Current smart-audio-follow enabled state (daemon is the source of truth,
/// loaded from the persisted store at startup + LWW-synced with the phone).
#[tauri::command]
pub fn get_smart_switch_enabled() -> bool {
    MEDIA_WATCH
        .get()
        .map(|mw| mw.enabled.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(true)
}

/// Everything the worker needs back from [`setup_audio`].
pub(crate) struct AudioSetup {
    pub(crate) session_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
    pub(crate) ble_audio_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
    pub(crate) switch_orchestrator:
        Arc<vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator>,
    pub(crate) media_watch: Arc<vortex_l3_daemon::core::media_watch::MediaWatch>,
    pub(crate) media_in_call: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) media_store: vortex_l3_daemon::core::media_runtime::MediaStateStore,
}

/// One-time audio/earbuds wiring for the worker: the switch orchestrator
/// (with its race-for-first-success sender), the smart audio-follow
/// watcher, the media runtime, the post-switch resume watcher, and the
/// orchestrator→webview switch-state bridge. Split out of run_worker.
/// Put the saved earbuds' card back on A2DP when it ends up headset-only
/// outside any switch flow.
///
/// `ensure_card_on_a2dp` already exists, but it only ever runs INSIDE
/// `connect_audio` — so it covers switches vortex performed and nothing else.
/// When the earbuds reconnect to this laptop on their own (they do; they are
/// paired here) and A2DP fails to come up, the card is left with only headset
/// profiles: playback is 16 kHz mono, or nothing at all. WirePlumber says as
/// much and gives up — "Could not find valid non-headset profile, not
/// switching" — four times in a week here, with no vortex activity anywhere
/// near it.
///
/// Deliberately narrow. It touches ONE card, the saved earbuds', and only when
/// three things hold: they are connected here, the card is on a headset
/// profile, and nobody is using the microphone (that is a call, and a call is
/// exactly when the headset profile is correct). It is a per-device
/// `set-card-profile` — nothing global, nothing that outlives vortex.
pub(crate) fn spawn_a2dp_profile_watch(adapter: bluer::Adapter, mac: String) {
    tokio::spawn(async move {
        let Ok(addr) = mac.parse::<bluer::Address>() else { return };
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tick.tick().await;
            // Only while the buds are actually here.
            if !vortex_l3_daemon::core::audio_switch::audio_active(&adapter, addr).await {
                continue;
            }
            if vortex_l3_daemon::core::audio_switch::a2dp_card_active(&mac).await {
                continue;
            }
            // A live call is the legitimate reason to be on headset.
            if laptop_on_headset_call() {
                continue;
            }
            tracing::info!(%mac, "earbuds are here but the card is headset-only — repairing");
            let _ =
                vortex_l3_daemon::core::audio_switch::ensure_card_on_a2dp(&adapter, addr, &mac)
                    .await;
        }
    });
}

/// Cached answer to "is this laptop on a Bluetooth call right now".
///
/// Cached because the acceptance provider is synchronous and is consulted on the
/// frame-handling path, where forking `pactl` twice is not acceptable. Refreshed
/// by [`spawn_headset_call_watch`] on a slow tick; a couple of seconds of
/// staleness is harmless, since a call lasts far longer than that.
static LAPTOP_ON_HEADSET_CALL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn laptop_on_headset_call() -> bool {
    LAPTOP_ON_HEADSET_CALL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Keep [`laptop_on_headset_call`] current for the saved earbuds.
pub(crate) fn spawn_headset_call_watch(mac: String) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut last = false;
        loop {
            tick.tick().await;
            let now = vortex_l3_daemon::core::audio_switch::headset_mic_in_use(&mac).await;
            if now != last {
                tracing::info!(on_call = now, "laptop headset-call state changed");
                last = now;
            }
            LAPTOP_ON_HEADSET_CALL.store(now, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

pub(crate) async fn setup_audio(
    app: &tauri::AppHandle,
    adapter: &bluer::Adapter,
    peer_store: Arc<dyn vortex_l3_daemon::core::storage::peers::PeerStore>,
) -> AudioSetup {
    // ----- Earbuds-switch orchestrator (Phase 1) -----
    // The session_writers map is shared between this side's sender
    // closure and the LAN-session module that opens TCP+IK. When
    // a flow is in-flight the corresponding writer is registered
    // and the orchestrator's outbound frames ride that socket.
    // No writer = no active session; we surface that as a brief
    // Failed state (auto-resets to Idle after FAILED_RESET_MS).
    let session_writers =
        vortex_l3_daemon::core::audio_lan_session::new_session_writer_map();
    // BLE-write fallback registry. Populated by the persistent
    // BLE audio-signal loop for the lifetime of one IK transport
    // session; lets the orchestrator's sender reach the peer
    // even when no LAN session is open (call-handoff windows
    // where mDNS hasn't re-resolved yet — the regression
    // ChatGPT flagged as review #4). Same `SessionWriter` shape
    // as `session_writers` so the dispatch is uniform.
    let ble_audio_writers =
        vortex_l3_daemon::core::audio_lan_session::new_session_writer_map();
    let switch_orchestrator: Arc<vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator> =
        Arc::new({
            let lan_writers = session_writers.clone();
            let ble_writers = ble_audio_writers.clone();
            vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator::new(
                Arc::new(vortex_l3_daemon::core::audio_orchestrator::BluerBt::new(
                    adapter.clone(),
                )),
                peer_store.clone(),
                Arc::new(move |peer_pub, frame| {
                    let lan_writers = lan_writers.clone();
                    let ble_writers = ble_writers.clone();
                    Box::pin(async move {
                        // Transport selection: when BOTH transports are
                        // up, RACE them and return on the first SUCCESS,
                        // so a hand-off always rides whichever path is
                        // healthy/fastest right now (LAN ~90 ms on Wi-Fi,
                        // BLE ~200 ms but works off-network). The receiver
                        // dedups by the frame's monotonic `nonce`, so
                        // sending the SAME frame over both is safe — the
                        // slower copy is dropped as a replay. Each write
                        // is `tokio::spawn`ed (not select-then-drop)
                        // because dropping a half-written LAN frame would
                        // corrupt the TCP session: the loser must run to
                        // completion, just in the background. Previously
                        // this was sequential LAN-then-BLE, which also
                        // never fell through to BLE when a *present* LAN
                        // writer FAILED — the race fixes that too.
                        let op_dbg = format!("{:?}", frame.op);
                        let prefix = hex::encode(&peer_pub[..4]);
                        let lan = { lan_writers.lock().await.get(&peer_pub).cloned() };
                        let ble = { ble_writers.lock().await.get(&peer_pub).cloned() };
                        fn flat(
                            r: Result<Result<(), String>, tokio::task::JoinError>,
                        ) -> Result<(), String> {
                            match r {
                                Ok(inner) => inner,
                                Err(e) => Err(format!("task: {e}")),
                            }
                        }
                        match (lan, ble) {
                            (None, None) => {
                                tracing::warn!(peer = %prefix, op = %op_dbg, "no active session writer (LAN + BLE both absent)");
                                Err("no active session".to_string())
                            }
                            (Some(w), None) => w(frame).await,
                            (None, Some(w)) => w(frame).await,
                            (Some(lw), Some(bw)) => {
                                let f2 = frame.clone();
                                let mut lan_task = tokio::spawn(lw(frame));
                                let mut ble_task = tokio::spawn(bw(f2));
                                tokio::select! {
                                    r = &mut lan_task => match flat(r) {
                                        Ok(()) => Ok(()), // LAN won; BLE finishes in background (deduped)
                                        Err(le) => match flat(ble_task.await) {
                                            Ok(()) => Ok(()),
                                            Err(be) => {
                                                tracing::warn!(peer = %prefix, op = %op_dbg, "both transports failed (lan: {le}; ble: {be})");
                                                Err(format!("both failed (lan: {le}; ble: {be})"))
                                            }
                                        },
                                    },
                                    r = &mut ble_task => match flat(r) {
                                        Ok(()) => Ok(()), // BLE won; LAN finishes in background (deduped)
                                        Err(be) => match flat(lan_task.await) {
                                            Ok(()) => Ok(()),
                                            Err(le) => {
                                                tracing::warn!(peer = %prefix, op = %op_dbg, "both transports failed (ble: {be}; lan: {le})");
                                                Err(format!("both failed (ble: {be}; lan: {le})"))
                                            }
                                        },
                                    },
                                }
                            }
                        }
                    })
                }),
                // Owner-vote: the laptop refuses to hand the earbuds over while
                // it is the one on a call.
                //
                // This side always answered Allow, so a phone-side media start
                // could pull the earbuds — and the microphone with them — out of
                // a live meeting here, mid-sentence. The phone has had exactly
                // this gate since Phase 2; the laptop never got its half.
                Arc::new(|| {
                    if laptop_on_headset_call() {
                        vortex_l3_daemon::core::audio_orchestrator::Acceptance::Reject(
                            vortex_l3_daemon::core::audio_op::RejectReason::InCall,
                        )
                    } else {
                        vortex_l3_daemon::core::audio_orchestrator::Acceptance::Allow
                    }
                }),
            )
        });
    switch_orchestrator.recover_on_start().await;

    // Keep the "am I on a call through these earbuds" flag fresh for the
    // acceptance gate above.
    if let Some(saved) = get_saved_earbuds() {
        spawn_headset_call_watch(saved.address.clone());
        spawn_a2dp_profile_watch(adapter.clone(), saved.address.clone());
    }

    // ----- First-run earbuds adoption. On a fresh install, if a
    //       Bluetooth audio device is already connected to this laptop
    //       AND the user has never configured earbuds, adopt it
    //       automatically so the card appears without opening the
    //       picker. Once saved it rides the AppState heartbeat, and the
    //       phone auto-pins it on its side too (VortexStackAppState) —
    //       so a single fresh launch lands the buds on BOTH devices.
    //       Gated by a one-shot marker so a later "Remove from Vortex"
    //       isn't silently undone on the next launch.
    if !vortex_l3_daemon::core::earbuds_store::autodetect_done() {
        if vortex_l3_daemon::core::earbuds_store::load().is_none() {
            if let Some(found) =
                vortex_l3_daemon::core::earbuds::detect_connected_earbud(adapter).await
            {
                match vortex_l3_daemon::core::earbuds_store::save(&found) {
                    Ok(()) => tracing::info!(
                        name = %found.name,
                        addr = %found.address,
                        "first-run: adopted already-connected earbuds"
                    ),
                    Err(e) => tracing::warn!("first-run earbuds adopt failed: {e}"),
                }
            }
        }
        let _ = vortex_l3_daemon::core::earbuds_store::mark_autodetect_done();
    }

    // ----- Smart audio-follow (Phase 3). Watch local MPRIS playback;
    //       when media starts on the laptop while the buds are
    //       elsewhere, grab them here (mirror of the Android
    //       MediaHandoffCoordinator). The *release* half — laptop
    //       hands the buds to the phone when the phone starts playing
    //       — reacts to the peer's `media_playing` flag in the
    //       heartbeat loop below. `media_in_call` gates the grab off
    //       during a call (calls outrank media).
    // Loads the persisted on/off + LWW timestamp from smart_switch_store.
    let media_watch = vortex_l3_daemon::core::media_watch::MediaWatch::new();
    // Publish the handle so the Settings toggle can flip `enabled`.
    let _ = MEDIA_WATCH.set(media_watch.clone());
    let media_in_call =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // ----- Media runtime (Phase 2). MPRIS pause/resume tied to
    //       the phone's call-phase signal. The store lives across
    //       the whole worker lifetime; pause is set on a `ringing`
    //       AppState and the orchestrator's Idle transition is
    //       what triggers the matching resume (so audio doesn't
    //       leak through laptop speakers before the buds are back).
    let media_store = vortex_l3_daemon::core::media_runtime::new_media_state_store();
    vortex_l3_daemon::core::media_watch::spawn(
        media_watch.clone(),
        switch_orchestrator.clone(),
        adapter.clone(),
        peer_store.clone(),
        media_in_call.clone(),
        // The watcher drains this same call-pause store when the buds
        // return, so a media grab that tripped the BLE fast-path
        // `pause_playing_for_call` can't leave a stale "Paused" record
        // that would make the NEXT real call fail to pause media.
        media_store.clone(),
    );

    // Watch the orchestrator state. When a flow returns to Idle
    // AND we have a pending pause record (= we paused for a call
    // that's now over), resume the players — but only AFTER the
    // PulseAudio bluez sink reaches a ready (non-SUSPENDED) state
    // and we've migrated existing streams to it. Without that wait
    // the player streams to the previous default sink (laptop
    // speaker) for half a syllable, then WirePlumber finishes the
    // route migration and re-suspends the inputs, leaving the
    // user with "I pressed play and it stopped" UX (P2.14).
    {
        let media_store_w = media_store.clone();
        let mut rx = switch_orchestrator.state();
        let orch_w = switch_orchestrator.clone();
        tokio::spawn(async move {
            use vortex_l3_daemon::core::audio_orchestrator::SwitchState;
            let mut was_active = false;
            // MAC of the in-flight flow, captured the moment we
            // see any non-Idle state. Cleared when we consume it.
            let mut last_active_mac: Option<String> = None;
            loop {
                if rx.changed().await.is_err() {
                    break;
                }
                let s = rx.borrow().clone();
                let active = !matches!(s, SwitchState::Idle | SwitchState::Failed(_));
                if active {
                    // Cache the MAC while the orchestrator still
                    // holds it — `current_mac` is cleared on Idle,
                    // and we need it for the route wait below.
                    if let Some(m) = orch_w.current_mac().await {
                        last_active_mac = Some(m);
                    }
                }
                if was_active && !active {
                    // Flow just finished. If we paused MPRIS for a
                    // call, route migration + resume; otherwise
                    // skip — moving sink-inputs to a SUSPENDED
                    // bluez sink without a Play on the heels of
                    // it causes the player to pause itself, which
                    // is exactly the regression we're avoiding.
                    let store = media_store_w.clone();
                    let mac = last_active_mac.take();
                    tokio::spawn(async move {
                        let need_resume = store.read().await.is_paused();
                        if !need_resume {
                            tracing::debug!("no MPRIS pause record; skipping route/resume");
                            return;
                        }
                        if let Some(mac) = mac {
                            let outcome = vortex_l3_daemon::core::audio_route::wait_for_route(&mac).await;
                            tracing::info!(
                                sink = ?outcome.sink,
                                ready = outcome.ready,
                                routed = outcome.routed,
                                elapsed_ms = outcome.elapsed.as_millis() as u64,
                                "audio-route wait result"
                            );
                        }
                        let resumed = vortex_l3_daemon::core::media_runtime::resume_paused_for_call(&store).await;
                        if !resumed.is_empty() {
                            tracing::info!(?resumed, "media resumed after call");
                        }
                    });
                }
                was_active = active;
            }
        });
    }

    // Safety net for the resume above. That watcher hangs the resume off a
    // switch-flow EDGE, which silently assumes a flow always runs after a
    // call. It doesn't: the phone's post-call reclaim rides the one-shot
    // `audio_claim_request`, and when that never lands — BLE down, LAN beat
    // missed, or the claim deferred past its window — no flow starts, no
    // edge fires, and the media we paused for the call stays paused with
    // the buds sitting right here. (Observed 2026-08-24: six claims
    // dropped, one resume.) So poll the same preconditions directly.
    //
    // Deliberately conservative — it only ever un-sticks, never routes:
    //   * the call must be over on BOTH signals we have (`media_in_call`
    //     from AppState, and the locally-owned call pill),
    //   * the orchestrator must be settled, so a live flow keeps ownership
    //     of the resume and this can't double-fire against the edge above,
    //   * the buds must be ON THIS LAPTOP — resuming otherwise would blast
    //     the speakers, the exact regression the strict routing rule exists
    //     to prevent,
    //   * and all of that must hold for two consecutive polls, so the edge
    //     watcher always wins the normal path and we only step in when it
    //     genuinely never fired.
    // `resume_paused_for_call` brings its own 5-minute staleness TTL, so a
    // long call still declines to surprise the user with audio.
    {
        let store = media_store.clone();
        let in_call = media_in_call.clone();
        let adapter_n = adapter.clone();
        let orch_n = switch_orchestrator.clone();
        let peers_n = peer_store.clone();
        tokio::spawn(async move {
            use vortex_l3_daemon::core::audio_orchestrator::SwitchState;
            const POLL: std::time::Duration = std::time::Duration::from_secs(2);
            let mut stable = 0u8;
            // Consecutive polls where the call is over, our media is held, and
            // the buds are on neither device.
            let mut idle_polls = 0u8;
            loop {
                tokio::time::sleep(POLL).await;
                // Cheap synchronous gates first; the `Ref` from the watch
                // channel is dropped on this statement, never held across
                // the awaits below.
                let call_over = !in_call.load(std::sync::atomic::Ordering::Relaxed)
                    && !crate::call::call_pill_active();
                let settled = matches!(
                    *orch_n.state().borrow(),
                    SwitchState::Idle | SwitchState::Failed(_)
                );
                let eligible = if !(call_over && settled && store.read().await.is_paused()) {
                    false
                } else {
                    // Last, because it's the only gate that costs a D-Bus
                    // round-trip.
                    match vortex_l3_daemon::core::earbuds_store::load()
                        .and_then(|s| s.address.parse::<bluer::Address>().ok())
                    {
                        Some(addr) => {
                            vortex_l3_daemon::core::audio_switch::audio_active(&adapter_n, addr)
                                .await
                        }
                        None => false,
                    }
                };
                if !eligible {
                    // The call is over and our media is still held, but the
                    // buds are not here. Nobody is going to bring them: the
                    // phone's post-call hand-back is a single fire-and-forget
                    // Claim, and if the laptop was asleep or out of range when
                    // it went out, that flag is consumed and never re-sent. The
                    // buds sit on nobody and the media stays paused until the
                    // user reaches for the tray.
                    //
                    // We know which earbuds they are, so ask for them
                    // ourselves. `request` is a no-op unless the orchestrator is
                    // Idle, so this can never cut across a live flow.
                    if call_over && settled && store.read().await.is_paused() {
                        idle_polls = idle_polls.saturating_add(1);
                        // ~10s of the buds being on nobody before stepping in,
                        // so the normal hand-back always wins the race.
                        if idle_polls == 5 {
                            let peer = {
                                let ps = peers_n.clone();
                                tokio::task::spawn_blocking(move || {
                                    ps.list().unwrap_or_default().first().map(|p| p.peer_static_pub)
                                })
                                .await
                                .ok()
                                .flatten()
                            };
                            if let (Some(saved), Some(peer)) =
                                (vortex_l3_daemon::core::earbuds_store::load(), peer)
                            {
                                tracing::info!(
                                    "call over and the buds are on nobody — reclaiming them here"
                                );
                                let _ = orch_n.request(peer, saved.address.clone()).await;
                            }
                        }
                    } else {
                        idle_polls = 0;
                    }
                    stable = 0;
                    continue;
                }
                idle_polls = 0;
                stable = stable.saturating_add(1);
                if stable < 2 {
                    continue;
                }
                stable = 0;
                let resumed =
                    vortex_l3_daemon::core::media_runtime::resume_paused_for_call(&store).await;
                if !resumed.is_empty() {
                    tracing::info!(
                        ?resumed,
                        "media resumed after call (safety net — no switch flow ever fired)"
                    );
                }
            }
        });
    }

    // Bridge: forward orchestrator state transitions to the webview.
    {
        let app_state = app.clone();
        let mut rx = switch_orchestrator.state();
        tokio::spawn(async move {
            // Emit the initial value too — Vue subscribes early and
            // would otherwise miss the seed "idle" tick.
            let _ = app_state.emit("vortex:switch_state", switch_state_dto(&rx.borrow()));
            loop {
                if rx.changed().await.is_err() {
                    break;
                }
                let s = rx.borrow().clone();
                let _ = app_state.emit("vortex:switch_state", switch_state_dto(&s));
            }
        });
    }

    AudioSetup {
        session_writers,
        ble_audio_writers,
        switch_orchestrator,
        media_watch,
        media_in_call,
        media_store,
    }
}
