package com.vortex.a3.core.earbuds

import android.util.Log
import com.vortex.a3.core.storage.PeerStore
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.atomic.AtomicReference

/**
 * Manual earbuds-switch state machine for the Phase 1 flow per
 * the earbuds-switch design notes §5.
 *
 * Two roles, one class:
 *  - **Initiator** (we want the buds) — driven by [request].
 *  - **Responder/owner** (we hold the buds; peer asked) — driven by
 *    incoming `AudioOp.Request` frames in [onIncoming].
 *
 * Single-peer V1: we run at most one flow at a time. A second
 * `request` while a flow is in progress is dropped and the UI shows
 * the existing state.
 *
 * **R3 — CAS transitions.** All state changes go through
 * [AtomicReference.compareAndSet]. We never hold a lock across the
 * suspending Bluetooth call — that's the classic source of "BlueZ
 * hangs and now our whole UI is wedged".
 *
 * **A1 — owner-vote pattern.** The side currently holding the buds is
 * the decision-maker. An initiator's `Request` always reaches the
 * owner, who decides to `Approve` (and release) or `Reject`. This
 * eliminates the "both sides claim simultaneously" race — there is
 * exactly one source of truth at any moment.
 */
class SwitchOrchestrator(
    private val controller: AudioDeviceHandle,
    private val peerStore: PeerStore,
    /** Send the frame to the given peer over whatever transport is up
     *  (LAN preferred, BLE fallback). Returns Result.failure on
     *  transport unavailable; the orchestrator surfaces this as a
     *  Failed state. */
    private val sender: suspend (peerStaticPub: ByteArray, frame: AudioOpFrame) -> Result<Unit>,
    /** Read by the orchestrator to decide whether to accept an
     *  incoming `Request` while in special-purpose flows (e.g. a
     *  call is active — peer must back off). Phase 2 wires this; for
     *  Phase 1 the default returns [Acceptance.Allow]. */
    private val acceptanceProvider: () -> Acceptance = { Acceptance.Allow },
    /** R1 — Crash-recovery writer. Null means tests / fakes that
     *  don't care about persistence. Production passes a real
     *  [SwitchPersistence] from VortexService. */
    private val persistence: SwitchPersistence? = null,
) {

    /** Resolved owner policy for an incoming `Request`. Set externally
     *  by `CallFlowOrchestrator` in Phase 2. Phase 1 always returns
     *  [Allow]. */
    sealed class Acceptance {
        object Allow : Acceptance()
        data class Reject(val reason: RejectReason) : Acceptance()
    }

    private val stateRef = AtomicReference<SwitchState>(SwitchState.Idle)
    private val flowState = MutableStateFlow<SwitchState>(SwitchState.Idle)
    val state: StateFlow<SwitchState> = flowState.asStateFlow()

    /** The buds we last handed to the peer with [claim], and when. */
    @Volatile private var lastClaimMac: String? = null
    @Volatile private var lastClaimAtMs: Long = 0L

    /** Did we ourselves hand [mac] over in the last few seconds?
     *
     *  Window sized off the thing that made this necessary: the call gate holds
     *  for ~6 s after a call ends, and the laptop's Request lands within a
     *  second or two of our Claim. Ten seconds covers that with room for a slow
     *  link, and is short enough that it cannot excuse an unrelated Request
     *  arriving during a genuinely live call later on. */
    private fun recentlyClaimed(mac: String): Boolean {
        val m = lastClaimMac ?: return false
        if (!m.equals(mac, ignoreCase = true)) return false
        return android.os.SystemClock.elapsedRealtime() - lastClaimAtMs < CLAIM_HONOUR_MS
    }

    /** Set when an initiator flow is in-flight. The matching peer +
     *  mac + nonce live here so an arriving `Approve` / `Released`
     *  knows which flow it belongs to. */
    @Volatile private var activeInitiator: ActiveFlow? = null

    /** Same for the responder side. */
    @Volatile private var activeResponder: ActiveFlow? = null

    /** Peer+mac last associated with a non-Idle state — kept so the
     *  persistence row records *which* flow we were running, even when
     *  the state value itself (Connecting/WaitingReleased/…) doesn't
     *  carry the peer. Reset on Idle. */
    @Volatile private var currentPeer: ByteArray? = null
    @Volatile private var currentMac: String? = null

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    // ---- Public API ----

    /** Fast call-end hand-off. Symmetric to the BLE `Request` we
     *  receive at call-start, but in the reverse direction: phone is
     *  holding the buds, call has ended, we want the laptop to take
     *  over INSTANTLY (not on the next 12 s LAN heartbeat tick). We
     *  disconnect locally in parallel with sending the Claim frame so
     *  the laptop's initiator-side `attempt_connect` succeeds on the
     *  first try instead of failing once on a 800 ms settle while the
     *  phone is still releasing.
     *
     *  No state-machine entry — phone is staying in Idle on its own
     *  end. The laptop will fire its own `Request` if it needs to
     *  confirm; we just announce + release. */
    fun claim(peerStaticPub: ByteArray, mac: String) {
        // Remember that WE handed these buds over, so the peer's answering
        // Request cannot be refused by a gate meant for unsolicited ones.
        lastClaimMac = mac
        lastClaimAtMs = android.os.SystemClock.elapsedRealtime()
        scope.launch {
            // Fire the BLE claim notification first — best to have it
            // on the wire before we drop our own connection, otherwise
            // a slow disconnect could delay it.
            try {
                val nonce = peerStore.nextAudioOutNonce(peerStaticPub)
                sender(
                    peerStaticPub,
                    AudioOpFrame(nonce, AudioOp.Claim, mac, nowSec()),
                )
            } catch (e: Throwable) {
                Log.w(TAG, "claim: sender failed: ${e.message}")
            }
        }
        scope.launch {
            // Detached release: the laptop's connect-retry loop is
            // what actually matters here; we just need the BT slot
            // free before it lands a connection attempt.
            val disc = withContext(Dispatchers.IO) { controller.disconnect(mac) }
            if (disc.isFailure) {
                Log.w(
                    TAG,
                    "claim: local disconnect failed: ${disc.exceptionOrNull()?.message}",
                )
            }
        }
    }

    /** Initiator entry — user tapped "Bring to me". */
    fun request(peerStaticPub: ByteArray, mac: String): Boolean {
        // Set the peer/mac BEFORE the CAS so the persistence row that
        // transition() writes already carries the flow identity. If
        // the CAS fails (we lost the race), we clear them again.
        currentPeer = peerStaticPub
        currentMac = mac
        if (!transition(SwitchState.Idle, SwitchState.Preparing)) {
            Log.w(TAG, "request ignored: already in $stateRef.get()")
            currentPeer = null
            currentMac = null
            return false
        }
        armFlowWatchdog()
        scope.launch { runInitiator(peerStaticPub, mac) }
        return true
    }

    /** Backstop against a wedged flow. Mirrors the Linux orchestrator's
     *  watchdog: if the state machine sits in a non-terminal state past
     *  [FLOW_WATCHDOG_MS] (e.g. a lost CAS at runInitiator that left
     *  Preparing set, or a stuck BT connect), force it back to Idle so
     *  later switches aren't dropped forever as "already in …". Race-safe
     *  — a non-Idle state blocks any new flow, so this can only reset the
     *  flow it was armed for, and attemptConnect bails the moment it sees
     *  Idle. */
    private fun armFlowWatchdog() {
        scope.launch {
            delay(FLOW_WATCHDOG_MS)
            val s = stateRef.get()
            if (s == SwitchState.Preparing ||
                s == SwitchState.Connecting ||
                s == SwitchState.AlmostDone
            ) {
                Log.w(TAG, "flow watchdog: in-flight too long ($s); forcing Idle")
                stateRef.set(SwitchState.Idle)
                flowState.value = SwitchState.Idle
                activeInitiator = null
                currentPeer = null
                currentMac = null
            }
        }
    }

    /** Frame ingress. Dispatch by op kind.
     *  - `Request` → responder flow (we are owner being asked)
     *  - `Approve` / `Released` / `Reject` / `Failed` → initiator flow update
     *  - `Done` → responder flow completion */
    suspend fun onIncoming(peerStaticPub: ByteArray, frame: AudioOpFrame) {
        // Replay rejection — atomic load-compare-commit. The prior
        // `loadAudioInNonce → if greater → commitAudioInNonce`
        // sequence let BLE and LAN drive the same frame into this
        // function concurrently and both pass the freshness check
        // (ChatGPT review #2). `tryAcceptAudioInNonce` does the
        // read+check+write inside one `synchronized` window so
        // exactly one delivery wins.
        if (!peerStore.tryAcceptAudioInNonce(peerStaticPub, frame.nonce)) {
            Log.w(TAG, "audio_op nonce ${frame.nonce} replayed; dropping")
            return
        }

        when (frame.op) {
            AudioOp.Request -> startResponderFlow(peerStaticPub, frame.mac)
            AudioOp.Claim -> onClaim(peerStaticPub, frame.mac)
            AudioOp.Approve -> initiatorOnApprove(peerStaticPub, frame.mac)
            AudioOp.Released -> initiatorOnReleased(peerStaticPub, frame.mac)
            is AudioOp.Reject -> initiatorOnReject(peerStaticPub, frame.op.reason)
            AudioOp.Done -> responderOnDone(peerStaticPub)
            is AudioOp.Failed -> peerReportedFailure(peerStaticPub, frame.op)
        }
    }

    /** Peer holds the buds and is asking us to claim them. We become
     *  the initiator. Idempotent: if a flow is already in progress
     *  the request() guard drops it. */
    private fun onClaim(peerPub: ByteArray, mac: String) {
        if (stateRef.get() != SwitchState.Idle) {
            Log.i(TAG, "onClaim: already in flow; dropping")
            return
        }
        Log.i(TAG, "onClaim: peer asked us to claim mac=$mac")
        request(peerPub, mac)
    }

    fun close() {
        scope.coroutineContext[Job]?.cancel()
    }

    // ---- Initiator flow ----

    private suspend fun runInitiator(peerPub: ByteArray, mac: String) {
        val nonce = peerStore.nextAudioOutNonce(peerPub)
        activeInitiator = ActiveFlow(peerPub, mac, nonce)
        Log.i(TAG, "initiator: request peer=${peerPub.toHexPrefix()} mac=$mac nonce=$nonce")

        // S3 — defensive local disconnect (only if we somehow already
        // hold the buds) + warm the profile proxy. Runs concurrent
        // with the Request send and the retry loop below.
        scope.launch {
            if (controller.isConnected(mac)) {
                Log.i(TAG, "initiator: local already has buds; releasing first")
                controller.disconnect(mac)
            }
            controller.prewarm()
        }

        // Fire-and-forget Request. We do NOT await an Approve / Released
        // round-trip before starting the local BT connect — that was
        // the sequential cost that made the previous V1 four times
        // slower than the ecosystem prototype. Approve / Released
        // become informational; the real "switch" is whichever side
        // wins the BT-level race (the retry loop below is tuned to
        // pick up the buds the moment the phone releases).
        scope.launch {
            val sendResult = sender(peerPub, AudioOpFrame(nonce, AudioOp.Request, mac, nowSec()))
            if (sendResult.isFailure) {
                Log.w(TAG, "send Request: ${sendResult.exceptionOrNull()?.message}")
                // Don't fail the flow — connect retries may still
                // win at the BT layer.
            }
        }

        if (!transition(SwitchState.Preparing, SwitchState.Connecting)) {
            activeInitiator = null
            return
        }
        attemptConnect(peerPub, mac)
    }

    /** Approve is informational under the fast-switch protocol. */
    private fun initiatorOnApprove(peerPub: ByteArray, mac: String) {
        val flow = activeInitiator ?: return
        if (!flow.matches(peerPub, mac)) return
        Log.i(TAG, "informational: peer Approve received")
    }

    /** Released is the optimistic-UI signal. Peer confirmed it
     *  dropped the buds; the local BT connect still has ~2-3 s to
     *  go. Flip to AlmostDone so the card un-dims and shows the new
     *  ownership immediately — attemptConnect continues running and
     *  transitions to Idle on success / Failed on exhaustion. */
    private fun initiatorOnReleased(peerPub: ByteArray, mac: String) {
        val flow = activeInitiator ?: return
        if (!flow.matches(peerPub, mac)) return
        Log.i(TAG, "peer Released received → buds free, connecting now + optimistic UI → AlmostDone")
        // Unblock attemptConnect immediately — the buds are confirmed
        // free, so the first connect can fire without waiting out the
        // fixed head-start ceiling. This is the P2.15 event-driven grab.
        flow.releasedSignal.complete(Unit)
        transition(SwitchState.Connecting, SwitchState.AlmostDone)
    }

    private fun initiatorOnReject(peerPub: ByteArray, reason: RejectReason) {
        val flow = activeInitiator ?: return
        if (!peerPub.contentEquals(flow.peerPub)) return
        failInitiator(peerPub, "peer rejected: $reason")
    }

    /**
     * Retry-driven local BT connect. The first attempt fires after a
     * short [PRE_CONNECT_HEAD_START_MS] grace so Linux has a chance to
     * actually drop the A2DP profile — empirically Linux's BLE
     * audio_signal listener → orchestrator → disconnect_profile path
     * takes ~500-700 ms from the moment the phone sends the Request
     * frame. Trying earlier wastes the first 1.5 s `CONNECT_SETTLE_MS`
     * window on a connect that physically cannot succeed because the
     * buds are still bonded to the peer. Old-project pattern (see
     * ecosystem commit 2c92ec5: "fixed with chatgpt") — phone waits
     * for the laptop's HANDOFF_READY ack before connecting. We don't
     * have the Linux→Phone BLE WRITE plumbed yet (P2.15), so this is
     * the time-based approximation.
     */
    private suspend fun attemptConnect(peerPub: ByteArray, mac: String) {
        // The buds connect over Bluetooth — if the phone's adapter is
        // OFF we can't grab them no matter how fast the signalling is.
        // onCallStart already gates the whole hand-off on isBluetoothOn,
        // but the user can toggle BT off AFTER the call started, so
        // re-check here before burning connect retries. Fail cleanly.
        if (!controller.isBluetoothEnabled()) {
            Log.w(TAG, "attemptConnect: phone Bluetooth is OFF — cannot grab buds; aborting")
            failInitiator(peerPub, "phone Bluetooth off")
            return
        }

        // Event-driven head-start (P2.15). The buds can only connect to
        // us AFTER the peer has dropped its link. Linux sends a
        // `Released` frame the instant its `device.disconnect()` + ACL
        // teardown completes — so we wait for THAT, not a blind fixed
        // delay. The instant Released lands, the buds are free and the
        // first connect lands a connection in ~one BlueZ round trip
        // (this is what got the old ecosystem build to ~1.1 s). The
        // ceiling is a fallback: if Released is lost (BLE+LAN both
        // dropped mid-switch) we still attempt rather than stall.
        val flow = activeInitiator
        if (flow != null && flow.matches(peerPub, mac)) {
            val released = withTimeoutOrNull(RELEASED_WAIT_CEILING_MS) {
                flow.releasedSignal.await()
            } != null
            Log.i(
                TAG,
                if (released) "attemptConnect: Released observed → connecting now"
                else "attemptConnect: Released ceiling (${RELEASED_WAIT_CEILING_MS}ms) hit → connecting anyway",
            )
        } else {
            // No active flow handle (shouldn't happen) — fall back to
            // the legacy fixed grace.
            delay(PRE_CONNECT_HEAD_START_MS)
        }
        var lastErr = "connect not attempted"
        for (attempt in 1..CONNECT_RETRY_COUNT) {
            // We handed these buds to the peer while this loop was running.
            //
            // `claim()` is deliberately outside the state machine — the phone
            // stays Idle on its own end — so nothing here noticed, and an
            // attempt already in flight went on to CONNECT the buds back to
            // this phone AFTER the hand-over. The user declines a call two
            // seconds in, the laptop is told to take the earbuds, and the
            // phone's own retry quietly takes them again.
            if (recentlyClaimed(mac)) {
                Log.i(TAG, "attemptConnect: we just claimed $mac for the peer — abandoning")
                failInitiator(peerPub, "handed to peer")
                return
            }
            // Bail on terminal states. AlmostDone is NOT terminal — BT
            // is still meant to run; the state just signals "UI thinks
            // we're done already".
            val s = stateRef.get()
            if (s is SwitchState.Failed || s == SwitchState.Idle) {
                Log.i(TAG, "attemptConnect: state already terminal; stopping retries")
                return
            }
            val result = withContext(Dispatchers.IO) { controller.connect(mac) }
            if (result.isSuccess) {
                Log.i(TAG, "initiator: switch complete attempt=$attempt peer=${peerPub.toHexPrefix()}")
                val nonce = peerStore.nextAudioOutNonce(peerPub)
                sender(peerPub, AudioOpFrame(nonce, AudioOp.Done, mac, nowSec()))
                // Could be in Connecting (BT raced ahead of Released)
                // or AlmostDone (Released arrived first — the optimistic
                // path). Either way the next state is Idle.
                val cur = stateRef.get()
                if (cur == SwitchState.Connecting || cur == SwitchState.AlmostDone) {
                    transition(cur, SwitchState.Idle)
                }
                activeInitiator = null
                return
            }
            lastErr = "attempt#$attempt: ${result.exceptionOrNull()?.message ?: "connect failed"}"
            Log.i(TAG, "connect retry: $lastErr")
            if (attempt < CONNECT_RETRY_COUNT) {
                delay(CONNECT_RETRY_PAUSE_MS)
            }
        }
        // All retries exhausted.
        val nonce = peerStore.nextAudioOutNonce(peerPub)
        sender(peerPub, AudioOpFrame(nonce, AudioOp.Failed(Stage.Connect, lastErr), mac, nowSec()))
        failInitiator(peerPub, lastErr)
    }

    private fun failInitiator(peerPub: ByteArray, reason: String) {
        Log.w(TAG, "initiator: failed peer=${peerPub.toHexPrefix()} reason=$reason")
        val failed = SwitchState.Failed(reason)
        stateRef.set(failed)
        flowState.value = failed
        persistence?.save(failed, currentPeer, currentMac)
        activeInitiator = null
        scope.launch {
            delay(FAILED_RESET_MS)
            // Only reset if no new flow started in the meantime.
            if (stateRef.compareAndSet(failed, SwitchState.Idle)) {
                flowState.value = SwitchState.Idle
                currentPeer = null
                currentMac = null
                persistence?.clear()
            }
        }
    }

    // ---- Responder flow ----

    private fun startResponderFlow(peerPub: ByteArray, mac: String) {
        // A Request that answers our OWN Claim is never refused.
        //
        // This is the post-call hand-back, and it was failing every time. At
        // call end the phone claims the buds for the laptop; the laptop answers
        // with a Request; and the phone rejected it `InCall`, because the call
        // gate reads `currentCall`, which deliberately lingers ~6 s on
        // PHASE_ENDED so the laptop can clear its pill promptly. So the phone
        // refused the very hand-off it had just asked for. On this deployment
        // that was 19 of 20 switch failures, and the laptop treats the reject as
        // terminal, so the earbuds ended up on nobody.
        //
        // Checked here rather than in the acceptance provider because this is
        // where the MAC is known, and the rule is about THIS device's own
        // recent request, not about any caller's policy.
        if (recentlyClaimed(mac)) {
            Log.i(TAG, "responder: honouring peer Request for our own recent claim of $mac")
        } else when (val accept = acceptanceProvider()) {
            is Acceptance.Reject -> {
                scope.launch {
                    val nonce = peerStore.nextAudioOutNonce(peerPub)
                    sender(
                        peerPub,
                        AudioOpFrame(nonce, AudioOp.Reject(accept.reason), mac, nowSec()),
                    )
                }
                return
            }
            Acceptance.Allow -> { /* fall through */ }
        }
        // We're allowed. If we're already running an initiator flow,
        // reject with Busy — owner-vote means only one flow at a time.
        if (stateRef.get() != SwitchState.Idle) {
            scope.launch {
                val nonce = peerStore.nextAudioOutNonce(peerPub)
                sender(
                    peerPub,
                    AudioOpFrame(nonce, AudioOp.Reject(RejectReason.Busy), mac, nowSec()),
                )
            }
            return
        }
        activeResponder = ActiveFlow(peerPub, mac, 0)
        scope.launch { runResponder(peerPub, mac) }
    }

    private suspend fun runResponder(peerPub: ByteArray, mac: String) {
        // Approve immediately so the initiator UI can render "peer
        // released, connecting" without waiting for the actual BT
        // disconnect.
        val approveNonce = peerStore.nextAudioOutNonce(peerPub)
        sender(peerPub, AudioOpFrame(approveNonce, AudioOp.Approve, mac, nowSec()))

        // Now actually disconnect locally.
        val disc = withContext(Dispatchers.IO) { controller.disconnect(mac) }
        if (disc.isFailure) {
            val msg = disc.exceptionOrNull()?.message ?: "local disconnect failed"
            val nonce = peerStore.nextAudioOutNonce(peerPub)
            sender(
                peerPub,
                AudioOpFrame(nonce, AudioOp.Failed(Stage.Disconnect, msg), mac, nowSec()),
            )
            activeResponder = null
            return
        }
        val releasedNonce = peerStore.nextAudioOutNonce(peerPub)
        sender(peerPub, AudioOpFrame(releasedNonce, AudioOp.Released, mac, nowSec()))

        // Wait for Done; ignore if it never comes (user clicked,
        // we don't fight them).
        delay(DONE_WAIT_MS)
        activeResponder = null
    }

    private fun responderOnDone(peerPub: ByteArray) {
        val flow = activeResponder ?: return
        if (!peerPub.contentEquals(flow.peerPub)) return
        activeResponder = null
    }

    private fun peerReportedFailure(peerPub: ByteArray, failed: AudioOp.Failed) {
        Log.w(TAG, "peer reported failure: ${failed.stage} ${failed.message}")
        if (activeInitiator?.peerPub?.contentEquals(peerPub) == true) {
            failInitiator(peerPub, "peer: ${failed.message}")
        }
        activeResponder = null
    }

    // ---- Helpers ----

    /** Atomic CAS transition + StateFlow notify. Returns true iff the
     *  transition actually happened (i.e. previous state matched). */
    private fun transition(from: SwitchState, to: SwitchState): Boolean {
        val ok = stateRef.compareAndSet(from, to)
        if (ok) {
            flowState.value = to
            if (to == SwitchState.Idle) {
                currentPeer = null
                currentMac = null
                persistence?.clear()
            } else {
                persistence?.save(to, currentPeer, currentMac)
            }
        }
        return ok
    }

    /** R1 — on app start, decide whether to resume / rollback the
     *  previously-persisted flow. Must be called once after
     *  construction, before [request] or any `onIncoming`. */
    fun recoverOnStart() {
        val p = persistence ?: return
        when (val act = p.recover()) {
            SwitchPersistence.Action.None -> { /* nothing to do */ }
            is SwitchPersistence.Action.Rollback -> {
                Log.w(TAG, "recover: rollback — ${act.previousReason}")
                stateRef.set(SwitchState.Failed(act.previousReason))
                flowState.value = SwitchState.Failed(act.previousReason)
                p.clear()
                scope.launch {
                    delay(FAILED_RESET_MS)
                    stateRef.compareAndSet(SwitchState.Failed(act.previousReason), SwitchState.Idle)
                    if (stateRef.get() == SwitchState.Idle) flowState.value = SwitchState.Idle
                }
            }
            is SwitchPersistence.Action.ResumeConnect -> {
                Log.i(TAG, "recover: resume Connecting mac=${act.mac}")
                currentPeer = act.peerPub
                currentMac = act.mac
                stateRef.set(SwitchState.Connecting)
                flowState.value = SwitchState.Connecting
                activeInitiator = ActiveFlow(act.peerPub, act.mac, 0)
                // attemptConnect is idempotent — if the buds came back
                // automatically while we were down, BT will report
                // "already connected" and we send Done immediately.
                scope.launch { attemptConnect(act.peerPub, act.mac) }
            }
        }
    }

    private fun nowSec(): Long = System.currentTimeMillis() / 1000L

    private fun ByteArray.toHexPrefix(): String =
        take(4).joinToString("") { "%02x".format(it) } + "…"

    private data class ActiveFlow(val peerPub: ByteArray, val mac: String, val nonce: Long) {
        /** Completed when the peer's `Released` frame arrives — the
         *  signal that the buds are physically free to grab. The
         *  initiator's connect loop awaits this (with a ceiling) so it
         *  fires the instant the peer drops the link, instead of after
         *  a blind fixed delay. Body val → excluded from data-class
         *  equals/hashCode/copy (we only key flows by peer+mac+nonce). */
        val releasedSignal = CompletableDeferred<Unit>()

        fun matches(p: ByteArray, m: String) = peerPub.contentEquals(p) && mac == m
        // contentEquals + value equality, not array identity
        override fun equals(other: Any?): Boolean {
            if (this === other) return true
            if (other !is ActiveFlow) return false
            return peerPub.contentEquals(other.peerPub) && mac == other.mac && nonce == other.nonce
        }
        override fun hashCode(): Int {
            var r = peerPub.contentHashCode()
            r = 31 * r + mac.hashCode()
            r = 31 * r + nonce.hashCode()
            return r
        }
    }

    companion object {
        /** How long a Claim we sent keeps the peer's answering Request
         *  exempt from the acceptance gate. See [recentlyClaimed]. */
        private const val CLAIM_HONOUR_MS: Long = 10_000

        private const val TAG = "VortexSwitch"

        /** Hard ceiling on a non-terminal flow before the watchdog forces
         *  Idle. Above any legitimate switch (connect retries top out near
         *  ~5.5s) so it only trips on a real wedge. */
        const val FLOW_WATCHDOG_MS: Long = 10_000

        /** How many local connect attempts to make before giving up.
         *  Matches the ecosystem prototype — second attempt usually
         *  wins once the phone finishes releasing the buds. */
        const val CONNECT_RETRY_COUNT: Int = 3

        /** Pause between connect retries. Tuned for BlueZ's
         *  peer-disconnect propagation time. */
        const val CONNECT_RETRY_PAUSE_MS: Long = 280

        /** Delay before the FIRST local connect attempt, giving the
         *  remote peer time to release the buds. Empirically ~500-700
         *  ms covers Linux's BLE-notify → orchestrator → BlueZ
         *  disconnect path. Without this head-start, attempt #1 burns
         *  the full 1500 ms `CONNECT_SETTLE_MS` waiting for a
         *  CONNECTED state that physically can't happen yet.
         *  Now only a fallback for the (unexpected) no-active-flow
         *  path; the normal path is event-driven on `Released` —
         *  see [RELEASED_WAIT_CEILING_MS]. */
        const val PRE_CONNECT_HEAD_START_MS: Long = 600

        /** Ceiling on how long the initiator waits for the peer's
         *  `Released` frame before connecting anyway (P2.15). The
         *  normal case completes far sooner — the await returns the
         *  moment Released arrives. This bound only matters if the
         *  frame is lost (both BLE and LAN dropped mid-switch); we'd
         *  rather attempt a connect that briefly fails than hang.
         *  Linux now sends `Released` the instant it *initiates* the ACL
         *  drop, but that frame still rides BLE — which the buds' A2DP
         *  stream to the laptop starves — so it lands ~1.2 s out. Rather
         *  than WAIT for it, we fire the connect EARLY and let BlueZ
         *  QUEUE it: a connect issued while the buds are still on the
         *  laptop stays in CONNECTING and lands the instant the laptop
         *  releases (verified live; this is exactly what the ecosystem
         *  build did — fire ~150 ms in, no release round-trip, ~1.1 s
         *  total). So this is a small head-start (enough for Linux to
         *  have begun the drop), NOT a release wait; Released, if it
         *  beats the ceiling, just fires us a hair sooner. */
        const val RELEASED_WAIT_CEILING_MS: Long = 250

        /** Responder keeps activeResponder alive this long after
         *  sending Released — long enough for the initiator's Done
         *  to arrive, after which we clear and return to Idle. */
        const val DONE_WAIT_MS: Long = 4_000

        /** UI shows "Failed: ..." for this long after a failure, then
         *  auto-resets to Idle so the user can retry without
         *  navigating away. */
        const val FAILED_RESET_MS: Long = 3_000
    }
}

/** All possible orchestrator states. Single source of truth — no
 *  ad-hoc booleans like the old ecosystem's eleven `@Volatile` flags. */
sealed class SwitchState {
    object Idle : SwitchState()
    object Preparing : SwitchState()
    object WaitingApproval : SwitchState()
    object WaitingReleased : SwitchState()
    /** Local BT connect is being attempted. UI dims + shows "Switching…". */
    object Connecting : SwitchState()
    /** Peer has confirmed Released. UI surfaces success already even
     *  though our BT connect is still settling (~2-3 s left). Click is
     *  still blocked until BT actually completes. */
    object AlmostDone : SwitchState()
    data class Failed(val reason: String) : SwitchState()
}
