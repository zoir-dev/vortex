package com.vortex.a3.core.ble

import android.bluetooth.BluetoothManager
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
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

    @Volatile
    private var activeCallback: AdvertiseCallback? = null

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
     * Start BLE advertising with the supplied [payload]. Used by both the
     * pairable-mode entry point and the trusted-presence rotation loop.
     *
     * The advertiser stops itself if [stop] is called or the process exits.
     */
    fun startWith(payload: AdvPayload, onResult: (StartResult) -> Unit) {
        if (activeCallback != null) {
            onResult(StartResult.Failed("already advertising"))
            return
        }
        val advertiser = advertiser
        if (advertiser == null) {
            onResult(StartResult.Failed("bluetooth not available"))
            return
        }

        val payloadBytes = payload.encode()

        // ADV_IND per spec §5.1.1: Flags + Service Data 128-bit AD only.
        // The Service Data field already carries the Vortex Service UUID, so
        // adding it via addServiceUuid() would duplicate it and overflow the
        // 31-byte legacy advertisement budget.
        val advertiseData = AdvertiseData.Builder()
            .addServiceData(ParcelUuid(Ble.VORTEX_SERVICE_UUID), payloadBytes)
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .build()

        // SCAN_RSP carries the device's Bluetooth alias. This DEVIATES
        // from spec §5.1.2 ("user-set device name MUST NOT appear
        // here") — a deliberate per-user override because the alias
        // is needed to disambiguate when several Vortex phones appear
        // in the Linux scan list. The standard Bluetooth GAP layer
        // already exposes this alias during normal BT discovery; the
        // marginal extra exposure here is the time-window difference
        // (foreground-bound while no trust). User is aware and accepts.
        val scanResponse = AdvertiseData.Builder()
            .setIncludeDeviceName(true)
            .build()

        // Pairable mode is a short user-opened window where discovery speed
        // matters → LOW_LATENCY (~100 ms interval). Trusted-presence runs
        // 24/7 → BALANCED (~250 ms) by default, BUT while the laptop link
        // is DOWN and recently lost ([fastModeProvider]) it also runs
        // LOW_LATENCY: the laptop's CONNECT_IND is answered at an
        // advertising event, so a denser schedule directly cuts the
        // walk-up reconnect (live-measured: screen-off connects ~11s vs
        // ~1.5s screen-on — MIUI throttles background advertising hard,
        // and a LOW_LATENCY request lands in a faster throttle tier).
        // Re-evaluated at every 60s token rotation.
        // `seeking` has to be its own term: [fastModeProvider] means "link is
        // DOWN and was lost recently", but a seek deliberately keeps the
        // current link UP (seek before release), so it evaluates false exactly
        // when we most want the dense schedule — the user is walking to
        // another machine right now. This is the first rung of the §D5 ladder.
        val advertiseMode = if (payload.flags.isPairable ||
            seeking ||
            fastModeProvider?.invoke() == true
        ) {
            AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY
        } else {
            AdvertiseSettings.ADVERTISE_MODE_BALANCED
        }
        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(advertiseMode)
            .setConnectable(true)
            .setTimeout(0) // Vortex manages the window
            // HIGH (vs MEDIUM): the laptop hears us from farther away, so
            // the walk-up reconnect starts at the range edge instead of
            // near the desk. TX cost is per-advertising-event — small.
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_HIGH)
            .build()

        val callback = object : AdvertiseCallback() {
            override fun onStartSuccess(settingsInEffect: AdvertiseSettings) {
                val mode = if (payload.flags.isPairable) "pairable" else "trusted-presence"
                Log.i(TAG, "advertise started: $mode, instance=${payloadBytes.copyOfRange(2, 10).toHexString()}")
                activePayload = payload
                onResult(StartResult.Started(payload))
            }

            override fun onStartFailure(errorCode: Int) {
                val msg = errorCodeMessage(errorCode)
                Log.e(TAG, "advertise failed: $msg")
                activeCallback = null
                onResult(StartResult.Failed(msg))
            }
        }

        activeCallback = callback
        try {
            advertiser.startAdvertising(settings, advertiseData, scanResponse, callback)
        } catch (e: SecurityException) {
            activeCallback = null
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
     * deliberate walk-up. With a single peer it does NOT cycle: restarting the
     * advertiser needlessly churns the RPA and costs battery, so the common
     * case keeps exactly the old one-advertise-per-bucket behaviour.
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
                    stop()
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
                        stop()
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
        val cb = activeCallback ?: return
        try {
            advertiser?.stopAdvertising(cb)
        } catch (e: SecurityException) {
            Log.w(TAG, "stopAdvertising threw: ${e.message}")
        }
        activeCallback = null
        activePayload = null
        Log.i(TAG, "advertise stopped")
    }

    /** Stop both adv and any rotation job. */
    fun stopAll() {
        presenceJob?.cancel()
        presenceJob = null
        stop()
    }

    fun isAdvertising(): Boolean = activeCallback != null

    fun activePayload(): AdvPayload? = activePayload

    private fun errorCodeMessage(code: Int): String = when (code) {
        AdvertiseCallback.ADVERTISE_FAILED_DATA_TOO_LARGE -> "ADVERTISE_FAILED_DATA_TOO_LARGE"
        AdvertiseCallback.ADVERTISE_FAILED_TOO_MANY_ADVERTISERS -> "ADVERTISE_FAILED_TOO_MANY_ADVERTISERS"
        AdvertiseCallback.ADVERTISE_FAILED_ALREADY_STARTED -> "ADVERTISE_FAILED_ALREADY_STARTED"
        AdvertiseCallback.ADVERTISE_FAILED_INTERNAL_ERROR -> "ADVERTISE_FAILED_INTERNAL_ERROR"
        AdvertiseCallback.ADVERTISE_FAILED_FEATURE_UNSUPPORTED -> "ADVERTISE_FAILED_FEATURE_UNSUPPORTED"
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
         *  events (LOW_LATENCY ≈ 100 ms, BALANCED ≈ 250 ms), short enough that
         *  N peers all get seen within a few seconds. Also the floor on how
         *  often we restart the advertising set, which re-randomises the RPA —
         *  cheaper dwells would inflate the laptop's BlueZ device cache and
         *  feed the stale-RPA connect wedge. */
        private const val MULTIPLEX_DWELL_MS = 1_500L

        /** Re-check interval while advertising is suspended (session live) or
         *  there is nothing to advertise. Bounds how long a *dropped*
         *  disconnect callback can leave us silent and therefore invisible. */
        private const val ACTIVE_RECHECK_MS = 15_000L
    }
}

private fun ByteArray.toHexString(): String =
    joinToString("") { "%02x".format(it) }
