//! Classic-BT (A2DP) earbuds switch primitives for the Phase 1 manual
//! switch flow per the earbuds-switch design notes §7.1.
//!
//! **S2 — direct D-Bus via `bluer`.** The old ecosystem shelled out to
//! `bluetoothctl` and `busctl`. Each spawn cost ~80-150 ms (process
//! fork + bash parse). We talk to BlueZ directly through `bluer`,
//! which wraps `org.bluez.Device1.{Connect,Disconnect,ConnectProfile,
//! DisconnectProfile}` as native async calls. Three BT ops per switch
//! (disconnect + connect + ready-check) saves ~450 ms.
//!
//! **Profile-targeted disconnect/connect.** Plain `Device.disconnect()`
//! tears down ALL profiles including GATT — which would close our BLE
//! Pairing/Reconnect transport. We disconnect ONLY the A2DP profile
//! (`0000110b-…`) so the Vortex BLE link stays up alongside the audio
//! handoff. Same idea on connect.

use std::time::Duration;

use bluer::{Adapter, Address};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// A2DP Sink — what the buds expose for media audio.
/// (Bluetooth SIG assigned Service Class UUID.)
const A2DP_SINK_UUID: Uuid = Uuid::from_u128(0x0000_110b_0000_1000_8000_0080_5f9b_34fb);

/// HFP Audio Gateway — voice path. Some buds support only HFP for
/// call audio, separate from the A2DP media path.
const HFP_AG_UUID: Uuid = Uuid::from_u128(0x0000_111f_0000_1000_8000_0080_5f9b_34fb);

/// HFP Hands-Free — the role the EARBUDS advertise (0x111e), as opposed to
/// [`HFP_AG_UUID`] (0x111f), which is the laptop's own side of the same
/// profile. These buds list Handsfree, so this is the UUID that names a
/// profile actually present on the remote device.
const HFP_HF_UUID: Uuid = Uuid::from_u128(0x0000_111e_0000_1000_8000_0080_5f9b_34fb);

#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error("bad MAC address: {0}")]
    BadAddress(String),
    #[error("bluer: {0}")]
    Bluer(#[from] bluer::Error),
    #[error("device not paired with this adapter")]
    NotPaired,
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("internal: {0}")]
    Internal(String),
}

/// Disconnect ONLY the audio profiles (A2DP + HFP). Leaves any active
/// BLE / GATT links alone so the Vortex transport stays up.
///
/// Idempotent: if the profiles are already disconnected, returns Ok.
pub async fn disconnect_audio(adapter: &Adapter, mac: &str) -> Result<(), SwitchError> {
    let addr: Address = mac.parse().map_err(|_| SwitchError::BadAddress(mac.into()))?;
    let device = adapter.device(addr)?;
    if !device.is_paired().await.unwrap_or(false) {
        return Err(SwitchError::NotPaired);
    }

    // Disconnect the WHOLE device (org.bluez.Device1.Disconnect), not
    // each audio profile separately. `DisconnectProfile` does a *graceful*
    // per-profile teardown that can take several SECONDS on these buds —
    // long enough that the phone's call hand-off gives up and falls back
    // to speakerphone (observed ~6-8 s laptop→phone). `Device.Disconnect`
    // forcefully drops the ACL link in ~50 ms, which is what gave the
    // reference build (ecosystem 63eeaee) its ~1 s laptop→phone hand-off.
    // Idempotent: a device that isn't connected just returns ok.
    match device.disconnect().await {
        Ok(()) => debug!(%addr, "device disconnect ok"),
        Err(e) if is_not_connected(&e) => {
            debug!(%addr, "device already disconnected — treating as ok");
        }
        Err(e) => warn!(%addr, "device disconnect failed: {e}"),
    }

    if !wait_audio_disconnected(adapter, addr, DISCONNECT_TIMEOUT).await {
        return Err(SwitchError::Timeout(DISCONNECT_TIMEOUT));
    }
    info!(%addr, "audio device disconnected");
    Ok(())
}

/// Initiate the ACL drop and return the instant BlueZ *accepts* the
/// `Device1.Disconnect` call (~50 ms), WITHOUT waiting for the audio
/// profiles to finish settling.
///
/// This exists for the call hand-off RELEASE path: the responder wants
/// to signal the phone "buds are free" as early as physically possible
/// so the phone can fire (and queue) its A2DP connect — BlueZ holds the
/// phone's connect request and lands it the moment the buds actually
/// drop. Waiting for [`wait_audio_disconnected`] (which polls
/// `audio_active` and lags the real drop by up to ~1 s) before telling
/// the phone wastes that whole window. Pair this with a background
/// [`confirm_audio_disconnected`] for our own state hygiene.
///
/// Returns Ok if the disconnect was accepted (or the device was already
/// disconnected), Err only on a genuine BlueZ failure.
pub async fn disconnect_audio_initiate(adapter: &Adapter, mac: &str) -> Result<(), SwitchError> {
    let addr: Address = mac.parse().map_err(|_| SwitchError::BadAddress(mac.into()))?;
    let device = adapter.device(addr)?;
    if !device.is_paired().await.unwrap_or(false) {
        return Err(SwitchError::NotPaired);
    }
    match device.disconnect().await {
        Ok(()) => {
            debug!(%addr, "device disconnect accepted (initiate)");
            Ok(())
        }
        Err(e) if is_not_connected(&e) => {
            debug!(%addr, "device already disconnected — treating as ok");
            Ok(())
        }
        Err(e) => {
            warn!(%addr, "device disconnect failed: {e}");
            Err(SwitchError::Internal(e.to_string()))
        }
    }
}

/// Block until the audio profiles for `mac` have fully dropped, or the
/// timeout elapses. Returns true if disconnected. Public companion to
/// [`disconnect_audio_initiate`] for callers that send their fast
/// signal first and confirm afterwards.
pub async fn confirm_audio_disconnected(adapter: &Adapter, mac: &str, timeout: Duration) -> bool {
    let Ok(addr) = mac.parse::<Address>() else { return false };
    wait_audio_disconnected(adapter, addr, timeout).await
}

/// Connect the A2DP profile (preferred for media). Falls back to HFP
/// if A2DP fails — some buds expose only the voice path.
///
/// **Single-shot.** Retry policy lives in the orchestrator
/// ([`audio_orchestrator::SwitchOrchestrator::attempt_connect`]) so we
/// don't end up multiplying timeouts (a 3×3×4s nested retry was
/// turning the worst case into 36 s of dead silence). Returns Ok the
/// instant either A2DP or HFP shows a live `bluez_*` sink within
/// `CONNECT_SETTLE`; otherwise propagates the last underlying error.
pub async fn connect_audio(adapter: &Adapter, mac: &str) -> Result<(), SwitchError> {
    let addr: Address = mac.parse().map_err(|_| SwitchError::BadAddress(mac.into()))?;
    let device = adapter.device(addr)?;
    if !device.is_paired().await.unwrap_or(false) {
        return Err(SwitchError::NotPaired);
    }

    // ---- Multipoint fast path ----
    // If the buds ALREADY expose a live A2DP sink here, there's nothing to
    // (re)connect: just ensure the card is on A2DP and return. The
    // downstream route step makes us the default sink and the buds switch
    // their active stream to us when playback starts — skipping the whole
    // ~3.5s single-point drop+reconnect (`wait_ms`).
    //
    // This is what makes MULTIPOINT earbuds (Sony / Bose / Jabra /
    // FreeBuds Pro …) switch near-instantly: their link to us stays up
    // even while the phone streams, so `audio_active` is already true and
    // a grab is just a route change (~0.2s).
    //
    // SAFE for single-point buds (e.g. FreeBuds SE 3): when they leave for
    // the phone they drop our link, so `audio_active` is false here and we
    // fall through to the normal connect below — behaviour unchanged. (It
    // also fast-paths a reclaim of buds still physically ours.)
    //
    // NOTE: validated only against single-point hardware so far (the
    // fall-through case); the multipoint branch is reasoned from the BlueZ
    // link-state semantics and needs a real multipoint device to confirm.
    // `audio_active` alone is NOT enough to take this shortcut: it is true for
    // an HFP-only link too (the headset profile publishes its own `bluez_*`
    // sink). Taking the fast path on that made buds left on `headset-head-unit`
    // by a call hand-off permanently sticky — every connect returned "already
    // live" and nothing ever put them back on A2DP. Require a real A2DP
    // profile; otherwise fall through to the full drop+reconnect below, which
    // is exactly what repairs the profile.
    if audio_active(adapter, addr).await && ensure_card_on_a2dp(adapter, addr, mac).await {
        info!(%addr, "A2DP already live (multipoint/reclaim) — fast route, no reconnect");
        return Ok(());
    }

    // If a stale link from a previous owner is still up, tear the
    // audio profiles down first — connecting on top of a live
    // connection often deadlocks BlueZ for ~10s.
    if device.is_connected().await.unwrap_or(false) {
        for uuid in [A2DP_SINK_UUID, HFP_AG_UUID] {
            let _ = device.disconnect_profile(&uuid).await;
        }
        let _ = wait_audio_disconnected(adapter, addr, Duration::from_millis(500)).await;
    }

    // Prewarm the BlueZ card to the A2DP-sink profile BEFORE asking
    // BlueZ to connect_profile. The old ecosystem's
    // `prewarm_linux_reclaim_path` did this exact step on the
    // SIG_PHONE_PREPARE_LINUX_RECLAIM signal: when the call is ending
    // the card is still pinned to HFP, and connect_profile(A2DP)
    // races BlueZ's own profile negotiation — that's why attempts
    // 1 and 2 fail with "Operation already in progress" and we lose
    // 3-6 seconds before attempt 3 succeeds. Pushing the card to
    // A2DP first means PipeWire creates the sink in IDLE
    // immediately and the very first connect_profile attempt lands.
    // Best-effort: ignore failures (card may not exist yet on the
    // first call after a fresh boot).
    let _ = force_card_to_a2dp(mac).await;

    // Single-shot: try A2DP, fall back to HFP, return Ok or Err. The
    // orchestrator (audio_orchestrator::attempt_connect) owns the
    // retry loop — running a second retry layer here turned the
    // worst-case wait into N×N×CONNECT_SETTLE (~36s with N=3,
    // settle=4s). One layer is enough: the orchestrator can react
    // to state changes between retries (peer Reject, user cancel)
    // which this function can't.
    let t_attempt = tokio::time::Instant::now();

    // A2DP first (media path) — what 99% of users want.
    //
    // **`br-connection-busy` is not a failure.** Right after
    // disconnect, BlueZ often returns "Operation already in
    // progress" / "br-connection-busy" from connect_profile —
    // meaning "I'm already negotiating, don't poke me again."
    // The old ecosystem (`connect_audio_device_fast` in
    // bt_classic.rs) treated this as success and polled the
    // actual connection state. We do the same: any error gets
    // fed into `wait_audio_connected`, and if the buds come up
    // within the settle window we return Ok.
    let t_profile_start = tokio::time::Instant::now();
    let mut a2dp = connect_profile_bounded(&device, &A2DP_SINK_UUID).await;
    // Transient BlueZ errors (br-connection-create-socket / -canceled /
    // page-timeout) mean the ACL link wasn't ready yet — NOT that A2DP is
    // unavailable. Retry the A2DP connect a couple of times with a short
    // settle before falling through to HFP. These buds are A2DP-only, so
    // the old straight-to-HFP path hit ProfileUnavailable and burned
    // ~12 s before a later attempt landed (observed live).
    let mut a2dp_tries = 0u8;
    while a2dp_tries < A2DP_TRANSIENT_RETRIES
        && matches!(&a2dp, ProfileOutcome::Err(e) if is_transient_connect(e))
    {
        a2dp_tries += 1;
        warn!(%addr, "A2DP transient connect error; retry {a2dp_tries}/{A2DP_TRANSIENT_RETRIES}");
        sleep(A2DP_TRANSIENT_PAUSE).await;
        a2dp = connect_profile_bounded(&device, &A2DP_SINK_UUID).await;
    }
    let bluez_ms = t_profile_start.elapsed().as_millis();
    let a2dp_busy = matches!(a2dp, ProfileOutcome::Busy);
    if matches!(a2dp, ProfileOutcome::Ok | ProfileOutcome::Busy) {
        // Connect the hands-free profile too, in the background, purely to make
        // the sink appear sooner.
        //
        // PipeWire's bluez5 device publishes the card immediately only once all
        // the profiles it expects are connected; short of that it waits out an
        // internal timer before giving up and publishing anyway. That timer is
        // the single largest cost in a switch here: the gap between
        // `connect_profile` returning and the sink existing measured 2.3-3.8 s
        // in EVERY successful switch, while BlueZ itself took 0.6-1.2 s. We
        // only ever connected A2DP and left HFP to the earbuds, so the timer
        // ran every time.
        //
        // Detached on purpose: this is an optimisation, not a requirement. If
        // the buds have no HFP, or it is slow, or it fails outright, the A2DP
        // path below is unaffected — it just takes as long as it used to.
        let hfp_adapter = adapter.clone();
        let hfp_addr = addr;
        tokio::spawn(async move {
            if let Ok(dev) = hfp_adapter.device(hfp_addr) {
                // HF (the buds' role) first; AG as the fallback for stacks
                // that want the local side named instead.
                if dev.connect_profile(&HFP_HF_UUID).await.is_err() {
                    let _ = dev.connect_profile(&HFP_AG_UUID).await;
                }
            }
        });
        let t_wait_start = tokio::time::Instant::now();
        if wait_audio_connected(adapter, addr, CONNECT_SETTLE).await {
            let wait_ms = t_wait_start.elapsed().as_millis();
            // A live sink is not proof A2DP won — see `ensure_card_on_a2dp`.
            // Claiming success here while the card sat on HFP is what made the
            // buds play for a moment and then go silent.
            if ensure_card_on_a2dp(adapter, addr, mac).await {
                let connect_ms = t_attempt.elapsed().as_millis();
                info!(
                    %addr,
                    connect_ms,
                    bluez_ms,
                    wait_ms,
                    a2dp_busy,
                    "A2DP connected"
                );
                return Ok(());
            }
            warn!(%addr, "audio sink is up but the card would not take an A2DP profile");
        }
    }
    let mut last_err: Option<SwitchError> = match a2dp {
        ProfileOutcome::Err(e) => {
            info!("A2DP connect_profile error (will fall back to HFP): {e}");
            Some(SwitchError::Bluer(e))
        }
        ProfileOutcome::TimedOut => {
            // BlueZ never answered connect_profile within the bound —
            // it's wedged (A2DP radio starved by the buds streaming
            // elsewhere, or a half-open ACL). Treat as a fast failure
            // so the orchestrator's retry/reset runs instead of letting
            // the D-Bus default (~25 s) freeze the whole switch flow in
            // `Connecting`. The dropped future cancels the in-flight
            // call; a follow-up connect just gets `br-connection-busy`
            // (handled as ok) if BlueZ did keep working on it.
            warn!(%addr, "A2DP connect_profile timed out ({PROFILE_CONNECT_TIMEOUT:?}); falling back to HFP");
            Some(SwitchError::Timeout(PROFILE_CONNECT_TIMEOUT))
        }
        ProfileOutcome::Ok | ProfileOutcome::Busy => None,
    };

    // HFP fallback (voice-only buds). Same busy-is-ok pattern.
    let hfp = connect_profile_bounded(&device, &HFP_AG_UUID).await;
    let hfp_busy = matches!(hfp, ProfileOutcome::Busy);
    if matches!(hfp, ProfileOutcome::Ok | ProfileOutcome::Busy)
        && wait_audio_connected(adapter, addr, CONNECT_SETTLE).await {
            info!(%addr, hfp_busy, "HFP connected (A2DP unavailable)");
            return Ok(());
        }
    match hfp {
        ProfileOutcome::Err(e) => {
            info!("HFP connect_profile error: {e}");
            last_err = Some(SwitchError::Bluer(e));
        }
        ProfileOutcome::TimedOut => {
            warn!(%addr, "HFP connect_profile timed out ({PROFILE_CONNECT_TIMEOUT:?})");
            last_err = Some(SwitchError::Timeout(PROFILE_CONNECT_TIMEOUT));
        }
        ProfileOutcome::Ok | ProfileOutcome::Busy => {}
    }
    Err(last_err.unwrap_or(SwitchError::Timeout(CONNECT_SETTLE)))
}

/// Returns true once the buds are NOT advertising any active audio
/// profile (A2DP / HFP). Other transports — e.g. our own BLE / GATT —
/// are ignored on purpose.
async fn wait_audio_disconnected(adapter: &Adapter, addr: Address, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !audio_active(adapter, addr).await {
            return true;
        }
        sleep(POLL_INTERVAL).await;
    }
    false
}

async fn wait_audio_connected(adapter: &Adapter, addr: Address, timeout: Duration) -> bool {
    // Fast path: already there. Avoids spawning `pactl subscribe` at
    // all when the connect_profile completed before we got here.
    if audio_active(adapter, addr).await {
        return true;
    }

    // Event-driven path: subscribe to PulseAudio/PipeWire events via
    // `pactl subscribe` and react the moment a sink-state line fires.
    // Polling every 80ms used to spend ~30ms per probe on subprocess
    // spawn + IPC + parse — 12-15 wasted probes during a typical
    // ~2 s sink-creation window. The subscribe stream costs one
    // long-lived subprocess and re-probes only when the event stream
    // says "something sink-related changed".
    //
    // `kill_on_drop` guarantees we don't leak the subprocess on the
    // timeout path (the child stays alive until the BufReader is
    // dropped at the end of this function).
    let mut child = match tokio::process::Command::new("pactl")
        .args(["subscribe"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("pactl subscribe spawn failed: {e}; falling back to polling");
            return wait_audio_connected_polling(adapter, addr, timeout).await;
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return wait_audio_connected_polling(adapter, addr, timeout).await,
    };
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                debug!("wait_audio_connected: timeout via event stream");
                return false;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(s)) => {
                        // Trim to the event types that can imply our
                        // sink is now alive. PipeWire emits "Event
                        // 'new' on sink #N" when a new sink shows up;
                        // "Event 'change' on card #N" can fire when
                        // BlueZ flips the A2DP profile on the card.
                        // Ignore client/sink-input/source noise — none
                        // of those affect bluez_output existence.
                        let interesting =
                            s.contains("on sink") || s.contains("on card");
                        if !interesting {
                            continue;
                        }
                        if audio_active(adapter, addr).await {
                            return true;
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Subscribe died (e.g. pactl restart). Fall
                        // back to polling for whatever budget remains
                        // so the orchestrator still gets an answer.
                        let remaining = deadline
                            .saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            return false;
                        }
                        warn!("pactl subscribe ended unexpectedly; polling remainder");
                        return wait_audio_connected_polling(adapter, addr, remaining).await;
                    }
                }
            }
        }
    }
}

/// Legacy polling implementation, kept as a fallback for when `pactl
/// subscribe` can't be started (e.g. pactl missing, sandbox-blocked,
/// or the stream dies mid-wait).
async fn wait_audio_connected_polling(
    adapter: &Adapter,
    addr: Address,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if audio_active(adapter, addr).await {
            return true;
        }
        sleep(POLL_INTERVAL).await;
    }
    false
}

/// True if either A2DP or HFP is currently up for this device. We
/// inspect the published UUIDs the *device* claims — `bluer` exposes
/// `Device1.UUIDs` which BlueZ updates as profiles connect / drop.
pub async fn audio_active(adapter: &Adapter, addr: Address) -> bool {
    let device = match adapter.device(addr) {
        Ok(d) => d,
        Err(_) => return false,
    };
    // If the device isn't connected on ANY transport, the audio
    // profiles are definitely gone.
    if !device.is_connected().await.unwrap_or(false) {
        return false;
    }
    // BlueZ's `uuids` lists *advertised* profiles, which always
    // includes A2DP/HFP for buds regardless of whether those profiles
    // are currently connected (this was a bug — `wait_audio_disconnected`
    // would always time out). The real signal of "A2DP is up right now"
    // is that PulseAudio / PipeWire-pulse exposes a `bluez_*` sink for
    // this MAC. When the profile drops, the sink disappears within
    // ~50 ms. Same check, both backends.
    let needle_under = addr.to_string().replace(':', "_");
    let needle_colon = addr.to_string();
    // Robust "is A2DP actually active": a `bluez_*` sink for this MAC
    // exists in PulseAudio/PipeWire — survives HFP-only buds, BlueZ's
    // stale UUID cache, and PipeWire's two name formats. Served from the
    // subscribe-backed sink cache so this hot path (200ms reconcile +
    // heartbeat, while we hold the buds) doesn't fork pactl every tick.
    crate::core::audio_sink_cache::has_bluez_sink_for(&[&needle_under, &needle_colon]).await
}

/// Push the BlueZ card for [mac] to its A2DP-sink profile. Done before
/// connect_profile so PipeWire creates the sink in IDLE (not SUSPENDED)
/// and the first A2DP connect attempt lands cleanly — mirrors the old
/// ecosystem's `set_audio_a2dp_profile` step.
async fn force_card_to_a2dp(mac: &str) -> bool {
    let card = card_name(mac);
    // Prefer the a2dp profile names the card ACTUALLY offers (codec-suffixed
    // variants differ per device and PipeWire version: `a2dp-sink-sbc_xq`,
    // `a2dp-sink-ldac`, `a2dp-sink-aptx_hd`, …). The hard-coded trio below is
    // only a fallback for the window where the card list can't be read.
    let mut candidates = card_a2dp_profiles(mac).await;
    for fallback in ["a2dp-sink", "a2dp_sink", "a2dp-sink-aac"] {
        if !candidates.iter().any(|p| p == fallback) {
            candidates.push(fallback.to_string());
        }
    }
    for profile in candidates {
        let res = tokio::process::Command::new("pactl")
            .args(["set-card-profile", &card, &profile])
            .output()
            .await;
        if let Ok(o) = res {
            if o.status.success() {
                debug!(%card, %profile, "card pushed to A2DP");
                return true;
            }
        }
    }
    false
}

fn card_name(mac: &str) -> String {
    format!("bluez_card.{}", mac.replace(':', "_"))
}

/// The `pactl list cards` block for this MAC, as raw lines. Empty when the
/// card doesn't exist (buds disconnected, or PipeWire not up yet).
async fn card_block(mac: &str) -> Vec<String> {
    let Ok(out) = tokio::process::Command::new("pactl")
        .args(["list", "cards"])
        .output()
        .await
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let want = card_name(mac);
    let mut block = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // Each card's section starts at `Card #N`; the name lands a few lines in.
        if trimmed.starts_with("Card #") {
            if inside {
                break;
            }
            continue;
        }
        if let Some(name) = trimmed.strip_prefix("Name: ") {
            inside = name.trim() == want;
            if inside {
                continue;
            }
        }
        if inside {
            block.push(line.to_string());
        }
    }
    block
}

/// The card profile currently selected for these buds, e.g. `a2dp-sink` or
/// `headset-head-unit`. `None` when the card doesn't exist.
pub async fn card_active_profile(mac: &str) -> Option<String> {
    parse_active_profile(&card_block(mac).await)
}

fn parse_active_profile(block: &[String]) -> Option<String> {
    block.iter().find_map(|l| {
        l.trim()
            .strip_prefix("Active Profile: ")
            .map(|p| p.trim().to_string())
    })
}

/// Every SELECTABLE `a2dp-*` profile this card offers, best codec first.
/// Empty means PipeWire has no A2DP endpoint for the device — no amount of
/// `set-card-profile` will help.
///
/// Ordered by the card's own `priority:` field, descending, because `pactl`
/// lists profiles in neither priority nor codec order: on FreeBuds SE 3 it
/// prints `a2dp-sink-sbc` (132), `a2dp-sink-sbc_xq` (131), `a2dp-sink`
/// (AAC, 133). Taking the list order would pin the user to SBC while AAC was
/// available — a silent quality downgrade every time we repaired the profile.
/// `available: no` profiles are dropped: they cannot be selected.
pub async fn card_a2dp_profiles(mac: &str) -> Vec<String> {
    parse_a2dp_profiles(&card_block(mac).await)
}

fn parse_a2dp_profiles(block: &[String]) -> Vec<String> {
    let mut found: Vec<(i64, String)> = block
        .iter()
        .filter_map(|l| {
            // Profile lines look like
            //   `\t\ta2dp-sink: High Fidelity Playback (A2DP Sink, codec AAC) \
            //    (sinks: 1, sources: 1, priority: 133, available: yes)`
            // Port lines share the `name: description` shape, so require the
            // a2dp prefix rather than relying on position in the block.
            let t = l.trim();
            let name = t.split(':').next()?.trim();
            if !(name.starts_with("a2dp-") || name.starts_with("a2dp_")) {
                return None;
            }
            if field_of(t, "available: ").is_some_and(|v| v == "no") {
                return None;
            }
            let priority = field_of(t, "priority: ")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            Some((priority, name.to_string()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, name)| name).collect()
}

/// Pull `key`'s value out of a `pactl` trailer like
/// `(sinks: 1, sources: 1, priority: 133, available: yes)`.
fn field_of(line: &str, key: &str) -> Option<String> {
    let rest = line.split(key).nth(1)?;
    let end = rest
        .find([',', ')'])
        .unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// True when the buds' card is on an A2DP profile *right now* — the real
/// "media audio works" signal, as opposed to [`audio_active`], which only
/// says a `bluez_*` sink exists (HFP exposes one too).
pub async fn a2dp_card_active(mac: &str) -> bool {
    card_active_profile(mac)
        .await
        .is_some_and(|p| p.starts_with("a2dp"))
}

/// Is the laptop itself using these earbuds AS A HEADSET right now — i.e. is
/// someone on a call here?
///
/// Two conditions, both required. The card is on a `headset-*` profile (HFP/HSP,
/// which is the only reason to give up stereo), AND something is actually
/// recording from the Bluetooth microphone. Either alone is not enough: a card
/// can be parked on HFP with nothing using it, and a mic can be in use on a
/// different device entirely.
///
/// There is no MPRIS signal for this. A Meet or Zoom call in a browser never
/// reports "Playing", which is exactly why the laptop looked idle to the switch
/// logic and let the phone take the earbuds mid-sentence — taking the microphone
/// with them.
pub async fn headset_mic_in_use(mac: &str) -> bool {
    let on_headset = card_active_profile(mac)
        .await
        .is_some_and(|p| p.starts_with("headset"));
    if !on_headset {
        return false;
    }
    let Ok(out) = tokio::process::Command::new("pactl")
        .args(["list", "source-outputs"])
        .output()
        .await
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = mac.replace(':', "_");
    text.lines()
        .filter(|l| l.trim_start().starts_with("Source:"))
        .any(|l| l.contains(&needle) || l.contains("bluez"))
}

/// Poll until the card reports an A2DP profile, or the timeout elapses.
/// `set-card-profile` returns before PipeWire has rebuilt the sink.
async fn settle_until_a2dp(mac: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if a2dp_card_active(mac).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(CARD_PROFILE_POLL).await;
    }
}

/// Whether BlueZ says this device supports the A2DP sink role at all. Buds
/// that only ever do HFP (some headsets) legitimately have no A2DP profile,
/// and must not be dragged through the recovery path below.
async fn device_advertises_a2dp(adapter: &Adapter, addr: Address) -> bool {
    let Ok(device) = adapter.device(addr) else { return false };
    device
        .uuids()
        .await
        .ok()
        .flatten()
        .is_some_and(|set| set.contains(&A2DP_SINK_UUID))
}

/// Make sure media audio really is on A2DP, and repair it when it isn't.
///
/// **Why this exists.** `audio_active` — and therefore
/// [`wait_audio_connected`] — is satisfied by ANY `bluez_*` sink, and the HFP
/// profile publishes one (mono, 16 kHz, riding SCO). So a call hand-off that
/// left the card on `headset-head-unit` was reported as `"A2DP connected"`,
/// the user heard telephone-quality audio that cut out with the SCO link, and
/// every later connect took the multipoint fast path and returned "already
/// live" — so it never recovered on its own. Live-observed on FreeBuds SE 3
/// after a call plus a wireplumber restart: the card was left offering only
/// `off` / `headset-head-unit-cvsd` / `headset-head-unit` while BlueZ still
/// advertised the A2DP Sink UUID.
///
/// Three escalating steps: select an A2DP profile if the card offers one;
/// if the card offers NONE but the device advertises A2DP, PipeWire lost the
/// endpoint (classic aftermath of a wireplumber restart while connected), so
/// recycle the ACL link once to force a re-probe; then re-select.
pub async fn ensure_card_on_a2dp(adapter: &Adapter, addr: Address, mac: &str) -> bool {
    if a2dp_card_active(mac).await {
        return true;
    }

    // Step 1 — the card offers A2DP, we're just parked on headset.
    if !card_a2dp_profiles(mac).await.is_empty() {
        if force_card_to_a2dp(mac).await && settle_until_a2dp(mac, CARD_PROFILE_SETTLE).await {
            info!(%mac, "card moved off the headset profile onto A2DP");
            return true;
        }
    }

    // Step 2 — no A2DP profile on the card at all.
    if !device_advertises_a2dp(adapter, addr).await {
        debug!(%mac, "device advertises no A2DP sink — HFP-only buds");
        return false;
    }
    // Resolve BEFORE the macro: an `.await` inside `warn!` args holds the
    // non-Send `format_args!` temporary across the yield, which makes the whole
    // `connect_audio` future non-Send and fails the orchestrator's Box::pin.
    let active_now = card_active_profile(mac).await;
    warn!(
        %mac,
        active = ?active_now,
        "buds are connected but their PipeWire card exposes no A2DP profile \
         (lost endpoint) — recycling the link to force a re-probe"
    );
    if let Ok(device) = adapter.device(addr) {
        match device.disconnect().await {
            Ok(()) | Err(_) => {}
        }
        let _ = wait_audio_disconnected(adapter, addr, DISCONNECT_TIMEOUT).await;
        sleep(RECYCLE_PAUSE).await;
        let _ = tokio::time::timeout(PROFILE_CONNECT_TIMEOUT, device.connect()).await;
        let _ = wait_audio_connected(adapter, addr, CONNECT_SETTLE).await;
    }

    // Step 3 — re-select on the rebuilt card.
    if settle_until_a2dp(mac, CARD_PROFILE_SETTLE).await
        || (force_card_to_a2dp(mac).await && settle_until_a2dp(mac, CARD_PROFILE_SETTLE).await)
    {
        info!(%mac, "A2DP endpoint recovered after the link recycle");
        return true;
    }
    warn!(
        %mac,
        "A2DP still unavailable after a link recycle — PipeWire needs a \
         `systemctl --user restart wireplumber` to rebuild this card"
    );
    false
}

/// Outcome of a single bounded `connect_profile` call. `Busy` (BlueZ
/// "already in progress") is treated like success by the caller — the
/// connect is in flight and we poll for it. `TimedOut` means BlueZ
/// never answered within [`PROFILE_CONNECT_TIMEOUT`] — a wedged radio,
/// surfaced as a fast failure so the flow doesn't freeze for the D-Bus
/// default (~25 s).
enum ProfileOutcome {
    Ok,
    Busy,
    Err(bluer::Error),
    TimedOut,
}

/// Call `connect_profile` with a hard upper bound. BlueZ normally
/// answers in well under 2 s; when A2DP can't establish (the buds are
/// busy streaming to another device, starving the single-antenna BT
/// radio) the D-Bus call can hang until the bus's own ~25 s reply
/// timeout. That window is long enough to wedge the orchestrator in
/// `Connecting` and make every subsequent claim bounce off the "busy"
/// guard — the "stuck in switching" symptom. Bounding the call to a few
/// seconds turns the hang into a retryable failure.
async fn connect_profile_bounded(device: &bluer::Device, uuid: &Uuid) -> ProfileOutcome {
    match tokio::time::timeout(PROFILE_CONNECT_TIMEOUT, device.connect_profile(uuid)).await {
        Ok(Ok(())) => ProfileOutcome::Ok,
        Ok(Err(e)) if is_busy_or_in_progress(&e) => ProfileOutcome::Busy,
        Ok(Err(e)) => ProfileOutcome::Err(e),
        Err(_) => ProfileOutcome::TimedOut,
    }
}

/// Recognize the "BlueZ is already working on this connection" family
/// of errors from `connect_profile`. These are NOT real failures — the
/// connect is in flight; the caller just needs to poll `is_connected`
/// for a moment. Mirrors the old ecosystem's `br-connection-busy`
/// special case in `connect_audio_device_fast`. Without this we burn
/// 3-6 seconds bouncing through retries while BlueZ silently completes
/// the connect on attempt 1.
fn is_busy_or_in_progress(e: &bluer::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("already in progress")
        || s.contains("br-connection-busy")
        || s.contains("connection-busy")
        || s.contains("operation already in progress")
}

/// Recognize transient ACL/connection errors from `connect_profile` that
/// are worth a quick retry (the link is being established) rather than a
/// fall-through to HFP. Distinct from [`is_busy_or_in_progress`] (which
/// means "already connecting, just poll") — these mean "the connect
/// attempt failed to even start because the radio/ACL wasn't ready."
fn is_transient_connect(e: &bluer::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("create-socket")
        || s.contains("connection-canceled")
        || s.contains("br-connection-canceled")
        || s.contains("page-timeout")
        || s.contains("page timeout")
        || s.contains("connection refused")
        || s.contains("host is down")
        || s.contains("connection timed out")
}

fn is_not_connected(e: &bluer::Error) -> bool {
    // BlueZ returns "Not Connected" / "Device not connected" depending
    // on version. Newer BlueZ (>=5.65) sometimes maps the same
    // condition to "Invalid arguments" when the profile UUID isn't in
    // the device's currently-connected set. Treat all of these as
    // idempotent — disconnecting a profile that isn't there is
    // exactly what we wanted.
    let s = e.to_string().to_ascii_lowercase();
    s.contains("not connected") || s.contains("invalid arguments")
}

// ---- Tuned constants — see the earbuds-switch design notes §6 ----

/// Window for the connection to actually establish after a successful
/// `connect_profile()` call. The buds-from-phone case is the long tail:
/// when the phone has just released the buds, the bluez_output sink
/// can take 1.5-3 seconds to appear in pactl. With a short
/// `CONNECT_SETTLE` the first attempt times out, we fall through to
/// HFP (which returns ProfileUnavailable immediately) and burn the
/// 200 ms pause + a second attempt — 5+ seconds of dead time. Old
/// ecosystem's `wait_for_audio_ready` used a 5-9 s budget at this
/// layer; 4 s is the sweet spot — long enough to catch the buds on
/// attempt 1 in the common case, short enough that a truly stuck
/// peer still gets retried before the user notices.
const CONNECT_SETTLE: Duration = Duration::from_millis(4000);

/// Hard upper bound on a single `connect_profile` D-Bus call. BlueZ
/// answers a healthy connect in well under 2 s; this bound only ever
/// trips when the call is genuinely wedged (A2DP radio starved by the
/// buds streaming elsewhere). Sits comfortably above the legit max and
/// far below the D-Bus bus default (~25 s), so a hang becomes a fast,
/// retryable failure instead of freezing the switch flow in `Connecting`.
const PROFILE_CONNECT_TIMEOUT: Duration = Duration::from_millis(6000);

/// How long to wait for PipeWire to actually apply a card-profile switch.
/// `pactl set-card-profile` returns as soon as the request is accepted; the
/// sink is torn down and rebuilt on the session manager's own schedule, so
/// reading the profile back immediately can still show the old one.
const CARD_PROFILE_SETTLE: Duration = Duration::from_millis(1500);

/// Re-read interval while waiting out [`CARD_PROFILE_SETTLE`]. Each probe
/// forks `pactl list cards`, so this is deliberately coarser than
/// [`POLL_INTERVAL`] — the profile flip is a ~100 ms event, not a ~10 ms one.
const CARD_PROFILE_POLL: Duration = Duration::from_millis(120);

/// Settle gap between the disconnect and reconnect of the A2DP-endpoint
/// recovery in [`ensure_card_on_a2dp`]. Reconnecting the instant the ACL
/// drops tends to race BlueZ's own teardown and land back on HFP.
const RECYCLE_PAUSE: Duration = Duration::from_millis(600);

/// How many times to retry the A2DP connect on a transient ACL error
/// (br-connection-create-socket etc.) before falling through to HFP.
const A2DP_TRANSIENT_RETRIES: u8 = 2;
/// Settle between transient-error A2DP retries — long enough for BlueZ to
/// finish establishing the ACL.
const A2DP_TRANSIENT_PAUSE: Duration = Duration::from_millis(220);

/// Disconnect is usually faster than connect. 1 s is enough for both
/// profiles to drop in practice.
const DISCONNECT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Poll interval while awaiting a state flip. 40 ms keeps the
/// release/connect confirmation tight (the reference build polled at
/// ~35 ms for its ~1 s hand-off) — the `is_connected()` fast-path in
/// `audio_active` means most polls don't even reach the pactl subprocess.
const POLL_INTERVAL: Duration = Duration::from_millis(40);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_match_sig_assignments() {
        // Sanity: the canonical Service Class UUIDs from the Bluetooth
        // SIG Assigned Numbers. If these ever change, every paired
        // device on the planet would have stopped working — so this
        // is really a regression guard for our own copy-paste.
        assert_eq!(
            A2DP_SINK_UUID.to_string(),
            "0000110b-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            HFP_AG_UUID.to_string(),
            "0000111f-0000-1000-8000-00805f9b34fb"
        );
    }

    /// Real `pactl list cards` block for HUAWEI FreeBuds SE 3, captured on
    /// Fedora 44 / PipeWire 1.6.8 right after the A2DP endpoint came back.
    /// Note the profile order: SBC (132) and SBC-XQ (131) are printed BEFORE
    /// AAC (133), which is exactly the trap the priority sort exists for.
    fn healthy_card_block() -> Vec<String> {
        [
            "\tProfiles:",
            "\t\toff: Off (sinks: 0, sources: 0, priority: 0, available: yes)",
            "\t\ta2dp-sink-sbc: High Fidelity Playback (A2DP Sink, codec SBC) (sinks: 1, sources: 1, priority: 132, available: yes)",
            "\t\ta2dp-sink-sbc_xq: High Fidelity Playback (A2DP Sink, codec SBC-XQ) (sinks: 1, sources: 1, priority: 131, available: yes)",
            "\t\ta2dp-sink: High Fidelity Playback (A2DP Sink, codec AAC) (sinks: 1, sources: 1, priority: 133, available: yes)",
            "\t\theadset-head-unit-cvsd: Headset Head Unit (HSP/HFP, codec CVSD) (sinks: 1, sources: 1, priority: 5, available: yes)",
            "\t\theadset-head-unit: Headset Head Unit (HSP/HFP, codec MSBC) (sinks: 1, sources: 1, priority: 6, available: yes)",
            "\tActive Profile: a2dp-sink",
            "\tPorts:",
            "\t\theadset-output: Headphones (type: Headset, priority: 0, latency offset: 0 usec, available)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The SAME buds after a call hand-off plus a wireplumber restart: BlueZ
    /// still advertises A2DP Sink, but PipeWire's card offers only headset
    /// profiles. This is the state that got logged as "A2DP connected".
    fn lost_endpoint_card_block() -> Vec<String> {
        [
            "\tProfiles:",
            "\t\toff: Off (sinks: 0, sources: 0, priority: 0, available: yes)",
            "\t\theadset-head-unit-cvsd: Headset Head Unit (HSP/HFP, codec CVSD) (sinks: 1, sources: 1, priority: 2, available: yes)",
            "\t\theadset-head-unit: Headset Head Unit (HSP/HFP, codec MSBC) (sinks: 1, sources: 1, priority: 3, available: yes)",
            "\tActive Profile: headset-head-unit",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn a2dp_profiles_are_ordered_best_codec_first() {
        // AAC (133) must win even though pactl printed the SBC variants first.
        assert_eq!(
            parse_a2dp_profiles(&healthy_card_block()),
            vec!["a2dp-sink", "a2dp-sink-sbc", "a2dp-sink-sbc_xq"]
        );
    }

    #[test]
    fn headset_and_port_lines_are_not_mistaken_for_a2dp() {
        // Ports share the `name: description (…)` shape as profiles, and the
        // `Active Profile:` line does too — none may leak into the list.
        let profiles = parse_a2dp_profiles(&healthy_card_block());
        assert!(profiles.iter().all(|p| p.starts_with("a2dp")));
    }

    #[test]
    fn lost_endpoint_card_offers_no_a2dp() {
        // The signal `ensure_card_on_a2dp` escalates on: nothing to select,
        // so pushing the profile is pointless and the link must be recycled.
        assert!(parse_a2dp_profiles(&lost_endpoint_card_block()).is_empty());
        assert_eq!(
            parse_active_profile(&lost_endpoint_card_block()).as_deref(),
            Some("headset-head-unit")
        );
    }

    #[test]
    fn active_profile_distinguishes_a2dp_from_headset() {
        // The exact check behind `a2dp_card_active` — the predicate that
        // replaced "a bluez_* sink exists", which HFP also satisfies.
        let healthy = parse_active_profile(&healthy_card_block()).unwrap();
        let broken = parse_active_profile(&lost_endpoint_card_block()).unwrap();
        assert!(healthy.starts_with("a2dp"));
        assert!(!broken.starts_with("a2dp"));
    }

    #[test]
    fn unavailable_profiles_are_skipped() {
        // An `available: no` profile cannot be selected; offering it would
        // make force_card_to_a2dp burn a pactl call on a guaranteed failure.
        let block: Vec<String> = [
            "\t\ta2dp-sink: High Fidelity Playback (A2DP Sink, codec AAC) (sinks: 1, sources: 1, priority: 133, available: no)",
            "\t\ta2dp-sink-sbc: High Fidelity Playback (A2DP Sink, codec SBC) (sinks: 1, sources: 1, priority: 132, available: yes)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(parse_a2dp_profiles(&block), vec!["a2dp-sink-sbc"]);
    }

    #[test]
    fn field_of_reads_pactl_trailers() {
        let line = "a2dp-sink: Playback (sinks: 1, sources: 1, priority: 133, available: yes)";
        assert_eq!(field_of(line, "priority: ").as_deref(), Some("133"));
        assert_eq!(field_of(line, "available: ").as_deref(), Some("yes"));
        assert_eq!(field_of(line, "nonexistent: "), None);
    }

    #[test]
    fn bad_mac_yields_clear_error() {
        // We don't call adapter (no async), just confirm the parse
        // catches obvious junk. The fuller live tests live on real
        // hardware via the orchestrator e2e test.
        let parse_result: Result<bluer::Address, _> = "not-a-mac".parse();
        assert!(parse_result.is_err());
    }
}
