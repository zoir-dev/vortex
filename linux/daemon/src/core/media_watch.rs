//! Smart audio-follow — laptop side. Polls local MPRIS playback; when
//! media starts on the laptop while the buds are elsewhere, grabs them
//! here via the orchestrator. Direct mirror of the Android
//! `MediaHandoffCoordinator`.
//!
//! This module only handles the *grab* half (local media → pull buds
//! here). The *release* half (laptop hands the buds to the phone when the
//! phone starts playing) reacts to the peer's advisory `media_playing`
//! flag in the Tauri heartbeat loop — see `ui-tauri/.../lib.rs`. Together
//! they complete a switch without needing a live AudioOp transport writer
//! (the same robustness the call_phase path relies on).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bluer::{Adapter, Address};
use tracing::{info, warn};
use zbus::Connection;

use crate::core::audio_orchestrator::SwitchOrchestrator;
use crate::core::audio_switch::{audio_active, disconnect_audio_initiate};
use crate::core::media_runtime::{
    clear as clear_call_pause, pause_all_playing, play_players, playing_players, MediaStateStore,
};
use crate::core::storage::peers::PeerStore;

/// Min gap between two auto-grabs.
const GRAB_COOLDOWN: Duration = Duration::from_millis(4_000);
/// After the buds leave us, don't fight to reclaim for this long.
const LOSS_SUPPRESS: Duration = Duration::from_millis(4_000);
/// How long the peer's "buds connected to me" report stays valid for the
/// grab gate. The heartbeat lands every ~12s, so 30s tolerates one missed
/// beat; past it we treat the link as down (nothing to grab).
const PEER_FRESH: Duration = Duration::from_millis(30_000);
/// Reconcile interval. 200ms (down from 400) for faster detection +
/// catch-up retry. `audio_active` no longer forks `pactl` per poll — it
/// reads the subscribe-backed `audio_sink_cache` — so this loop is cheap
/// to run while we own the buds.
const POLL: Duration = Duration::from_millis(200);
/// After our media stops while we hold the buds, wait this long before
/// handing them back to the phone (so a brief pause / track change
/// doesn't bounce the buds).
const RETURN_DELAY: Duration = Duration::from_millis(2_000);
/// Grace after gaining the buds during which the return timer can't arm —
/// covers the route-change auto-pause some players do right after a grab,
/// so it isn't mistaken for the user stopping.
const RETURN_GRACE: Duration = Duration::from_millis(3_000);
/// If the buds don't arrive this long after a FORWARD grab, resume the
/// paused media anyway so it isn't stuck silent. Sized safely ABOVE the
/// A2DP connect floor (~3.3s, up to ~5.9s with BT churn) so the normal
/// path always resumes on the buds (own=true) and never trips this
/// speaker-fallback timeout first.
const RESUME_TIMEOUT: Duration = Duration::from_millis(8_000);
/// For a LOSS-remember (peer took the buds), wait much longer before
/// resuming on the laptop speakers — the buds usually return well before
/// this when the peer's media stops; this is only a last-ditch un-stick.
const LOSS_RESUME_TIMEOUT: Duration = Duration::from_secs(90);
/// After a forward grab overruns [`RESUME_TIMEOUT`], keep the remembered
/// pause alive this much longer instead of dropping it: the orchestrator
/// watchdog runs the switch out to ~14s, so the buds routinely arrive
/// AFTER the 8s window under BT churn — and dropping the record meant the
/// late arrival resumed nothing ("switched but never played").
const LATE_RESUME_WINDOW: Duration = Duration::from_millis(15_000);

/// Arbitration model. The proven ecosystem was SYMMETRIC for *grabbing*:
/// each side pulls the buds to itself on its own media play-edge
/// (`watchLinuxPlaybackEdge` on the laptop ⇔ `MediaHandoffCoordinator`
/// rising-edge on the phone). When the laptop starts playing while the
/// buds are on the phone, the laptop pauses its own media, grabs the buds,
/// and the phone — having lost them — pauses its playback. That's the
/// behaviour users expect ("audio follows whoever just hit play").
///
/// [`LAPTOP_AUTO_GRAB`] enables that laptop-side grab. The earlier
/// `false` here disabled it on a mistaken read that the ecosystem was
/// asymmetric — it wasn't; the laptop grabbed too.
///
/// We deliberately do NOT enable a laptop *push-on-stop* ([`LAPTOP_PUSH_ON_STOP`]
/// stays false): when laptop media stops the buds simply STAY here until
/// the phone's next play-edge grabs them back. This is the plan's v1
/// "no release-on-stop" rule — it removes the dueling-push ping-pong
/// vector while keeping the natural "follow the player" feel. The
/// phone-driven reclaim still works independently (it arrives as the
/// peer's `audio_claim_request`, handled in lib.rs).
///
/// Anti-ping-pong for the grab is the same guard set the ecosystem used:
/// rising-edge only, [`GRAB_COOLDOWN`], [`LOSS_SUPPRESS`] after the buds
/// leave us, and the call gate (calls outrank media).
const LAPTOP_AUTO_GRAB: bool = true;
/// Whether the laptop pushes the buds back to the phone when its own media
/// stops. v1: false — the phone reclaims on its own play-edge instead. See
/// [`LAPTOP_AUTO_GRAB`] for the rationale.
const LAPTOP_PUSH_ON_STOP: bool = false;

/// Shared control + published state for the laptop media watcher.
pub struct MediaWatch {
    /// UI toggle — when false the watcher still tracks playing-state (for
    /// the heartbeat) but never grabs. SHARED, LWW-synced with the phone.
    pub enabled: AtomicBool,
    /// Unix-seconds timestamp of the last explicit toggle of [`enabled`].
    /// The LWW key for cross-device sync: a peer value with a strictly
    /// greater timestamp is adopted. Persisted in `smart_switch_store`.
    pub enabled_changed_at: std::sync::atomic::AtomicU64,
    /// Whether media is currently playing locally. The heartbeat builder
    /// reads this into the outgoing AppState `media_playing` flag.
    pub playing: AtomicBool,
    /// OUR play-start on the local [`mono_ms`] timeline (0 = not playing).
    /// The heartbeat builder turns it into a relative AGE (`mono_ms() - this`)
    /// for the wire. Frozen across our own hand-off pauses (set by the
    /// watcher loop) so a switch-induced resume doesn't look like a newer
    /// play. Compared with [`peer_play_epoch_mono`] in the grab gate.
    pub play_epoch_mono: std::sync::atomic::AtomicU64,
    /// The PEER's play-start RE-ANCHORED to our [`mono_ms`] timeline
    /// (`mono_ms() - peer_age` at receive), written by the heartbeat consumer
    /// in lan.rs. 0 when the peer isn't playing. The grab gate yields only
    /// when this is strictly GREATER than ours (peer started more recently).
    pub peer_play_epoch_mono: std::sync::atomic::AtomicU64,
    /// Last-seen value of the PEER's `media_playing`, kept by the
    /// heartbeat loop so it can fire the buds-release on the peer's
    /// not-playing → playing edge. See lib.rs.
    pub peer_playing: AtomicBool,
    /// One-shot: the watcher sets this when our media stopped and we want
    /// the phone to take the buds back (and resume its paused media). The
    /// heartbeat builder reads it into the outgoing AppState
    /// `audio_claim_request` (swap-to-false so a single heartbeat carries
    /// it). See lib.rs.
    pub claim_peer: AtomicBool,
    /// `Some(t)` = the last time the peer's AppState reported the buds
    /// connected to IT. Carries both signals a grab needs: the buds are on
    /// the phone (something to pull) AND the link is alive (a recent `t`).
    /// `None` when the peer says the buds are elsewhere / in the case. Set
    /// by the heartbeat consumer in lib.rs; read with a freshness window in
    /// the grab gate so a stale value (peer gone) doesn't trigger a grab.
    pub peer_holds_buds_seen: std::sync::Mutex<Option<Instant>>,
}

impl MediaWatch {
    /// Build from the persisted smart-switch setting (enabled + LWW ts), so
    /// the toggle survives a daemon restart and the saved timestamp keeps
    /// participating in cross-device LWW.
    pub fn new() -> Arc<Self> {
        let saved = crate::core::smart_switch_store::load();
        Arc::new(Self {
            enabled: AtomicBool::new(saved.enabled),
            enabled_changed_at: std::sync::atomic::AtomicU64::new(saved.changed_at),
            playing: AtomicBool::new(false),
            play_epoch_mono: std::sync::atomic::AtomicU64::new(0),
            peer_play_epoch_mono: std::sync::atomic::AtomicU64::new(0),
            peer_playing: AtomicBool::new(false),
            claim_peer: AtomicBool::new(false),
            peer_holds_buds_seen: std::sync::Mutex::new(None),
        })
    }

    /// Apply a new on/off value with its change timestamp, persist it, and
    /// (when the timestamp is newer) update the in-memory flag. Returns true
    /// if the value actually changed. Shared by the local toggle command and
    /// the LWW peer-adoption path. A `changed_at` not strictly greater than
    /// the current one is ignored (older or duplicate writer).
    pub fn apply_setting(&self, enabled: bool, changed_at: u64) -> bool {
        use std::sync::atomic::Ordering;
        // Adopt only on a STRICTLY newer timestamp. `changed_at == 0` means
        // "no explicit opinion", so it never wins — and two zeros (neither
        // side has toggled yet) compare equal and are dropped, which is what
        // stops the every-heartbeat re-adoption when both sit at the default.
        if changed_at <= self.enabled_changed_at.load(Ordering::Relaxed) {
            return false;
        }
        self.enabled.store(enabled, Ordering::Relaxed);
        self.enabled_changed_at.store(changed_at, Ordering::Relaxed);
        let _ = crate::core::smart_switch_store::save(
            &crate::core::smart_switch_store::SmartSwitch { enabled, changed_at },
        );
        true
    }
}

/// Process-monotonic milliseconds for the last-play-wins clock. MONOTONIC
/// (not wall-clock) on purpose: the epoch is never sent raw — we send a
/// relative AGE and the receiver re-anchors it to ITS OWN `mono_ms`
/// (`peer_start = mono_ms() - peer_age`). So both our own epoch and the
/// peer's re-anchored epoch live on THIS process's monotonic timeline,
/// which makes the comparison immune to cross-device wall-clock skew and
/// to NTP steps. The single shared `START` means every caller (the watcher
/// loop and the heartbeat builder in lan.rs) reads the same timeline.
pub fn mono_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Spawn the local-playback watcher. `in_call` gates grabbing off while a
/// call is in progress (calls outrank media). The trusted peer and the
/// saved earbuds MAC are read fresh each grab so a later pairing change
/// is picked up without a restart.
pub fn spawn(
    watch: Arc<MediaWatch>,
    orchestrator: Arc<SwitchOrchestrator>,
    adapter: Adapter,
    peer_store: Arc<dyn PeerStore>,
    in_call: Arc<AtomicBool>,
    call_pause_store: MediaStateStore,
) {
    tokio::spawn(async move {
        let conn = match Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                warn!("media-watch: session bus unavailable: {e}; auto-follow disabled");
                return;
            }
        };
        let mut last_playing = false;
        let mut last_own = false;
        // Our play-start on the mono_ms timeline, mirrored into
        // watch.play_epoch_mono. 0 = not playing. See the epoch-maintenance
        // block in the loop for the freeze-across-handoff semantics.
        let mut play_epoch_mono: u64 = 0;
        let mut last_grab: Option<Instant> = None;
        let mut suppress_until: Option<Instant> = None;
        // Hand-off pause/resume (see the phone MediaHandoffCoordinator for
        // the model). `paused` holds the players we silenced; `have_paused`
        // means resume them when the buds return; `grabbing` marks a
        // forward grab in flight (used for the advertised flag).
        let mut paused: Vec<String> = Vec::new();
        let mut have_paused = false;
        let mut grabbing = false;
        // Set when a forward grab overran RESUME_TIMEOUT: the hold is kept
        // (players still remembered) for LATE_RESUME_WINDOW so a slow A2DP
        // connect still gets its resume. Cleared on resume / final drop.
        let mut grab_late = false;
        let mut paused_at: Option<Instant> = None;
        // Return-to-phone timer: armed when our media stops while we own
        // the buds; fires after RETURN_DELAY.
        let mut return_at: Option<Instant> = None;
        // Players that were Playing on the PREVIOUS tick. On a loss we
        // remember these (route-sensitive players auto-pause before we can
        // query them, so a query-at-loss returns nothing).
        let mut last_playing_set: Vec<String> = Vec::new();
        // When we last GAINED the buds. We don't arm the return timer for
        // RETURN_GRACE afterwards, so a route-change auto-pause right after
        // a grab (firefox stops the instant the sink switches) doesn't fire
        // a spurious return before our resume has settled the player.
        let mut gained_at: Option<Instant> = None;

        loop {
            tokio::time::sleep(POLL).await;
            let now = Instant::now();

            let mac = match crate::core::earbuds_store::load() {
                Some(s) => s.address,
                None => continue,
            };
            let addr: Address = match mac.parse() {
                Ok(a) => a,
                Err(_) => continue,
            };

            let playing_set = playing_players(&conn).await;
            let playing = !playing_set.is_empty();
            let own = audio_active(&adapter, addr).await;

            // Lost the buds (the phone grabbed them). Arm anti-ping-pong;
            // if we were playing, remember the players to resume from
            // position when they return. We use the PREVIOUS tick's set
            // (last_playing_set): a route-sensitive player like firefox has
            // already auto-paused by now, so querying live returns nothing
            // — the exact bug that resumed 0 players. Also pause any
            // straggler still playing through the laptop speakers.
            if last_own && !own {
                suppress_until = Some(now + LOSS_SUPPRESS);
                // Did the buds go TO THE PEER, or did they just go away?
                //
                // The difference decides whether we hold our media for a
                // hand-off that is actually happening. Without it, switching
                // the earbuds off or dropping them in the case looked identical
                // to the phone grabbing them: we remembered the players, and
                // the enforcer below then re-paused the user's own Play every
                // 200 ms for the next 90 seconds. Continuing on the laptop
                // speakers — the obvious thing to do when your earbuds die —
                // became impossible, and the machine was visibly fighting the
                // person using it.
                let peer_took_them = watch
                    .peer_holds_buds_seen
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(|t| t.elapsed() < PEER_FRESH)
                    .unwrap_or(false);
                if last_playing && !have_paused && !peer_took_them {
                    info!("media-watch: buds left but no peer holds them — leaving media alone");
                }
                if last_playing && !have_paused && peer_took_them {
                    let _ = pause_all_playing(&conn).await;
                    paused = last_playing_set.clone();
                    info!(
                        "media-watch: buds left laptop while playing → remember {} player(s)",
                        paused.len()
                    );
                    have_paused = true;
                    paused_at = Some(now);
                }
                grabbing = false;
            }
            if !last_own && own {
                gained_at = Some(now);
            }
            last_own = own;

            // --- Last-play-wins epoch maintenance ---
            // Stamp a FRESH epoch only on a genuine user play-edge: playing
            // rose AND we are not mid-handoff (have_paused). When have_paused
            // is set, a resume is the SAME listening session recovering from
            // our own switch-pause, so we KEEP the prior epoch (freeze) — that
            // is what stops an auto-resume from out-bidding the peer and
            // re-triggering a grab. A genuine stop (nothing playing, nothing
            // remembered) clears it. (Cross-device compare assumes NTP-level
            // clock agreement; play-edges that matter are seconds apart.)
            if playing && !last_playing && !have_paused {
                play_epoch_mono = mono_ms();
                watch.play_epoch_mono.store(play_epoch_mono, Ordering::Relaxed);
            } else if !playing && !have_paused {
                play_epoch_mono = 0;
                watch.play_epoch_mono.store(0, Ordering::Relaxed);
            }

            // Yield/loss enforcer: while we hold a hand-off pause record
            // (have_paused) but do NOT own the buds, any player that
            // auto-resumed is (a) leaking to the laptop SPEAKER and (b) about
            // to mint a spurious epoch. Re-pause every tick so the local media
            // stays silent until the buds return or we resume — the laptop
            // equivalent of the phone's pause-enforcer.
            if have_paused && !own && playing {
                // Only while the hand-off is still real. `have_paused` can
                // outlive the peer that caused it (its link drops, its media
                // stops, it hands the buds to nobody), and re-pausing past that
                // point is just silencing the user for no reason.
                let peer_still_has_them = watch
                    .peer_holds_buds_seen
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(|t| t.elapsed() < PEER_FRESH)
                    .unwrap_or(false);
                if peer_still_has_them {
                    let _ = pause_all_playing(&conn).await;
                } else if have_paused {
                    info!("media-watch: peer no longer holds the buds — releasing the pause hold");
                    have_paused = false;
                    paused.clear();
                    paused_at = None;
                }
            }

            // Resume what we paused once the buds are ours (or on timeout).
            // A FORWARD grab should land in ~1-2s, so a short timeout means
            // "give up". A LOSS-remember (the peer took the buds) waits far
            // longer — the buds only come back when the peer's media stops,
            // which can be minutes; a short timeout here would clear the
            // remembered state and never resume on the eventual return.
            if have_paused {
                // Convergence (#2): while we're holding for a loss/yield (NOT a
                // forward grab) and the peer is STILL the more-recent player,
                // keep pushing the give-up timer forward so it never fires.
                // Without this the 90s LOSS_RESUME_TIMEOUT would clear
                // have_paused, our media would auto-resume and mint a fresh
                // (newer) epoch, and we'd re-grab — a 90s-period bounce. We
                // stay silent until the peer ACTUALLY stops (then the buds
                // return → resume, or — if the return is lost — the timer
                // finally runs out from the moment the peer stopped).
                if !grabbing && !own {
                    let peer_still_winner = watch.peer_playing.load(Ordering::Relaxed) && {
                        let pe = watch.peer_play_epoch_mono.load(Ordering::Relaxed);
                        pe != 0 && (play_epoch_mono == 0 || pe > play_epoch_mono)
                    };
                    if peer_still_winner {
                        paused_at = Some(now);
                    }
                }
                let limit = if grabbing {
                    RESUME_TIMEOUT
                } else if grab_late {
                    LATE_RESUME_WINDOW
                } else {
                    LOSS_RESUME_TIMEOUT
                };
                let timed_out = paused_at
                    .map(|p| now.duration_since(p) > limit)
                    .unwrap_or(true);
                if own {
                    let to_resume = std::mem::take(&mut paused);
                    have_paused = false;
                    grabbing = false;
                    grab_late = false;
                    // The buds are back on the laptop, so any call-pause
                    // record is moot — drain it. A media grab trips the BLE
                    // fast-path `pause_playing_for_call`, and without this a
                    // stale "Paused" record would survive and make the next
                    // REAL call's pause bail ("state already Paused").
                    clear_call_pause(&call_pause_store).await;
                    info!(
                        "media-watch: buds back; resuming {} player(s)",
                        to_resume.len()
                    );
                    if !to_resume.is_empty() {
                        // Force the output onto the buds (wake the SUSPENDED
                        // bluez sink) BEFORE playing — exactly like the
                        // call-end resume — so playback starts fast and
                        // never lands on a not-ready sink (no play-then-
                        // pause). Spawned so the poll loop never stalls on
                        // the route wait.
                        let mac_c = mac.clone();
                        let conn_c = conn.clone();
                        let adapter_c = adapter.clone();
                        let addr_c = addr;
                        tokio::spawn(async move {
                            let outcome = crate::core::audio_route::wait_for_route(&mac_c).await;
                            // Hold the freshly-routed bluez sink continuously
                            // awake across the A2DP codec-negotiation + late
                            // SUSPEND/re-open wave, THEN let it settle briefly
                            // before the first real Play. This makes the codec
                            // negotiate under a silent stream — so when firefox
                            // resumes it joins a stable route and does NOT
                            // auto-pause, which is what eliminates the audible
                            // "play→pause→play→pause" stutter on the return to
                            // Linux (the safety-net below then mostly no-ops).
                            if let Some(sink) = outcome.sink {
                                crate::core::audio_route::spawn_sink_keepalive(
                                    sink,
                                    Duration::from_millis(3_000),
                                );
                                tokio::time::sleep(Duration::from_millis(350)).await;
                            }
                            // Players (firefox) ignore the first Play while the
                            // sink re-routes, AND — critically — WirePlumber
                            // fires route-migration auto-pause *waves* that can
                            // re-pause a player up to several seconds AFTER it
                            // started. So we must NOT stop at the first success:
                            // we keep checking each player across a long window
                            // and re-issue Play to any that fell back to Paused.
                            // This is the ecosystem 63eeaee safety-net behaviour
                            // (the old version that "worked very well") and is
                            // the fix for "resumes, then pauses again". Absolute
                            // checkpoints (ms after route-ready): out to ~7s,
                            // past the late sink-suspend/re-open wave.
                            // Sleep DELTAS between checkpoints; cumulative reach
                            // ~7s (0,300,600,1000,1500,2200,3000,4000,5000,6000,
                            // 7000 ms after route-ready).
                            let deltas: [u64; 11] =
                                [0, 300, 300, 400, 500, 700, 800, 1000, 1000, 1000, 1000];
                            let mut elapsed_ms = 0u64;
                            for delta in deltas {
                                if delta > 0 {
                                    tokio::time::sleep(Duration::from_millis(delta)).await;
                                    elapsed_ms += delta;
                                }
                                // STRICT sound-only-in-earbuds + anti-ping-pong:
                                // if the buds have LEFT the laptop (the phone
                                // grabbed them back) while this safety-net is
                                // still running, ABORT — do NOT keep force-
                                // playing. Two reasons: (a) playing now would
                                // blast the laptop SPEAKERS (the buds are
                                // elsewhere); (b) re-asserting a "Playing" MPRIS
                                // state is read by the watcher's grab gate as a
                                // fresh local play-edge → it re-grabs the buds →
                                // the phone loses them → ping-pong every ~12s
                                // (the user-reported "ovoz tashqariga chiqib
                                // ketadi / bazan o'tmaydi" bug). A SUSPENDED-but-
                                // present bluez sink still counts as ours, so the
                                // A2DP codec-negotiation churn right after a grab
                                // does NOT trip this — only a real loss does.
                                if !audio_active(&adapter_c, addr_c).await {
                                    info!(
                                        "media-watch resume safety-net: buds left laptop — aborting re-play (sound stays in earbuds)"
                                    );
                                    break;
                                }
                                let mut replayed: Vec<String> = Vec::new();
                                for p in &to_resume {
                                    if !crate::core::media_runtime::player_is_playing(&conn_c, p)
                                        .await
                                        .unwrap_or(false)
                                    {
                                        play_players(&conn_c, std::slice::from_ref(p)).await;
                                        replayed.push(p.clone());
                                    }
                                }
                                if !replayed.is_empty() {
                                    info!(
                                        checkpoint_ms = elapsed_ms,
                                        ?replayed,
                                        "media-watch resume safety-net: re-played paused player(s)"
                                    );
                                }
                            }
                        });
                    }
                } else if timed_out {
                    if grabbing {
                        // The switch is likely still in flight (the
                        // orchestrator watchdog runs to ~14s; A2DP beats 8s
                        // only on a clean connect). Do NOT drop the
                        // remembered players — downgrade to a bounded late
                        // window so the buds' LATE arrival still resumes.
                        // Dropping here was the "buds switched over but
                        // nothing played" bug.
                        grabbing = false;
                        grab_late = true;
                        paused_at = Some(now);
                        tracing::warn!(
                            players = paused.len(),
                            "media-watch: buds slow to arrive; holding pause for a late resume"
                        );
                    } else {
                        // STRICT sound-only-in-earbuds: the buds never
                        // arrived (grab failed / peer kept them past the
                        // un-stick window). Resuming here would blast the
                        // laptop SPEAKERS — stay silent and just drop the
                        // remembered pause. The user's next play press
                        // re-runs the grab, and plays instantly if the buds
                        // showed up meanwhile.
                        let n = paused.len();
                        paused.clear();
                        have_paused = false;
                        grab_late = false;
                        tracing::warn!(
                            players = n,
                            "media-watch: buds never arrived; staying paused (sound only in earbuds)"
                        );
                    }
                }
            }

            // Media playing while the buds are elsewhere → pause + grab
            // immediately; catch-up keeps retrying until owned. Loss-
            // suppress is honored (NOT pierced by a fresh edge): a player
            // that auto-pauses/plays on every route change (firefox) would
            // otherwise oscillate grab↔return and orphan the buds.
            //
            // `have_paused && grabbing` is what makes the retry the comment
            // below promises actually happen. Gating on `playing` alone could
            // not: the first thing a grab does is pause the local media, so by
            // the next tick `playing` is false and the block was never
            // re-entered. A grab that failed — the orchestrator busy, its
            // three-second Failed window, the buds refusing to answer a page —
            // was therefore attempted exactly once and then abandoned, with the
            // media still held and the buds on nobody. The `grabbing` flag is
            // only set by this path (the loss-remember clears it), so this
            // re-enters for OUR unfinished grab and not for a yield hold.
            if LAPTOP_AUTO_GRAB && (playing || (have_paused && grabbing)) && !own {
                let suppressed = suppress_until.map(|s| now < s).unwrap_or(false);
                let cooling = last_grab
                    .map(|g| now.duration_since(g) < GRAB_COOLDOWN)
                    .unwrap_or(false);
                // Nothing to grab unless the phone is reachable AND currently
                // holds the buds. Without this, playing on the laptop with no
                // phone around / the buds in their case would pause our media
                // for a switch that can't complete (the symmetric of the
                // Android `peerHoldsBuds` gate).
                let peer_has_buds = watch
                    .peer_holds_buds_seen
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(|t| t.elapsed() < PEER_FRESH)
                    .unwrap_or(false);
                // Last-play-wins: if the peer is playing AND started its
                // session MORE recently than ours (greater epoch), it won —
                // YIELD. Pause our media so it doesn't leak to the laptop
                // speaker (the enforcer above keeps it down) and do NOT grab;
                // the buds stay on the phone. This convergence gate replaces
                // the old dueling grab↔grab ping-pong: only the more-recent
                // player pulls the buds, the other stays paused.
                let peer_played_last = watch.peer_playing.load(Ordering::Relaxed) && {
                    let pe = watch.peer_play_epoch_mono.load(Ordering::Relaxed);
                    pe != 0 && play_epoch_mono != 0 && pe > play_epoch_mono
                };
                if peer_played_last {
                    if !have_paused {
                        paused = pause_all_playing(&conn).await;
                        have_paused = true;
                        paused_at = Some(now);
                        grabbing = false;
                        info!("media-watch: peer played more recently → yield buds + pause local media");
                    } else {
                        // A player started while an older hold is active
                        // (the enforcer will silence it this tick). Merge it
                        // into the remembered set so the eventual resume
                        // targets it too — the hold's list predates it.
                        for p in &playing_set {
                            if !paused.contains(p) {
                                paused.push(p.clone());
                            }
                        }
                    }
                } else if watch.enabled.load(Ordering::Relaxed)
                    && !in_call.load(Ordering::Relaxed) // call #1 priority
                    && !suppressed
                    && !cooling
                    && peer_has_buds
                {
                    let peer_pub = peer_store
                        .list()
                        .ok()
                        .and_then(|v| v.first().map(|p| p.peer_static_pub));
                    if let Some(peer_pub) = peer_pub {
                        if !have_paused {
                            paused = pause_all_playing(&conn).await;
                            have_paused = true;
                        } else {
                            // Grabbing on top of an existing hold (earlier
                            // loss/yield): the player the user just started
                            // isn't in the remembered set — merge it so the
                            // arrival resume doesn't skip it.
                            for p in &playing_set {
                                if !paused.contains(p) {
                                    paused.push(p.clone());
                                }
                            }
                        }
                        grabbing = true;
                        grab_late = false;
                        // (Re)start the resume-timeout clock from the grab,
                        // even when we were ALREADY paused from a preceding
                        // loss-remember. Otherwise a stale loss-era paused_at
                        // makes RESUME_TIMEOUT expire before the A2DP connect
                        // (~4s) finishes — the watcher then resumes while
                        // own=false and the audio plays through the laptop
                        // SPEAKER for a few seconds instead of staying paused
                        // until the buds arrive. Stamping it here means the
                        // timeout is measured from the grab, so own=true wins
                        // the race and the resume lands on the buds. This is
                        // the "pause until the buds switch over" fix.
                        paused_at = Some(now);
                        // request() returns fast (CAS + spawn). Only burn
                        // the cooldown if the switch actually STARTED; on a
                        // busy Err, retry next tick instead of waiting the
                        // whole cooldown — the "sometimes doesn't switch" bug.
                        match orchestrator.request(peer_pub, mac.clone()).await {
                            Ok(()) => {
                                last_grab = Some(now);
                                info!("media-watch: media on laptop & buds elsewhere → pause + grab to laptop");
                            }
                            Err(e) => {
                                tracing::debug!("media-watch grab busy/err (retry next tick): {e}");
                            }
                        }
                    }
                }
            }

            // Return-to-phone: our media stopped while we hold the buds →
            // after RETURN_DELAY, disconnect them and flag the phone to
            // grab (it resumes its own paused media).
            if playing {
                return_at = None;
            } else if LAPTOP_PUSH_ON_STOP
                && own
                && last_playing
                && gained_at
                    .map(|g| now.duration_since(g) >= RETURN_GRACE)
                    .unwrap_or(true)
            {
                return_at = Some(now);
            }
            if let Some(t) = return_at {
                if own
                    && !playing
                    && now.duration_since(t) >= RETURN_DELAY
                    && watch.enabled.load(Ordering::Relaxed)
                    && !in_call.load(Ordering::Relaxed)
                {
                    return_at = None;
                    info!(
                        "media-watch: media stopped {}ms ago → hand buds back to phone",
                        RETURN_DELAY.as_millis()
                    );
                    // Disconnect the buds, then claim the phone two ways:
                    // a fast AudioOp::Claim over BLE/LAN (lands in ~200 ms)
                    // and the audio_claim_request flag on the next heartbeat
                    // as a fallback. The phone grabs and resumes its media.
                    let peer_pub = peer_store
                        .list()
                        .ok()
                        .and_then(|v| v.first().map(|p| p.peer_static_pub));
                    let a = adapter.clone();
                    let m = mac.clone();
                    let o = orchestrator.clone();
                    tokio::spawn(async move {
                        let _ = disconnect_audio_initiate(&a, &m).await;
                        if let Some(pp) = peer_pub {
                            o.send_claim(pp, m).await;
                        }
                    });
                    watch.claim_peer.store(true, Ordering::Relaxed);
                }
            }
            last_playing = playing;
            last_playing_set = playing_set;

            // Advertise handoff-aware media-device status so the phone's
            // release trigger stays valid even while we've paused.
            let advertised = grabbing || (playing && own);
            watch.playing.store(advertised, Ordering::Relaxed);
        }
    });
}
