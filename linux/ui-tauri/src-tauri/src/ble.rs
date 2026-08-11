//! BLE reconnect: the persistent GATT link, RPA discovery, and the
//! direct-connect/scan fast path. Split out of lib.rs.

use std::sync::Arc;
use std::time::Duration;

use vortex_l3_daemon::core::ble::client::VortexClient;
use vortex_l3_daemon::core::ble::scanner::run_filtered_scan;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::pairing::reconnect::run_ik_initiator;
use vortex_l3_daemon::core::storage::peers::PeerStore;

use crate::NotifWriter;

/// Consecutive connect failures (phone present, but the connect aborts —
/// `le-connection-abort-by-local` / services-not-resolved) before we escalate
/// to a full adapter power-cycle. The per-cycle `remove_device` cleanup clears
/// stale RPA objects; only when that repeatedly fails to help do we assume the
/// controller itself is wedged and reset it. ~6 cycles ≈ a few seconds of dead
/// retries before the invisible self-heal kicks in.
const CONNECT_WEDGE_THRESHOLD: u32 = 6;

/// Early-wake for the BLE state heartbeat (the 12s loop inside the
/// persistent session). `notify_one` stores a permit, so a nudge that
/// fires while the heartbeat is mid-write still takes effect on the
/// next `notified().await` instead of being lost. Used by the
/// locked-hint watcher so the phone's lock icon updates in ~1s.
pub(crate) fn state_nudge() -> &'static tokio::sync::Notify {
    static NUDGE: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    NUDGE.get_or_init(tokio::sync::Notify::new)
}

/// Epoch-ms of the last PROOF the phone was nearby: a token-validated
/// trusted-presence advertisement or a live-session event. The proximity
/// watcher treats "no BLE session AND this stale" as the phone having
/// left (advertisements keep this fresh during RPA-churn reconnects, so
/// a flapping session alone never reads as absence).
pub(crate) static LAST_PRESENCE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn touch_presence() {
    LAST_PRESENCE_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Epoch-ms of the last AppState/frame received over ANY transport (BLE OR
/// LAN). Distinct from [`LAST_PRESENCE_MS`] (BLE-only, feeds proximity auto-
/// lock): this answers "are we in LIVE CONTACT with the phone right now?" and
/// is used to clear mirror pills (call / handoff) the instant we go fully
/// offline — their buttons (Accept / Mute / open) are dead without a link, so a
/// lingering pill is misleading. Touched by both the BLE STATE consumer and the
/// LAN heartbeat.
pub(crate) static LAST_PEER_CONTACT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn touch_peer_contact() {
    LAST_PEER_CONTACT_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Ms since we last heard from the phone over any transport (huge if never).
pub(crate) fn peer_contact_age_ms() -> u64 {
    let last = LAST_PEER_CONTACT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    now_ms().saturating_sub(last)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Find the first trusted-presence advertiser on-air whose 8-byte
/// presence token matches one of our trusted peers' current ±1 PRS
/// bucket. Used by the BLE persistent listener to locate the phone
/// when its random BD_ADDR has rotated since the last connection.
///
/// **PRS token validation (ChatGPT review #2).** Previously this just
/// matched the `TRUSTED_PRESENCE` flag bit, so any nearby device that
/// flipped bit-1 in our service-data payload could trick the loop
/// into burning a full IK connect cycle on a rogue address. Now we
/// pre-compute each trusted peer's expected token for the current
/// bucket and ±1 (clock-skew tolerance, spec §6.5.2) and only
/// accept addresses whose advertised token is in that set.
/// Mirror of `bin/ui.rs::PRESENCE_ROTATION_SEC`. Both binaries
/// MUST share this value with the Android publisher in spec §7.3.
const PRESENCE_ROTATION_SEC: u64 = 60;

/// The presence tokens we'd accept RIGHT NOW from any trusted peer: each
/// peer's PRS-derived token for the current bucket ±2. ±1 was the spec
/// minimum (spec §6.5.2) but left the phone unreachable over BLE when its
/// clock skewed / Doze deferred a rotation past one bucket; ±2 tolerates
/// 2–3 minutes of drift. Security-wise the token is a privacy/anti-DoS
/// filter, not authentication (IK still gates trust), so widening the
/// accept set from 3 to 5 of 2^64 values is immaterial. Recompute at every
/// validation — buckets rotate every 60 s, so a set captured at wait-start
/// goes stale.
pub(crate) fn expected_presence_tokens(
    peers: &[vortex_l3_daemon::core::storage::peers::TrustedPeer],
) -> std::collections::HashSet<[u8; 8]> {
    use std::time::{SystemTime, UNIX_EPOCH};
    use vortex_l3_daemon::core::crypto::presence::{current_bucket, derive_presence_token};
    let now_sec = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bucket_now = current_bucket(now_sec, PRESENCE_ROTATION_SEC);
    peers
        .iter()
        .flat_map(|p| {
            [-2i64, -1, 0, 1, 2]
                .iter()
                .map(move |d| derive_presence_token(&p.prs, (bucket_now as i64 + *d) as u64))
        })
        .collect()
}

pub(crate) async fn find_trusted_presence_peer(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    wait: Duration,
) -> Option<bluer::Address> {
    // Load trusted peers off the runtime — secret-service is blocking.
    let peers = {
        let store = peer_store.clone();
        tokio::task::spawn_blocking(move || store.list().unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    if peers.is_empty() {
        return None;
    }
    let expected = expected_presence_tokens(&peers);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<bluer::Address>(1);
    let scan = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            let _ = run_filtered_scan(adapter, move |c| {
                if !c.payload.flags.is_trusted_presence() {
                    return;
                }
                if !expected.contains(&c.payload.payload_8) {
                    // Flag set but token doesn't match any of our
                    // trusted peers' current ±1 bucket — could be a
                    // rogue advertiser trying to burn our IK budget,
                    // or a stale phone whose clock drifted out of
                    // the ±1 window. Drop either way.
                    return;
                }
                // Validated trusted-presence advert. try_send into a
                // bounded(1) channel: subsequent valid hits drop —
                // we only need the first.
                let _ = tx.try_send(c.address);
            })
            .await;
        })
    };
    let res = tokio::time::timeout(wait, rx.recv()).await.ok().flatten();
    // Abort AND join. `abort()` only *requests* cancellation; the spawned
    // task still owns the `discover_devices()` stream until its future is
    // actually dropped, and bluer only issues StopDiscovery on that drop.
    // Without awaiting the handle here, the next iteration raced a
    // still-alive discovery: the session never released, so the BlueZ
    // adapter stayed `Discovering` *continuously* (Auto transport ⇒ a
    // back-to-back BR/EDR Inquiry every ~10.24 s that hogs the radio), and
    // every connect was starved for ~10 s (confirmed via btmon + a
    // `DiscoveryActive` error on the second filter set). Awaiting the
    // aborted task drops the stream deterministically, so discovery stops
    // and the NEXT scan starts a fresh LE-only session (no inquiry).
    scan.abort();
    let _ = scan.await;
    if res.is_some() {
        touch_presence();
    }
    // Belt-and-braces: also wait until the adapter actually reports
    // not-discovering before the caller connects (StopDiscovery is async
    // over D-Bus). Connecting while still `Discovering` was the original
    // "Bluetooth operation in progress: In Progress" failure.
    if res.is_some() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !adapter.is_discovering().await.unwrap_or(false) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!("presence scan: adapter still discovering after 2s; connecting anyway");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    res
}

/// Set when BlueZ rejects advertisement-monitor registration (old daemon /
/// controller without passive-scan support) so we stop re-trying it and log
/// the downgrade exactly once.
static MONITOR_UNSUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What ended one presence wait.
enum PresenceWait {
    /// A validated trusted-presence advertiser is on air at this address.
    Found(bluer::Address),
    /// No find, but the caller should re-evaluate (LAN saw the phone /
    /// periodic trust re-check) and call again.
    Reevaluate,
    /// The monitor path is unavailable — caller falls back to scanning.
    Unsupported,
}

/// Read the candidate's Vortex service data from the BlueZ cache and check
/// it's a trusted-presence advert whose token matches a trusted peer ±2
/// buckets. The monitor pattern only matched "some Vortex advertiser" — this
/// is the same anti-rogue gate the scan path applies.
async fn validate_presence_candidate(
    adapter: &bluer::Adapter,
    peers: &[vortex_l3_daemon::core::storage::peers::TrustedPeer],
    addr: bluer::Address,
) -> bool {
    use vortex_l3_daemon::core::ble::{AdvPayload, VORTEX_SERVICE_UUID};
    let Ok(device) = adapter.device(addr) else { return false };
    let Ok(Some(sd)) = device.service_data().await else { return false };
    let Some(bytes) = sd.get(&VORTEX_SERVICE_UUID) else { return false };
    let Ok(payload) = AdvPayload::decode(bytes) else { return false };
    payload.flags.is_trusted_presence()
        && expected_presence_tokens(peers).contains(&payload.payload_8)
}

/// Wait until the trusted phone is on-air, the seamless-continuity way: a BlueZ
/// advertisement monitor (or-pattern on our service-data AD) does the
/// watching — BlueZ/the controller filters passively and wakes us with a
/// DeviceFound event, so there is NO active-scan duty cycle while the phone
/// is away. Falls back to [`monitor_unsupported_wait`]'s adaptive scan loop
/// when the monitor API is unavailable.
async fn monitor_presence_wait(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    peers: &[vortex_l3_daemon::core::storage::peers::TrustedPeer],
    retry_nudge: &tokio::sync::Notify,
) -> PresenceWait {
    use bluer::monitor::{Monitor, MonitorEvent, Pattern, RssiSamplingPeriod, Type};
    use futures::StreamExt;

    // AD type 0x21 = Service Data, 128-bit UUID: the field starts with the
    // UUID in little-endian, then our 10-byte AdvPayload. Matching just the
    // UUID prefix catches every Vortex advertiser; the token gate above
    // rejects foreign/stale ones.
    const AD_SERVICE_DATA_128: u8 = 0x21;
    let uuid_le: Vec<u8> = vortex_l3_daemon::core::ble::VORTEX_SERVICE_UUID
        .as_bytes()
        .iter()
        .rev()
        .copied()
        .collect();

    let manager = match adapter.monitor().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("advertisement monitor unavailable ({e}); using scan fallback");
            return PresenceWait::Unsupported;
        }
    };
    let mut handle = match manager
        .register(Monitor {
            monitor_type: Type::OrPatterns,
            patterns: Some(vec![Pattern::new(AD_SERVICE_DATA_128, 0, &uuid_le)]),
            // First-sighting-only: we just need the wake-up; once connected
            // the persistent GATT link takes over.
            rssi_sampling_period: Some(RssiSamplingPeriod::First),
            ..Default::default()
        })
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("advertisement monitor rejected ({e}); using scan fallback");
            return PresenceWait::Unsupported;
        }
    };
    tracing::info!("advertisement monitor armed (passive presence watch)");

    loop {
        tokio::select! {
            ev = handle.next() => match ev {
                Some(MonitorEvent::DeviceFound(id)) => {
                    if validate_presence_candidate(adapter, peers, id.device).await {
                        tracing::info!(addr = %id.device, "presence monitor: trusted peer on air");
                        touch_presence();
                        return PresenceWait::Found(id.device);
                    }
                    // A Vortex advertiser that didn't validate: usually the
                    // BlueZ service-data cache lagging a token rotation (the
                    // monitor only re-fires after lost→found). One active
                    // scan round fetches FRESH adv data; if it's genuinely a
                    // foreign device the scan rejects it too and we keep
                    // waiting on the monitor.
                    tracing::debug!(addr = %id.device, "presence monitor: candidate failed token gate; one scan round");
                    if let Some(a) =
                        find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
                    {
                        return PresenceWait::Found(a);
                    }
                }
                Some(MonitorEvent::DeviceLost(_)) => {}
                Some(_) => {}
                None => {
                    // Monitor stream ended (bluetoothd restart?) — re-arm via
                    // the caller; if it keeps failing the Unsupported latch
                    // moves us to the scan fallback.
                    tracing::warn!("advertisement monitor stream ended; re-evaluating");
                    return PresenceWait::Reevaluate;
                }
            },
            _ = retry_nudge.notified() => {
                tracing::info!("presence wait: woken by LAN cross-transport nudge");
                return PresenceWait::Reevaluate;
            }
            // Periodic re-evaluation: trust may have been revoked, and the
            // outer loop re-checks the switch-orchestrator gate too.
            _ = tokio::time::sleep(Duration::from_secs(300)) => return PresenceWait::Reevaluate,
        }
    }
}

/// Scan-loop fallback for hosts without the advertisement-monitor API: the
/// old 15 s filtered scan, but with an adaptive sleep between rounds
/// (5→10→20→45 s) so a phone that's away for hours doesn't keep the radio
/// ~65% busy. A LAN nudge or the ~5 min re-evaluation cap ends the wait so
/// the caller can re-check trust and the direct-connect fast path.
async fn monitor_unsupported_wait(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    retry_nudge: &tokio::sync::Notify,
) -> PresenceWait {
    let started = tokio::time::Instant::now();
    let mut backoff = Duration::from_secs(5);
    loop {
        if let Some(a) =
            find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
        {
            return PresenceWait::Found(a);
        }
        if started.elapsed() > Duration::from_secs(300) {
            return PresenceWait::Reevaluate;
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = retry_nudge.notified() => {
                tracing::info!("presence wait (scan): woken by LAN cross-transport nudge");
                return PresenceWait::Reevaluate;
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(45));
    }
}

/// Block until a trusted phone is discoverable, returning its current RPA —
/// or `None` when the caller should loop (re-check trust, retry the direct
/// connect after a LAN nudge, etc.). Monitor-first with a sticky downgrade
/// to the adaptive scan loop.
pub(crate) async fn wait_for_presence(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    retry_nudge: &tokio::sync::Notify,
) -> Option<bluer::Address> {
    use std::sync::atomic::Ordering;
    // Trust snapshot for this wait (token sets are recomputed per event; the
    // 5-minute re-evaluation refreshes the peer list itself).
    let peers = {
        let store = peer_store.clone();
        tokio::task::spawn_blocking(move || store.list().unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    if peers.is_empty() {
        tokio::time::sleep(Duration::from_secs(10)).await;
        return None;
    }
    // Phase 1 — BURST: one active scan round first. Right after a drop the
    // phone is usually still advertising next to us, and an active scan
    // finds it in ~1-3 s — whereas the passive monitor rides the kernel's
    // duty-cycled background scan and (live-measured) can take ~30 s to see
    // a rotated RPA. Burst keeps reconnect at the old speed; the monitor
    // below is for the patient zero-cost watch when the phone is genuinely
    // away. Worst-case coverage bonus: every ~5 min re-evaluation passes
    // through here, so even a missed monitor event self-heals (15 s scan
    // per ~5 min ≈ 5% duty vs the old always-on ~65%).
    if let Some(a) =
        find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
    {
        return Some(a);
    }
    // Phase 2 — patient watch.
    if !MONITOR_UNSUPPORTED.load(Ordering::Relaxed) {
        match monitor_presence_wait(adapter, peer_store, &peers, retry_nudge).await {
            PresenceWait::Found(a) => return Some(a),
            PresenceWait::Reevaluate => return None,
            PresenceWait::Unsupported => {
                MONITOR_UNSUPPORTED.store(true, Ordering::Relaxed);
            }
        }
    }
    match monitor_unsupported_wait(adapter, peer_store, retry_nudge).await {
        PresenceWait::Found(a) => Some(a),
        _ => None,
    }
}

/// Open a GATT session to the trusted phone, preferring a direct
/// connect to the BT-bonded identity address over a presence scan.
///
/// Once `do_pair` has captured the bonded identity (BlueZ resolved the
/// rotating RPA at bond time), we don't need to scan on every reconnect
/// — the kernel has the IRK and BlueZ can route an LE connect request
/// to whichever RPA the phone is currently advertising. Skipping the
/// scan is the *whole point* of bonding: it removes the SCAN+A2DP
/// radio conflict that makes the BLE link flaky while audio is
/// streaming to the earbuds.
///
/// Returns `None` when both paths are exhausted (no bonded entry that
/// connects + no presence advert on air); the caller should back off
/// briefly and retry.
pub(crate) async fn connect_bonded_or_scan(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    last_rpa: &mut Option<bluer::Address>,
    retry_nudge: &tokio::sync::Notify,
    // Running count of consecutive *connect attempts that failed* (the
    // abort-by-local / services-not-resolved BlueZ wedge). Bumped only when a
    // connect was actually tried and failed — NOT when the phone is simply
    // absent (no presence to connect to) — so the caller's power-cycle escalation
    // never fires just because the phone walked away. Reset to 0 on any success.
    consec_connect_fail: &mut u32,
) -> Option<VortexClient> {
    // ----- Learn-latest-RPA fast path -----
    //
    // Most BLE link drops aren't from the phone rotating its RPA — they're
    // from A2DP starving the radio, a brief range blip, or our own teardown.
    // In all those cases the phone is STILL advertising/connectable at the
    // same RPA we last used. So before paying for a 15 s presence-scan, try a
    // direct connect to the last-known-good RPA. When it works (the common
    // case) reconnect is sub-second instead of scan + connect. When the phone
    // really did rotate, this fails fast (5 s cap) and we fall through to the
    // scan, which then refreshes `last_rpa`. This is the bondless reconnect
    // model the old project shipped stably (learn-latest-RPA + persistent link).
    if let Some(addr) = *last_rpa {
        match tokio::time::timeout(
            // 3s, not 5s: a present RPA connects at the LE layer in <1-2s
            // (even in Doze — the controller answers, not the app), so this
            // only ever burns when the RPA is GONE (rotated). Shorter = faster
            // fall-through to the scan on rotation, WITHOUT adding scans (the
            // scan happens either way) — so no extra battery; the 3s margin
            // keeps a slow-but-healthy connect from triggering a spurious scan,
            // which IS the battery-expensive path.
            Duration::from_secs(3),
            VortexClient::connect(adapter, addr),
        )
        .await
        {
            Ok(Ok(client)) => {
                tracing::info!(addr = %addr, "BLE persistent: last-RPA direct-connect succeeded");
                *consec_connect_fail = 0;
                return Some(client);
            }
            Ok(Err(e)) => {
                tracing::debug!(addr = %addr, "BLE persistent: last-RPA connect failed: {e}; scanning");
                clear_pending_connect(adapter, addr).await;
                forget_stale_device(adapter, addr).await;
            }
            Err(_) => {
                tracing::debug!(addr = %addr, "BLE persistent: last-RPA connect timed out; scanning");
                // A tokio timeout drops the future but BlueZ keeps the connect
                // attempt in flight — without this the next connect to a
                // different RPA fails "In Progress". Cancel it explicitly.
                clear_pending_connect(adapter, addr).await;
                forget_stale_device(adapter, addr).await;
            }
        }
    }
    // ----- Presence wait -----
    // Block until the phone's trusted-presence beacon is on air (passive
    // advertisement monitor; adaptive scan loop on hosts without it), then
    // connect to its current RPA and remember it for the fast path above.
    let addr = match wait_for_presence(adapter, peer_store, retry_nudge).await {
        Some(a) => a,
        None => {
            // Re-evaluate signal (LAN nudge / periodic) — the caller loops,
            // which re-runs the direct-connect fast path first.
            return None;
        }
    };
    match VortexClient::connect(adapter, addr).await {
        Ok(client) => {
            *last_rpa = Some(addr);
            *consec_connect_fail = 0;
            Some(client)
        }
        Err(e) => {
            // Stale cache or a flaky connect — drop the remembered RPA so the
            // next pass scans fresh rather than retrying a dead address.
            if *last_rpa == Some(addr) {
                *last_rpa = None;
            }
            clear_pending_connect(adapter, addr).await;
            forget_stale_device(adapter, addr).await;
            // The phone WAS present (we got here past wait_for_presence) but the
            // connect failed — this is the wedge signal the caller escalates on.
            *consec_connect_fail = consec_connect_fail.saturating_add(1);
            tracing::warn!("P2.13: BLE connect to {addr} ({consec_connect_fail}x): {e}");
            None
        }
    }
}

/// Last-resort BlueZ self-heal: power-cycle the adapter to clear a wedged
/// controller / stale RPA device objects after repeated connect failures
/// (`le-connection-abort-by-local`, services-not-resolved). This is exactly the
/// manual "restart Bluetooth" that unwedges it — done invisibly so the user
/// never sees the link as broken. Linux lets us toggle power with no prompt.
/// Heavy hammer (it momentarily drops other BLE devices), so the caller only
/// invokes it after [`CONNECT_WEDGE_THRESHOLD`] consecutive failures, which the
/// natural per-cycle `remove_device` cleanup never resolved.
async fn power_cycle_adapter(adapter: &bluer::Adapter) {
    tracing::warn!("BLE: repeated connect failures — power-cycling adapter to clear the stack");
    if let Err(e) = adapter.set_powered(false).await {
        tracing::warn!("BLE power-cycle: set_powered(false) failed: {e}");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    match adapter.set_powered(true).await {
        Ok(()) => tracing::info!("BLE: adapter power-cycled — retrying reconnect"),
        Err(e) => tracing::warn!("BLE power-cycle: set_powered(true) failed: {e}"),
    }
    // Let BlueZ re-init the controller before the next scan/connect.
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// Drop a failed RPA's device entry from BlueZ entirely. Without this the
/// next discovery round re-serves the DEAD address from the adapter's
/// advertisement cache (its stale token still validates — the ±2-bucket
/// tolerance spans ~3 min), so the loop burned two+ 15s connect timeouts
/// on a rotated-away RPA before a genuinely fresh sighting could win
/// (live-observed: 67s walk-up reconnect, the user typed their password
/// long before the eager unlock could fire). RPA entries are transient by
/// nature — removing one can't lose anything durable.
async fn forget_stale_device(adapter: &bluer::Adapter, addr: bluer::Address) {
    match tokio::time::timeout(Duration::from_secs(3), adapter.remove_device(addr)).await {
        Ok(Ok(())) => tracing::debug!(addr = %addr, "stale RPA entry removed from BlueZ"),
        Ok(Err(e)) => tracing::debug!(addr = %addr, "remove_device: {e} (ignored)"),
        Err(_) => tracing::debug!(addr = %addr, "remove_device timed out (ignored)"),
    }
}

/// Cancel a connect attempt that failed/timed out so BlueZ doesn't keep it
/// "in progress". A `tokio::time::timeout` around `device.connect()` drops the
/// future but the underlying BlueZ connection attempt lives on server-side; the
/// next connect to any RPA then fails with "operation in progress: In Progress".
/// Calling `disconnect()` clears that pending state. Best-effort + bounded.
pub(crate) async fn clear_pending_connect(adapter: &bluer::Adapter, addr: bluer::Address) {
    if let Ok(dev) = adapter.device(addr) {
        let _ = tokio::time::timeout(Duration::from_secs(3), dev.disconnect()).await;
    }
}

/// Persistent BLE link to the trusted phone (P2.13).
///
/// Loops forever:
///   1. Open a GATT session via [`connect_bonded_or_scan`] — direct
///      connect to the BT-bonded identity if we have one, otherwise
///      presence-scan + connect to the phone's current RPA.
///   2. Run the IK initiator over that session.
///   3. Hand the resulting Noise transport state to
///      [`audio_signal::run_listener`] which subscribes to the
///      AUDIO_SIGNAL characteristic and dispatches incoming AUDIO_OP
///      frames straight into the [`SwitchOrchestrator`].
///   4. When the listener returns (BLE disconnect / GATT error), back
///      off briefly and start again.
///
/// The loop is fully independent of the LAN reconnect — they read the
/// same trust record but each owns its own transport. The BLE path is
/// what gives us ~200 ms call-handoff vs. the 5–12 s LAN heartbeat.
pub(crate) async fn run_ble_persistent_loop(
    adapter: bluer::Adapter,
    identity: IdentityRecord,
    peer_store: Arc<dyn PeerStore>,
    switch_orchestrator: Arc<vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator>,
    media_store: vortex_l3_daemon::core::media_runtime::MediaStateStore,
    ble_audio_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
    state_tx: tokio::sync::mpsc::UnboundedSender<(
        [u8; 32],
        vortex_l3_daemon::core::appstate::AppState,
    )>,
    notif_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::notif_mirror::NotificationMirror,
    >,
    live_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::live_activity::LiveActivity,
    >,
    icon_tx: tokio::sync::mpsc::UnboundedSender<(String, u16, u16, Vec<u8>)>,
    call_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::call_event::CallEvent,
    >,
    contacts_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    call_log_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    sms_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    sms_thread_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    clipboard_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::clipboard_mirror::ClipboardMirror,
    >,
    clipboard_image_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16, Vec<u8>)>,
    clipboard_offer_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::clipboard_mirror::ClipboardImageOffer,
    >,
    handoff_tx: tokio::sync::mpsc::UnboundedSender<
        vortex_l3_daemon::core::handoff::HandoffEvent,
    >,
    // Generic additive-frame channel (e.g. NOTES_SYNC) — the listener forwards
    // (frame_ty, payload) here; the owning feature module filters + handles it.
    raw_frame_tx: tokio::sync::mpsc::UnboundedSender<(u8, Vec<u8>)>,
    notif_writer: Arc<tokio::sync::Mutex<Option<NotifWriter>>>,
    clipboard_writer: Arc<tokio::sync::Mutex<Option<crate::ClipboardWriter>>>,
    clipboard_image_writer: Arc<tokio::sync::Mutex<Option<crate::ClipboardImageWriter>>>,
    call_writer: Arc<tokio::sync::Mutex<Option<crate::CallWriter>>>,
    // Generic laptop→phone sealed-frame writer; filled on connect, used by any
    // feature (e.g. notes) to send a frame without its own transport plumbing.
    sealed_writer: Arc<tokio::sync::Mutex<Option<crate::SealedWriter>>>,
    retry_nudge: Arc<tokio::sync::Notify>,
) {
    use vortex_l3_daemon::core::ble::audio_signal;
    use vortex_l3_daemon::core::audio_lan_session::SessionWriter;
    use vortex_l3_daemon::core::audio_op::AudioOpFrame;
    // Last RPA we successfully connected to — the learn-latest-RPA fast path
    // (see connect_bonded_or_scan) retries it directly before scanning.
    let mut last_rpa: Option<bluer::Address> = None;
    // Consecutive IK handshake failures against a reachable phone — e.g. the
    // phone dropped its trust record (user un-paired there). Escalating
    // backoff so we don't hammer connect+IK every few seconds forever.
    let mut consec_ik_fail: u32 = 0;
    // Consecutive connect failures against a present phone — drives the
    // adapter power-cycle self-heal (see CONNECT_WEDGE_THRESHOLD).
    let mut consec_connect_fail: u32 = 0;
    loop {
        // Need a trusted peer record to authenticate against.
        // Same blocking-pool offload as the LAN heartbeat: libsecret
        // calls block their executor thread, and this loop ran on
        // every restart, doubling the contention.
        let peer = {
            let store = peer_store.clone();
            let first = tokio::task::spawn_blocking(move || {
                store.list().unwrap_or_default().into_iter().next()
            })
            .await
            .unwrap_or(None);
            match first {
                Some(p) => p,
                None => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            }
        };

        // Self-heal: power the adapter on if it's off (soft rfkill / user
        // toggle / post-suspend). Linux allows this programmatically with no
        // prompt — unlike Android 13+, where only the user may re-enable
        // Bluetooth. Best-effort: a hard rfkill block still wins, so back
        // off and wait for a nudge instead of spinning.
        if !adapter.is_powered().await.unwrap_or(false) {
            match adapter.set_powered(true).await {
                Ok(()) => tracing::info!("BLE adapter was off — powered it on for reconnect"),
                Err(e) => {
                    tracing::warn!("BLE adapter off and power-on failed ({e}); waiting");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                        _ = retry_nudge.notified() => {}
                    }
                    continue;
                }
            }
        }

        // Don't contend the single BT radio with an in-flight A2DP switch.
        // A GATT (re)connect to the phone's rotated RPA can take up to ~9s,
        // and connect_audio's A2DP connect shares the same controller — run
        // them at once and the A2DP leg wedges, stalling the whole switch
        // (the "return is very slow / didn't switch at all" report). Defer the
        // GATT (re)connect until the orchestrator is back to Idle. Bounded so
        // a permanently-stuck flow can't starve the call-signal channel.
        {
            use vortex_l3_daemon::core::audio_orchestrator::SwitchState;
            let mut waited_ms = 0u64;
            while *switch_orchestrator.state().borrow() != SwitchState::Idle
                && waited_ms < 20_000
            {
                tokio::time::sleep(Duration::from_millis(400)).await;
                waited_ms += 400;
            }
        }

        // Step 1+2 — open a GATT session. Tries a direct connect to the
        // last-known-good RPA first (no scan, no SCAN+A2DP radio conflict),
        // falling back to presence-scan + RPA connect when the phone rotated.
        let client = match connect_bonded_or_scan(
            &adapter,
            &peer_store,
            &mut last_rpa,
            &retry_nudge,
            &mut consec_connect_fail,
        )
        .await
        {
            Some(c) => c,
            None => {
                // Invisible self-heal: the phone is present but connects keep
                // aborting (BlueZ RPA-churn wedge) — power-cycle the adapter to
                // clear the controller, exactly the manual "restart Bluetooth"
                // that fixes it. Only after the per-cycle remove_device cleanup
                // has failed CONNECT_WEDGE_THRESHOLD times in a row, so a merely
                // absent phone (which never bumps the counter) won't trigger it.
                if consec_connect_fail >= CONNECT_WEDGE_THRESHOLD {
                    power_cycle_adapter(&adapter).await;
                    consec_connect_fail = 0;
                    last_rpa = None;
                    continue;
                }
                // wait_for_presence already absorbed the long wait (or a
                // nudge asked for an immediate re-evaluation) — only a short
                // settle here before the loop re-runs the fast path.
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if client.audio_signal.is_none() {
            tracing::warn!(
                "P2.13: peer has no AUDIO_SIGNAL characteristic — older phone build?"
            );
            // Drop client, retry slowly — eventual app update on the phone
            // will fix this. No point hammering the link in the meantime.
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }

        // Step 3 — IK initiator. The phone holds the same trusted record
        // and will return the matching counter in msg2.
        // Blocking-pool offload (see run_worker comment).
        let local_counter = {
            let store = peer_store.clone();
            let peer_pub = peer.peer_static_pub;
            tokio::task::spawn_blocking(move || {
                store.load_counter(&peer_pub).unwrap_or(0)
            })
            .await
            .unwrap_or(0)
        };
        tracing::info!("P2.13: BLE IK starting");
        let outcome = match run_ik_initiator(
            &client,
            &identity.static_priv.0,
            &peer.peer_static_pub,
            &peer.prs,
            local_counter,
            Duration::from_secs(10),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                consec_ik_fail = consec_ik_fail.saturating_add(1);
                let backoff = [3u64, 10, 30, 60][consec_ik_fail.min(4) as usize - 1];
                tracing::warn!(
                    "P2.13: BLE IK failed ({consec_ik_fail}x): {e}; backing off {backoff}s"
                );
                // A LAN nudge ends the backoff early — the phone just (re)appeared,
                // a re-pair may have fixed the trust mismatch.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                    _ = retry_nudge.notified() => {}
                }
                continue;
            }
        };
        consec_ik_fail = 0;
        tracing::info!("P2.13: BLE IK returned; peer_counter={}", outcome.peer_counter);

        let Some(transport) = outcome.transport else {
            tracing::error!("P2.13: IK outcome missing transport state — internal bug");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let transport = Arc::new(tokio::sync::Mutex::new(transport));
        tracing::info!(
            peer = %hex::encode(&peer.peer_static_pub[..4]),
            "P2.13: BLE audio-signal session established"
        );
        touch_presence();

        // Bump counter off the hot path — D-Bus to libsecret can stall
        // for hundreds of ms when contended with the BLE adapter's
        // D-Bus traffic, and we don't want that to delay the listener
        // subscription that the phone is waiting for.
        let counter_store = peer_store.clone();
        let counter_peer = peer.peer_static_pub;
        let counter_value = outcome.peer_counter;
        tokio::spawn(async move {
            let _ = counter_store.bump_counter(&counter_peer, counter_value);
        });

        // Step 4 — register a BLE-write writer for the orchestrator
        // sender chain (review #4 fallback) so Approve / Released /
        // Done frames can ride the same BLE link the listener uses
        // when no LAN session is open. We wrap `client` in an `Arc`
        // so the writer closure can live independently of the
        // listener-loop call — the listener still borrows `&client`
        // because it needs the `audio_signal` characteristic for
        // subscription, but bluer characteristics are reference-
        // counted under the hood so the writer's clone is safe.
        let client_arc = Arc::new(client);
        let writer_transport = transport.clone();
        let writer_client = client_arc.clone();
        let writer_fn: SessionWriter = Arc::new(move |frame: AudioOpFrame| {
            let transport = writer_transport.clone();
            let client = writer_client.clone();
            Box::pin(async move {
                audio_signal::write_audio_op(&client, transport, frame).await
            })
        });
        {
            let mut m = ble_audio_writers.lock().await;
            m.insert(peer.peer_static_pub, writer_fn);
        }
        // Wake the proximity watcher NOW — link-up is its eager-unlock
        // trigger; waiting out its 2s sampling tick is wasted unlock time.
        crate::proximity::nudge().notify_one();

        // Publish the laptop→phone notification writer for this live link so
        // the capture consumer can push desktop notifications to the phone.
        {
            let nw_transport = transport.clone();
            let nw_client = client_arc.clone();
            let writer: NotifWriter = Arc::new(move |notif| {
                let transport = nw_transport.clone();
                let client = nw_client.clone();
                Box::pin(async move {
                    audio_signal::write_notification(&client, transport, &notif).await
                })
            });
            *notif_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone clipboard writer for this live link so the
        // clipboard sync consumer can push copied text to the phone.
        {
            let cw_transport = transport.clone();
            let cw_client = client_arc.clone();
            let writer: crate::ClipboardWriter = Arc::new(move |clip| {
                let transport = cw_transport.clone();
                let client = cw_client.clone();
                Box::pin(async move {
                    audio_signal::write_clipboard(&client, transport, &clip).await
                })
            });
            *clipboard_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone clipboard IMAGE writer (chunked) for this
        // live link so the sync consumer can push a copied image to the phone.
        {
            let cw_transport = transport.clone();
            let cw_client = client_arc.clone();
            let writer: crate::ClipboardImageWriter = Arc::new(move |png| {
                let transport = cw_transport.clone();
                let client = cw_client.clone();
                Box::pin(async move {
                    audio_signal::write_clipboard_image(&client, transport, &png).await
                })
            });
            *clipboard_image_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone call-control writer for this live link so
        // the call-banner consumer can answer/decline/end/mute via BLE.
        {
            let cw_transport = transport.clone();
            let cw_client = client_arc.clone();
            let writer: crate::CallWriter = Arc::new(move |ctrl| {
                let transport = cw_transport.clone();
                let client = cw_client.clone();
                Box::pin(async move {
                    audio_signal::write_call_control(&client, transport, &ctrl).await
                })
            });
            *call_writer.lock().await = Some(writer);
        }

        // Publish the GENERIC sealed-frame writer for this live link so any
        // feature (e.g. notes) can send a `(ty, payload)` frame to the phone.
        {
            let sw_transport = transport.clone();
            let sw_client = client_arc.clone();
            let writer: crate::SealedWriter = Arc::new(move |ty, payload| {
                let transport = sw_transport.clone();
                let client = sw_client.clone();
                Box::pin(async move {
                    audio_signal::write_sealed(&client, transport, ty, &payload).await
                })
            });
            *sealed_writer.lock().await = Some(writer);
        }

        // Heartbeat our app-state to the phone over BLE while the link is up,
        // so the phone shows us CONNECTED (battery/charging) and stays fresh
        // even when Wi-Fi blocks device-to-device traffic (AP isolation) and
        // the LAN heartbeat can never complete. The phone marks us disconnected
        // if it stops hearing from us, so a one-shot push on connect isn't
        // enough — it must repeat. Exits when the session drops (write fails);
        // the next session re-arms it. Symmetric to the phone's repeated pushes.
        {
            let st_client = client_arc.clone();
            let st_transport = transport.clone();
            let st_adapter = adapter.clone();
            tokio::spawn(async move {
                // Let the phone register its receive cipher first (it does so on
                // the IK-reconnect callback). Writing frame #0 before that, the
                // phone drops it (no cipher yet) and its Noise recv-nonce never
                // advances → every later frame fails "AEAD open failed" and the
                // phone shows us disconnected over BLE. A short delay closes that
                // startup race.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let mut first = true;
                // A single STATE write can fail TRANSIENTLY just after the link
                // comes up (BlueZ "Failed to initiate write" — GATT not ready)
                // or under contention. Tolerate a few in a row before declaring
                // the session gone — see the write-error arm below.
                const STATE_PUSH_MAX_FAILS: u32 = 6;
                let mut consecutive_fail: u32 = 0;
                loop {
                    let mut state = vortex_l3_daemon::core::appstate::AppState::now_laptop();
                    // Attach the laptop's currently-connected earbuds so the
                    // phone's UI shows them over BLE too — now_laptop() leaves
                    // this None, and on a BLE-only link (no LAN heartbeat) that
                    // made the phone show the buds DISCONNECTED even while they
                    // were live on the laptop. Reuse the loop's adapter (a fresh
                    // bluer::Session per tick leaks D-Bus connections).
                    state.earbuds =
                        vortex_l3_daemon::core::earbuds::scan_local_earbuds(&st_adapter).await;
                    // Lock-screen state for the phone's remote-lock button
                    // (logind LockedHint; one D-Bus property read per beat).
                    state.locked =
                        vortex_l3_daemon::core::session_lock::locked_hint().await;
                    // Laptop→phone screen-cast offer (where to dial + key) while
                    // we're casting; None otherwise.
                    state.laptop_cast = crate::laptop_cast::current_offer();
                    state.laptop_cast_error = crate::laptop_cast::current_error();
                    // Continuity Camera: ask the phone for its camera as a webcam.
                    state.camera_req = crate::camera::camera_wanted();
                    state.camera_facing = crate::camera::camera_facing();
                    // Find-My: the "ring my phone" request (unix-millis of last tap).
                    state.ring_seq = crate::ring::ring_seq();
                    // Now-playing snapshot for the phone's laptop-media
                    // notification — must ride the BLE STATE path too so the
                    // notification works on a BLE-only link (AP isolation).
                    crate::media_remote::fill_now_playing(&mut state).await;
                    match audio_signal::write_state(&st_client, st_transport.clone(), &state).await
                    {
                        Ok(()) => {
                            consecutive_fail = 0;
                            // A successful write proves the phone is in range —
                            // keeps the proximity watcher's presence fresh while
                            // connected (the advertisement monitor only runs
                            // between sessions).
                            touch_presence();
                            // It ALSO proves the BLE link is live, so it counts
                            // as peer contact — the liveness signal that gates
                            // the disconnect-clear of mirror pills. Without this,
                            // a BLE-only call (phone sends nothing over BLE while
                            // the call is up, and LAN can't complete under AP
                            // isolation) starved peer_contact past the 35s
                            // threshold → the call/handoff pill was falsely
                            // cleared and flickered every ~30s. This 12s beat
                            // keeps it fresh whenever the link is genuinely up.
                            touch_peer_contact();
                            if first {
                                tracing::info!(
                                    earbuds = ?state.earbuds,
                                    "→ BLE state heartbeat to phone (keeps laptop connected over BLE)"
                                );
                                first = false;
                            }
                        }
                        Err(e) => {
                            // Don't kill the heartbeat on ONE failed write —
                            // that used to leave a BLE-ONLY phone (no LAN) showing
                            // us DISCONNECTED forever after a transient first-write
                            // race. Retry on a short delay; only give up once the
                            // link is genuinely gone (run_listener also returns on
                            // a real disconnect and tears the session down).
                            consecutive_fail += 1;
                            if consecutive_fail >= STATE_PUSH_MAX_FAILS {
                                tracing::info!(
                                    "BLE state heartbeat stopped (session gone after {consecutive_fail} failed writes): {e}"
                                );
                                // The link is genuinely dead (N consecutive
                                // "Not connected" writes). DON'T wait for the
                                // notify-listener's stream to close on its own —
                                // that rides the BLE supervision timeout (~15-20s
                                // of dead air before the reconnect scan even
                                // starts; live-measured a ~36s total reconnect).
                                // Proactively drop the stale device so BlueZ
                                // invalidates the characteristic, the listener
                                // returns NOW, and the scan begins immediately.
                                // remove_device is idempotent with the listener's
                                // own cleanup (the loser just gets "Does Not
                                // Exist"). Cuts reconnect to ~5s on a real drop.
                                let _ = st_adapter.remove_device(st_client.address).await;
                                break;
                            }
                            tracing::debug!("BLE state write failed (#{consecutive_fail}); retrying: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                    }
                    // Early-wake: the locked-hint watcher (lan.rs) nudges us
                    // when the lock screen flips, so the phone's lock icon
                    // updates in ~1s instead of waiting out the 12s beat.
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(12)) => {}
                        _ = state_nudge().notified() => {}
                    }
                }
            });
        }

        // Event-driven drop detection. Watch the device's BlueZ `Connected`
        // property and force the teardown THE INSTANT it flips false — don't
        // wait for the 12 s heartbeat's next failed write, nor the ~20 s BLE
        // supervision timeout on the notify stream (live-measured: that gap was
        // ~17 s of dead air before reconnect even started). Zero extra radio
        // traffic — BlueZ already tracks Connected — so no battery cost. Calling
        // remove_device closes the listener's notify stream → run_listener
        // returns → the reconnect scan begins. The heartbeat's own 6-fail
        // teardown stays as a backstop for the rare race where the drop lands
        // between this subscribe and the event arriving. With this, a
        // glitch/brief drop reconnects in ~4 s instead of ~16 s.
        {
            let cw_adapter = adapter.clone();
            let cw_addr = client_arc.address;
            tokio::spawn(async move {
                use futures::StreamExt;
                let dev = match cw_adapter.device(cw_addr) {
                    Ok(d) => d,
                    Err(_) => return,
                };
                let mut events = match dev.events().await {
                    Ok(e) => e,
                    Err(_) => return,
                };
                while let Some(ev) = events.next().await {
                    if let bluer::DeviceEvent::PropertyChanged(
                        bluer::DeviceProperty::Connected(false),
                    ) = ev
                    {
                        tracing::info!(
                            addr = %cw_addr,
                            "BLE Connected→false (event) — forcing teardown for instant reconnect"
                        );
                        let _ = cw_adapter.remove_device(cw_addr).await;
                        break;
                    }
                }
                // Stream ended (device removed / reconnect) → task exits; the
                // next session spawns a fresh watcher.
            });
        }

        // Step 5 — subscribe + dispatch. Returns on disconnect.
        let _ = audio_signal::run_listener(
            &client_arc,
            transport,
            peer.peer_static_pub,
            switch_orchestrator.clone(),
            media_store.clone(),
            Some(state_tx.clone()),
            Some(notif_tx.clone()),
            Some(live_tx.clone()),
            Some(icon_tx.clone()),
            Some(call_tx.clone()),
            Some(contacts_tx.clone()),
            Some(call_log_tx.clone()),
            Some(sms_tx.clone()),
            Some(sms_thread_tx.clone()),
            Some(clipboard_tx.clone()),
            Some(clipboard_image_tx.clone()),
            Some(clipboard_offer_tx.clone()),
            Some(handoff_tx.clone()),
            Some(raw_frame_tx.clone()),
        )
        .await;

        // Tear down the BLE writer for this session — orchestrator
        // sender must not hold a stale handle into a closed link.
        {
            let mut m = ble_audio_writers.lock().await;
            m.remove(&peer.peer_static_pub);
        }
        // Drop the session device's BlueZ entry: its cached advertisement
        // (token valid for up to ±2 buckets ≈ 3 min) would otherwise
        // satisfy the proximity confirm-scan FROM THE CACHE and veto a
        // legitimate fast lock (live-hit: "confirm-scan still sees the
        // phone" 2s after the phone's radio went silent). A phone that's
        // genuinely still here re-creates the entry with a live adv.
        forget_stale_device(&adapter, client_arc.address).await;
        // Wake the proximity watcher — link-down starts its fast-lock path.
        crate::proximity::nudge().notify_one();
        // Drop the notification writer too — no live link to push to.
        *notif_writer.lock().await = None;
        // Drop the clipboard writer too.
        *clipboard_writer.lock().await = None;
        // Drop the clipboard image writer too.
        *clipboard_image_writer.lock().await = None;
        // Drop the call-control writer too.
        *call_writer.lock().await = None;

        // Wake the LAN heartbeat NOW: with BLE down it's the only liveness /
        // hand-off path again, and its relaxed BLE-alive cadence would
        // otherwise sleep minutes before noticing the link is gone.
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one();
        }

        // Explicitly disconnect the RPA we were on. When the LE link drops
        // (RPA rotation, A2DP starvation, range), BlueZ often keeps the old
        // RPA's device object in a "Connected" state — a phantom. Across many
        // reconnects those phantoms pile up and the NEXT connect attempt fails
        // with "Bluetooth operation in progress: In Progress". Tearing the old
        // one down here keeps the adapter clean so reconnect is reliable.
        let dropped_addr = client_arc.address;
        if let Ok(dev) = adapter.device(dropped_addr) {
            let _ = tokio::time::timeout(Duration::from_secs(3), dev.disconnect()).await;
        }

        tracing::info!(addr = %dropped_addr, "P2.13: BLE audio-signal listener returned; reopening");
        // Brief settle before reopening. The explicit disconnect above
        // already drained the dropped RPA (that's what previously needed a
        // long wait to dodge "In Progress"), so 500 ms is plenty — a full
        // 2 s just padded every reconnect. clear_pending_connect on the
        // connect path covers any residual in-flight teardown.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
