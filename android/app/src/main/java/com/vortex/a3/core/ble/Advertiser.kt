package com.vortex.a3.core.ble

import android.bluetooth.BluetoothManager
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertisingSet
import android.bluetooth.le.AdvertisingSetCallback
import android.bluetooth.le.AdvertisingSetParameters
import android.content.Context
import android.os.ParcelUuid
import android.util.Log
import com.vortex.a3.core.crypto.Presence
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import java.security.SecureRandom

/** BLE advertiser per spec §5.1 + §7.3. */
class Advertiser(private val context: Context) {

    private val adapter by lazy {
        val bm = context.getSystemService(BluetoothManager::class.java)
        bm.adapter
    }

    private val advertiser by lazy { adapter?.bluetoothLeAdvertiser }

    /** The live advertising set, or null while off air. */
    @Volatile
    private var advertisingSet: AdvertisingSet? = null

    /** Callback identifying the live set (also the stop handle). */
    @Volatile
    private var setCallback: AdvertisingSetCallback? = null

    /** Whether the set currently on air was started at the LOW_LATENCY
     *  interval. A payload swap can only be done in place while the interval
     *  stays the same; changing it needs a real restart. */
    @Volatile
    private var activeLowLatency: Boolean? = null

    /** Result sink for the start / data-swap in flight. The presence loop is
     *  sequential — it starts one advertisement and then waits out the dwell —
     *  so a single slot is enough. */
    @Volatile
    private var pendingResult: ((StartResult) -> Unit)? = null

    @Volatile
    private var pendingPayload: AdvPayload? = null

    /** Returns true while the phone should advertise in reconnect-seeking
     *  (LOW_LATENCY) mode — wired by VortexStack to "no live laptop GATT
     *  connection AND it dropped recently". Checked at every rotation. */
    @Volatile
    var fastModeProvider: (() -> Boolean)? = null

    /** Wakes the rotation loop early (conflated: at most one pending). */
    private val rotationKick = Channel<Unit>(Channel.CONFLATED)

    /** Re-advertise NOW with a freshly evaluated mode instead of waiting
     *  out the rotation sleep. Called on GATT connect/disconnect edges so
     *  the reconnect-seeking LOW_LATENCY boost engages the moment the
     *  laptop link drops (waiting for the next 60s rotation cost the
     *  whole first reconnect window). */
    fun kickRotation() {
        rotationKick.trySend(Unit)
    }

    @Volatile
    private var activePayload: AdvPayload? = null

    /** Background rotation job for trusted-presence mode (null in pairable). */
    @Volatile
    private var presenceJob: Job? = null

    /** Result of a startAdvertising call. */
    sealed class StartResult {
        data class Started(val payload: AdvPayload) : StartResult()
        data class Failed(val reason: String) : StartResult()
    }

    /**
     * Put [payload] on air.
     *
     * Reuses the advertising set whenever one is already running at the right
     * interval, swapping only the service data. That in-place swap is the
     * point: stopping and restarting a set makes the controller mint a fresh
     * RPA, and a laptop that has just resolved our token then spends its whole
     * connect timeout dialling an address we have already abandoned. With two
     * remembered laptops the rotation loop tore the set down every
     * [MULTIPLEX_DWELL_MS] — far shorter than any real connect — so the GATT
     * link could never be established at all, and every BLE-only payload
     * (clipboard text above all) was dropped in silence.
     *
     * A connect is dialled against an ADDRESS, not a token, so a stable
     * address also means the laptop can still finish connecting after we have
     * swapped on to the next peer's token — multiplexing no longer costs
     * connectability.
     *
     * Privacy is unaffected: the controller keeps rotating the RPA on its own
     * timer. We merely stop forcing a rotation every dwell.
     *
     * The advertiser stops itself if [stop] is called or the process exits.
     */
    fun startWith(payload: AdvPayload, onResult: (StartResult) -> Unit) {
        val advertiser = advertiser
        if (advertiser == null) {
            onResult(StartResult.Failed("bluetooth not available"))
            return
        }
        val lowLatency = wantsLowLatency(payload)
        val live = advertisingSet
        // A pairing window always gets a FRESH set. It is a new, deliberately
        // identity-exposing session, so inheriting the presence beacon's
        // address would carry the old one straight into it; the swap is an
        // optimisation for the 24/7 presence rotation, not for this.
        if (live != null && activeLowLatency == lowLatency && !payload.flags.isPairable) {
            swapPayload(live, payload, onResult)
        } else {
            startSet(advertiser, payload, lowLatency, onResult)
        }
    }

    /**
     * Whether [payload] should ride the dense (~100 ms) schedule.
     *
     * Pairable mode is a short user-opened window where discovery speed
     * matters. Trusted-presence runs 24/7 and is normally ~250 ms, BUT while
     * the laptop link is DOWN and recently lost ([fastModeProvider]) it goes
     * dense too: the laptop's CONNECT_IND is answered at an advertising event,
     * so a denser schedule directly cuts the walk-up reconnect (live-measured:
     * screen-off connects ~11s vs ~1.5s screen-on — MIUI throttles background
     * advertising hard, and a dense request lands in a faster throttle tier).
     *
     * `seeking` has to be its own term: [fastModeProvider] means "link is DOWN
     * and was lost recently", but a seek deliberately keeps the current link UP
     * (seek before release), so it evaluates false exactly when we most want
     * the dense schedule — the user is walking to another machine right now.
     */
    private fun wantsLowLatency(payload: AdvPayload): Boolean =
        payload.flags.isPairable || seeking || fastModeProvider?.invoke() == true

    /**
     * ADV_IND per spec §5.1.1: Flags + Service Data 128-bit AD only.
     * The Service Data field already carries the Vortex Service UUID, so
     * adding it via addServiceUuid() would duplicate it and overflow the
     * 31-byte legacy advertisement budget.
     */
    private fun advertiseDataFor(payload: AdvPayload): AdvertiseData =
        AdvertiseData.Builder()
            .addServiceData(ParcelUuid(Ble.VORTEX_SERVICE_UUID), payload.encode())
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .build()

    /**
     * SCAN_RSP carries the device's Bluetooth alias. This DEVIATES from spec
     * §5.1.2 ("user-set device name MUST NOT appear here") — a deliberate
     * per-user override because the alias is needed to disambiguate when
     * several Vortex phones appear in the Linux scan list. The standard
     * Bluetooth GAP layer already exposes this alias during normal BT
     * discovery; the marginal extra exposure here is the time-window
     * difference (foreground-bound while no trust). User is aware and accepts.
     */
    private fun scanResponseData(): AdvertiseData =
        AdvertiseData.Builder()
            .setIncludeDeviceName(true)
            .build()

    /** Swap the service data on the live set — same set, same address. */
    private fun swapPayload(
        set: AdvertisingSet,
        payload: AdvPayload,
        onResult: (StartResult) -> Unit,
    ) {
        pendingPayload = payload
        pendingResult = onResult
        try {
            set.setAdvertisingData(advertiseDataFor(payload))
        } catch (e: SecurityException) {
            pendingPayload = null
            pendingResult = null
            onResult(StartResult.Failed("missing BLUETOOTH_ADVERTISE permission: ${e.message}"))
        }
    }

    /** Start a fresh advertising set (first time on air, or the interval changed). */
    private fun startSet(
        advertiser: android.bluetooth.le.BluetoothLeAdvertiser,
        payload: AdvPayload,
        lowLatency: Boolean,
        onResult: (StartResult) -> Unit,
    ) {
        // Tear down any set running at the wrong interval first.
        stop()

        val parameters = AdvertisingSetParameters.Builder()
            // Legacy mode keeps the exact ADV_IND + SCAN_RSP shape the Linux
            // scanner already parses; only the control API changes.
            .setLegacyMode(true)
            .setConnectable(true)
            .setScannable(true)
            .setInterval(
                if (lowLatency) {
                    AdvertisingSetParameters.INTERVAL_LOW
                } else {
                    AdvertisingSetParameters.INTERVAL_MEDIUM
                },
            )
            // HIGH (vs MEDIUM): the laptop hears us from farther away, so the
            // walk-up reconnect starts at the range edge instead of near the
            // desk. TX cost is per-advertising-event — small.
            .setTxPowerLevel(AdvertisingSetParameters.TX_POWER_HIGH)
            .build()

        val callback = object : AdvertisingSetCallback() {
            override fun onAdvertisingSetStarted(
                set: AdvertisingSet?,
                txPower: Int,
                status: Int,
            ) {
                val sink = pendingResult
                val started = pendingPayload
                pendingResult = null
                pendingPayload = null
                if (status != ADVERTISE_SUCCESS || set == null) {
                    Log.e(TAG, "advertise failed: ${errorCodeMessage(status)}")
                    setCallback = null
                    advertisingSet = null
                    activeLowLatency = null
                    sink?.invoke(StartResult.Failed(errorCodeMessage(status)))
                    return
                }
                advertisingSet = set
                activeLowLatency = lowLatency
                activePayload = started
                val mode = if (started?.flags?.isPairable == true) "pairable" else "trusted-presence"
                Log.i(
                    TAG,
                    "advertise started: $mode, instance=${started?.encode()?.copyOfRange(2, 10)?.toHexString()}",
                )
                started?.let { sink?.invoke(StartResult.Started(it)) }
            }

            override fun onAdvertisingDataSet(set: AdvertisingSet?, status: Int) {
                val sink = pendingResult
                val swapped = pendingPayload
                pendingResult = null
                pendingPayload = null
                if (status != ADVERTISE_SUCCESS) {
                    Log.w(TAG, "advertise payload swap failed: ${errorCodeMessage(status)}")
                    sink?.invoke(StartResult.Failed(errorCodeMessage(status)))
                    return
                }
                activePayload = swapped
                // Deliberately quieter than a start: this fires every dwell
                // while multiplexing, and the address has NOT changed.
                Log.i(
                    TAG,
                    "advertise payload swapped: instance=${swapped?.encode()?.copyOfRange(2, 10)?.toHexString()}",
                )
                swapped?.let { sink?.invoke(StartResult.Started(it)) }
            }

            override fun onAdvertisingSetStopped(set: AdvertisingSet?) {
                advertisingSet = null
                activeLowLatency = null
            }
        }

        setCallback = callback
        pendingPayload = payload
        pendingResult = onResult
        try {
            advertiser.startAdvertisingSet(
                parameters,
                advertiseDataFor(payload),
                scanResponseData(),
                null,
                null,
                callback,
            )
        } catch (e: SecurityException) {
            setCallback = null
            pendingPayload = null
            pendingResult = null
            onResult(StartResult.Failed("missing BLUETOOTH_ADVERTISE permission: ${e.message}"))
        }
    }

    /**
     * Start a pairable advertisement with a fresh random instance ID.
     * Used during a user-opened pairing window (spec §6.1).
     */
    fun startPairableAdvertise(onResult: (StartResult) -> Unit) {
        val instanceId = ByteArray(8).also { SecureRandom().nextBytes(it) }
        startWith(AdvPayload.pairable(instanceId), onResult)
    }

    /**
     * Same as [startPairableAdvertise] but uses a caller-provided 8-byte
     * instance ID. Used when the BLE advertise and the LAN mDNS instance
     * must share the same `payload_8` so a discoverer can correlate them
     * as the same device (spec §5.4).
     */
    fun startPairableAdvertiseWith(instanceId: ByteArray, onResult: (StartResult) -> Unit) {
        require(instanceId.size == 8) { "instanceId must be 8 bytes" }
        startWith(AdvPayload.pairable(instanceId), onResult)
    }

    /**
     * True while a peer session is live. When it is, the presence loop
     * advertises **nothing**: the session itself is the proof of presence, so
     * a beacon on top of it is pure battery cost. Wired by VortexStack to the
     * GATT server's connection state.
     *
     * This is the biggest saving in the whole state machine — the phone is
     * connected most of the time, and it used to beacon 24/7 regardless. It is
     * safe for the laptop's proximity auto-lock precisely because that treats
     * "authenticated session OR token-validated advertisement" as presence,
     * and on a drop [kickRotation] puts us back on air immediately.
     */
    var linkedProvider: (() -> Boolean)? = null

    /**
     * The PRS of every peer whose token we may advertise, most-recently-used
     * first. Returning several enables token multiplexing (below).
     */
    var presencePeersProvider: (() -> List<ByteArray>)? = null

    /**
     * Set while the user is looking for a *different* laptop ("Switch").
     * Forces advertising even though a session is live, so the other laptop
     * can see us without dropping the one we are on first.
     */
    @Volatile
    var seeking: Boolean = false

    /**
     * Presence + seeking loop (spec §7.3, design doc §D1/§D5).
     *
     * One advertising set, driven through three phases:
     *
     *  * **Active** — a session is live and we are not seeking: advertise
     *    nothing, and re-check often enough that a missed disconnect callback
     *    self-heals in seconds rather than a full rotation window.
     *  * **Seeking / Dark** — no session (or the user pressed Switch):
     *    advertise `TRUSTED_PRESENCE`. [fastModeProvider] already supplies the
     *    ladder — LOW_LATENCY while the link was recently lost, BALANCED after
     *    that. BALANCED is the floor rather than silence on purpose: the
     *    laptop's proximity confirmation scan is short, and a present-but-
     *    silent phone would be mistaken for one that walked away.
     *
     * **Token multiplexing.** The advertisement carries exactly one 8-byte
     * token and the ADV_IND is already at the legacy 31-byte ceiling, so N
     * remembered laptops cannot be addressed at once. With more than one peer
     * the loop cycles them, dwelling [MULTIPLEX_DWELL_MS] on each, so any of
     * them sees us within N × dwell — a few seconds, which is nothing on a
     * deliberate walk-up. Cycling is free of the old cost: the loop swaps the
     * service data on one long-lived advertising set rather than restarting it,
     * so the Bluetooth address stays put and a laptop that has resolved its
     * token can still complete a connect after the token has moved on.
     */
    fun startPresenceLoop(
        scope: CoroutineScope,
        rotationWindowSec: Long = 60L,
        onError: (String) -> Unit = {},
    ) {
        presenceJob?.cancel()
        stop()
        presenceJob = scope.launch {
            // Consecutive start failures. Each round retries regardless
            // (restarting an advertiser is cheap and the radio may have just
            // come back), but a persistent failure must not stay silent —
            // the phone is INVISIBLE over BLE while this fails. Surface it
            // once via onError after a few misses, then again only if it
            // keeps failing after a recovery.
            var consecFails = 0
            var wasSilent = false
            while (isActive) {
                val linked = linkedProvider?.invoke() == true
                if (linked && !seeking) {
                    if (!wasSilent) {
                        Log.i(TAG, "presence: session live — advertising suspended")
                        wasSilent = true
                    }
                    stop()
                    // Short re-check, not a full bucket: if a disconnect
                    // callback is ever dropped we would otherwise stay dark
                    // (and invisible) for up to a whole rotation window.
                    withTimeoutOrNull(ACTIVE_RECHECK_MS) { rotationKick.receive() }
                    continue
                }
                if (wasSilent) {
                    Log.i(TAG, "presence: link down or seeking — advertising resumed")
                    wasSilent = false
                }

                val peers = presencePeersProvider?.invoke().orEmpty()
                if (peers.isEmpty()) {
                    stop()
                    withTimeoutOrNull(ACTIVE_RECHECK_MS) { rotationKick.receive() }
                    continue
                }

                val nowSec = System.currentTimeMillis() / 1000
                val bucket = Presence.currentBucket(nowSec, rotationWindowSec)
                val onStart: (StartResult) -> Unit = { result ->
                    when (result) {
                        is StartResult.Started -> consecFails = 0
                        is StartResult.Failed -> {
                            consecFails++
                            Log.w(TAG, "presence advertise failed (${consecFails}x): ${result.reason}")
                            if (consecFails == PRESENCE_FAIL_ALERT_AT) onError(result.reason)
                        }
                    }
                }

                if (peers.size == 1) {
                    // No stop() first: [startWith] reuses the live set and
                    // swaps the service data, so the bucket rotation no longer
                    // costs an address change either.
                    startWith(AdvPayload.trustedPresence(Presence.deriveToken(peers[0], bucket)), onStart)
                    // Sleep until ~5s past the next bucket boundary so we
                    // refresh just inside the new window — OR until a kick
                    // (connect/disconnect edge) asks for an immediate
                    // re-advertise with a re-evaluated mode. Receivers
                    // tolerate ±1 bucket so a small drift is fine.
                    val sleepSec = rotationWindowSec - (nowSec % rotationWindowSec) + 5L
                    withTimeoutOrNull(sleepSec * 1000) { rotationKick.receive() }
                } else {
                    // Multiplex one pass over the peers, then re-evaluate the
                    // phase from the top (the session may have come back, or
                    // the peer set changed).
                    for (prs in peers) {
                        if (!isActive) break
                        // Swap the token in place. Tearing the set down here is
                        // what used to re-randomise the RPA every dwell and made
                        // the laptop's connect time out every single attempt.
                        startWith(AdvPayload.trustedPresence(Presence.deriveToken(prs, bucket)), onStart)
                        val kicked = withTimeoutOrNull(MULTIPLEX_DWELL_MS) { rotationKick.receive() }
                        // A kick means the phase changed — abandon the pass
                        // instead of finishing a cycle nobody is waiting for.
                        if (kicked != null) break
                    }
                }
            }
        }
    }

    /**
     * Single-peer entry point, kept for the pairing-completion path which has
     * exactly one peer and no service running yet.
     */
    fun startTrustedPresence(
        prs: ByteArray,
        scope: CoroutineScope,
        rotationWindowSec: Long = 60L,
        /** True while a peer is connected over GATT. See the rotation loop. */
        isConnected: () -> Boolean = { false },
        onError: (String) -> Unit = {},
    ) {
        require(prs.size == 32) { "PRS must be 32 bytes" }
        val only = listOf(prs.copyOf())
        presencePeersProvider = { only }
        startPresenceLoop(scope, rotationWindowSec, onError)
    }

    fun stop() {
        val cb = setCallback ?: return
        try {
            advertiser?.stopAdvertisingSet(cb)
        } catch (e: SecurityException) {
            Log.w(TAG, "stopAdvertisingSet threw: ${e.message}")
        }
        setCallback = null
        advertisingSet = null
        activeLowLatency = null
        activePayload = null
        pendingResult = null
        pendingPayload = null
        Log.i(TAG, "advertise stopped")
    }

    /** Stop both adv and any rotation job. */
    fun stopAll() {
        presenceJob?.cancel()
        presenceJob = null
        stop()
    }

    fun isAdvertising(): Boolean = setCallback != null

    fun activePayload(): AdvPayload? = activePayload

    private fun errorCodeMessage(code: Int): String = when (code) {
        AdvertisingSetCallback.ADVERTISE_FAILED_DATA_TOO_LARGE -> "ADVERTISE_FAILED_DATA_TOO_LARGE"
        AdvertisingSetCallback.ADVERTISE_FAILED_TOO_MANY_ADVERTISERS -> "ADVERTISE_FAILED_TOO_MANY_ADVERTISERS"
        AdvertisingSetCallback.ADVERTISE_FAILED_ALREADY_STARTED -> "ADVERTISE_FAILED_ALREADY_STARTED"
        AdvertisingSetCallback.ADVERTISE_FAILED_INTERNAL_ERROR -> "ADVERTISE_FAILED_INTERNAL_ERROR"
        AdvertisingSetCallback.ADVERTISE_FAILED_FEATURE_UNSUPPORTED -> "ADVERTISE_FAILED_FEATURE_UNSUPPORTED"
        else -> "advertise error $code"
    }

    companion object {
        private const val TAG = "VortexAdv"

        /** Consecutive trusted-presence start failures before [startPresenceLoop]'s
         *  onError fires (the loop itself keeps retrying every bucket). */
        private const val PRESENCE_FAIL_ALERT_AT = 3

        /** How long each peer's token stays on air during multiplexing.
         *
         *  Long enough for a scanning laptop to catch several advertising
         *  events (dense ≈ 100 ms, normal ≈ 250 ms), short enough that N peers
         *  all get seen within a few seconds.
         *
         *  This no longer bounds how often the RPA changes: a dwell now swaps
         *  the service data on the live advertising set instead of restarting
         *  it, so the address survives the whole pass. That is what closes the
         *  stale-RPA connect wedge — the laptop dials an address, not a token,
         *  so it can still finish connecting after we have moved on to the next
         *  peer. Before the swap, any dwell shorter than a connect timeout
         *  (~8 s) made the link unestablishable with two or more peers. */
        private const val MULTIPLEX_DWELL_MS = 1_500L

        /** Re-check interval while advertising is suspended (session live) or
         *  there is nothing to advertise. Bounds how long a *dropped*
         *  disconnect callback can leave us silent and therefore invisible. */
        private const val ACTIVE_RECHECK_MS = 15_000L
    }
}

private fun ByteArray.toHexString(): String =
    joinToString("") { "%02x".format(it) }
