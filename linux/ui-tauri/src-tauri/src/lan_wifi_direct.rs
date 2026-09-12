//! Wi-Fi Direct fast transfer (instant-share style) — split out of `lan.rs`. The phone
//! makes a 5 GHz P2P group; the laptop joins it (nmcli), pulls pending files
//! over the ~20 MB/s direct link, then restores its normal Wi-Fi (single adapter
//! ⇒ briefly offline). `lan.rs`'s heartbeat reads `wd_active()` to target the GO
//! IP, and calls `restore_wifi` once the pull drains.

use std::time::Duration;

use tauri::{AppHandle, Emitter};

// --------------------------------------------------------------------------
// Wi-Fi Direct fast transfer (instant-share style): the phone makes a 5 GHz P2P group;
// the laptop joins it, pulls pending files over the direct link (~20 MB/s vs the
// router path), then restores its normal Wi-Fi. Single adapter ⇒ briefly offline.
// --------------------------------------------------------------------------

/// Android always puts the P2P group owner here; `LanServer` binds all ifaces.
pub(crate) const WIFI_DIRECT_GO_IP: [u8; 4] = [192, 168, 49, 1];

pub(crate) struct WdState {
    /// The Wi-Fi connection to restore after the transfer (None = unknown).
    saved_wifi: Option<String>,
}

pub(crate) static WIFI_DIRECT: std::sync::Mutex<Option<WdState>> = std::sync::Mutex::new(None);

/// When the pull queue last went empty while we were on the group link.
/// `None` = not idle (files pending, or not on the group).
static WD_IDLE_SINCE: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// How long the group link is held after the pull queue drains.
///
/// Restoring the instant the queue emptied made a paced batch thrash the Wi-Fi:
/// the phone releases files in windows, so the queue legitimately goes empty
/// between them, and each gap produced a full leave-group + rejoin. Observed
/// live — restore at 15:16:53, rejoin at 15:16:57 — one disconnect/reconnect
/// notification per gap, dozens over a 65-file share.
///
/// Sized under the phone's 60 s idle GO teardown, so we let go before the group
/// disappears underneath us, and the 60 s force-restore watchdog still bounds
/// the worst case.
const WD_IDLE_GRACE: Duration = Duration::from_secs(25);

/// How long a pull may make NO progress before the watchdog restores Wi-Fi.
/// Bounds a genuinely stuck transfer without capping a healthy long one.
const WD_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Restore the normal Wi-Fi only once the queue has been empty for
/// [`WD_IDLE_GRACE`]. Call on every heartbeat round while on the group link.
///
/// `queue_empty` is passed in rather than read here so the caller's notion of
/// "queued" stays the single source of truth.
pub(crate) async fn restore_when_idle(app: &AppHandle, queue_empty: bool) {
    if !wd_active() {
        if let Ok(mut g) = WD_IDLE_SINCE.lock() {
            *g = None;
        }
        return;
    }
    if !queue_empty {
        // More to pull — hold the link and reset the idle clock.
        if let Ok(mut g) = WD_IDLE_SINCE.lock() {
            *g = None;
        }
        return;
    }
    let elapsed = {
        let Ok(mut g) = WD_IDLE_SINCE.lock() else { return };
        let since = g.get_or_insert_with(std::time::Instant::now);
        since.elapsed()
    };
    if elapsed < WD_IDLE_GRACE {
        tracing::debug!(
            idle_s = elapsed.as_secs(),
            "Wi-Fi Direct: queue empty but holding the group link"
        );
        return;
    }
    tracing::info!(
        idle_s = elapsed.as_secs(),
        "Wi-Fi Direct: idle past the grace window → restoring Wi-Fi"
    );
    if let Ok(mut g) = WD_IDLE_SINCE.lock() {
        *g = None;
    }
    restore_wifi(app).await;
}

pub(crate) fn wd_active() -> bool {
    WIFI_DIRECT.lock().map(|g| g.is_some()).unwrap_or(false)
}

async fn nmcli(args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("nmcli")
        .args(args)
        .output()
        .await
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// The active Wi-Fi connection name (to restore later).
async fn current_wifi() -> Option<String> {
    let s = nmcli(&["-t", "-f", "TYPE,CONNECTION", "device", "status"]).await?;
    s.lines()
        .find_map(|l| l.strip_prefix("wifi:").map(str::to_string))
        .filter(|c| !c.is_empty() && c != "--")
}

/// nmcli that also returns stderr (for diagnosing a failed join).
async fn nmcli_err(args: &[&str]) -> Result<(), String> {
    match tokio::process::Command::new("nmcli").args(args).output().await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Join the phone's P2P group. A fresh 5 GHz GO isn't in the cached scan, so
/// rescan before EACH attempt. A STALE NM profile for this SSID (left by a prior
/// join) loses its security config → "key-mgmt property is missing"; delete it
/// first so `dev wifi connect` recreates it cleanly from the live scan.
async fn join_go(ssid: &str, pass: &str) -> bool {
    let _ = nmcli(&["con", "delete", ssid]).await; // ignore "unknown connection"
    for attempt in 1..=5 {
        let _ = nmcli(&["dev", "wifi", "rescan"]).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        match nmcli_err(&["dev", "wifi", "connect", ssid, "password", pass]).await {
            Ok(()) => return true,
            Err(e) => {
                tracing::warn!(attempt, "Wi-Fi Direct join attempt failed: {e}");
                // Drop the half-made profile so the next attempt starts clean.
                let _ = nmcli(&["con", "delete", ssid]).await;
            }
        }
    }
    false
}

/// Restore the saved Wi-Fi and clear the Wi-Fi-Direct state. Idempotent.
pub(crate) async fn restore_wifi(app: &AppHandle) {
    let saved = WIFI_DIRECT
        .lock()
        .ok()
        .and_then(|mut g| g.take())
        .and_then(|s| s.saved_wifi);
    if let Some(name) = saved {
        let _ = nmcli(&["con", "up", &name]).await;
        tracing::info!(name = %name, "Wi-Fi Direct: Wi-Fi restored");
    } else {
        // We never captured the pre-join Wi-Fi (current_wifi() failed at join
        // time), so there's no specific connection to bring back. With a single
        // adapter the laptop is otherwise stranded on the GO group — force the
        // Wi-Fi radio to re-associate with the strongest known AP so it doesn't
        // stay offline. Best-effort: a radio off/on triggers NM auto-connect.
        tracing::warn!("Wi-Fi Direct: no saved Wi-Fi to restore; cycling radio to auto-reconnect");
        let _ = nmcli(&["radio", "wifi", "off"]).await;
        let _ = nmcli(&["radio", "wifi", "on"]).await;
    }
    let _ = app.emit("vortex:wifi-direct", false);
}

/// Are we already on the same local network as `peer`, over a link fast enough
/// that a P2P group would not be worth a disconnect?
///
/// Wi-Fi Direct costs BOTH devices their AP association — unavoidable with one
/// radio — so it should only be paid when the ordinary path cannot do the job.
/// Two devices on the same AP already have a perfectly good route through it.
///
/// Portable by construction, because the Windows port needs this too:
///
///  * the local address that would reach `peer` comes from a connected UDP
///    socket. `connect` on UDP sends nothing; it just asks the routing table,
///    and `local_addr` then reports the answer. Works the same on Windows.
///  * the netmask for that address comes from `if_addrs`, which wraps
///    `getifaddrs` on Unix and `GetAdaptersAddresses` on Windows.
///
/// The one part that is genuinely platform-specific is deciding whether an
/// interface is a *fast* LAN link, which is why it lives in
/// [`is_fast_lan_iface`] on its own.
fn peer_on_same_fast_lan(peer: std::net::IpAddr) -> bool {
    let Some(local) = local_addr_toward(peer) else {
        return false; // no route we can name → let Wi-Fi Direct try
    };
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return false;
    };
    for iface in ifaces {
        if iface.addr.ip() != local {
            continue;
        }
        if !is_fast_lan_iface(&iface.name) {
            tracing::debug!(
                iface = %iface.name,
                "route to peer is not a fast LAN link; Wi-Fi Direct still worthwhile"
            );
            return false;
        }
        // Same interface AND same subnet: the AP path already reaches them.
        let same_subnet = match (iface.addr, peer) {
            (if_addrs::IfAddr::V4(v4), std::net::IpAddr::V4(p)) => {
                let mask = u32::from(v4.netmask);
                u32::from(v4.ip) & mask == u32::from(p) & mask
            }
            // v6 link-local/ULA subnetting is not what this decision hinges on.
            _ => false,
        };
        if same_subnet {
            tracing::info!(
                iface = %iface.name, %peer,
                "peer already reachable on our LAN — skipping Wi-Fi Direct"
            );
            return true;
        }
    }
    false
}

/// Which local address the OS would use to reach `peer`.
///
/// A connected UDP socket is the portable way to ask: no packet is sent, the
/// kernel just does the route lookup so `local_addr` can report the source it
/// would pick. Port 9 (discard) is conventional for this and never contacted.
fn local_addr_toward(peer: std::net::IpAddr) -> Option<std::net::IpAddr> {
    let bind: &str = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = std::net::UdpSocket::bind(bind).ok()?;
    sock.connect((peer, 9)).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// Is `name` a link fast enough that the AP path beats a P2P group?
///
/// The interesting exclusion is **Bluetooth PAN**: the phone and laptop can be
/// on a PAN at the same time as Wi-Fi, and a PAN route shares no subnet with
/// the AP while being far too slow to treat as "already on the LAN". Cellular
/// and VPN/tunnel links are excluded for the same reason.
///
/// `if_addrs` does not report interface *type*, so this is the platform-
/// specific part. On Linux `/sys/class/net/<if>/phy80211` positively identifies
/// 802.11, and everything not on the denylist is assumed to be a wired NIC.
///
/// TODO(windows): replace the denylist with the real thing —
/// `GetAdaptersAddresses` reports `IfType`, so accept `IF_TYPE_ETHERNET_CSMACD`
/// and `IF_TYPE_IEEE80211` and reject the rest. `netdev` would also expose it
/// cross-platform if a dependency is preferable to the cfg split.
fn is_fast_lan_iface(name: &str) -> bool {
    // Slow or virtual links, by conventional naming. Deliberately a denylist:
    // a misnamed fast link only costs us a pointless P2P group, while a missed
    // PAN would silently route a big transfer over Bluetooth.
    const SLOW: [&str; 6] = ["bnep", "ppp", "wwan", "rmnet", "tun", "tap"];
    if SLOW.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // The P2P interface is Wi-Fi too — it must never count as "the LAN we are
    // already on", or joining would look unnecessary from inside the group.
    if name.starts_with("p2p") {
        return false;
    }
    true
}

/// Hook target (set in the worker): the phone offered a P2P group. If files are
/// pending, switch onto it so the heartbeat pulls them over the fast link.
pub(crate) fn on_wifi_direct_offer(app: AppHandle, ssid: String, pass: String) {
    let pending = crate::PENDING_FILE_OFFERS
        .get()
        .and_then(|m| m.lock().ok().map(|g| !g.is_empty()))
        .unwrap_or(false);
    if !pending || wd_active() {
        return;
    }
    // Already on the same LAN as the phone? Then the AP path already reaches
    // it, and a P2P group would buy little while costing BOTH devices their AP
    // association — one radio each, so that is unavoidable. Observed: a
    // 65-file share on a shared network switched networks purely because one
    // file happened to exceed the size trigger.
    if let Some(peer_ip) = crate::lan::last_good_peer_ip() {
        if peer_on_same_fast_lan(peer_ip) {
            tracing::info!(
                %peer_ip,
                "Wi-Fi Direct offer ignored: peer is on our LAN already"
            );
            return;
        }
    }
    tokio::spawn(async move {
        let saved = current_wifi().await;
        tracing::info!(?saved, %ssid, "Wi-Fi Direct: joining group for fast pull");
        if !join_go(&ssid, &pass).await {
            tracing::warn!("Wi-Fi Direct: join failed; staying on router path");
            return;
        }
        if let Ok(mut g) = WIFI_DIRECT.lock() {
            *g = Some(WdState { saved_wifi: saved });
        }
        let _ = app.emit("vortex:wifi-direct", true);
        if let Some(n) = crate::SYNC_NUDGE.get() {
            n.notify_one(); // pull now over the GO
        }
        // Watchdog: never strand the laptop on the GO (failed pull / lost link).
        //
        // Polls for STALL rather than sleeping out a fixed deadline. A flat 60 s
        // from join force-restored in the middle of any batch that legitimately
        // took longer — observed cutting a 65-file share at file 26, costing the
        // one Wi-Fi disconnect/reconnect that survived the idle-grace fix. What
        // must be caught is a pull making no progress, which is precisely what
        // `file_pull_active()` already answers (queued AND progressing).
        let app2 = app.clone();
        tokio::spawn(async move {
            let joined_at = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if !wd_active() {
                    return; // restored elsewhere (idle grace, or a new join)
                }
                // With nothing queued there is nothing to be stuck on — the
                // idle-grace path owns that decision, and stepping on it here
                // is what cut a healthy batch short.
                if !crate::lan::files_queued() {
                    continue;
                }
                // Files ARE queued: measure real progress. `queue_progress_age`
                // is the time since one last COMPLETED, which survives the
                // queue oscillating empty→full under a paced sender.
                let stalled = crate::lan::queue_progress_age()
                    .map(|age| age >= WD_STALL_TIMEOUT)
                    // Nothing has ever completed — fall back to time on the
                    // group so a join that never delivers still gets unstuck.
                    .unwrap_or(joined_at.elapsed() >= WD_STALL_TIMEOUT);
                if stalled {
                    tracing::warn!(
                        "Wi-Fi Direct: queued pull made no progress for {}s → force-restore Wi-Fi",
                        WD_STALL_TIMEOUT.as_secs()
                    );
                    restore_wifi(&app2).await;
                    return;
                }
            }
        });
    });
}
