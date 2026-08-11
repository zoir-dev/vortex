//! LAN heartbeat: discover the phone (mDNS / cached IP / gateway), open
//! a TCP Noise-IK session, sync AppState. Split out of lib.rs.

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};

use vortex_l3_daemon::core::appstate::AppState;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::lan::discovery::discover_first;
use vortex_l3_daemon::core::lan::tcp_client::run_lan_reconnect;
use vortex_l3_daemon::core::storage::peers::PeerStore;

use crate::{app_state_to_dto, emit_peers};


/// The phone's LAN server port (Android `LanServer.DEFAULT_PORT`). Used by
/// the gateway fallback when mDNS can't resolve the peer over its hotspot.
pub(crate) const LAN_DEFAULT_PORT: u16 = 51820;

use crate::lan_wifi_direct::{restore_wifi, wd_active, WIFI_DIRECT_GO_IP};
use crate::lan_state::{dispatch_appstate_call, dispatch_lock_command};

/// Last peer IP that mDNS successfully resolved to. When mDNS later comes
/// up empty (a known intermittent on both real Wi-Fi and hotspots), we
/// retry this cached address BEFORE the gateway guess — on a normal router
/// network the gateway is the router, not the phone, so the old gateway-
/// only fallback would wedge the link until mDNS recovered. Caching the
/// real peer IP keeps the connection alive across mDNS hiccups on both
/// network shapes (the phone's hotspot IP gets cached the same way).
pub(crate) static LAST_GOOD_PEER_IP: std::sync::Mutex<Option<std::net::IpAddr>> =
    std::sync::Mutex::new(None);

fn last_peer_ip_path() -> Option<std::path::PathBuf> {
    let mut p = std::path::PathBuf::from(std::env::var_os("HOME")?);
    p.push(".cache/vortex/last_peer_ip");
    Some(p)
}

/// Cache the peer IP that just resolved — in memory AND on disk — so after a
/// daemon restart the very first heartbeat reuses it instead of the
/// (wrong-on-a-shared-network) gateway guess that caused a transient
/// "disconnected" until mDNS recovered.
pub(crate) fn persist_last_peer_ip(ip: std::net::IpAddr) {
    let changed = {
        let mut g = LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *g != Some(ip);
        *g = Some(ip);
        changed
    };
    if changed {
        if let Some(p) = last_peer_ip_path() {
            let _ = vortex_l3_daemon::core::fs_private::write_private(&p, ip.to_string().as_bytes());
        }
    }
}

/// Forget the cached peer IP (in-memory + disk). Called on peer forget so the
/// LAN fast-path never dials the previous phone's address.
pub(crate) fn clear_last_peer_ip() {
    *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(p) = last_peer_ip_path() {
        let _ = std::fs::remove_file(&p);
    }
}

/// Load the persisted peer IP into the in-memory cache at startup.
pub(crate) fn load_last_peer_ip() {
    if let Some(ip) = last_peer_ip_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<std::net::IpAddr>().ok())
    {
        *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip);
        tracing::info!(%ip, "loaded cached peer IP (LAN fast-path on restart)");
    }
}

/// A peer AppState carried the phone's OWN Wi-Fi IP (`wifi_ip`, sent on every
/// push over BOTH transports). Adopt it into the cached-peer-IP fast path.
/// This is what keeps the cache fresh across DHCP renews even while BLE is
/// the only live link — the phone answers no mDNS then (multicast lock
/// released), so without this hint a stale lease left mirror/cast/camera
/// dialing a dead address with no way to rediscover.
/// The phone's display refresh rate, as last reported in its AppState. `0` =
/// never reported (an older build), where the mirror falls back to its own
/// conservative default.
pub(crate) static PEER_DISPLAY_HZ: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

pub(crate) fn note_peer_reported_ip(state: &AppState) {
    // Same push carries the panel's refresh rate; keep it for the mirror's
    // frame-rate request. Sanity-bounded: a nonsense value from a strange ROM
    // must not have us asking an encoder for a thousand frames a second.
    if let Some(hz) = state.display_hz {
        if (20..=240).contains(&hz)
            && crate::lan::PEER_DISPLAY_HZ.swap(hz, std::sync::atomic::Ordering::Relaxed) != hz
        {
            tracing::info!(hz, "peer reports its display refresh rate (mirror fps ceiling)");
        }
    }
    let Some(s) = state.wifi_ip.as_deref() else { return };
    let Ok(ip) = s.parse::<std::net::IpAddr>() else { return };
    if ip.is_loopback() || ip.is_unspecified() {
        return;
    }
    persist_last_peer_ip(ip);
}

/// Resolve the phone's CURRENT LAN address for an on-demand session (screen
/// mirror, cast, camera). Unlike a raw `LAST_GOOD_PEER_IP` read this VERIFIES
/// the address: probe the cached IP with a short TCP connect, fall back to a
/// fresh mDNS browse (which re-primes the cache), then the probed default
/// gateway (phone-as-hotspot). `fresh` skips the cached-IP probe — the
/// one-shot retry path after a session failed on an address that answered
/// TCP but wasn't (or no longer was) our phone.
pub(crate) async fn resolve_peer_addr(fresh: bool) -> Option<std::net::SocketAddr> {
    use std::net::{IpAddr, SocketAddr};
    /// Probe with RETRIES, unlike the heartbeat's single 2 s attempt. A phone
    /// whose Wi-Fi radio is dozing takes longer than that to answer a cold
    /// connect, and the heartbeat gets away with it because it ticks again in a
    /// few seconds. This path does not: someone pressed a button, and one
    /// unlucky probe turned into "phone not reachable on LAN" while the very
    /// same phone accepted a heartbeat connection twenty seconds later.
    async fn probe(sa: SocketAddr) -> bool {
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            if matches!(
                tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(sa))
                    .await,
                Ok(Ok(_))
            ) {
                return true;
            }
        }
        false
    }
    if wd_active() {
        return Some(SocketAddr::new(IpAddr::from(WIFI_DIRECT_GO_IP), LAN_DEFAULT_PORT));
    }
    if !fresh {
        let cached = *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ip) = cached {
            let sa = SocketAddr::new(ip, LAN_DEFAULT_PORT);
            if probe(sa).await {
                return Some(sa);
            }
            tracing::info!(%ip, "cached peer IP failed the probe; rediscovering");
        }
    }
    match discover_first(Duration::from_secs(6)).await {
        Ok(Some(c)) => {
            if let Some(ip) = c
                .addresses
                .iter()
                .find(|a| matches!(a, IpAddr::V4(_)))
                .copied()
                .or_else(|| c.addresses.first().copied())
            {
                persist_last_peer_ip(ip);
                return Some(SocketAddr::new(ip, c.port));
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("resolve_peer_addr: mdns: {e}"),
    }
    // mDNS empty (hotspot mode, or the phone released its multicast lock).
    // The gateway IS the phone when it's the hotspot — but PROBE it so a
    // normal router (which refuses :51820) is never handed back as the phone.
    if let Some(g) = default_gateway_v4() {
        let sa = SocketAddr::new(IpAddr::V4(g), LAN_DEFAULT_PORT);
        if probe(sa).await {
            return Some(sa);
        }
    }
    // Last resort: the cached IP again. mDNS goes quiet whenever the phone
    // holds no multicast lock (which is most of the time while BLE carries the
    // link), so a cache primed by the phone's own `wifi_ip` push is often the
    // only truth there is — worth one more try before telling the user their
    // phone is unreachable.
    if !fresh {
        let cached = *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ip) = cached {
            let sa = SocketAddr::new(ip, LAN_DEFAULT_PORT);
            if probe(sa).await {
                tracing::info!(%ip, "cached peer IP answered on the second pass");
                return Some(sa);
            }
        }
    }
    None
}

/// Read the IPv4 default-gateway from `/proc/net/route`. The default route
/// is the row whose Destination is `00000000`; its Gateway is a
/// little-endian hex u32. Returns None if there's no default route.
pub(crate) fn default_gateway_v4() -> Option<std::net::Ipv4Addr> {
    let data = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in data.lines().skip(1) {
        let mut f = line.split_whitespace();
        let _iface = f.next()?;
        let dest = f.next()?;
        let gw = f.next()?;
        if dest == "00000000" {
            let raw = u32::from_str_radix(gw, 16).ok()?;
            let o = raw.to_le_bytes();
            let ip = std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3]);
            if !ip.is_unspecified() {
                return Some(ip);
            }
        }
    }
    None
}
pub(crate) async fn try_lan_reconnect(
    app: &AppHandle,
    identity: &IdentityRecord,
    peer_store: Arc<dyn PeerStore>,
    switch_orchestrator: Option<Arc<vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator>>,
    session_writers: Option<vortex_l3_daemon::core::audio_lan_session::SessionWriterMap>,
    media_store: Option<vortex_l3_daemon::core::media_runtime::MediaStateStore>,
    // Tracks the last call_phase seen on the heartbeat so we only
    // react to transitions (e.g. `None` → `ringing`), not every
    // repeated tick of the same value.
    last_call_phase: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    // True when the persistent BLE audio link is live. When it is, the
    // BLE `Request` fast-path already drives the call-start pause +
    // release, so we MUST NOT also act on the LAN `call_phase` field —
    // a stale/spurious `ringing` (e.g. the phone's pendingCallPhase left
    // set with no real call) would otherwise release the buds with
    // nobody to grab them, orphaning them off both sides. LAN call_phase
    // is strictly the BLE-down fallback.
    ble_live: bool,
    // Shared bluer adapter from the worker. Reuse instead of calling
    // `bluer::Session::new()` per heartbeat — the per-tick session
    // creation accumulated D-Bus connections and was the cause of
    // Tauri hanging after a few call cycles (the runtime's blocking
    // pool would exhaust waiting on libsecret/bluez D-Bus calls).
    shared_adapter: Option<bluer::Adapter>,
    // Phase 3 — smart audio-follow shared state. `media_watch.playing`
    // is published on the outgoing heartbeat; `media_watch.peer_playing`
    // tracks the peer's last-seen value so we fire the buds-release on
    // the peer's not-playing → playing edge. `media_in_call` is updated
    // from call_phase so the laptop watcher suppresses grabs during a call.
    media_watch: Option<Arc<vortex_l3_daemon::core::media_watch::MediaWatch>>,
    media_in_call: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Option<AppState>, String> {
    let mut peers = peer_store
        .list()
        .map_err(|e| format!("list: {e}"))?;
    if peers.is_empty() {
        return Err("no trusted peers".to_string());
    }
    // Newest-first so a fresh re-pair takes precedence over any
    // legacy entries that may still linger (we forget on save now,
    // but be defensive — older installs may still have duplicates).
    peers.sort_by_key(|p| std::cmp::Reverse(p.paired_at));

    // Fast path (the LAN twin of the BLE last-RPA direct-connect): most ticks
    // the phone is still at the IP that completed the previous handshake, so
    // probe it with a quick TCP connect before paying for a 6 s mDNS browse.
    // Doubly important while BLE is up: the phone releases its multicast lock
    // then (battery), so it won't answer mDNS at all and the browse would be
    // guaranteed dead time on every tick. A dead/stale IP fails the probe in
    // ≤2 s and we fall through to full discovery — no wedging.
    let fast_path: Option<std::net::SocketAddr> = if wd_active() {
        // On the phone's P2P group → skip the cached-IP probe (that IP is the
        // router subnet, now unreachable); target the GO directly below.
        None
    } else {
        let cached = *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner());
        // No subnet precondition here, on purpose: the 2 s TCP probe itself
        // is the reachability check, and it covers ROUTED setups (phone on
        // 10.x behind the same router as a 192.168.x laptop) that an
        // octet-prefix test wrongly rejects. This matters doubly because
        // while BLE is up the phone answers no mDNS (multicast lock
        // released) — this probe is then the ONLY fast LAN path.
        match cached {
            Some(ip) => {
                let sa = std::net::SocketAddr::new(ip, LAN_DEFAULT_PORT);
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    tokio::net::TcpStream::connect(sa),
                )
                .await
                {
                    Ok(Ok(_probe)) => Some(sa), // reachable — probe socket drops here
                    _ => None,
                }
            }
            None => None,
        }
    };

    // Try mDNS first. It works on a normal router, but Android's mDNS
    // responder does NOT reliably answer queries from clients of its OWN
    // WiFi hotspot — so when the phone is the hotspot (the usual field
    // setup) discovery comes up empty even though the TCP server is
    // reachable. Fall back to the default gateway: when the phone is the
    // hotspot it IS the gateway, so gateway:DEFAULT_PORT hits the phone
    // directly. Harmless on a router (the connect just fails and retries).
    let socket_addr = if wd_active() {
        // Wi-Fi Direct pull: the LanServer is at the group owner's fixed IP.
        std::net::SocketAddr::new(std::net::IpAddr::from(WIFI_DIRECT_GO_IP), LAN_DEFAULT_PORT)
    } else if let Some(sa) = fast_path {
        sa
    } else {
        match discover_first(Duration::from_secs(6))
        .await
        .map_err(|e| format!("mdns: {e}"))?
    {
        Some(candidate) => {
            let ip = candidate
                .addresses
                .iter()
                .find(|a| matches!(a, std::net::IpAddr::V4(_)))
                .copied()
                .or_else(|| candidate.addresses.first().copied())
                .ok_or_else(|| "no IP".to_string())?;
            // Remember it (in memory + on disk) so an mDNS hiccup OR a daemon
            // restart doesn't drop us to the (wrong-on-a-shared-network)
            // gateway guess.
            persist_last_peer_ip(ip);
            std::net::SocketAddr::new(ip, candidate.port)
        }
        None => {
            // mDNS empty. Prefer the last IP mDNS gave us — BUT only while it's
            // still on the current subnet. When the network changes (the phone
            // becomes a Wi-Fi hotspot → a fresh /24, or you join a new AP) the
            // cached IP is stale and would wedge the link retrying a dead
            // address; fall back to the gateway, which IS the phone in hotspot
            // mode. Only if we've never resolved one do we guess the gateway.
            let cached = *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner());
            let gw = default_gateway_v4();
            let cached_on_subnet = match (cached, gw) {
                (Some(std::net::IpAddr::V4(ip)), Some(g)) => ip.octets()[..3] == g.octets()[..3],
                (Some(_), None) => true, // no gateway to compare against; trust the cache
                _ => false,
            };
            if let (Some(ip), true) = (cached, cached_on_subnet) {
                tracing::info!(%ip, "mDNS empty; retrying last-known peer IP");
                std::net::SocketAddr::new(ip, LAN_DEFAULT_PORT)
            } else if let Some(g) = gw {
                if cached.is_some() {
                    tracing::info!(%g, "mDNS empty; cached IP off-subnet (network changed) → gateway");
                } else {
                    tracing::info!(%g, "mDNS empty, no cache; falling back to gateway (phone-as-hotspot)");
                }
                std::net::SocketAddr::new(std::net::IpAddr::V4(g), LAN_DEFAULT_PORT)
            } else {
                return Err(
                    "no LAN candidate (mDNS empty, cached IP off-subnet, no gateway)".to_string(),
                );
            }
        }
    }
    };

    // Build local AppState. Locale + theme are intentionally omitted
    // from the wire — per-device preferences with no cross-device sync.
    let mut local_state = vortex_l3_daemon::core::appstate::AppState::now_laptop();
    // Detect any currently-connected wireless earbuds on this laptop
    // and attach to the AppState so the phone's UI can render them.
    // Reuse the worker's adapter — creating a fresh `bluer::Session`
    // per heartbeat leaks D-Bus connections and eventually wedges the
    // tokio runtime.
    if let Some(adapter) = shared_adapter.as_ref() {
        local_state.earbuds =
            vortex_l3_daemon::core::earbuds::scan_local_earbuds(adapter).await;
    }
    // One-shot laptop→phone call-control fallback (a pill/banner button when
    // BLE was down): ride this AppState over LAN. take() so it sends once.
    if let Ok(mut g) = crate::PENDING_CALL_CONTROL.lock() {
        local_state.call_control = g.take();
    }
    // One-shot laptop→phone notification action/reply INVOKE backstop — only
    // queued when the BLE write failed; ride this AppState over LAN. take().
    if let Ok(mut g) = crate::notifications::PENDING_NOTIF_INVOKE.lock() {
        local_state.notif_invoke = g.take();
    }
    // Lock-screen state for the phone's remote-lock button (logind
    // LockedHint; one cheap D-Bus property read per heartbeat).
    local_state.locked = vortex_l3_daemon::core::session_lock::locked_hint().await;
    // Laptop→phone screen-cast offer (where to dial + the key) while casting.
    local_state.laptop_cast = crate::laptop_cast::current_offer();
    local_state.laptop_cast_error = crate::laptop_cast::current_error();
    // Continuity Camera: request the phone's camera as a laptop webcam.
    local_state.camera_req = crate::camera::camera_wanted();
    local_state.camera_facing = crate::camera::camera_facing();
    // Find-My: the "ring my phone" request (unix-millis of the last tap).
    local_state.ring_seq = crate::ring::ring_seq();
    // Now-playing snapshot (title/artist/app + raw playing) for the phone's
    // laptop-media notification. Cheap MPRIS property reads per heartbeat.
    crate::media_remote::fill_now_playing(&mut local_state).await;
    // Phase 3 — advertise whether media is playing locally so the phone
    // can render an owner indicator and (symmetrically) decide to release.
    // Also carry the one-shot return claim: when our media stopped and the
    // watcher wants the phone to take the buds back, set
    // audio_claim_request so the phone grabs them and resumes its paused
    // media (swap-to-false so a single heartbeat carries the flag).
    if let Some(mw) = media_watch.as_ref() {
        use std::sync::atomic::Ordering;
        local_state.media_playing = mw.playing.load(Ordering::Relaxed);
        // Last-play-wins: send our play AGE (ms in the current session, frozen
        // across hand-off pauses), not an absolute time, so the phone can
        // tie-break free of clock skew by re-anchoring it to its own clock.
        local_state.media_play_age_ms = {
            let e = mw.play_epoch_mono.load(Ordering::Relaxed);
            if e == 0 {
                0
            } else {
                vortex_l3_daemon::core::media_watch::mono_ms().saturating_sub(e)
            }
        };
        // Shared smart-switch setting + its LWW timestamp, so the phone can
        // adopt our value if ours is newer (and vice versa).
        local_state.smart_switch_enabled = mw.enabled.load(Ordering::Relaxed);
        local_state.smart_switch_changed_at = mw.enabled_changed_at.load(Ordering::Relaxed);
        if mw.claim_peer.swap(false, Ordering::Relaxed) {
            local_state.audio_claim_request = true;
        }
    }

    let mut last_err: Option<String> = None;
    for peer in peers {
        // Offload the libsecret read to the blocking pool — see the
        // 8-thread comment in run_worker for why we don't run this
        // synchronously on a worker thread.
        let local_counter = {
            let store = peer_store.clone();
            let peer_pub = peer.peer_static_pub;
            tokio::task::spawn_blocking(move || {
                store.load_counter(&peer_pub).unwrap_or(0)
            })
            .await
            .unwrap_or(0)
        };
        // Bulk-sync request: name our mirror caches' hashes so the phone
        // ships full JSON only for stale ones over this TCP session (and
        // skips the redundant BLE burst for ones that match). Empty hash =
        // no cache yet → the phone always sends.
        let mut bulk_obj = serde_json::json!({
            "contacts": crate::contacts::cache_hash(),
            "call_log": crate::call_log::cache_hash(),
            "sms": crate::sms::cache_hash(),
            // Watermark datasets: "send me everything newer than this".
            "sms_history": crate::sms::history_since().to_string(),
            "call_log_history": crate::call_log::history_since().to_string(),
            // Deletion reconcile: hash of our history store's id list.
            "sms_ids": crate::sms::ids_hash(),
        });
        // Pending phone-shared image → pull it by token this round (the phone
        // serves the PNG as reliable CLIPBOARD_IMAGE chunks over this TCP).
        let requested_img_token: Option<String> = crate::PENDING_IMAGE_TOKEN
            .get()
            .and_then(|m| m.lock().ok().and_then(|g| g.clone()));
        if let Some(token) = &requested_img_token {
            bulk_obj["clipboard_image"] = serde_json::Value::String(token.clone());
        }
        // Instant-share file pull: request the FRONT queued file this round (the rest
        // follow on subsequent nudged rounds).
        if let Some(token) = crate::PENDING_FILE_OFFERS
            .get()
            .and_then(|m| m.lock().ok().and_then(|g| g.front().map(|(t, _, _, _)| t.clone())))
        {
            bulk_obj["clipboard_file"] = serde_json::Value::String(token);
        }
        let bulk_request = bulk_obj.to_string();
        match run_lan_reconnect(
            socket_addr,
            &identity.static_priv.0,
            &peer.peer_static_pub,
            &peer.prs,
            local_counter,
            local_state.clone(),
            Duration::from_secs(15),
            Some(&bulk_request),
        )
        .await
        {
            Ok(outcome) => {
                // The handshake at this address just SUCCEEDED — that's the
                // strongest possible "this is our phone's IP" signal, stronger
                // than any discovery guess. Cache it whichever path picked it
                // (fast-path, mDNS, or the gateway guess), so mirror/cast/
                // camera always dial the address that last actually worked.
                persist_last_peer_ip(outcome.remote.ip());
                // And adopt the phone's self-reported Wi-Fi IP when it differs
                // (e.g. we reached it via a hotspot NAT alias).
                if let Some(s) = &outcome.peer_state {
                    note_peer_reported_ip(s);
                }
                if outcome.peer_counter < local_counter {
                    tracing::warn!(
                        "possible trust rollback: peer counter={} local={}",
                        outcome.peer_counter,
                        local_counter
                    );
                }
                // Deliver any bulk-sync datasets the phone shipped (our
                // cache was stale) — same cache+emit path as the BLE
                // chunk consumer.
                for (ty_byte, json) in &outcome.bulk {
                    match *ty_byte {
                        vortex_l3_daemon::core::ble::frame::ty::CONTACTS => {
                            crate::contacts::deliver(app, json, "LAN bulk-sync");
                        }
                        vortex_l3_daemon::core::ble::frame::ty::CALL_LOG => {
                            crate::call_log::deliver(app, json, "LAN bulk-sync");
                        }
                        vortex_l3_daemon::core::ble::frame::ty::SMS => {
                            crate::sms::deliver(app, json, "LAN bulk-sync");
                        }
                        vortex_l3_daemon::core::ble::frame::ty::SMS_THREAD => {
                            crate::sms::merge_history(app, json);
                        }
                        vortex_l3_daemon::core::ble::frame::ty::CALL_LOG_HISTORY => {
                            crate::call_log::merge_history(app, json);
                        }
                        vortex_l3_daemon::core::ble::frame::ty::SMS_IDS => {
                            crate::sms::reconcile_ids(app, json);
                        }
                        vortex_l3_daemon::core::ble::frame::ty::CLIPBOARD_IMAGE => {
                            // Reliable LAN pull of a phone-shared clipboard image →
                            // system clipboard + history. Clear the pending token.
                            crate::clipboard_sync::apply_synced_image(app, json.clone()).await;
                            if let Some(slot) = crate::PENDING_IMAGE_TOKEN.get() {
                                if let Ok(mut g) = slot.lock() {
                                    *g = None;
                                }
                            }
                        }
                        vortex_l3_daemon::core::ble::frame::ty::CLIPBOARD_FILE => {
                            // Instant-share file pull → save to Downloads. Pop the
                            // FRONT queued offer for its name/mime/id; if more
                            // remain, nudge so the next one pulls immediately.
                            let meta = crate::PENDING_FILE_OFFERS
                                .get()
                                .and_then(|m| m.lock().ok().and_then(|mut g| g.pop_front()));
                            if let Some((_, name, mime, id)) = meta {
                                match crate::clipboard_sync::apply_synced_file(
                                    app,
                                    &name,
                                    &mime,
                                    json.clone(),
                                )
                                .await
                                {
                                    Some(_) => crate::transfers::complete(id),
                                    None => crate::transfers::fail(id),
                                }
                            }
                            let more = crate::PENDING_FILE_OFFERS
                                .get()
                                .and_then(|m| m.lock().ok().map(|g| !g.is_empty()))
                                .unwrap_or(false);
                            if more {
                                if let Some(nudge) = crate::SYNC_NUDGE.get() {
                                    nudge.notify_one();
                                }
                            }
                        }
                        other => tracing::warn!(
                            "bulk-sync delivered unknown dataset 0x{other:02x}; ignoring"
                        ),
                    }
                }
                // If we requested a clipboard-image token this round but the
                // phone didn't serve it (it had evicted that token — a newer
                // copy replaced it), the CLIPBOARD_IMAGE arm above never cleared
                // it. Drop it now so we don't re-request a dead token on every
                // heartbeat forever. Guard on equality so a FRESH offer that
                // arrived mid-round survives.
                if let Some(req) = &requested_img_token {
                    if let Some(slot) = crate::PENDING_IMAGE_TOKEN.get() {
                        if let Ok(mut g) = slot.lock() {
                            if g.as_deref() == Some(req.as_str()) {
                                *g = None;
                            }
                        }
                    }
                }
                // Wi-Fi Direct: once every queued file is pulled over the group
                // link, hop back to the normal Wi-Fi; otherwise pull the next now.
                if wd_active() {
                    let empty = crate::PENDING_FILE_OFFERS
                        .get()
                        .and_then(|m| m.lock().ok().map(|g| g.is_empty()))
                        .unwrap_or(true);
                    if empty {
                        tracing::info!("Wi-Fi Direct: all files pulled → restoring Wi-Fi");
                        restore_wifi(app).await;
                    } else if let Some(n) = crate::SYNC_NUDGE.get() {
                        n.notify_one();
                    }
                }
                // SecretService D-Bus can stall here for hundreds of
                // ms when contended with the BLE adapter, which used
                // to hang the heartbeat (and therefore the whole
                // tokio runtime, since the audio-signal listener
                // shares the same workers). Move it off the hot
                // path. Same fix the BLE persistent loop applies.
                {
                    let store_c = peer_store.clone();
                    let peer_c = peer.peer_static_pub;
                    let val = outcome.peer_counter;
                    tokio::spawn(async move {
                        let _ = store_c.bump_counter(&peer_c, val);
                    });
                }

                // Bidirectional forget: if the peer set `revoked=true`
                // on the AppState we just received, they've asked us
                // to drop our trust record for them. Do it right here
                // and emit a fresh peer list so the UI clears.
                if let Some(s) = &outcome.peer_state {
                    if s.revoked {
                        tracing::info!(
                            "peer revoked us; forgetting {}",
                            hex::encode(&peer.peer_static_pub[..8])
                        );
                        let _ = peer_store.forget(&peer.peer_static_pub);
                        emit_peers(app, peer_store.clone()).await;
                        return Ok(None);
                    }
                    // Peer is asking us to claim the buds (they own them
                    // and tapped swap on their side). Run our initiator
                    // flow in a side-task so we don't block the heartbeat.
                    // We bail out early if the orchestrator is already
                    // in a flow — phone may set the flag on several
                    // consecutive heartbeats while waiting for us to
                    // notice, and starting parallel sessions opens TCPs
                    // that never get used.
                    // Phase 2 — phone's call-phase change. We pause /
                    // disconnect on `null` → `ringing|active`, and the
                    // orchestrator-state watcher elsewhere resumes
                    // MPRIS when the buds come back. The
                    // `audio_claim_request` flag (set by the phone on
                    // call end) drives the claim half of that flow,
                    // so we don't need to launch a separate session
                    // here — we just track the transition.
                    if let (Some(last_mu), Some(store)) =
                        (last_call_phase.as_ref(), media_store.as_ref())
                    {
                        let cur = s.call_phase.clone();
                        let prev = {
                            let mut g = last_mu.lock().await;
                            let prev = g.clone();
                            *g = cur.clone();
                            prev
                        };
                        tracing::info!(
                            ?prev,
                            ?cur,
                            "call_phase read from AppState"
                        );
                        if prev != cur {
                            let in_call = matches!(cur.as_deref(), Some("ringing") | Some("active"));
                            let was_in_call = matches!(prev.as_deref(), Some("ringing") | Some("active"));
                            // Keep the laptop media-watcher's call gate
                            // current so it suppresses auto-grabs during a
                            // call (calls outrank media).
                            if let Some(ic) = media_in_call.as_ref() {
                                ic.store(in_call, std::sync::atomic::Ordering::Relaxed);
                            }
                            if !was_in_call && in_call && ble_live {
                                // BLE is live → the BLE `Request` fast-path
                                // already paused + released for a REAL call.
                                // Acting on the LAN call_phase too would
                                // double-release, and worse, a stale
                                // `ringing` (no real call) would orphan the
                                // buds. Skip — BLE owns this transition.
                                tracing::info!(?cur, "call_phase ringing but BLE live; deferring to BLE Request fast-path (no LAN release)");
                            } else if !was_in_call && in_call {
                                // Call starting (BLE down) — pause + release.
                                tracing::info!(?cur, "phone entered a call; pausing media + releasing buds");
                                let store_c = store.clone();
                                tokio::spawn(async move {
                                    let paused = vortex_l3_daemon::core::media_runtime::pause_playing_for_call(&store_c).await;
                                    if !paused.is_empty() {
                                        tracing::info!(?paused, "paused for call");
                                    }
                                });
                                // Release the buds so the phone can grab
                                // them. Fire-and-forget — phone is
                                // already trying its own connect retries.
                                // Reuse the shared adapter; per-tick
                                // `bluer::Session::new()` was leaking
                                // D-Bus connections and hanging the
                                // runtime after a handful of calls.
                                if let (Some(saved), Some(adapter)) = (
                                    vortex_l3_daemon::core::earbuds_store::load(),
                                    shared_adapter.clone(),
                                ) {
                                    let mac = saved.address;
                                    tokio::spawn(async move {
                                        if let Err(e) =
                                            vortex_l3_daemon::core::audio_switch::disconnect_audio(
                                                &adapter, &mac,
                                            )
                                            .await
                                        {
                                            tracing::debug!("call-start disconnect: {e}");
                                        }
                                    });
                                }
                            }
                            // The call-end case (was_in_call → !in_call)
                            // is handled by audio_claim_request — phone
                            // sets that flag alongside clearing
                            // call_phase, so the existing claim path
                            // fires and the orchestrator-state watcher
                            // resumes media on the resulting Idle.
                        }
                    }

                    // Phase 3 — peer started playing media: hand the buds
                    // over so the phone can grab them. Mirrors the
                    // call_phase release but driven by the advisory
                    // `media_playing` flag, so it works even with no live
                    // AudioOp writer (the phone's own connect-retry lands
                    // once we drop the ACL). Only release if we're NOT
                    // ourselves playing — don't yank the buds out of an
                    // active laptop session — and we currently hold them.
                    if let Some(mw) = media_watch.as_ref() {
                        use std::sync::atomic::Ordering;
                        let peer_now = s.media_playing;
                        mw.peer_playing.store(peer_now, Ordering::Relaxed);
                        // Last-play-wins: re-anchor the phone's play AGE to OUR
                        // monotonic clock (`mono_now - age`) so the compare is
                        // clock-skew immune, remember it for the grab gate, and
                        // use it to gate the RELEASE below — only hand the buds
                        // over if the phone played MORE recently than us (a
                        // greater re-anchored epoch) or we're idle.
                        let peer_epoch_mono = if s.media_play_age_ms > 0 && s.media_playing {
                            vortex_l3_daemon::core::media_watch::mono_ms()
                                .saturating_sub(s.media_play_age_ms)
                        } else {
                            0
                        };
                        mw.peer_play_epoch_mono
                            .store(peer_epoch_mono, Ordering::Relaxed);
                        let our_epoch = mw.play_epoch_mono.load(Ordering::Relaxed);
                        let peer_played_last = our_epoch == 0
                            || (peer_epoch_mono != 0 && peer_epoch_mono > our_epoch);
                        // Liveness + "buds on the phone" signal for the grab
                        // gate: stamp now when the phone reports the buds
                        // connected to it, clear otherwise. The watcher only
                        // auto-grabs while this is fresh.
                        if let Ok(mut g) = mw.peer_holds_buds_seen.lock() {
                            *g = if s.earbuds.as_ref().map(|e| e.connected).unwrap_or(false) {
                                Some(std::time::Instant::now())
                            } else {
                                None
                            };
                        }
                        // Shared smart-switch setting, LWW: adopt the phone's
                        // value when its timestamp is strictly newer than
                        // ours. apply_setting persists + no-ops on a stale ts.
                        // On adoption, tell the UI so the Settings toggle
                        // tracks the change live.
                        if mw.apply_setting(s.smart_switch_enabled, s.smart_switch_changed_at) {
                            tracing::info!(
                                enabled = s.smart_switch_enabled,
                                "smart-switch: adopted peer setting (LWW)"
                            );
                            let _ = app.emit("vortex:smart_switch", s.smart_switch_enabled);
                        }
                        let in_call_now = matches!(
                            s.call_phase.as_deref(),
                            Some("ringing") | Some("active")
                        );
                        // Level-triggered (NOT a one-shot edge): while the
                        // phone is the media device AND we still hold the
                        // buds (the audio_active check below), keep releasing
                        // them each heartbeat until they actually leave. A
                        // one-shot rising edge would be lost forever if we
                        // were mid-switch when it fired — a core cause of
                        // "sometimes doesn't switch". Disconnect is idempotent.
                        if peer_now
                            && peer_played_last
                            && mw.enabled.load(Ordering::Relaxed)
                            && !in_call_now
                        {
                            if let (Some(saved), Some(adapter)) = (
                                vortex_l3_daemon::core::earbuds_store::load(),
                                shared_adapter.clone(),
                            ) {
                                let mac = saved.address;
                                if let Ok(addr) = mac.parse::<bluer::Address>() {
                                    // Only release if we actually hold the buds.
                                    if vortex_l3_daemon::core::audio_switch::audio_active(
                                        &adapter, addr,
                                    )
                                    .await
                                    {
                                        tracing::info!(%mac, "peer started media; releasing buds so the phone can grab");
                                        tokio::spawn(async move {
                                            let _ = vortex_l3_daemon::core::audio_switch::disconnect_audio_initiate(&adapter, &mac).await;
                                        });
                                    }
                                }
                            }
                        }
                    }

                    if s.audio_claim_request {
                        let already_busy = switch_orchestrator
                            .as_ref()
                            .map(|o| *o.state().borrow() != vortex_l3_daemon::core::audio_orchestrator::SwitchState::Idle)
                            .unwrap_or(false);
                        if already_busy {
                            tracing::info!("peer set audio_claim_request but we're busy; ignoring");
                        } else if let (Some(orch), Some(writers)) =
                            (switch_orchestrator.clone(), session_writers.clone())
                        {
                            tracing::info!("peer set audio_claim_request; running initiator");
                            let peer_c = peer.clone();
                            let identity_priv = identity.static_priv.0;
                            let peer_store_c = peer_store.clone();
                            let addr_target = outcome.remote;
                            let mac_addr = vortex_l3_daemon::core::earbuds_store::load()
                                .map(|s| s.address)
                                .unwrap_or_default();
                            if mac_addr.is_empty() {
                                tracing::warn!("audio_claim_request: no saved earbuds MAC; skipping");
                            } else {
                                tokio::spawn(async move {
                                    let local_counter = peer_store_c
                                        .load_counter(&peer_c.peer_static_pub)
                                        .unwrap_or(0);
                                    let _ = vortex_l3_daemon::core::audio_lan_session::start_initiator_session(
                                        addr_target,
                                        &identity_priv,
                                        &peer_c.peer_static_pub,
                                        &peer_c.prs,
                                        local_counter,
                                        mac_addr,
                                        orch,
                                        writers,
                                    ).await;
                                });
                            }
                        }
                    }
                }

                if let Some(state) = outcome.peer_state.clone() {
                    // Auto-pin the peer's earbuds locally (no-op unless they're
                    // connected, carry an address, and we have none saved) — so
                    // the card appears on this device too, over LAN as well.
                    crate::earbuds::persist_peer_earbuds(&state);
                    // Tray tooltip + battery menu rows: live battery + which
                    // device holds the buds (shared helper in tray.rs).
                    crate::tray::update_battery_rows(
                        &app,
                        local_state.earbuds.as_ref(),
                        Some(&state),
                    );
                    // Additive call-mirror path: feed the call carried in this
                    // AppState (over LAN) into the call consumer so the banner/
                    // pill survive a BLE drop mid-call. Deduped by (id, phase).
                    dispatch_appstate_call(&state.call);
                    // Additive browsing-handoff path (LAN backstop): the page the
                    // phone is on → the "continue" pill survives a BLE drop and
                    // stays fresh. Consumer dedups by URL.
                    crate::handoff::dispatch_appstate_handoff(&state.handoff);
                    // Laptop→phone screen mirror: start/stop casting our screen
                    // off the phone's view-request level (edge-tracked).
                    crate::laptop_cast::dispatch_request(state.laptop_mirror_req, state.laptop_mirror_extend);
                    // Continuity Camera: dial the phone's camera into the v4l2
                    // webcam when it offers (we requested it); stop when it ends.
                    crate::camera::dispatch_offer(
                        &state.camera_offer,
                        LAST_GOOD_PEER_IP.lock().ok().and_then(|g| *g),
                    );
                    // Remote lock/unlock button on the phone (seq-deduped).
                    dispatch_lock_command(&state);
                    // Media transport button on the phone's laptop-media
                    // notification (seq-deduped) → MPRIS.
                    crate::media_remote::dispatch_media_command(&state);
                    // Owner-present gate: record the phone's unlock state for
                    // proximity auto-unlock.
                    crate::proximity::note_phone_unlocked(state.unlocked);
                    // A LAN sync proves we're in live contact (any-transport) —
                    // gates the disconnect-clear of mirror pills.
                    crate::ble::touch_peer_contact();
                    let dto = app_state_to_dto(hex::encode(peer.peer_static_pub), state);
                    let _ = app.emit("vortex:peer_state", dto);
                }
                return Ok(outcome.peer_state);
            }
            Err(e) => {
                last_err = Some(format!("lan: {e}"));
            }
        }
    }
    // Every peer failed on this address. If the cached-IP fast path picked it
    // (a TCP probe alone can't prove it's still OUR phone — the lease may have
    // moved to another device), drop the cache so the next tick does full
    // mDNS/gateway discovery instead of wedging on a poisoned fast path.
    if fast_path.is_some() {
        *LAST_GOOD_PEER_IP.lock().unwrap_or_else(|e| e.into_inner()) = None;
        tracing::info!("fast-path IP failed the handshake; cache dropped for rediscovery");
    }
    Err(last_err.unwrap_or_else(|| "no peer accepted reconnect".to_string()))
}



/// Watch logind's `LockedHint` and nudge BOTH state heartbeats (LAN +
/// BLE) the moment the lock screen flips — whether from a phone command
/// or a local Super+L — so the phone's lock icon tracks reality in ~1s
/// instead of going stale until the next periodic beat (the staleness
/// made a fresh "lock" tap look like an unlock and prompt for biometrics).
pub(crate) fn spawn_locked_watch(sync_nudge: std::sync::Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        let res = vortex_l3_daemon::core::session_lock::watch_locked_hint(move |locked| {
            tracing::info!(locked, "session LockedHint changed; nudging state heartbeats");
            sync_nudge.notify_waiters();
            crate::ble::state_nudge().notify_one();
        })
        .await;
        if let Err(e) = res {
            tracing::warn!("locked-hint watch unavailable: {e}");
        }
    });
}

pub(crate) fn spawn_power_watcher(sync_nudge: std::sync::Arc<tokio::sync::Notify>) {
                let watch_nudge = sync_nudge.clone();
                tokio::spawn(async move {
                    use vortex_l3_daemon::core::status::{read_local_battery, read_local_charging};
                    let mut last_charging = read_local_charging();
                    let mut last_level = read_local_battery().0;
                    loop {
                        // 500 ms keeps the laptop→phone detection snappy; the
                        // read is two tiny sysfs files so the poll is near-free.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let charging = read_local_charging();
                        let level = read_local_battery().0;
                        // Charging flip → push instantly. Battery % → only on a
                        // >=2-point delta so a slow drain doesn't spam syncs.
                        let level_changed = match (last_level, level) {
                            (Some(a), Some(b)) => (a as i16 - b as i16).abs() >= 2,
                            (None, Some(_)) | (Some(_), None) => true,
                            (None, None) => false,
                        };
                        if charging != last_charging || level_changed {
                            last_charging = charging;
                            last_level = level;
                            tracing::info!(charging, ?level, "power change → nudging heartbeat");
                            watch_nudge.notify_one();
                        }
                    }
                });
}


pub(crate) fn spawn_heartbeat(
    app: tauri::AppHandle,
    identity: IdentityRecord,
    peer_store: std::sync::Arc<dyn PeerStore>,
    auto_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    switch_orchestrator: std::sync::Arc<vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator>,
    session_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
    media_store: vortex_l3_daemon::core::media_runtime::MediaStateStore,
    last_call_phase: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    media_watch: std::sync::Arc<vortex_l3_daemon::core::media_watch::MediaWatch>,
    media_in_call: std::sync::Arc<std::sync::atomic::AtomicBool>,
    adapter: bluer::Adapter,
    last_reconnect_at: std::sync::Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
    sync_nudge: std::sync::Arc<tokio::sync::Notify>,
    ble_audio_writers: vortex_l3_daemon::core::audio_lan_session::SessionWriterMap,
) {
            let auto_app = app.clone();
            let auto_identity = identity.clone();
            let auto_peer_store = peer_store.clone();
            let auto_lock_clone = auto_lock.clone();
            let auto_orch = switch_orchestrator.clone();
            let auto_writers = session_writers.clone();
            let auto_media = media_store.clone();
            let auto_last_phase = last_call_phase.clone();
            let auto_media_watch = media_watch.clone();
            let auto_media_in_call = media_in_call.clone();
            let auto_adapter = adapter.clone();
            let auto_last_reconnect = last_reconnect_at.clone();
            let auto_nudge = sync_nudge.clone();
            // Shared with the BLE persistent loop: it inserts a writer here
            // once its IK session is up and removes it on drop, so a
            // non-empty map means "BLE link is live". The heartbeat uses
            // that to back off (BLE already provides liveness + the fast
            // call-signal path).
            let auto_ble_writers = ble_audio_writers.clone();
            tokio::spawn(async move {
                let mut consec_lan_fail = 0u32;
                loop {
                    let (had_trust, lan_synced) = {
                        let _g = auto_lock_clone.lock().await;
                        // SecretService's sync trait calls `block_in_place`
                        // internally — when several callers (heartbeat,
                        // BLE persistent loop, audio-op nonces) all hit
                        // libsecret at once the executor wedges. Push the
                        // call to the blocking pool so the worker thread
                        // keeps spinning timers / IO.
                        let have_trust = {
                            let store = auto_peer_store.clone();
                            tokio::task::spawn_blocking(move || {
                                !store.list().unwrap_or_default().is_empty()
                            })
                            .await
                            .unwrap_or(false)
                        };
                        let mut synced = false;
                        if have_trust {
                            let ble_live = !auto_ble_writers.lock().await.is_empty();
                            synced = matches!(
                                try_lan_reconnect(
                                    &auto_app,
                                    &auto_identity,
                                    auto_peer_store.clone(),
                                    Some(auto_orch.clone()),
                                    Some(auto_writers.clone()),
                                    Some(auto_media.clone()),
                                    Some(auto_last_phase.clone()),
                                    ble_live,
                                    Some(auto_adapter.clone()),
                                    Some(auto_media_watch.clone()),
                                    Some(auto_media_in_call.clone()),
                                )
                                .await,
                                Ok(Some(_))
                            );
                            *auto_last_reconnect.lock().await =
                                Some(tokio::time::Instant::now());
                        }
                        (have_trust, synced)
                    };
                    // A FRESH LAN drop reconnects fast (2 s) instead of waiting
                    // a full tick — but only a few times: if LAN is genuinely
                    // absent (BLE-only / AP isolation / hotspot) back off so we
                    // don't spin a TCP+IK every 2 s forever.
                    if lan_synced {
                        // LAN down→up edge = "the phone just appeared on the
                        // network" — the cross-transport presence hint.
                        // Wake the BLE presence wait so it retries its direct
                        // connect now instead of riding out its backoff.
                        // Edge-triggered on purpose: a steady-state LAN tick
                        // must not poke BLE every cycle when the phone simply
                        // has Bluetooth off.
                        if consec_lan_fail > 0 {
                            if let Some(n) = crate::BLE_RETRY_NUDGE.get() {
                                n.notify_one();
                            }
                        }
                        consec_lan_fail = 0;
                    } else if had_trust {
                        consec_lan_fail = consec_lan_fail.saturating_add(1);
                    }
                    // Adaptive cadence — the seamless-continuity shape: BLE is
                    // the signalling plane, Wi-Fi comes up on demand. While
                    // the persistent BLE link is live it carries liveness,
                    // state pushes AND the ~200 ms call signal, so the LAN
                    // tick's only job is keeping the cached-IP fast path
                    // warm — 4 min is plenty (~20× fewer phone Wi-Fi wakes
                    // than 12 s). Safe to sleep that long because the BLE
                    // loop nudges us the moment its link drops. With no BLE,
                    // LAN is the only liveness/hand-off path → stay brisk
                    // at 12 s.
                    let next = if crate::call::call_pill_active() {
                        // A call is mirrored on the laptop right now. The pill's
                        // timer ticks locally and only stops on an explicit
                        // `ended`, which rides this AppState as `call=None`. Poll
                        // briskly so the end clears the pill within ~2 s even
                        // when the BLE fast-path call signal isn't reaching us
                        // (no AUDIO_SIGNAL subscription / call-audio radio
                        // contention) — otherwise the idle cadence below would
                        // let the timer run on long after the caller hung up.
                        Duration::from_secs(2)
                    } else if had_trust && !lan_synced && consec_lan_fail <= 3 {
                        Duration::from_secs(2)
                    } else if auto_ble_writers.lock().await.is_empty() {
                        Duration::from_secs(12)
                    } else {
                        Duration::from_secs(240)
                    };
                    // Wake on the tick OR the moment a local state change
                    // nudges us — whichever comes first (hybrid event push).
                    tokio::select! {
                        _ = tokio::time::sleep(next) => {}
                        _ = auto_nudge.notified() => {
                            tracing::info!("heartbeat woken early by local state-change nudge");
                        }
                    }
                }
            });
}
