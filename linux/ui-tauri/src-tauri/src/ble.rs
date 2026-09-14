//! BLE reconnect: the persistent GATT link, RPA discovery, and the
//! direct-connect/scan fast path. Split out of lib.rs.

use std::sync::Arc;
use std::time::Duration;

use vortex_l3_daemon::core::ble::client::VortexClient;
use vortex_l3_daemon::core::ble::scanner::run_filtered_scan;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::pairing::reconnect::run_ik_initiator;
use vortex_l3_daemon::core::platform::linux::{LinuxAudioHandoff, LinuxGattLink};
use vortex_l3_daemon::core::storage::peers::PeerStore;

use crate::NotifWriter;

/// Consecutive connect failures (phone present, but the connect aborts —
/// `le-connection-abort-by-local` / services-not-resolved) before we back off
/// harder between attempts.
///
/// This used to escalate to powering the whole adapter off and on. That is
/// removed: it takes down every other BLE device on the machine — the user's
/// earbuds mid-stream, a Bluetooth mouse, another user's peripherals — to fix a
/// problem with ONE link, which is exactly what vortex is not allowed to do.
/// The log from the one time it fired shows it did not even work: the very next
/// connect failed the same way, because the real cause was the phone handing
/// out a fresh RPA every time (see the advertiser fix on the Android side), not
/// a wedged controller.
const CONNECT_WEDGE_THRESHOLD: u32 = 6;

/// The RPA of the BLE session that is live right now, so the app can hand the
/// link back on its way out. `None` between sessions.
///
/// Everything else about teardown was already handled — the loop disconnects
/// and forgets the device whenever the listener returns. What was missing is
/// that a PROCESS EXIT never reaches that code: the loop dies with the process
/// and BlueZ, which owns the connection independently of us, keeps it open.
/// The phone's GATT server therefore still sees a connected peer, holds its
/// RPA and stops advertising in a way a scan can find — so the freshly started
/// app scans, backs off 15s → 60s, and never reconnects. Measured on this
/// machine: BLE dead for six minutes after a restart, with LAN quietly
/// covering for it. Clearing the entry by hand and letting it reconnect took
/// eleven seconds.
static SESSION_ADDR: std::sync::Mutex<Option<bluer::Address>> =
    std::sync::Mutex::new(None);

fn note_session_addr(addr: Option<bluer::Address>) {
    if let Ok(mut g) = SESSION_ADDR.lock() {
        *g = addr;
    }
}

/// Hand the BLE link back before the process goes away.
///
/// Synchronous and hard-bounded, because it runs from Tauri's `RunEvent::Exit`
/// on the main thread: an exit that hangs on D-Bus is worse than one that
/// leaves a stale entry. Its own runtime rather than the worker's, which may
/// already be shutting down by the time this runs.
pub(crate) fn shutdown_link_blocking() {
    let Some(addr) = SESSION_ADDR.lock().ok().and_then(|g| *g) else {
        return; // no live session — nothing to hand back
    };
    tracing::info!(%addr, "shutting down — dropping the BLE link so the phone re-advertises");
    let worker = std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async move {
            let Ok(session) = bluer::Session::new().await else { return };
            let Ok(adapter) = session.default_adapter().await else { return };
            // Disconnect is the half the PHONE sees: it ends the GATT link, so
            // the phone stops holding its RPA and advertises again.
            if let Ok(dev) = adapter.device(addr) {
                let _ = tokio::time::timeout(Duration::from_millis(1200), dev.disconnect()).await;
            }
            // Removing the entry is the half WE need: it drops the cached
            // advertisement so the next run's discovery cannot re-serve this
            // dead RPA — the same reason `forget_stale_device` exists.
            let _ =
                tokio::time::timeout(Duration::from_millis(1200), adapter.remove_device(addr)).await;
        });
    });
    let _ = worker.join();
    note_session_addr(None);
}


/// Does a failed STATE write mean the LINK is gone, or only that the ATT
/// bearer was busy?
///
/// The distinction decides whether we tear the session down, so it has to be
/// conservative in the safe direction: anything we do not recognise is treated
/// as busy and the link is kept. A link that is really dead costs us nothing
/// to keep believing in for a few more beats — `run_listener` returns on a real
/// disconnect and tears the session down anyway — whereas killing a live link
/// costs a full scan, IK handshake, resubscribe and bulk re-push.
///
/// Matched on substrings because these arrive as D-Bus error text from BlueZ,
/// not as typed variants.
fn state_write_means_link_gone(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    // BlueZ says the device, the characteristic, or the D-Bus object is gone.
    e.contains("not connected")
        || e.contains("notconnected")
        || e.contains("does not exist")
        || e.contains("doesnotexist")
        || e.contains("unknown object")
        || e.contains("unknownobject")
        || e.contains("no such device")
        || e.contains("object removed")
        // zbus's wording for UnknownObject, seen live as "the target object was
        // either not present or removed". Missing it kept a genuinely dead link
        // "alive" for 40 beats of pointless retries instead of reconnecting.
        || e.contains("not present or removed")
        || e.contains("disconnected")
}


/// Last BLE address we completed a Noise IK exchange with, per peer.
///
/// Recorded only *after* IK succeeds, so the address is positively tied to
/// that `peer_static_pub` — before IK we merely believe an RPA belongs to the
/// peer whose presence token matched, and acting on a belief would let us
/// remove a stranger's BlueZ device object.
///
/// Used by `Forget` to clean up the peer's BlueZ device object (see
/// [`forget_stale_device`]). Vortex deliberately creates no BT bond on Linux
/// (see the 2026-06-02 note in `pairing.rs`), so there is usually no *bond*
/// to drop here — but a cached device object with a stale RPA does linger, and
/// leaving it behind is what feeds the RPA-churn connect wedge on the next
/// pairing. Entries are dropped on forget; the map holds one small entry per
/// trusted peer, so it needs no eviction.
static PEER_BLE_ADDRS: std::sync::Mutex<
    Option<std::collections::HashMap<[u8; 32], bluer::Address>>,
> = std::sync::Mutex::new(None);

/// Tie `addr` to `peer_pub` after a successful IK.
pub(crate) fn remember_peer_addr(peer_pub: &[u8; 32], addr: bluer::Address) {
    if let Ok(mut g) = PEER_BLE_ADDRS.lock() {
        g.get_or_insert_with(std::collections::HashMap::new)
            .insert(*peer_pub, addr);
    }
}

/// Remove and return the address last tied to `peer_pub`, if any.
pub(crate) fn take_peer_addr(peer_pub: &[u8; 32]) -> Option<bluer::Address> {
    PEER_BLE_ADDRS
        .lock()
        .ok()
        .and_then(|mut g| g.as_mut().and_then(|m| m.remove(peer_pub)))
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
/// Every trusted peer's ±2-bucket presence tokens, mapped to the peer that
/// owns them.
///
/// Flattened, this answers "is *a* trusted peer nearby",
/// which was a complete answer while a laptop could only trust one phone. With
/// several it is not: this loop authenticates *as* the peer it selected, so
/// seeing B's beacon and running IK with A's static key fails every time. The
/// failure is also sticky rather than self-correcting — the address we just
/// connected to is remembered as the fast path's `last_rpa`, so every
/// subsequent cycle re-dials the same wrong phone ahead of scanning.
///
/// Carrying the owner alongside the token is what lets the caller hand the
/// handshake the identity that actually answered.
pub(crate) fn presence_token_owners(
    peers: &[vortex_l3_daemon::core::storage::peers::TrustedPeer],
) -> std::collections::HashMap<[u8; 8], [u8; 32]> {
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
            [-2i64, -1, 0, 1, 2].iter().map(move |d| {
                (
                    derive_presence_token(&p.prs, (bucket_now as i64 + *d) as u64),
                    p.peer_static_pub,
                )
            })
        })
        .collect()
}

/// Consecutive scan rounds that asked for discovery and got an adapter which
/// said it was not discovering. Zero means the radio is answering.
static NOT_DISCOVERING_ROUNDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How many rounds in a row the adapter has refused to discover — read by the
/// diagnostics panel, which is what turns this into something the user can act
/// on rather than a silence that looks like an absent phone.
pub(crate) fn not_discovering_rounds() -> u32 {
    NOT_DISCOVERING_ROUNDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one round's verdict. Warns on the way in, not on every round after:
/// a wedged adapter would otherwise fill the log with the same line for hours.
fn note_discovery_health(discovering: bool) {
    use std::sync::atomic::Ordering;
    if discovering {
        if NOT_DISCOVERING_ROUNDS.swap(0, Ordering::Relaxed) > 0 {
            tracing::info!("BLE adapter is discovering again");
        }
        return;
    }
    let n = NOT_DISCOVERING_ROUNDS.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 {
        tracing::warn!(
            "BLE adapter accepted the scan but reports Discovering=false — \
             the controller is wedged; turning Bluetooth off and on clears it"
        );
    }
}
/// A trusted peer seen on air during a switch scan.
#[derive(Debug, Clone)]
pub(crate) struct PeerCandidate {
    pub peer_static_pub: [u8; 32],
    pub name: Option<String>,
    pub rssi: i16,
}

/// Scan for trusted-presence beacons from trusted peers OTHER than `exclude`.
///
/// The reconnect path asks [`presence_token_owners`] "who is nearby" and takes
/// whoever answers. A switch cannot: it has to leave the active peer out, so it
/// filters the peer set first and scans only for the rest.
///
/// Excluding the active peer is what makes "Switch" coherent: the user pressed
/// it precisely because they do not want the device they are already on
/// (design doc §D3).
pub(crate) async fn scan_other_trusted_peers(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    exclude: Option<[u8; 32]>,
    wait: Duration,
) -> Vec<PeerCandidate> {
    use std::collections::HashMap;
    use vortex_l3_daemon::core::crypto::presence::{current_bucket, derive_presence_token};

    let peers = {
        let store = peer_store.clone();
        tokio::task::spawn_blocking(move || store.list().unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    let others: Vec<_> = peers
        .into_iter()
        .filter(|p| exclude.as_ref() != Some(&p.peer_static_pub))
        .collect();
    if others.is_empty() {
        return Vec::new();
    }

    // token -> peer, over the same ±2 bucket window the reconnect path
    // tolerates (clock skew / a Doze-deferred rotation).
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bucket_now = current_bucket(now_sec, PRESENCE_ROTATION_SEC);
    let mut by_token: HashMap<[u8; 8], ([u8; 32], Option<String>)> = HashMap::new();
    for p in &others {
        for d in [-2i64, -1, 0, 1, 2] {
            let tok = derive_presence_token(&p.prs, (bucket_now as i64 + d) as u64);
            by_token.insert(tok, (p.peer_static_pub, p.peer_name.clone()));
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<PeerCandidate>(16);
    let scan = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            let _ = run_filtered_scan(adapter, move |c| {
                if !c.payload.flags.is_trusted_presence() {
                    return;
                }
                let Some((peer_pub, stored_name)) = by_token.get(&c.payload.payload_8) else {
                    return;
                };
                let _ = tx.try_send(PeerCandidate {
                    peer_static_pub: *peer_pub,
                    // Prefer the live SCAN_RSP name, fall back to the name
                    // recorded at pairing.
                    name: c.local_name.clone().or_else(|| stored_name.clone()),
                    rssi: c.rssi.unwrap_or(0),
                });
            })
            .await;
        })
    };

    // Collect for the whole window rather than stopping at the first hit: the
    // point is to know whether there is ONE candidate (auto-connect) or
    // several (ask the user), so an early return would make the picker
    // depend on which phone happened to advertise first.
    let mut found: HashMap<[u8; 32], PeerCandidate> = HashMap::new();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(cand)) => {
                // Keep the strongest sighting per peer — RSSI wobbles a lot
                // between advertising events.
                found
                    .entry(cand.peer_static_pub)
                    .and_modify(|e| {
                        if cand.rssi > e.rssi {
                            *e = cand.clone();
                        }
                    })
                    .or_insert(cand);
            }
            Ok(None) | Err(_) => break,
        }
    }
    // Same abort+join discipline as find_trusted_presence_peer: bluer only
    // issues StopDiscovery when the scan future is actually dropped, so
    // without the join the next scan races a still-live discovery session.
    scan.abort();
    let _ = scan.await;
    let mut out: Vec<_> = found.into_values().collect();
    out.sort_by(|a, b| b.rssi.cmp(&a.rssi));
    out
}

pub(crate) async fn find_trusted_presence_peer(
    adapter: &bluer::Adapter,
    peer_store: &Arc<dyn PeerStore>,
    wait: Duration,
) -> Option<(bluer::Address, [u8; 32])> {
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
    // Owner map, not a flat token set: the caller has to authenticate as
    // whichever peer answered, not as whichever one happens to be stored first.
    let owners = presence_token_owners(&peers);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(bluer::Address, [u8; 32])>(1);
    let scan = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            let _ = run_filtered_scan(adapter, move |c| {
                if !c.payload.flags.is_trusted_presence() {
                    return;
                }
                let Some(peer_pub) = owners.get(&c.payload.payload_8).copied() else {
                    // Flag set but token doesn't match any of our
                    // trusted peers' current ±1 bucket — could be a
                    // rogue advertiser trying to burn our IK budget,
                    // or a stale phone whose clock drifted out of
                    // the ±1 window. Drop either way.
                    return;
                };
                // Validated trusted-presence advert. try_send into a
                // bounded(1) channel: subsequent valid hits drop —
                // we only need the first.
                let _ = tx.try_send((c.address, peer_pub));
            })
            .await;
        })
    };
    // Watch for a wedged controller while the round runs.
    //
    // BlueZ can accept StartDiscovery — other clients even get
    // `org.bluez.Error.InProgress` — and then never discover: the adapter's
    // `Discovering` property stays false and not one advertising report
    // arrives. Observed live, with the radio dead for over an hour while this
    // loop scanned into it every 45 s and found nothing, which is
    // indistinguishable from "the phone is away" without this check.
    //
    // Only powering the adapter off and on cleared it, and that is NOT done
    // here: it drops every connection the adapter holds, the user's audio
    // included. The stuck state is reported instead (see `diagnostics`), and
    // acting on it stays the user's call.
    let stuck_probe = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            // BlueZ flips `Discovering` well inside a second of accepting the
            // call; two is slack, not a guess.
            tokio::time::sleep(Duration::from_secs(2)).await;
            adapter.is_discovering().await.unwrap_or(true)
        })
    };
    let res = tokio::time::timeout(wait, rx.recv()).await.ok().flatten();
    note_discovery_health(stuck_probe.await.unwrap_or(true));
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
        crate::presence::touch_presence();
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
    /// A validated trusted-presence advertiser is on air at this address,
    /// and its token identified it as this trusted peer.
    Found(bluer::Address, [u8; 32]),
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
) -> Option<[u8; 32]> {
    use vortex_l3_daemon::core::ble::{AdvPayload, VORTEX_SERVICE_UUID};
    let device = adapter.device(addr).ok()?;
    let sd = device.service_data().await.ok()??;
    let bytes = sd.get(&VORTEX_SERVICE_UUID)?;
    let payload = AdvPayload::decode(bytes).ok()?;
    if !payload.flags.is_trusted_presence() {
        return None;
    }
    presence_token_owners(peers)
        .get(&payload.payload_8)
        .copied()
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
                    if let Some(peer_pub) =
                        validate_presence_candidate(adapter, peers, id.device).await
                    {
                        tracing::info!(
                            addr = %id.device,
                            peer = %hex::encode(&peer_pub[..4]),
                            "presence monitor: trusted peer on air"
                        );
                        crate::presence::touch_presence();
                        return PresenceWait::Found(id.device, peer_pub);
                    }
                    // A Vortex advertiser that didn't validate: usually the
                    // BlueZ service-data cache lagging a token rotation (the
                    // monitor only re-fires after lost→found). One active
                    // scan round fetches FRESH adv data; if it's genuinely a
                    // foreign device the scan rejects it too and we keep
                    // waiting on the monitor.
                    tracing::debug!(addr = %id.device, "presence monitor: candidate failed token gate; one scan round");
                    if let Some((a, peer_pub)) =
                        find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
                    {
                        return PresenceWait::Found(a, peer_pub);
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
        if let Some((a, peer_pub)) =
            find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
        {
            return PresenceWait::Found(a, peer_pub);
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
) -> Option<(bluer::Address, [u8; 32])> {
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
    if let Some(found) =
        find_trusted_presence_peer(adapter, peer_store, Duration::from_secs(15)).await
    {
        return Some(found);
    }
    // Phase 2 — patient watch.
    if !MONITOR_UNSUPPORTED.load(Ordering::Relaxed) {
        match monitor_presence_wait(adapter, peer_store, &peers, retry_nudge).await {
            PresenceWait::Found(a, peer_pub) => return Some((a, peer_pub)),
            PresenceWait::Reevaluate => return None,
            PresenceWait::Unsupported => {
                MONITOR_UNSUPPORTED.store(true, Ordering::Relaxed);
            }
        }
    }
    match monitor_unsupported_wait(adapter, peer_store, retry_nudge).await {
        PresenceWait::Found(a, peer_pub) => Some((a, peer_pub)),
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
    // Last address we completed a handshake against, WITH the peer it proved
    // to be. The identity has to travel with the address: re-dialling a
    // remembered RPA is only a shortcut if we also authenticate as the peer
    // that answered there last time.
    last_rpa: &mut Option<(bluer::Address, [u8; 32])>,
    retry_nudge: &tokio::sync::Notify,
    // Running count of consecutive *connect attempts that failed* (the
    // abort-by-local / services-not-resolved BlueZ wedge). Bumped only when a
    // connect was actually tried and failed — NOT when the phone is simply
    // absent (no presence to connect to) — so the caller's power-cycle escalation
    // never fires just because the phone walked away. Reset to 0 on any success.
    consec_connect_fail: &mut u32,
) -> Option<(VortexClient, [u8; 32])> {
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
    if let Some((addr, peer_pub)) = *last_rpa {
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
                return Some((client, peer_pub));
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
    let (addr, peer_pub) = match wait_for_presence(adapter, peer_store, retry_nudge).await {
        Some(found) => found,
        None => {
            // Re-evaluate signal (LAN nudge / periodic) — the caller loops,
            // which re-runs the direct-connect fast path first.
            return None;
        }
    };
    // Capped, for the same reason the fast path above is capped — and this is
    // the one that actually burns. Measured on a cold start: six doomed attempts
    // at 13-18 s each, two minutes of dead time, then a connect that succeeded in
    // under a second once the address was fresh.
    //
    // The address comes from a scan, but the phone rotates its presence RPA every
    // 60 s. Uncapped, one doomed attempt plus the next 15 s scan is ~33 s — the
    // same order as the rotation, so we spend most of the time dialling addresses
    // the phone has already abandoned, and each failure costs a further rotation.
    // Capping turns that into several short attempts per window, each on a fresher
    // address. 8 s is well above a healthy connect-plus-service-discovery (the
    // fast path expects sub-second on a known-good RPA) and well below the 13-18 s
    // a dead RPA was costing.
    const CONNECT_CAP: Duration = Duration::from_secs(8);
    let connect = match tokio::time::timeout(CONNECT_CAP, VortexClient::connect(adapter, addr)).await
    {
        Ok(r) => r,
        // A tokio timeout drops the future, but BlueZ keeps the connect attempt in
        // flight — the next connect to a different RPA would fail "In Progress".
        // `clear_pending_connect` in the error arm below cancels it, so route the
        // timeout through the same cleanup rather than duplicating it.
        Err(_) => Err(vortex_l3_daemon::core::ble::client::ClientError::Timeout(
            "connect (capped)",
        )),
    };
    match connect {
        Ok(client) => {
            *last_rpa = Some((addr, peer_pub));
            *consec_connect_fail = 0;
            Some((client, peer_pub))
        }
        Err(e) => {
            // Stale cache or a flaky connect — drop the remembered RPA so the
            // next pass scans fresh rather than retrying a dead address.
            if last_rpa.map(|(a, _)| a) == Some(addr) {
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

/// Drop a failed RPA's device entry from BlueZ entirely. Without this the
/// next discovery round re-serves the DEAD address from the adapter's
/// advertisement cache (its stale token still validates — the ±2-bucket
/// tolerance spans ~3 min), so the loop burned two+ 15s connect timeouts
/// on a rotated-away RPA before a genuinely fresh sighting could win
/// (live-observed: 67s walk-up reconnect, the user typed their password
/// long before the eager unlock could fire). RPA entries are transient by
/// nature — removing one can't lose anything durable.
pub(crate) async fn forget_stale_device(adapter: &bluer::Adapter, addr: bluer::Address) {
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
    raw_frame_tx: tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::ble::frame::RawFrame>,
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
    let mut last_rpa: Option<(bluer::Address, [u8; 32])> = None;
    // Consecutive IK handshake failures against a reachable phone — e.g. the
    // phone dropped its trust record (user un-paired there). Escalating
    // backoff so we don't hammer connect+IK every few seconds forever.
    let mut consec_ik_fail: u32 = 0;
    // Consecutive connect failures against a present phone — drives the
    // adapter power-cycle self-heal (see CONNECT_WEDGE_THRESHOLD).
    let mut consec_connect_fail: u32 = 0;
    let mut announced_no_peer = false;
    tracing::info!("BLE persistent loop started");
    loop {
        // Need a trusted peer record to authenticate against.
        // Same blocking-pool offload as the LAN heartbeat: libsecret
        // calls block their executor thread, and this loop ran on
        // every restart, doubling the contention.
        // Trust gate only. WHICH peer this cycle authenticates as is decided by
        // whichever one actually answers — its presence token names it (see
        // `presence_token_owners`) — not by the store's iteration order. Taking
        // `list().next()` here was a single-peer assumption: with a second
        // laptop paired it would find B's beacon and then run IK with A's
        // static key, failing every time, and remember B's address as the fast
        // path so the next cycle re-dialled it.
        let peers = {
            let store = peer_store.clone();
            tokio::task::spawn_blocking(move || store.list().unwrap_or_default())
                .await
                .unwrap_or_default()
        };
        if peers.is_empty() {
            // Announce once, not every 10 s. Without it, a loop idling
            // for want of a trusted peer and a loop failing to connect
            // look identical in the log — both are silence. The portable
            // loop already says this; the BlueZ one did not.
            if !announced_no_peer {
                tracing::info!("no trusted peer yet; BLE loop idle until pairing");
                announced_no_peer = true;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        announced_no_peer = false;

        // Honour an explicit switch. The fast path re-dials whoever answered
        // last, which straight after a switch is the peer the user just moved
        // away from — it is still trusted, still nearby and still connectable,
        // so the shortcut would win the race and quietly undo the switch.
        // Ownership is the arbiter's call, so drop the shortcut and let
        // discovery find the peer that now owns the session.
        if let Some(active) = crate::arbiter::active() {
            if let Some((_, remembered)) = last_rpa {
                if remembered != active && peers.iter().any(|p| p.peer_static_pub == active) {
                    tracing::info!(
                        remembered = %hex::encode(&remembered[..4]),
                        active = %hex::encode(&active[..4]),
                        "BLE persistent: active peer changed — dropping the last-RPA shortcut"
                    );
                    last_rpa = None;
                }
            }
        }

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
        let (client, peer) = match connect_bonded_or_scan(
            &adapter,
            &peer_store,
            &mut last_rpa,
            &retry_nudge,
            &mut consec_connect_fail,
        )
        .await
        {
            Some((c, peer_pub)) => {
                // Trust can be revoked while a connect is in flight (Forget on
                // either end). Re-resolving the record here rather than reusing
                // a snapshot means we never hand IK a peer we no longer trust.
                match peers.iter().find(|p| p.peer_static_pub == peer_pub) {
                    Some(p) => (c, p.clone()),
                    None => {
                        tracing::warn!(
                            peer = %hex::encode(&peer_pub[..4]),
                            "BLE persistent: answered by a peer we no longer trust; dropping"
                        );
                        continue;
                    }
                }
            }
            None => {
                // The phone is present but connects keep aborting. Drop the
                // cached address and wait longer before trying again — the
                // per-device `remove_device` cleanup has already run, and
                // hammering a busy controller is what makes it worse.
                //
                // Deliberately NOT an adapter power-cycle any more: that fixed
                // one link by breaking every other Bluetooth device on the
                // machine, and the one time it fired here it did not fix even
                // this one.
                if consec_connect_fail >= CONNECT_WEDGE_THRESHOLD {
                    tracing::warn!(
                        consec_connect_fail,
                        "BLE: repeated connect failures — backing off (adapter left alone)"
                    );
                    consec_connect_fail = 0;
                    last_rpa = None;
                    tokio::time::sleep(Duration::from_secs(5)).await;
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
        // Same client, presented through the seam — the IK flow is
        // platform-neutral now and the audio-signal work below still uses
        // `client` directly for its characteristic.
        let link = LinuxGattLink::from_client(adapter.clone(), &client);
        let outcome = match run_ik_initiator(
            &link,
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
        // IK proved this address really is this peer — safe to remember for
        // Forget's BlueZ cleanup (see PEER_BLE_ADDRS), and to point the
        // phone-specific caches at this peer.
        remember_peer_addr(&peer.peer_static_pub, client.address);
        crate::arbiter::note_connected(&peer.peer_static_pub);
        // Ownership, separately from the link (design doc §D4). A refusal is
        // logged rather than acted on for now: nothing sends `PeerHandoff.CLAIM`
        // yet, so the only way to reach Busy is a second trusted phone
        // connecting while one is active — worth seeing in the log.
        if let crate::arbiter::Claim::Busy { current } =
            crate::arbiter::claim(&peer.peer_static_pub)
        {
            tracing::warn!(
                peer = %hex::encode(&peer.peer_static_pub[..4]),
                active = %hex::encode(&current[..4]),
                "second peer connected while another is active; link up but not active"
            );
        }

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
        crate::presence::touch_presence();

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
        // Remember the live session's RPA so a process exit can hand the link
        // back (see `shutdown_link_blocking`). Cleared in the teardown below.
        note_session_addr(Some(client_arc.address));
        // The same live connection, presented through the seam: `audio_signal`
        // speaks `&dyn GattLink` now, so every writer below and the listener
        // itself take this. `client_arc` stays for the address and the typed
        // helpers around it.
        let link_arc = Arc::new(LinuxGattLink::from_client(adapter.clone(), &client_arc));
        let writer_transport = transport.clone();
        let writer_link = link_arc.clone();
        let writer_fn: SessionWriter = Arc::new(move |frame: AudioOpFrame| {
            let transport = writer_transport.clone();
            let link = writer_link.clone();
            Box::pin(async move {
                audio_signal::write_audio_op(&*link, transport, frame).await
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
            let nw_client = link_arc.clone();
            let writer: NotifWriter = Arc::new(move |notif| {
                let transport = nw_transport.clone();
                let link = nw_client.clone();
                Box::pin(async move {
                    audio_signal::write_notification(&*link, transport, &notif).await
                })
            });
            *notif_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone clipboard writer for this live link so the
        // clipboard sync consumer can push copied text to the phone.
        {
            let cw_transport = transport.clone();
            let cw_link = link_arc.clone();
            let writer: crate::ClipboardWriter = Arc::new(move |clip| {
                let transport = cw_transport.clone();
                let link = cw_link.clone();
                Box::pin(async move {
                    audio_signal::write_clipboard(&*link, transport, &clip).await
                })
            });
            *clipboard_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone clipboard IMAGE writer (chunked) for this
        // live link so the sync consumer can push a copied image to the phone.
        {
            let cw_transport = transport.clone();
            let cw_link = link_arc.clone();
            let writer: crate::ClipboardImageWriter = Arc::new(move |png| {
                let transport = cw_transport.clone();
                let link = cw_link.clone();
                Box::pin(async move {
                    audio_signal::write_clipboard_image(&*link, transport, &png).await
                })
            });
            *clipboard_image_writer.lock().await = Some(writer);
        }

        // Publish the laptop→phone call-control writer for this live link so
        // the call-banner consumer can answer/decline/end/mute via BLE.
        {
            let cw_transport = transport.clone();
            let cw_link = link_arc.clone();
            let writer: crate::CallWriter = Arc::new(move |ctrl| {
                let transport = cw_transport.clone();
                let link = cw_link.clone();
                Box::pin(async move {
                    audio_signal::write_call_control(&*link, transport, &ctrl).await
                })
            });
            *call_writer.lock().await = Some(writer);
        }

        // Publish the GENERIC sealed-frame writer for this live link so any
        // feature (e.g. notes) can send a `(ty, payload)` frame to the phone.
        {
            let sw_transport = transport.clone();
            let sw_client = link_arc.clone();
            let writer: crate::SealedWriter = Arc::new(move |ty, sub, payload| {
                let transport = sw_transport.clone();
                let link = sw_client.clone();
                Box::pin(async move {
                    audio_signal::write_sealed(&*link, transport, ty, sub, &payload).await
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
            let st_link = link_arc.clone();
            // Kept as a bluer address: `remove_device` is a BlueZ call, and
            // round-tripping it through the seam's PeerAddr would buy nothing.
            let st_addr = client_arc.address;
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
                // Busy-bearer beats, counted separately: they never tear the
                // link down, they only decide how loudly we mention it.
                let mut busy_fail: u32 = 0;
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
                    let (otp, otp_seq) = crate::send_to_phone::pending();
                    state.open_on_phone = otp;
                    state.open_on_phone_seq = otp_seq;
                    // Now-playing snapshot for the phone's laptop-media
                    // notification — must ride the BLE STATE path too so the
                    // notification works on a BLE-only link (AP isolation).
                    crate::media_remote::fill_now_playing(&mut state).await;
                    // Shared Do Not Disturb (LWW) — must ride the BLE STATE path too
                    // so local toggles reach the phone in ~50ms instead of waiting
                    // minutes for LAN.
                    let (dnd_on, dnd_at) = crate::dnd::state();
                    state.dnd = dnd_on;
                    state.dnd_changed_at = dnd_at;
                    if let Some(mw) = crate::MEDIA_WATCH.get() {
                        state.smart_switch_enabled =
                            mw.enabled.load(std::sync::atomic::Ordering::Relaxed);
                        state.smart_switch_changed_at =
                            mw.enabled_changed_at.load(std::sync::atomic::Ordering::Relaxed);
                    }
                    match audio_signal::write_state(&*st_link, st_transport.clone(), &state).await
                    {
                        Ok(()) => {
                            consecutive_fail = 0;
                            // A successful write proves the phone is in range —
                            // keeps the proximity watcher's presence fresh while
                            // connected (the advertisement monitor only runs
                            // between sessions).
                            crate::presence::touch_presence();
                            // It ALSO proves the BLE link is live, so it counts
                            // as peer contact — the liveness signal that gates
                            // the disconnect-clear of mirror pills. Without this,
                            // a BLE-only call (phone sends nothing over BLE while
                            // the call is up, and LAN can't complete under AP
                            // isolation) starved peer_contact past the 35s
                            // threshold → the call/handoff pill was falsely
                            // cleared and flickered every ~30s. This 12s beat
                            // keeps it fresh whenever the link is genuinely up.
                            crate::presence::touch_peer_contact();
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
                            //
                            // "Genuinely gone" is the load-bearing word. A BUSY
                            // bearer is not a dead link, and counting it as one is
                            // how vortex used to tear down its own healthy
                            // connection: a clipboard image push occupies the ATT
                            // bearer with thousands of chunks, every STATE write
                            // comes back "In Progress", six of those hit the cap,
                            // `remove_device` fired — and the phone recorded the
                            // result as HCI 0x13, "remote user terminated". Every
                            // long-lived session that ended on this machine ended
                            // that way. So only escalate on errors that mean the
                            // device or characteristic is actually gone.
                            if !state_write_means_link_gone(&e) {
                                busy_fail += 1;
                                // A busy bearer is proof of a LIVE link — BlueZ
                                // only reports it for a device it is connected
                                // to — so it counts as presence and as contact
                                // just as a successful write does. Without this,
                                // a long bulk transfer (clipboard image, contacts
                                // backfill) starved both clocks for its whole
                                // duration: the proximity watcher read the phone
                                // as having left, and the mirror pills were swept
                                // while the phone sat right there.
                                crate::presence::touch_presence();
                                crate::presence::touch_peer_contact();
                                // Back off hard rather than hammering: each retry
                                // re-seals the frame and therefore burns a Noise
                                // nonce whether or not the bytes ever leave, and
                                // the phone has to resync past every one of them.
                                if busy_fail == 1 {
                                    tracing::debug!("BLE state write: bearer busy, backing off: {e}");
                                } else if busy_fail % 20 == 0 {
                                    tracing::info!(
                                        "BLE state heartbeat: bearer busy for {busy_fail} beats \
                                         (bulk transfer in flight?) — link kept: {e}"
                                    );
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                continue;
                            }
                            busy_fail = 0;
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
                                let _ = st_adapter.remove_device(st_addr).await;
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
                        _ = crate::presence::state_nudge().notified() => {}
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
            &*link_arc,
            transport,
            peer.peer_static_pub,
            // The one frame type that needs the local audio stack, behind the
            // seam. `Some` here because this IS the platform that has one.
            Some(Arc::new(LinuxAudioHandoff::new(
                switch_orchestrator.clone(),
                media_store.clone(),
            ))),
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
        // Every filesystem handle the phone held is now unresolvable, and
        // anything waiting on a reply will never get one. Dropping both turns
        // what would be a hung mount into an honest I/O error.
        crate::fs_link::clear_handles();

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
        note_session_addr(None); // this session is over; nothing for exit to undo
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
