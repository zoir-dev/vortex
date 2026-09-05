//! Wait for the PulseAudio / PipeWire-pulse route to settle on the
//! freshly-connected Bluetooth audio sink (P2.14).
//!
//! Background
//! ----------
//! `audio_switch::connect_audio` returns the moment BlueZ reports the
//! A2DP profile as connected. PulseAudio (or pipewire-pulse) discovers
//! that over D-Bus and *then* creates the corresponding `bluez_*` sink
//! and starts negotiating SBC/AAC. That takes another 200 ms – 1 s in
//! practice, and during the gap:
//!
//!  * `pactl list short sinks` still shows the laptop speaker as default.
//!  * The bluez sink is either absent or in `SUSPENDED` state.
//!  * If we call `Player.Play` on an MPRIS player NOW, the player
//!    streams to whatever sink is currently default (laptop speaker),
//!    you hear half a syllable through the laptop, then WirePlumber
//!    finishes the migration, suspends every input on the old sink,
//!    and the player ends up paused again.
//!
//! The 63eeaee prototype solved this with `pactl subscribe` + readiness
//! polling: wait for the bluez sink to appear, wait until it leaves
//! `SUSPENDED`, move every existing sink-input to it, then mark default.
//! This module is a tighter port of that helper — `tokio::process` for
//! the pactl calls so we don't block the async runtime, bounded by a
//! single wall-clock deadline so we never get stuck on a misbehaving
//! sound server.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Hard cap on the wait for the sink to *appear* in pactl. Once it
/// shows up we don't wait for SUSPENDED→IDLE — PipeWire creates the
/// bluez sink in SUSPENDED and only flips it to IDLE/RUNNING when a
/// stream actually plays through it, so waiting longer is a deadlock.
/// We promote it to default + migrate streams instead; the player's
/// own Play call wakes the sink.
const SINK_APPEAR_TIMEOUT: Duration = Duration::from_millis(2_500);

/// Best-effort window for the sink to leave SUSPENDED on its own. If
/// the system already had a stream routed (e.g. a probe sound) the
/// sink will be IDLE/RUNNING before we get here — that's the happy
/// path. Otherwise we fall through and route streams to it while still
/// SUSPENDED; the player's resume() then wakes it. Old ecosystem
/// burned ~24ms here (3 fast polls @ 8ms); we mirror that — anything
/// longer is wasted time the user perceives as silence before resume.
const SINK_READY_BEST_EFFORT: Duration = Duration::from_millis(60);

/// Poll cadence while looking for the sink to appear or leave SUSPENDED.
/// Old ecosystem used 8ms fast polls; 20ms keeps detection latency low
/// without spamming pactl during long waits.
const POLL: Duration = Duration::from_millis(20);

/// Outcome of [`wait_for_route`].
#[derive(Debug, Clone)]
pub struct RouteOutcome {
    /// Name of the bluez sink we settled on (if found).
    pub sink: Option<String>,
    /// Wall-clock time spent waiting.
    pub elapsed: Duration,
    /// True if we actually saw the sink reach a non-SUSPENDED state.
    pub ready: bool,
    /// True if we successfully made the sink the default + moved
    /// existing sink-inputs to it. `false` indicates a partial result
    /// the caller can still treat as best-effort.
    pub routed: bool,
}

/// Block until the bluez sink for [`mac`] is created and ready, then
/// promote it to the default sink and migrate any existing streams.
///
/// Returns even if pactl is unavailable — the caller should be prepared
/// for `routed: false`, since on a system without pactl we still want
/// to call resume eventually (better to play through the wrong sink
/// than to never resume at all).
pub async fn wait_for_route(mac: &str) -> RouteOutcome {
    let started = Instant::now();
    if !pactl_available().await {
        debug!("audio-route: pactl unavailable; skipping wait");
        return RouteOutcome {
            sink: None,
            elapsed: started.elapsed(),
            ready: false,
            routed: false,
        };
    }
    // BlueZ device address `AA:BB:CC:DD:EE:FF` shows up in pactl sink
    // names in BOTH formats depending on the backend / version:
    //   - PulseAudio classic + some pipewire-pulse: `..._AA_BB_CC_DD_EE_FF...`
    //   - Newer pipewire-pulse 17+: `...AA:BB:CC:DD:EE:FF...`
    // Try both so we match regardless of which the user is running.
    let underscored = mac.replace(':', "_");
    let colon_form = mac.to_string();

    let mut sink: Option<String> = None;
    let appear_deadline = started + SINK_APPEAR_TIMEOUT;
    let mut rounds: u32 = 0;
    let mut a2dp_forced = false;
    // Does this device even have an A2DP profile? Headsets that are HFP-only
    // legitimately never reach one, and for those the HFP sink IS the answer —
    // so the profile gate below must not lock them out forever.
    let has_a2dp = !crate::core::audio_switch::card_a2dp_profiles(mac)
        .await
        .is_empty();

    while Instant::now() < appear_deadline {
        if let Some(found) = find_sink_for(&[underscored.as_str(), colon_form.as_str()]).await {
            // A sink existing is NOT the same as the audio being ready.
            // The card publishes a sink on the HFP profile too, so this test
            // used to pass while the buds were still in hands-free mode — and
            // WirePlumber stores volume PER PROFILE. On this deployment the
            // HFP route sits at 0.15 linear (~53%) and the A2DP route at
            // 0.001 (~10%), so starting playback here meant the first second
            // or two came out roughly five times too loud, in telephone-grade
            // mono, before the profile settled onto A2DP and the volume
            // dropped. Wait for the profile the audio is actually meant for.
            if !has_a2dp || crate::core::audio_switch::a2dp_card_active(mac).await {
                sink = Some(found);
                break;
            }
            // Sink is up but we're on HFP: nudge the card over once, then keep
            // waiting for the profile rather than accepting this sink.
            if !a2dp_forced {
                a2dp_forced = true;
                let _ = force_a2dp_card_profile(&underscored).await;
            }
            tokio::time::sleep(POLL).await;
            continue;
        }
        rounds += 1;
        // If the sink hasn't appeared after ~160ms (8 polls @ 20ms), the
        // BlueZ card might still be on HFP from the call. Force the
        // A2DP profile once so PipeWire creates the sink in IDLE state
        // (active profile) instead of SUSPENDED. Without this the sink
        // appears in SUSPENDED, our wake retry races the resume Play,
        // and the user hears play-from-laptop → pause → play-from-buds.
        // (Pattern from ecosystem prewarm_linux_reclaim_path.)
        if rounds == 8 && !a2dp_forced {
            a2dp_forced = true;
            let _ = force_a2dp_card_profile(&underscored).await;
        }
        tokio::time::sleep(POLL).await;
    }
    // NOTE: do NOT force A2DP when the sink already appeared on its
    // own. PipeWire treats `set-card-profile` to the same profile as a
    // re-negotiation: it tears down the live sink and rebuilds it
    // SUSPENDED. That doubles the return-to-Linux latency. We only
    // touch the card profile inside the appear-wait loop above, after
    // 8 polls without a sink (the "stuck on HFP" case from the old
    // ecosystem).
    let Some(sink_name) = sink.clone() else {
        warn!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            mac, "audio-route: timed out waiting for bluez sink to appear"
        );
        return RouteOutcome {
            sink: None,
            elapsed: started.elapsed(),
            ready: false,
            routed: false,
        };
    };

    // ACTION-DRIVEN READINESS (the "force the output to the buds, THEN
    // play" the user remembered from the ecosystem build). The bluez
    // sink is born SUSPENDED and only flips to IDLE/RUNNING when a real
    // stream flows through it. If we return now and let the player's
    // resume Play be that first stream, PipeWire negotiates the A2DP
    // SBC/AAC codec AT THAT MOMENT — the sink node churns
    // (SUSPENDED → config → IDLE) and the player observes a
    // device-change and auto-pauses itself. THAT is the "played then
    // immediately paused" the user kept seeing on the return.
    //
    // Fix: wake the sink OURSELVES before resume. First give it a brief
    // best-effort window to leave SUSPENDED on its own (happy path: a
    // probe sound or the prewarmed profile already woke it); if it is
    // still asleep, force it with a short zero-volume silent probe.
    // That makes the codec negotiation happen NOW, while nothing audible
    // is playing — so when we resume, the player's Play lands on a live,
    // stable route and stays playing. Gating resume on this ACTION (sink
    // actually IDLE/RUNNING) instead of on a timer is the whole point.
    let mut ready = sink_is_ready(&sink_name).await;
    if !ready {
        let settle_deadline = Instant::now() + SINK_READY_BEST_EFFORT;
        while Instant::now() < settle_deadline {
            if sink_is_ready(&sink_name).await {
                ready = true;
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        if !ready {
            // Still SUSPENDED — force the output awake with a silent
            // probe so the A2DP codec negotiates before the user's audio.
            force_sink_unsuspended(&sink_name).await;
            ready = sink_is_ready(&sink_name).await;
        }
    }

    // FAST PATH: if the bluez sink is already the default (BlueZ +
    // PipeWire auto-promoted it on connect), skip set-default — just
    // move any sink-inputs and return. Old ecosystem does the same:
    // `audio ready after N+0 rounds, route_set=pre-set`.
    let already_default = current_default_sink().await.as_deref() == Some(sink_name.as_str());
    if already_default {
        let move_ok = move_all_inputs_to(&sink_name).await;
        // Second pass only if first moved something — old ecosystem
        // skips the second pass when first didn't move anything.
        if move_ok {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = move_all_inputs_to(&sink_name).await;
        }
        let elapsed = started.elapsed();
        info!(
            sink = %sink_name,
            elapsed_ms = elapsed.as_millis() as u64,
            ready,
            "audio-route: ready (pre-set as default)"
        );
        return RouteOutcome {
            sink: Some(sink_name),
            elapsed,
            ready,
            routed: true,
        };
    }

    // KEY: move sink-inputs FIRST, then set-default. If we set-default
    // first WirePlumber fires a "follow default" move that the player
    // sees as a device-change → auto-pause. By moving first
    // WirePlumber sees streams already on the right sink and skips
    // its own move. This comment + ordering is verbatim from the old
    // ecosystem's bt_classic::wait_for_audio_ready.
    let move_ok = move_all_inputs_to(&sink_name).await;
    let set_default_ok = set_default_sink(&sink_name).await;
    // Second move pass after a brief settle: catches sink-inputs
    // created mid-migration (e.g. mpv forking a new stream on play).
    // Skip when first move did nothing — matches old ecosystem.
    let move_ok2 = if move_ok {
        tokio::time::sleep(Duration::from_millis(30)).await;
        move_all_inputs_to(&sink_name).await
    } else {
        true
    };
    let move_ok = move_ok || move_ok2;

    let elapsed = started.elapsed();
    info!(
        sink = %sink_name,
        elapsed_ms = elapsed.as_millis() as u64,
        ready,
        moved = move_ok,
        defaulted = set_default_ok,
        "audio-route: ready"
    );
    RouteOutcome {
        sink: Some(sink_name),
        elapsed,
        ready,
        routed: move_ok && set_default_ok,
    }
}

async fn pactl_available() -> bool {
    Command::new("pactl")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn find_sink_for(needles: &[&str]) -> Option<String> {
    let out = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // pactl list short sinks layout: `<id>\t<name>\t<driver>\t<format>\t<state>`
        let Some(name) = line.split('\t').nth(1) else { continue };
        if !name.starts_with("bluez") {
            continue;
        }
        if needles.iter().any(|n| name.contains(n)) {
            return Some(name.to_string());
        }
    }
    None
}

async fn sink_is_ready(name: &str) -> bool {
    // `pactl list short sinks` prints the state in the last column.
    // RUNNING / IDLE → ready; SUSPENDED → not yet.
    let Ok(out) = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .await
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut parts = line.split('\t');
        let _id = parts.next();
        let sink_name = match parts.next() {
            Some(n) => n,
            None => continue,
        };
        if sink_name != name {
            continue;
        }
        let state = parts.next_back().unwrap_or("").trim();
        return matches!(state, "RUNNING" | "IDLE");
    }
    false
}

async fn move_all_inputs_to(sink: &str) -> bool {
    let Ok(out) = Command::new("pactl")
        .args(["list", "short", "sink-inputs"])
        .output()
        .await
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let mut ok = true;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(id) = line.split('\t').next() else { continue };
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let res = Command::new("pactl")
            .args(["move-sink-input", id, sink])
            .output()
            .await;
        if res.map(|o| !o.status.success()).unwrap_or(true) {
            ok = false;
        }
    }
    ok
}

/// Force the sink out of SUSPENDED state. `pactl suspend-sink NAME 0`
/// only clears the suspend flag — PipeWire's auto-suspend module
/// re-suspends within milliseconds if no stream is active. So we ALSO
/// run a brief silent `paplay /dev/zero` through the sink: that's an
/// actual stream landing on the sink, which transitions it to IDLE
/// (and crucially STAYS in IDLE after the probe exits, for long enough
/// that the MPRIS Play call lands on a live route). Without this the
/// player Play goes to a SUSPENDED sink → audio plays from the
/// previous default (laptop speakers) for ~50-200ms → WirePlumber
/// finishes route → player auto-pauses on device-change → safety-net
/// re-Plays. That's the "play → silence → pause → play" glitch the
/// user kept hearing on call-end.
async fn force_sink_unsuspended(name: &str) -> bool {
    // Step 1: clear the suspend flag.
    let _ = Command::new("pactl")
        .args(["suspend-sink", name, "0"])
        .output()
        .await;
    // Step 2: feed the sink ~120ms of zero-volume silence to wake it
    // from SUSPENDED to IDLE. We use `timeout` so paplay exits cleanly
    // even if the sink rejects the stream. /dev/zero is infinite so
    // the timeout is what bounds the probe duration.
    let probe = Command::new("timeout")
        .args([
            "0.12",
            "paplay",
            "--device",
            name,
            "--raw",
            "--rate=44100",
            "--format=s16le",
            "--channels=2",
            "--volume=0",
            "/dev/zero",
        ])
        .output()
        .await;
    // `timeout` exits 124 on the kill (expected). Either status is fine.
    probe.is_ok()
}

/// Spawn a DETACHED, zero-volume silent stream that holds `name` in
/// IDLE/RUNNING for `hold`. Unlike [`force_sink_unsuspended`]'s brief
/// 120ms probe (just enough to wake the sink before a single resume),
/// this spans the WHOLE A2DP settle window: the SBC/AAC codec
/// negotiation AND PipeWire's late SUSPEND→re-open wave that lands
/// ~1-3s after the first stream. With the sink held continuously awake,
/// the resumed player joins an already-stable route — it never observes
/// a device-change, so it never auto-pauses, so the safety-net never has
/// to re-Play it. THAT removes the audible "play → pause → play → pause"
/// stutter the user heard on the return to Linux (each re-Play was a
/// burst of sound). Detached + best-effort: if `paplay`/`timeout` are
/// missing it's a silent no-op and the checkpoint safety-net still
/// covers the player the old way.
pub fn spawn_sink_keepalive(name: String, hold: Duration) {
    let secs = format!("{:.2}", hold.as_secs_f32());
    tokio::spawn(async move {
        let _ = Command::new("timeout")
            .args([
                secs.as_str(),
                "paplay",
                "--device",
                name.as_str(),
                "--raw",
                "--rate=44100",
                "--format=s16le",
                "--channels=2",
                "--volume=0",
                "/dev/zero",
            ])
            .output()
            .await;
    });
}

/// Force the BlueZ card to the A2DP-sink profile. After a call ends the
/// card may still be on HFP/HSP (the voice profile) — if we move sink
/// inputs to the HFP "sink" the player auto-pauses on the device-change,
/// and an A2DP sink only materializes later in SUSPENDED state. Pushing
/// the profile here matches the old ecosystem's
/// `prewarm_linux_reclaim_path` and is the difference between "play
/// from laptop → pause → play from buds" and a clean resume.
///
/// `underscored_mac` is the MAC with `:` replaced by `_`, matching
/// PipeWire/PulseAudio card naming.
async fn force_a2dp_card_profile(underscored_mac: &str) -> bool {
    let card = format!("bluez_card.{}", underscored_mac);
    // Try both naming conventions used by different PipeWire/PA versions.
    for profile in ["a2dp-sink", "a2dp_sink", "a2dp-sink-aac"] {
        let out = Command::new("pactl")
            .args(["set-card-profile", &card, profile])
            .output()
            .await;
        if let Ok(o) = out {
            if o.status.success() {
                debug!(card = %card, profile, "forced A2DP profile");
                return true;
            }
        }
    }
    false
}

/// Read the current default sink name from pactl. Returns None if pactl
/// is unavailable or the output is unparseable. Used to detect the
/// "sink is already default" fast path (matches old ecosystem
/// `default_sink()`).
async fn current_default_sink() -> Option<String> {
    let out = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

async fn set_default_sink(name: &str) -> bool {
    Command::new("pactl")
        .args(["set-default-sink", name])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
