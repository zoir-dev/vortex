//! Linux mirror of `a3/.../earbuds/SwitchOrchestrator.kt`. Same state
//! machine and same *wire contract* — the binding part both sides MUST
//! agree on (either can be initiator or owner): the `AudioOp` JSON wire
//! format (`audio_op.rs` ⇔ `AudioOp.kt`), the per-peer nonce semantics,
//! and the GATT UUIDs (`ble/mod.rs` ⇔ `core/ble/Constants.kt`, spec §10.1).
//!
//! The timeout / retry constants at the bottom of this file are NOT part
//! of that contract — each side tunes them independently (e.g.
//! `CONNECT_RETRY_COUNT` is deliberately 2 here vs 3 on Android). Don't
//! "sync" them to the peer; only the wire format, nonce, and UUIDs bind.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{watch, Mutex};
use tracing::{info, warn};

use crate::core::audio_op::{AudioOp, AudioOpFrame, RejectReason, Stage};
use crate::core::audio_switch::{
    confirm_audio_disconnected, connect_audio, disconnect_audio_initiate, SwitchError,
};
use crate::core::audio_switch_persistence as persist;
use crate::core::storage::peers::PeerStore;

/// Sender callback supplied by the transport layer (LAN preferred,
/// BLE fallback). Returns an error if neither transport is up — the
/// orchestrator surfaces that as `SwitchState::Failed`.
pub type Sender = Arc<
    dyn Fn([u8; 32], AudioOpFrame) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// External policy decision for an incoming Request — set by the
/// Phase-2 call orchestrator. Phase 1 always returns `Allow`.
#[derive(Debug, Clone)]
pub enum Acceptance {
    Allow,
    Reject(RejectReason),
}

pub type AcceptanceProvider = Arc<dyn Fn() -> Acceptance + Send + Sync>;

/// BT-layer side effects the switch flow drives, behind a trait so the
/// state machine can be exercised in unit tests with a fake backend
/// instead of a live `bluer::Adapter` (the dependency that made the
/// orchestrator untestable without hardware). Production wiring uses
/// [`BluerBt`], a thin wrapper over the real adapter + the `audio_switch`
/// free functions — the trait adds no behaviour, only a seam.
pub trait BtControl: Send + Sync {
    /// Attempt a single A2DP connect to `mac` (the initiator's claim).
    fn connect<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>>;
    /// Initiate — but do NOT wait to settle — an ACL drop of `mac` (the
    /// responder's release). Returns once BlueZ accepts the Disconnect.
    fn disconnect_initiate<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>>;
    /// Background confirmation that `mac` actually dropped, bounded by
    /// `timeout`. Hygiene only — does not gate the peer.
    fn confirm_disconnected<'a>(&'a self, mac: &'a str, timeout: Duration) -> BoxFuture<'a, bool>;
    /// Whether `mac` is currently connected to us — the live half of the
    /// duplicate-reclaim guard.
    fn is_connected<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, bool>;
}

/// Production [`BtControl`] over a real `bluer::Adapter`. Each method
/// delegates to the matching `audio_switch` free function, so behaviour
/// is byte-for-byte what it was before the trait was introduced.
pub struct BluerBt {
    adapter: bluer::Adapter,
}

impl BluerBt {
    pub fn new(adapter: bluer::Adapter) -> Self {
        Self { adapter }
    }
}

impl BtControl for BluerBt {
    fn connect<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>> {
        Box::pin(connect_audio(&self.adapter, mac))
    }
    fn disconnect_initiate<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>> {
        Box::pin(disconnect_audio_initiate(&self.adapter, mac))
    }
    fn confirm_disconnected<'a>(&'a self, mac: &'a str, timeout: Duration) -> BoxFuture<'a, bool> {
        Box::pin(confirm_audio_disconnected(&self.adapter, mac, timeout))
    }
    fn is_connected<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Ok(addr) = mac.parse::<bluer::Address>() else {
                return false;
            };
            match self.adapter.device(addr) {
                Ok(device) => device.is_connected().await.unwrap_or(false),
                Err(_) => false,
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchState {
    Idle,
    Preparing,
    WaitingApproval,
    WaitingReleased,
    /// Local BT connect is being attempted. UI treats this as "still
    /// switching" and dims the card.
    Connecting,
    /// Peer has confirmed it released the buds (Released frame arrived)
    /// but our local BT connect hasn't completed yet. UI surfaces this
    /// as "Connected here" already — the BT takes another ~2-3 s to
    /// physically establish A2DP, but the user gets instant feedback.
    /// Click is still blocked (a new flow can't start until BT settles).
    AlmostDone,
    Failed(String),
}

/// One in-flight switch — the matching peer + mac + nonce. We only
/// allow one flow at a time (V1 single-peer).
#[derive(Debug, Clone)]
struct ActiveFlow {
    peer_pub: [u8; 32],
    mac: String,
}

impl ActiveFlow {
    fn matches(&self, peer: &[u8; 32], mac: &str) -> bool {
        &self.peer_pub == peer && self.mac == mac
    }
}

pub struct SwitchOrchestrator {
    bt: Arc<dyn BtControl>,
    peer_store: Arc<dyn PeerStore>,
    sender: Sender,
    acceptance: AcceptanceProvider,
    state_tx: watch::Sender<SwitchState>,
    state_rx: watch::Receiver<SwitchState>,
    initiator: Arc<Mutex<Option<ActiveFlow>>>,
    /// Bumped every time a flow starts. A `connect_profile` that outlives its
    /// own flow compares this before acting on its result — see
    /// [`Self::attempt_connect`].
    flow_gen: Arc<std::sync::atomic::AtomicU64>,
    responder: Arc<Mutex<Option<ActiveFlow>>>,
    /// Identity of the in-flight flow, kept in sync with [`Self::initiator`]
    /// so the persistence row captures *which* peer + mac the current
    /// non-Idle state belongs to. Cleared when state returns to Idle.
    /// Held outside the initiator/responder mutexes so a `save` call
    /// doesn't have to await another lock.
    current_peer: Arc<Mutex<Option<[u8; 32]>>>,
    current_mac: Arc<Mutex<Option<String>>>,
    /// When the last switch for a given mac completed. Used to suppress
    /// the duplicate "reclaim" that BLE and LAN both deliver a beat
    /// apart: the second one would tear down the freshly-grabbed A2DP
    /// link and race the peer to re-establish it — the trigger that
    /// wedges the connect and shows up as "stuck in switching".
    last_complete: Arc<Mutex<Option<(String, tokio::time::Instant)>>>,
}

impl SwitchOrchestrator {
    pub fn new(
        bt: Arc<dyn BtControl>,
        peer_store: Arc<dyn PeerStore>,
        sender: Sender,
        acceptance: AcceptanceProvider,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel(SwitchState::Idle);
        Self {
            bt,
            peer_store,
            sender,
            acceptance,
            state_tx,
            state_rx,
            initiator: Arc::new(Mutex::new(None)),
            flow_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            responder: Arc::new(Mutex::new(None)),
            current_peer: Arc::new(Mutex::new(None)),
            current_mac: Arc::new(Mutex::new(None)),
            last_complete: Arc::new(Mutex::new(None)),
        }
    }

    pub fn state(&self) -> watch::Receiver<SwitchState> {
        self.state_rx.clone()
    }

    /// MAC of the in-flight switch flow, or `None` when idle. Used by
    /// the Tauri layer to wait for the matching PulseAudio sink to
    /// settle before calling `resume_paused_for_call` — without that
    /// gate the player plays through the laptop speaker for half a
    /// syllable before WirePlumber migrates the route and re-pauses
    /// it.
    pub async fn current_mac(&self) -> Option<String> {
        self.current_mac.lock().await.clone()
    }

    /// Initiator entry — user clicked "Bring to laptop".
    pub async fn request(self: &Arc<Self>, peer_pub: [u8; 32], mac: String) -> Result<(), String> {
        // Duplicate-reclaim guard. BLE (Claim frame) and LAN
        // (audio_claim_request flag) both deliver the same "bring the
        // buds back" intent within a second or two of each other. The
        // first wins and grabs the buds; the second would run a full
        // initiator flow against an A2DP link we *already* hold — the
        // pre-connect teardown in `connect_audio` drops the healthy
        // link and races the peer to re-establish it, which is exactly
        // what wedged the connect in the observed "stuck in switching"
        // trace. If we completed a switch for this mac moments ago and
        // the buds are still ours, the work is already done: ack the
        // peer with Done so its UI clears, and no-op.
        if self.is_recent_duplicate(&mac).await {
            info!(%mac, "request: duplicate reclaim within window; buds already ours — Done + no-op");
            if let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await {
                let _ = (self.sender)(
                    peer_pub,
                    AudioOpFrame { nonce, op: AudioOp::Done, mac: mac.clone(), ts: now_sec() },
                )
                .await;
            }
            return Ok(());
        }
        // Stash identity BEFORE the CAS so the persistence row written
        // by `cas_state` knows which peer + mac this flow belongs to.
        // If the CAS loses the race, we clear them again.
        *self.current_peer.lock().await = Some(peer_pub);
        *self.current_mac.lock().await = Some(mac.clone());
        if !self.cas_state(SwitchState::Idle, SwitchState::Preparing) {
            *self.current_peer.lock().await = None;
            *self.current_mac.lock().await = None;
            return Err(format!("already in {:?}", *self.state_rx.borrow()));
        }
        *self.initiator.lock().await = Some(ActiveFlow { peer_pub, mac: mac.clone() });

        let me = self.clone();
        tokio::spawn(async move { me.run_initiator(peer_pub, mac).await });
        Ok(())
    }

    /// Send an `AudioOp::Claim` to the peer — "I hold the buds; come grab
    /// them." Used by the laptop media-watch's return-to-phone path: when
    /// our media stops we disconnect the buds and claim the phone, which
    /// grabs them and resumes its own paused media. Fire-and-forget over
    /// the same BLE+LAN race as every other frame (fast, ~200 ms via BLE,
    /// vs. the slow AppState heartbeat the claim flag would otherwise ride).
    pub async fn send_claim(self: &Arc<Self>, peer_pub: [u8; 32], mac: String) {
        let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await else {
            warn!("send_claim: nonce unavailable; peer not asked to claim (the audio_claim_request heartbeat flag is the fallback)");
            return;
        };
        let _ = (self.sender)(
            peer_pub,
            AudioOpFrame { nonce, op: AudioOp::Claim, mac, ts: now_sec() },
        )
        .await;
    }

    /// R1 — on daemon start, replay the persistence decision tree
    /// and either resume the in-flight Connecting attempt or surface
    /// a rolled-back Failed state. Must be called once after
    /// construction, before any frame ingress.
    pub async fn recover_on_start(self: &Arc<Self>) {
        match persist::recover() {
            persist::Action::None => {}
            persist::Action::Rollback(reason) => {
                warn!(%reason, "recover: rollback");
                let _ = self.state_tx.send(SwitchState::Failed(reason.clone()));
                let _ = persist::clear();
                let state_tx = self.state_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(FAILED_RESET_MS)).await;
                    let _ = state_tx.send(SwitchState::Idle);
                });
            }
            persist::Action::ResumeConnect { peer_pub, mac } => {
                info!(?peer_pub, %mac, "recover: resume Connecting");
                *self.current_peer.lock().await = Some(peer_pub);
                *self.current_mac.lock().await = Some(mac.clone());
                *self.initiator.lock().await = Some(ActiveFlow { peer_pub, mac: mac.clone() });
                let _ = self.state_tx.send(SwitchState::Connecting);
                // Same backstop as run_initiator: a resumed Connecting
                // attempt that wedges must still self-heal to Idle.
                self.arm_flow_watchdog();
                let me = self.clone();
                tokio::spawn(async move { me.attempt_connect(peer_pub, mac).await });
            }
        }
    }

    /// Frame ingress.
    pub async fn on_incoming(
        self: Arc<Self>,
        peer_pub: [u8; 32],
        frame: AudioOpFrame,
    ) -> Result<(), String> {
        // Replay rejection — atomic load-compare-commit. The old
        // load → if-greater → commit shape let BLE and LAN deliver
        // the same frame concurrently and both see the pre-commit
        // value, so both dispatched a handler for the same frame
        // (ChatGPT review #2). `try_accept_audio_in_nonce` does
        // the read+check+write inside one mutex window. Pushed to
        // the blocking pool for the same reason as `next_out_nonce`
        // (O4): it's a secret-service D-Bus round-trip that can
        // stall for seconds under mutex contention, and at switch
        // time BLE + LAN frames arrive in a burst.
        let accepted = {
            let ps = self.peer_store.clone();
            let nonce = frame.nonce;
            tokio::task::spawn_blocking(move || ps.try_accept_audio_in_nonce(&peer_pub, nonce))
                .await
                .map_err(|e| format!("try_accept join: {e}"))?
                .map_err(|e| format!("try_accept_audio_in_nonce: {e}"))?
        };
        if !accepted {
            warn!(nonce = frame.nonce, "audio_op nonce replay; dropping");
            return Ok(());
        }

        match frame.op {
            AudioOp::Request => self.start_responder(peer_pub, frame.mac).await,
            AudioOp::Claim => self.on_claim(peer_pub, frame.mac).await,
            AudioOp::Approve => self.on_approve(peer_pub, frame.mac).await,
            AudioOp::Released => self.on_released(peer_pub, frame.mac).await,
            AudioOp::Reject { reason } => self.on_reject(peer_pub, reason).await,
            AudioOp::Done => self.on_done(peer_pub).await,
            AudioOp::Failed { stage, message } => self.on_peer_failed(peer_pub, stage, message).await,
        }
        Ok(())
    }

    /// Peer holds the buds and is asking us to claim them. We become
    /// the initiator and run a normal request flow. Idempotent: if a
    /// flow is already in progress, drop silently (peer will retry on
    /// the next click).
    async fn on_claim(self: Arc<Self>, peer_pub: [u8; 32], mac: String) {
        if *self.state_rx.borrow() != SwitchState::Idle {
            info!("on_claim: already in flow; dropping");
            return;
        }
        info!(?peer_pub, %mac, "on_claim: peer asked us to claim");
        if let Err(e) = self.request(peer_pub, mac).await {
            warn!("on_claim: request() rejected: {e}");
        }
    }

    // ---- Initiator ----

    async fn run_initiator(self: Arc<Self>, peer_pub: [u8; 32], mac: String) {
        // Arm the stuck-flow watchdog the instant the initiator starts.
        // Everything below can only leave the state machine in a
        // non-terminal in-flight state (Preparing → Connecting →
        // AlmostDone); if any BlueZ/D-Bus leg wedges (e.g. a hung
        // connect_profile that even the bounded timeout somehow
        // outlives, or an unforeseen await), this guarantees the state
        // returns to Idle so later switches aren't dropped as "busy".
        self.arm_flow_watchdog();

        self.flow_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await else {
            self.fail_initiator("nonce store unavailable".to_string());
            return;
        };
        info!(?peer_pub, %mac, nonce, "initiator: requesting buds");

        // NOTE: we deliberately do NOT fire a separate background
        // `disconnect_audio` here anymore. The initiator is the CLAIMER —
        // the buds are on the peer, not on us (or only a stale auto-
        // reconnect). `connect_audio` already tears down a stale local
        // link in its own pre-connect cleanup. Running a second disconnect
        // in parallel raced `connect_profile`'s negotiation and BlueZ
        // answered `br-connection-canceled`, which made connect_profile
        // retry internally for ~10.8s (observed bluez_ms=10780 on the
        // return). One disconnect path (inside connect_audio) = no race.

        // Fire-and-forget Request. We do NOT await an Approve / Released
        // round-trip before starting the local BT connect — that was the
        // sequential cost (~700 ms) that made the old V1 implementation
        // four times slower than the prototype. Approve / Released are
        // now informational only; the actual switch handshake is at the
        // BT layer (retry connect until the phone finishes releasing).
        let me_send = self.clone();
        let peer_send = peer_pub;
        let mac_send = mac.clone();
        tokio::spawn(async move {
            let frame = AudioOpFrame {
                nonce,
                op: AudioOp::Request,
                mac: mac_send,
                ts: now_sec(),
            };
            if let Err(e) = (me_send.sender)(peer_send, frame).await {
                // The phone won't receive the polite ask; the connect
                // retries below will still drive the switch, but the
                // phone's audio may stay grabbed until its OS notices
                // BT was lost. Log and continue.
                warn!("send Request: {e}");
            }
        });

        // Transition to Connecting and start the retry loop in parallel.
        // attempt_connect bails out early if state has already flipped
        // to Failed (e.g. peer Reject came in).
        if !self.cas_state(SwitchState::Preparing, SwitchState::Connecting) {
            return;
        }
        self.attempt_connect(peer_pub, mac).await;
    }

    /// Approve is informational: the peer has acknowledged it's
    /// releasing. We don't gate the local connect on it any more — by
    /// the time the Approve arrives, attempt_connect has already fired
    /// its first attempt.
    async fn on_approve(self: Arc<Self>, peer_pub: [u8; 32], _mac: String) {
        let g = self.initiator.lock().await;
        let Some(ref f) = *g else { return };
        if f.peer_pub != peer_pub {
            return;
        }
        info!("informational: peer Approve received");
    }

    /// Released is the optimistic-UI signal: the peer has confirmed
    /// it dropped the buds. The local BT connect still has ~2-3 s to
    /// go (BlueZ + buds radio), but the UI can show "Connected here"
    /// already. We flip the state to AlmostDone so the UI clears the
    /// "Switching…" dim immediately. The retry loop in attempt_connect
    /// keeps running and either transitions to Idle on success or
    /// Failed on exhaustion.
    async fn on_released(self: Arc<Self>, peer_pub: [u8; 32], _mac: String) {
        let g = self.initiator.lock().await;
        let Some(ref f) = *g else { return };
        if f.peer_pub != peer_pub {
            return;
        }
        drop(g);
        info!("peer Released received; optimistic UI → AlmostDone");
        let _ = self.cas_state(SwitchState::Connecting, SwitchState::AlmostDone);
    }

    /// Reject is the only NACK that matters — peer refuses to release.
    ///
    /// `InCall` is treated as TEMPORARY rather than final, because on the
    /// measured deployment it almost never meant what it says. The phone keeps
    /// an ended call in its state for ~6 s so the laptop can clear the call
    /// pill promptly, and its acceptance gate reads that same field — so the
    /// hand-back it had just asked for came back refused, and this function
    /// made the refusal terminal. 19 of 20 switch failures in a week were this
    /// one loop, and the earbuds were left on neither device.
    ///
    /// The phone side now honours a Request answering its own Claim, which
    /// removes the cause. This is the belt to that pair of braces: even against
    /// an older phone build, a call that has genuinely just ended stops being
    /// "in call" within seconds, so the right response is to ask again rather
    /// than to give up. Anything else really is the peer saying no.
    async fn on_reject(self: Arc<Self>, peer_pub: [u8; 32], reason: RejectReason) {
        let g = self.initiator.lock().await;
        let Some(ref f) = *g else { return };
        if f.peer_pub != peer_pub {
            return;
        }
        let mac = f.mac.clone();
        drop(g);
        if !matches!(reason, RejectReason::InCall) {
            self.fail_initiator(format!("peer rejected: {reason:?}"));
            return;
        }
        info!(?peer_pub, "peer rejected: InCall — retrying rather than giving up");
        let me = self.clone();
        tokio::spawn(async move {
            // 1s, 2s, 4s: covers the phone's ~6 s post-call linger without
            // hammering, and gives up well before a user would try by hand.
            for wait_ms in [1_000u64, 2_000, 4_000] {
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                // Someone else moved the flow on — nothing left to retry.
                let s = me.state_rx.borrow().clone();
                if matches!(s, SwitchState::Idle | SwitchState::Failed(_)) {
                    return;
                }
                if me.resend_request(peer_pub, &mac).await {
                    return; // asked again; the answer arrives as its own event
                }
            }
            me.fail_initiator("peer rejected: InCall (still in a call after 7s)".into());
        });
    }

    /// Send another `Request` for the same flow. `true` when it went out.
    ///
    /// Used only by the InCall retry above: the flow is already in-flight, so
    /// this re-asks rather than starting anything new.
    async fn resend_request(&self, peer_pub: [u8; 32], mac: &str) -> bool {
        let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await else {
            return false;
        };
        let frame = AudioOpFrame {
            nonce,
            op: AudioOp::Request,
            mac: mac.to_string(),
            ts: now_sec(),
        };
        match (self.sender)(peer_pub, frame).await {
            Ok(()) => {
                info!(?peer_pub, %mac, nonce, "initiator: re-requesting buds after InCall");
                true
            }
            Err(e) => {
                warn!("resend Request: {e}");
                false
            }
        }
    }

    /// Retry-driven local BT connect. The first attempt fires
    /// immediately (in parallel with the Request frame send). If the
    /// phone is still holding the buds, the attempt fails fast and we
    /// retry after CONNECT_RETRY_PAUSE_MS — that pause is tuned to be
    /// roughly the time it takes BlueZ to notice a peer-side
    /// disconnect, so by attempt 2 or 3 we usually win.
    async fn attempt_connect(self: Arc<Self>, peer_pub: [u8; 32], mac: String) {
        // The flow this attempt belongs to. A watchdog reset or a peer
        // Reject can retire our flow and let a NEW one start while a
        // `connect_profile` from this one is still in BlueZ — that call can
        // outlive the 14s watchdog by a wide margin. Its late result then
        // flipped the NEW flow to Idle mid-connect, or dragged it to Failed.
        // Carrying the generation makes a stale continuation harmless.
        let my_gen = self.flow_gen.load(std::sync::atomic::Ordering::SeqCst);
        let mut last_err = String::from("connect not attempted");
        for attempt in 1..=CONNECT_RETRY_COUNT {
            // If a peer Reject arrived between attempts, bail out — the
            // peer isn't going to release.
            // Bail on terminal states. AlmostDone is NOT terminal —
            // the BT connect is still meant to run; the state just
            // means "UI has been told it's done already".
            let s = self.state_rx.borrow().clone();
            if matches!(s, SwitchState::Failed(_) | SwitchState::Idle) {
                tracing::debug!("attempt_connect: state already terminal; stopping retries");
                return;
            }
            match self.bt.connect(&mac).await {
                Ok(()) => {
                    if self.flow_gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                        tracing::info!("attempt_connect: a newer flow owns this now — dropping our result");
                        return;
                    }
                    info!(?peer_pub, attempt, "initiator: switch complete");
                    // Clear the crash-recovery row here as well as in `cas_state`.

                    // This path transitions through `send_if_modified`, which does not

                    // touch persistence — so a COMPLETED switch left "connecting" on

                    // disk, and the next launch rolled it back as stale and spent three

                    // seconds in Failed. A claim arriving in that window was dropped.

                    let _ = crate::core::audio_switch_persistence::clear();
                    // Transition to Idle FIRST so the UI clears
                    // "Switching…" and the lib.rs route+resume watcher
                    // fires immediately. Without this, a slow
                    // `next_audio_out_nonce` (secret-service D-Bus
                    // round-trip under mutex contention) can wedge for
                    // 10-30s, the LAN session's 15s hard-cap drops
                    // the writer, and we end up sending Done into the
                    // void AND the UI is stuck.
                    //
                    // Could be in Connecting (BT raced ahead of
                    // Released) or AlmostDone (Released arrived first).
                    // Either way the next state is Idle.
                    self.state_tx.send_if_modified(|current| {
                        if matches!(*current, SwitchState::Connecting | SwitchState::AlmostDone) {
                            *current = SwitchState::Idle;
                            true
                        } else {
                            false
                        }
                    });
                    *self.initiator.lock().await = None;
                    // Stamp completion so a duplicate reclaim arriving on
                    // the other transport in the next couple of seconds is
                    // recognised as a no-op (see `is_recent_duplicate`).
                    *self.last_complete.lock().await =
                        Some((mac.clone(), tokio::time::Instant::now()));
                    // Fire Done as background best-effort. The peer
                    // doesn't strictly need it — the switch is complete
                    // either way — but it lets the peer's UI flip out
                    // of "switching" on its side without waiting for
                    // its own timeout.
                    let sender = self.sender.clone();
                    let peer_store = self.peer_store.clone();
                    let mac_for_done = mac.clone();
                    tokio::spawn(async move {
                        if let Some(nonce) = next_out_nonce(&peer_store, peer_pub).await {
                            let _ = sender(
                                peer_pub,
                                AudioOpFrame {
                                    nonce,
                                    op: AudioOp::Done,
                                    mac: mac_for_done,
                                    ts: now_sec(),
                                },
                            )
                            .await;
                        }
                    });
                    return;
                }
                Err(e) => {
                    last_err = format!("attempt#{attempt}: {e}");
                    tracing::debug!(%last_err, "connect retry");
                    if attempt < CONNECT_RETRY_COUNT {
                        tokio::time::sleep(Duration::from_millis(CONNECT_RETRY_PAUSE_MS)).await;
                    }
                }
            }
        }
        // All retries exhausted. Best-effort: tell the peer so its UI
        // clears. We fail locally regardless of whether the frame sends.
        if let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await {
            let _ = (self.sender)(
                peer_pub,
                AudioOpFrame {
                    nonce,
                    op: AudioOp::Failed {
                        stage: Stage::Connect,
                        message: last_err.clone(),
                    },
                    mac: mac.clone(),
                    ts: now_sec(),
                },
            )
            .await;
        }
        self.fail_initiator(last_err);
    }

    fn fail_initiator(&self, reason: String) {
        warn!(reason, "initiator: failed");
        let failed = SwitchState::Failed(reason.clone());
        let _ = self.state_tx.send(failed.clone());
        // Persist the Failed snapshot so a relaunch can still surface
        // it (the rollback path in recover_on_start clears + shows the
        // reason briefly). Best-effort.
        let peer = self.current_peer.try_lock().ok().and_then(|g| *g);
        let mac = self.current_mac.try_lock().ok().and_then(|g| g.clone());
        let saved = persist::Saved {
            discriminator: persist::disc::FAILED.to_string(),
            reason: Some(reason.clone()),
            enter_ms: now_ms(),
            peer_pub_hex: peer.map(|p| persist::pub_to_hex(&p)).unwrap_or_default(),
            mac: mac.unwrap_or_default(),
        };
        if let Err(e) = persist::save(&saved) {
            warn!("audio_switch persist save (failed state): {e}");
        }
        let initiator = self.initiator.clone();
        let state_tx = self.state_tx.clone();
        let current_peer = self.current_peer.clone();
        let current_mac = self.current_mac.clone();
        tokio::spawn(async move {
            *initiator.lock().await = None;
            tokio::time::sleep(Duration::from_millis(FAILED_RESET_MS)).await;
            // Only clear identity + persistence if no new flow has
            // started in the meantime — checked by reading the channel.
            if matches!(*state_tx.borrow(), SwitchState::Failed(ref r) if r == &reason) {
                let _ = state_tx.send(SwitchState::Idle);
                *current_peer.lock().await = None;
                *current_mac.lock().await = None;
                let _ = persist::clear();
            }
        });
    }

    // ---- Responder ----

    async fn start_responder(self: Arc<Self>, peer_pub: [u8; 32], mac: String) {
        match (self.acceptance)() {
            Acceptance::Reject(reason) => {
                if let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await {
                    let _ = (self.sender)(
                        peer_pub,
                        AudioOpFrame { nonce, op: AudioOp::Reject { reason }, mac, ts: now_sec() },
                    )
                    .await;
                }
                return;
            }
            Acceptance::Allow => {}
        }
        if *self.state_rx.borrow() != SwitchState::Idle {
            if let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await {
                let _ = (self.sender)(
                    peer_pub,
                    AudioOpFrame {
                        nonce,
                        op: AudioOp::Reject { reason: RejectReason::Busy },
                        mac,
                        ts: now_sec(),
                    },
                )
                .await;
            }
            return;
        }
        *self.responder.lock().await = Some(ActiveFlow { peer_pub, mac: mac.clone() });
        let me = self.clone();
        tokio::spawn(async move { me.run_responder(peer_pub, mac).await });
    }

    async fn run_responder(self: Arc<Self>, peer_pub: [u8; 32], mac: String) {
        // Send Approve first as a fire-and-forget courtesy (informational
        // on the phone). Spawn it so a LAN IK stall can't delay the ACL
        // drop below — the disconnect is the critical path.
        let approve_self = self.clone();
        let approve_peer = peer_pub;
        let approve_mac = mac.clone();
        tokio::spawn(async move {
            if let Some(approve_nonce) = next_out_nonce(&approve_self.peer_store, approve_peer).await {
                let _ = (approve_self.sender)(
                    approve_peer,
                    AudioOpFrame { nonce: approve_nonce, op: AudioOp::Approve, mac: approve_mac, ts: now_sec() },
                )
                .await;
            }
        });

        // Fast ACL drop. `disconnect_audio_initiate` returns the instant
        // BlueZ accepts the Disconnect (~50 ms) — it does NOT wait for
        // the profiles to finish settling. We send `Released` right here,
        // because the phone uses Released as its connect trigger: the
        // sooner it fires, the sooner the phone queues its A2DP connect,
        // which BlueZ lands the moment the buds actually drop. The old
        // code sent Released only after `wait_audio_disconnected`
        // confirmed — and that poll lags the real drop by up to ~1 s
        // (observed: the phone had already grabbed the buds before our
        // "audio device disconnected" log fired). That lag was the bulk
        // of the laptop→phone switch time.
        match self.bt.disconnect_initiate(&mac).await {
            Ok(()) => {
                // Released is the phone's connect trigger — the one frame
                // we cannot afford to silently drop. If the nonce store
                // errors we can't send a valid frame (a nonce=0 frame is
                // replay-dropped), so log loudly. The phone still recovers
                // once its OS notices the ACL drop, just slower.
                match next_out_nonce(&self.peer_store, peer_pub).await {
                    Some(released_nonce) => {
                        let _ = (self.sender)(
                            peer_pub,
                            AudioOpFrame {
                                nonce: released_nonce,
                                op: AudioOp::Released,
                                mac: mac.clone(),
                                ts: now_sec(),
                            },
                        )
                        .await;
                    }
                    None => warn!(
                        %mac,
                        "responder: nonce unavailable; Released NOT sent — phone connect will be delayed"
                    ),
                }
            }
            Err(e) => {
                if let Some(nonce) = next_out_nonce(&self.peer_store, peer_pub).await {
                    let _ = (self.sender)(
                        peer_pub,
                        AudioOpFrame {
                            nonce,
                            op: AudioOp::Failed { stage: stage_from(&e), message: e.to_string() },
                            mac: mac.clone(),
                            ts: now_sec(),
                        },
                    )
                    .await;
                }
                *self.responder.lock().await = None;
                return;
            }
        }

        // Confirm the drop in the background for our own state hygiene —
        // this does NOT gate the phone (it already has Released). If the
        // buds were accepted-but-never-dropped (rare), log it.
        let confirm_bt = self.bt.clone();
        let confirm_mac = mac.clone();
        tokio::spawn(async move {
            if !confirm_bt
                .confirm_disconnected(&confirm_mac, Duration::from_millis(DISCONNECT_TIMEOUT_MS))
                .await
            {
                tracing::warn!(%confirm_mac, "release confirm timed out (phone likely already grabbed the buds)");
            }
        });

        tokio::time::sleep(Duration::from_millis(DONE_WAIT_MS)).await;
        // Lost-Done cleanup — but only if the slot still belongs to THIS
        // flow. The happy path cleared it via on_done seconds ago, and a NEW
        // responder flow may already occupy the slot; clearing blindly here
        // would wipe that live flow's busy-guard mid-switch.
        let mut g = self.responder.lock().await;
        if let Some(ref f) = *g {
            if f.matches(&peer_pub, &mac) {
                tracing::warn!("responder Done never arrived; freeing the slot (watchdog)");
                *g = None;
            }
        }
    }

    async fn on_done(&self, peer_pub: [u8; 32]) {
        let g = self.responder.lock().await;
        let Some(ref f) = *g else { return };
        if f.peer_pub != peer_pub {
            return;
        }
        drop(g);
        *self.responder.lock().await = None;
    }

    async fn on_peer_failed(&self, peer_pub: [u8; 32], _stage: Stage, message: String) {
        warn!(?peer_pub, %message, "peer reported failure");
        let g = self.initiator.lock().await;
        let same = g.as_ref().map(|f| f.peer_pub == peer_pub).unwrap_or(false);
        drop(g);
        if same {
            self.fail_initiator(format!("peer: {message}"));
        }
        *self.responder.lock().await = None;
    }

    // ---- Helpers ----

    /// True if we completed a switch for `mac` within the dedup window
    /// AND the buds are still physically connected to us. Both halves
    /// matter: the time check catches the BLE+LAN double-delivery, and
    /// the live `is_connected` check makes sure we don't suppress a
    /// *legitimate* reclaim after the peer grabbed the buds back in the
    /// meantime (a real new call), where the buds are no longer ours.
    async fn is_recent_duplicate(&self, mac: &str) -> bool {
        let recent = {
            let g = self.last_complete.lock().await;
            matches!(
                &*g,
                Some((m, t)) if m == mac
                    && t.elapsed() < Duration::from_millis(DUP_CLAIM_WINDOW_MS)
            )
        };
        if !recent {
            return false;
        }
        self.bt.is_connected(mac).await
    }

    /// Defence-in-depth against "stuck in switching". A switch flow is
    /// only ever driven from the initiator path, and while it sits in a
    /// non-terminal state (`Preparing`/`Connecting`/`AlmostDone`) the
    /// `start_responder` and `on_claim` busy-guards drop every new
    /// switch request — so a flow that never reaches a terminal state
    /// wedges the daemon until restart. The bounded `connect_profile`
    /// (audio_switch.rs) is the primary fix; this watchdog is the
    /// backstop that guarantees recovery no matter where a leg hangs.
    ///
    /// It watches the state channel and returns the moment a terminal
    /// state (`Idle`/`Failed` — `Failed` runs its own reset) is reached.
    /// If [`FLOW_WATCHDOG_MS`] elapses while still in-flight, it forces
    /// the state back to `Idle` and clears the flow identity. This is
    /// race-safe: a non-Idle state blocks any *new* flow from starting,
    /// so the watchdog can only ever reset the very flow it was armed
    /// for — there is no window where it could stomp a fresh switch.
    fn arm_flow_watchdog(&self) {
        let mut rx = self.state_rx.clone();
        let state_tx = self.state_tx.clone();
        let initiator = self.initiator.clone();
        let current_peer = self.current_peer.clone();
        let current_mac = self.current_mac.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::sleep(Duration::from_millis(FLOW_WATCHDOG_MS));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => {
                        let s = rx.borrow().clone();
                        if matches!(
                            s,
                            SwitchState::Preparing
                                | SwitchState::Connecting
                                | SwitchState::AlmostDone
                        ) {
                            warn!(?s, "flow watchdog: in-flight too long; forcing Idle");
                            let _ = state_tx.send(SwitchState::Idle);
                            *initiator.lock().await = None;
                            *current_peer.lock().await = None;
                            *current_mac.lock().await = None;
                            let _ = persist::clear();
                        }
                        return;
                    }
                    changed = rx.changed() => {
                        if changed.is_err() {
                            return; // sender dropped — orchestrator gone
                        }
                        if matches!(
                            *rx.borrow(),
                            SwitchState::Idle | SwitchState::Failed(_)
                        ) {
                            return; // reached terminal; nothing to guard
                        }
                    }
                }
            }
        });
    }

    fn cas_state(&self, from: SwitchState, to: SwitchState) -> bool {
        let mut changed = false;
        self.state_tx.send_if_modified(|current| {
            if *current == from {
                *current = to.clone();
                changed = true;
                true
            } else {
                false
            }
        });
        if changed {
            // Lock-free probe of the identity slots — `try_lock` rather
            // than .blocking_lock(), because cas_state may be called
            // from inside a tokio task and blocking_lock would panic
            // under the runtime. If the lock is held by `request` for
            // a microsecond, we just skip the save; the next transition
            // will catch up. Persistence is best-effort.
            let peer = self.current_peer.try_lock().ok().and_then(|g| *g);
            let mac = self.current_mac.try_lock().ok().and_then(|g| g.clone());
            if to == SwitchState::Idle {
                let _ = persist::clear();
                if let Ok(mut g) = self.current_peer.try_lock() {
                    *g = None;
                }
                if let Ok(mut g) = self.current_mac.try_lock() {
                    *g = None;
                }
            } else {
                let saved = persist::Saved {
                    discriminator: discriminator_for(&to).to_string(),
                    reason: if let SwitchState::Failed(r) = &to { Some(r.clone()) } else { None },
                    enter_ms: now_ms(),
                    peer_pub_hex: peer.map(|p| persist::pub_to_hex(&p)).unwrap_or_default(),
                    mac: mac.unwrap_or_default(),
                };
                if let Err(e) = persist::save(&saved) {
                    warn!("audio_switch persist save failed: {e}");
                }
            }
        }
        changed
    }
}

/// Allocate the next outbound nonce off the async worker threads.
///
/// `next_audio_out_nonce` is a secret-service D-Bus read-modify-write
/// (synchronous, and can stall for seconds under mutex contention), so
/// running it inline would block a runtime worker — hence `spawn_blocking`
/// (this is the O4 fix, applied to *every* send path, not just `Done`).
///
/// Returns `None` on a store / join error. Callers MUST then NOT send the
/// frame: a `nonce=0` frame is replay-dropped by the peer (it has already
/// accepted higher nonces), so it would be a silent no-op. Skipping the
/// send and logging turns that silent failure into a visible one — the K1
/// "fail-loud" fix.
async fn next_out_nonce(peer_store: &Arc<dyn PeerStore>, peer_pub: [u8; 32]) -> Option<u64> {
    let ps = peer_store.clone();
    match tokio::task::spawn_blocking(move || ps.next_audio_out_nonce(&peer_pub)).await {
        Ok(Ok(n)) => Some(n),
        Ok(Err(e)) => {
            warn!("audio nonce store error: {e}; dropping outbound frame");
            None
        }
        Err(e) => {
            warn!("audio nonce task join error: {e}; dropping outbound frame");
            None
        }
    }
}

fn stage_from(e: &SwitchError) -> Stage {
    match e {
        SwitchError::Bluer(_) | SwitchError::Internal(_) => Stage::Connect,
        SwitchError::Timeout(_) => Stage::WaitReady,
        SwitchError::BadAddress(_) | SwitchError::NotPaired => Stage::Disconnect,
    }
}

fn now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn discriminator_for(s: &SwitchState) -> &'static str {
    match s {
        SwitchState::Idle => persist::disc::IDLE,
        SwitchState::Preparing => persist::disc::PREPARING,
        SwitchState::WaitingApproval => persist::disc::WAITING_APPROVAL,
        SwitchState::WaitingReleased => persist::disc::WAITING_RELEASED,
        SwitchState::Connecting => persist::disc::CONNECTING,
        SwitchState::AlmostDone => persist::disc::CONNECTING,
        SwitchState::Failed(_) => persist::disc::FAILED,
    }
}

// ---- Tuned constants — see the earbuds-switch design notes §6 ----

/// How many local connect attempts to make before giving up.
///
/// `connect_audio` is now single-shot (it used to retry internally
/// too — 3×3×4s settle was the 36 s worst-case latency disaster
/// ChatGPT flagged). With single-shot semantics the retry pressure
/// here drops: attempt 1 usually wins, attempt 2 covers the
/// "phone hadn't released yet" race. A third pass rarely buys
/// anything and just stretches the user-visible silence.
const CONNECT_RETRY_COUNT: u32 = 2;
/// Pause between connect retries. Matches the ecosystem value — long
/// enough for BlueZ to register the peer's disconnect, short enough
/// to keep the total switch under 1 s in the happy path.
const CONNECT_RETRY_PAUSE_MS: u64 = 280;
/// Lost-Done watchdog for the responder slot. The happy path clears the
/// slot the moment the peer's `Done` frame arrives (`on_done`); this
/// timer only matters when that frame is LOST — without it the slot
/// would stay occupied forever and every later incoming Request would
/// be rejected Busy. It does NOT delay the switch itself (the sleep
/// runs after Released is already sent and the peer is connecting).
const DONE_WAIT_MS: u64 = 4_000;
/// Background confirm window for the release path — matches
/// `audio_switch::DISCONNECT_TIMEOUT`. Does not gate the phone.
const DISCONNECT_TIMEOUT_MS: u64 = 1_000;
const FAILED_RESET_MS: u64 = 3_000;
/// Hard ceiling on how long a single switch flow may sit in a
/// non-terminal in-flight state before the watchdog force-resets it to
/// Idle. Sized above any legitimate switch: the return-connect floor is
/// ~4.5 s and the bounded retry loop tops out near ~10 s, so 14 s only
/// trips on a genuine wedge. See [`SwitchOrchestrator::arm_flow_watchdog`].
const FLOW_WATCHDOG_MS: u64 = 14_000;
/// How long after a completed switch a same-mac reclaim is treated as a
/// duplicate (BLE + LAN double-delivery) rather than a fresh request.
/// The two transports land ~1-2.5 s apart in practice; 4 s covers that
/// gap with margin while staying short enough that any genuinely new
/// reclaim a few seconds later still runs normally. See
/// [`SwitchOrchestrator::is_recent_duplicate`].
const DUP_CLAIM_WINDOW_MS: u64 = 4_000;

#[cfg(test)]
mod tests {
    //! Unit tests for the duplicate-reclaim guard — the BLE+LAN
    //! double-delivery defence that the [`BtControl`] seam was
    //! introduced to make testable without a live `bluer::Adapter`.
    //!
    //! These cover the two halves the guard ANDs together: the dedup
    //! *time window* (the same frame arriving twice on two transports a
    //! beat apart) and the *live connection* check (the buds are still
    //! physically ours, so the reclaim really is a no-op rather than a
    //! genuine new request after the peer grabbed them back).

    use super::*;
    use crate::core::audio_op::AudioOp;
    use crate::core::storage::peers::TrustedPeer;
    use crate::core::storage::StorageError;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::Mutex as StdMutex;

    const PEER: [u8; 32] = [7u8; 32];
    const MAC: &str = "AC:47:1B:25:71:C2";

    /// One `connect` attempt's result.
    #[derive(Clone)]
    enum Outcome {
        Ok,
        Fail(String),
        /// Never resolves — models a wedged `connect_profile`, so the
        /// flow watchdog (the only live timer) has to drive recovery.
        Hang,
    }

    /// Programmable BT seam: scripts per-attempt `connect` outcomes,
    /// counts calls, and answers `is_connected` with a fixed verdict
    /// (recording the macs it was asked about).
    struct FakeBt {
        connected: bool,
        is_connected_calls: StdMutex<Vec<String>>,
        connect_script: StdMutex<VecDeque<Outcome>>,
        connect_fallback: Outcome,
        connect_calls: AtomicU32,
        disconnect_ok: bool,
    }

    impl FakeBt {
        /// `connected` = the verdict `is_connected` returns. `connect`
        /// succeeds by default (the dedup-guard tests never reach it).
        fn new(connected: bool) -> Self {
            Self {
                connected,
                is_connected_calls: StdMutex::new(Vec::new()),
                connect_script: StdMutex::new(VecDeque::new()),
                connect_fallback: Outcome::Ok,
                connect_calls: AtomicU32::new(0),
                disconnect_ok: true,
            }
        }
        /// Builder: fallback `connect` outcome once any script is drained.
        fn with_connect(mut self, o: Outcome) -> Self {
            self.connect_fallback = o;
            self
        }
        /// Builder: queue per-attempt `connect` outcomes, consumed in
        /// order before falling back to [`Self::with_connect`].
        fn with_connect_script(self, outs: impl IntoIterator<Item = Outcome>) -> Self {
            *self.connect_script.lock().unwrap() = outs.into_iter().collect();
            self
        }
        fn connects(&self) -> u32 {
            self.connect_calls.load(Ordering::SeqCst)
        }
    }

    impl BtControl for FakeBt {
        fn connect<'a>(&'a self, _mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .connect_script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.connect_fallback.clone());
            Box::pin(async move {
                match outcome {
                    Outcome::Ok => Ok(()),
                    Outcome::Fail(m) => Err(SwitchError::Internal(m)),
                    Outcome::Hang => std::future::pending().await,
                }
            })
        }
        fn disconnect_initiate<'a>(&'a self, _mac: &'a str) -> BoxFuture<'a, Result<(), SwitchError>> {
            let ok = self.disconnect_ok;
            Box::pin(async move {
                if ok {
                    Ok(())
                } else {
                    Err(SwitchError::Internal("disconnect failed".into()))
                }
            })
        }
        fn confirm_disconnected<'a>(&'a self, _mac: &'a str, _t: Duration) -> BoxFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn is_connected<'a>(&'a self, mac: &'a str) -> BoxFuture<'a, bool> {
            self.is_connected_calls.lock().unwrap().push(mac.to_string());
            let connected = self.connected;
            Box::pin(async move { connected })
        }
    }

    /// In-memory peer store — only the audio-nonce methods are exercised
    /// by the orchestrator; the identity CRUD methods are never called.
    /// Tracks the inbound high-water mark so the replay gate behaves like
    /// the real store, and can force `next_audio_out_nonce` to error to
    /// exercise the "fail-loud" path (K1/O4).
    struct FakePeerStore {
        out_nonce: AtomicU64,
        seen_in: StdMutex<HashMap<[u8; 32], u64>>,
        out_fails: AtomicBool,
    }

    impl FakePeerStore {
        fn new() -> Self {
            Self {
                out_nonce: AtomicU64::new(0),
                seen_in: StdMutex::new(HashMap::new()),
                out_fails: AtomicBool::new(false),
            }
        }
    }

    impl PeerStore for FakePeerStore {
        fn save(&self, _peer: &TrustedPeer) -> Result<(), StorageError> {
            Ok(())
        }
        fn load(&self, _p: &[u8; 32]) -> Result<TrustedPeer, StorageError> {
            Err(StorageError::NotFound)
        }
        fn list(&self) -> Result<Vec<TrustedPeer>, StorageError> {
            Ok(Vec::new())
        }
        fn forget(&self, _p: &[u8; 32]) -> Result<(), StorageError> {
            Ok(())
        }
        fn next_audio_out_nonce(&self, _p: &[u8; 32]) -> Result<u64, StorageError> {
            if self.out_fails.load(Ordering::SeqCst) {
                return Err(StorageError::Backend("forced nonce failure".into()));
            }
            Ok(self.out_nonce.fetch_add(1, Ordering::SeqCst) + 1)
        }
        fn load_audio_in_nonce(&self, p: &[u8; 32]) -> Result<u64, StorageError> {
            Ok(*self.seen_in.lock().unwrap().get(p).unwrap_or(&0))
        }
        fn commit_audio_in_nonce(&self, p: &[u8; 32], nonce: u64) -> Result<(), StorageError> {
            let mut m = self.seen_in.lock().unwrap();
            let e = m.entry(*p).or_insert(0);
            if nonce > *e {
                *e = nonce;
            }
            Ok(())
        }
    }

    type Captured = Arc<StdMutex<Vec<AudioOpFrame>>>;

    /// Build an orchestrator wired to fakes with an always-`Allow`
    /// acceptance and a fresh peer store. Returns the orchestrator plus
    /// the captured-frames buffer so a test can assert what was sent.
    fn make_orch(bt: Arc<FakeBt>) -> (Arc<SwitchOrchestrator>, Captured) {
        let (orch, cap, _store) = make_orch_full(bt, Arc::new(FakePeerStore::new()), allow());
        (orch, cap)
    }

    /// Full builder — lets a test supply its own peer store (to flip the
    /// nonce-failure toggle) and acceptance policy (Allow / Reject).
    fn make_orch_full(
        bt: Arc<FakeBt>,
        store: Arc<FakePeerStore>,
        acceptance: AcceptanceProvider,
    ) -> (Arc<SwitchOrchestrator>, Captured, Arc<FakePeerStore>) {
        let captured: Captured = Arc::new(StdMutex::new(Vec::new()));
        let cap = captured.clone();
        let sender: Sender = Arc::new(move |_peer: [u8; 32], frame: AudioOpFrame| {
            let cap = cap.clone();
            Box::pin(async move {
                cap.lock().unwrap().push(frame);
                Ok(())
            })
        });
        let orch = Arc::new(SwitchOrchestrator::new(
            bt,
            store.clone(),
            sender,
            acceptance,
        ));
        (orch, captured, store)
    }

    fn allow() -> AcceptanceProvider {
        Arc::new(|| Acceptance::Allow)
    }
    fn reject(reason: RejectReason) -> AcceptanceProvider {
        Arc::new(move || Acceptance::Reject(reason))
    }

    fn frame(nonce: u64, op: AudioOp) -> AudioOpFrame {
        AudioOpFrame { nonce, op, mac: MAC.to_string(), ts: 0 }
    }

    /// Serialize the tests that drive real state transitions (those hit
    /// the persistence file) and redirect the state dir to a temp path so
    /// they neither clobber the user's daemon state nor race on the
    /// process-wide `XDG_STATE_HOME`. Held for the test's duration.
    fn isolate_persist() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: StdMutex<()> = StdMutex::new(());
        let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("vortex_orch_test_state");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("XDG_STATE_HOME", &dir);
        let _ = persist::clear();
        g
    }

    /// Await a state matching `pred`, with a virtual-time ceiling so a
    /// wedged machine fails the test instead of hanging forever.
    async fn wait_state(
        rx: &mut watch::Receiver<SwitchState>,
        pred: impl FnMut(&SwitchState) -> bool,
    ) -> SwitchState {
        tokio::time::timeout(Duration::from_secs(60), rx.wait_for(pred))
            .await
            .expect("timed out waiting for state")
            .expect("state channel closed")
            .clone()
    }

    /// Await the first sent frame matching `pred` (frames are emitted from
    /// background tasks, so poll the captured log).
    async fn wait_frame(cap: &Captured, pred: impl Fn(&AudioOpFrame) -> bool) -> AudioOpFrame {
        let fut = async {
            loop {
                let found = cap.lock().unwrap().iter().find(|f| pred(f)).cloned();
                if let Some(f) = found {
                    return f;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(60), fut)
            .await
            .expect("timed out waiting for frame")
    }

    fn count_frames(cap: &Captured, pred: impl Fn(&AudioOpFrame) -> bool) -> usize {
        cap.lock().unwrap().iter().filter(|f| pred(f)).count()
    }

    #[tokio::test]
    async fn recent_reclaim_with_buds_still_ours_is_a_duplicate() {
        let bt = Arc::new(FakeBt::new(true)); // buds still connected to us
        let (orch, _cap) = make_orch(bt.clone());
        // Stamp a just-completed switch for this mac.
        *orch.last_complete.lock().await = Some((MAC.to_string(), tokio::time::Instant::now()));
        assert!(orch.is_recent_duplicate(MAC).await);
        // The seam's live check was actually consulted.
        assert_eq!(bt.is_connected_calls.lock().unwrap().as_slice(), &[MAC.to_string()]);
    }

    #[tokio::test]
    async fn recent_reclaim_after_peer_grabbed_back_is_not_a_duplicate() {
        // Within the time window, but the buds are no longer ours — a
        // genuine new request (e.g. a real incoming call on the peer),
        // not the BLE+LAN echo. Must NOT be suppressed.
        let bt = Arc::new(FakeBt::new(false));
        let (orch, _cap) = make_orch(bt.clone());
        *orch.last_complete.lock().await = Some((MAC.to_string(), tokio::time::Instant::now()));
        assert!(!orch.is_recent_duplicate(MAC).await);
        assert_eq!(bt.is_connected_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_prior_completion_is_never_a_duplicate() {
        let bt = Arc::new(FakeBt::new(true));
        let (orch, _cap) = make_orch(bt.clone());
        // last_complete stays None.
        assert!(!orch.is_recent_duplicate(MAC).await);
        // Time check fails first — the live BT check is never reached.
        assert!(bt.is_connected_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn different_mac_is_not_a_duplicate() {
        let bt = Arc::new(FakeBt::new(true));
        let (orch, _cap) = make_orch(bt.clone());
        *orch.last_complete.lock().await = Some(("00:11:22:33:44:55".to_string(), tokio::time::Instant::now()));
        assert!(!orch.is_recent_duplicate(MAC).await);
        assert!(bt.is_connected_calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reclaim_after_window_expires_is_not_a_duplicate() {
        let bt = Arc::new(FakeBt::new(true));
        let (orch, _cap) = make_orch(bt.clone());
        *orch.last_complete.lock().await = Some((MAC.to_string(), tokio::time::Instant::now()));
        // Advance the paused clock past the dedup window.
        tokio::time::advance(Duration::from_millis(DUP_CLAIM_WINDOW_MS + 1)).await;
        assert!(!orch.is_recent_duplicate(MAC).await);
        // Time check fails first — no live BT probe.
        assert!(bt.is_connected_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_request_acks_with_done_and_stays_idle() {
        // The full public-surface behaviour: a duplicate reclaim must
        // NOT start a flow (state stays Idle) and must send the peer a
        // `Done` so its UI clears.
        let bt = Arc::new(FakeBt::new(true));
        let (orch, captured) = make_orch(bt.clone());
        *orch.last_complete.lock().await = Some((MAC.to_string(), tokio::time::Instant::now()));

        let r = orch.request(PEER, MAC.to_string()).await;
        assert!(r.is_ok());
        // Never left Idle — no initiator flow was started.
        assert_eq!(*orch.state_rx.borrow(), SwitchState::Idle);
        // Exactly one frame, and it's a Done for this mac.
        let frames = captured.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0].op, AudioOp::Done));
        assert_eq!(frames[0].mac, MAC);
    }

    // ---- Initiator state machine ----

    #[tokio::test(start_paused = true)]
    async fn initiator_happy_path_completes_and_sends_request_then_done() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false));
        let (orch, cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Idle).await;

        assert_eq!(bt.connects(), 1, "happy path connects exactly once");
        wait_frame(&cap, |f| matches!(f.op, AudioOp::Request)).await;
        let done = wait_frame(&cap, |f| matches!(f.op, AudioOp::Done)).await;
        assert!(done.nonce >= 1, "Done must carry a non-zero (replay-safe) nonce");
    }

    #[tokio::test(start_paused = true)]
    async fn initiator_retries_once_then_succeeds() {
        let _env = isolate_persist();
        let bt = Arc::new(
            FakeBt::new(false).with_connect_script([Outcome::Fail("phone still holds".into()), Outcome::Ok]),
        );
        let (orch, cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Idle).await;

        assert_eq!(bt.connects(), 2, "first attempt fails, second wins");
        wait_frame(&cap, |f| matches!(f.op, AudioOp::Done)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn initiator_exhausts_retries_then_fails_then_resets_to_idle() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Fail("never connects".into())));
        let (orch, cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| matches!(s, SwitchState::Failed(_))).await;
        assert_eq!(bt.connects(), CONNECT_RETRY_COUNT, "all retries consumed");
        wait_frame(&cap, |f| matches!(f.op, AudioOp::Failed { .. })).await;

        // fail_initiator self-heals back to Idle after FAILED_RESET_MS.
        wait_state(&mut rx, |s| *s == SwitchState::Idle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn initiator_peer_reject_transitions_to_failed() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Hang)); // hold in Connecting
        let (orch, _cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Connecting).await;
        // A reason other than InCall is the peer genuinely saying no.
        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Reject { reason: RejectReason::Busy }))
            .await
            .unwrap();

        let st = wait_state(&mut rx, |s| matches!(s, SwitchState::Failed(_))).await;
        if let SwitchState::Failed(r) = st {
            assert!(r.contains("rejected"), "reason should mention the peer reject: {r}");
        }
    }

    /// `InCall` must NOT be terminal.
    ///
    /// The old behaviour — any Reject ends the flow — is what made every
    /// post-call hand-back fail: the phone holds an ended call in its state for
    /// ~6 s so the laptop can clear the call pill, and its acceptance gate reads
    /// the same field, so it refused the hand-off it had itself just asked for.
    /// This test exists to keep the retry, and to keep `Failed` from appearing
    /// during the window where a retry is still pending.
    #[tokio::test(start_paused = true)]
    async fn initiator_in_call_reject_retries_instead_of_failing() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Hang)); // hold in Connecting
        let (orch, _cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Connecting).await;
        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Reject { reason: RejectReason::InCall }))
            .await
            .unwrap();

        // Give the first backoff step (1s) room to run under paused time.
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let st = rx.borrow().clone();
        assert!(
            !matches!(st, SwitchState::Failed(_)),
            "InCall must not end the flow while a retry is still pending, got {st:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn released_moves_connecting_to_almost_done() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Hang)); // BT not done yet
        let (orch, _cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Connecting).await;
        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Released))
            .await
            .unwrap();

        wait_state(&mut rx, |s| *s == SwitchState::AlmostDone).await;
    }

    #[tokio::test(start_paused = true)]
    async fn initiator_fails_loud_when_nonce_store_errors() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false));
        let store = Arc::new(FakePeerStore::new());
        store.out_fails.store(true, Ordering::SeqCst);
        let (orch, _cap, _store) = make_orch_full(bt.clone(), store, allow());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        let st = wait_state(&mut rx, |s| matches!(s, SwitchState::Failed(_))).await;
        if let SwitchState::Failed(r) = st {
            assert!(r.contains("nonce"), "fail-loud reason should mention the nonce store: {r}");
        }
        assert_eq!(bt.connects(), 0, "must not attempt a BT connect without a valid nonce");
    }

    #[tokio::test(start_paused = true)]
    async fn flow_watchdog_forces_idle_on_wedge() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Hang)); // connect never returns
        let (orch, _cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Connecting).await;
        // Nothing reaches a terminal state on its own — only the
        // FLOW_WATCHDOG_MS backstop can rescue a wedged flow.
        let st = wait_state(&mut rx, |s| *s == SwitchState::Idle).await;
        assert_eq!(st, SwitchState::Idle);
    }

    // ---- Responder state machine ----

    #[tokio::test(start_paused = true)]
    async fn responder_allow_sends_approve_and_released() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false));
        let (orch, cap) = make_orch(bt.clone());

        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Request))
            .await
            .unwrap();

        let approve = wait_frame(&cap, |f| matches!(f.op, AudioOp::Approve)).await;
        let released = wait_frame(&cap, |f| matches!(f.op, AudioOp::Released)).await;
        assert!(approve.nonce >= 1 && released.nonce >= 1);
        assert_ne!(approve.nonce, released.nonce, "each outbound frame gets a fresh nonce");
    }

    #[tokio::test(start_paused = true)]
    async fn responder_reject_when_acceptance_denies() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false));
        let (orch, cap, _store) =
            make_orch_full(bt.clone(), Arc::new(FakePeerStore::new()), reject(RejectReason::InCall));

        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Request))
            .await
            .unwrap();

        let r = wait_frame(&cap, |f| matches!(f.op, AudioOp::Reject { .. })).await;
        assert!(matches!(r.op, AudioOp::Reject { reason: RejectReason::InCall }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            count_frames(&cap, |f| matches!(f.op, AudioOp::Released)),
            0,
            "a rejected request must never release the buds"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn responder_rejects_busy_when_a_flow_is_active() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false).with_connect(Outcome::Hang)); // initiator stuck Connecting
        let (orch, cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.request(PEER, MAC.to_string()).await.unwrap();
        wait_state(&mut rx, |s| *s == SwitchState::Connecting).await;
        orch.clone()
            .on_incoming(PEER, frame(1, AudioOp::Request))
            .await
            .unwrap();

        let r = wait_frame(&cap, |f| matches!(f.op, AudioOp::Reject { reason: RejectReason::Busy })).await;
        assert!(matches!(r.op, AudioOp::Reject { reason: RejectReason::Busy }));
    }

    #[tokio::test(start_paused = true)]
    async fn replayed_nonce_is_dropped() {
        let _env = isolate_persist();
        let bt = Arc::new(FakeBt::new(false));
        let (orch, cap) = make_orch(bt.clone());

        orch.clone()
            .on_incoming(PEER, frame(5, AudioOp::Request))
            .await
            .unwrap();
        wait_frame(&cap, |f| matches!(f.op, AudioOp::Approve)).await;

        // Same nonce again — the load-compare-commit gate must drop it.
        orch.clone()
            .on_incoming(PEER, frame(5, AudioOp::Request))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            count_frames(&cap, |f| matches!(f.op, AudioOp::Approve)),
            1,
            "a replayed frame must not trigger a second responder run"
        );
    }

    // ---- Crash recovery ----

    #[tokio::test(start_paused = true)]
    async fn recover_resumes_fresh_connecting_and_completes() {
        let _env = isolate_persist();
        let saved = persist::Saved {
            discriminator: persist::disc::CONNECTING.to_string(),
            reason: None,
            enter_ms: now_ms(),
            peer_pub_hex: persist::pub_to_hex(&PEER),
            mac: MAC.to_string(),
        };
        persist::save(&saved).unwrap();

        let bt = Arc::new(FakeBt::new(false));
        let (orch, cap) = make_orch(bt.clone());
        let mut rx = orch.state();

        orch.recover_on_start().await;
        wait_state(&mut rx, |s| *s == SwitchState::Idle).await;
        assert_eq!(bt.connects(), 1, "resume drives exactly one connect attempt");
        wait_frame(&cap, |f| matches!(f.op, AudioOp::Done)).await;
    }
}
