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
        val advertiseMode = if (payload.flags.isPairable ||
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
     * Start trusted-presence advertising with a rotating token derived
     * from [prs] per spec §7.3. The token rotates every
     * [rotationWindowSec] seconds so passive observers cannot link
     * sightings across windows.
     *
     * The supplied [scope] owns the rotation job. Cancel the scope (or
     * call [stop]) to end advertising.
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
        // Cancel any existing rotation before starting a new one. Stop
        // current adv too so we start the new mode cleanly.
        presenceJob?.cancel()
        stop()
        val prsCopy = prs.copyOf()
        presenceJob = scope.launch {
            // Consecutive start failures. Each bucket retries regardless
            // (restarting an advertiser is cheap and the radio may have just
            // come back), but a persistent failure must not stay silent —
            // the phone is INVISIBLE over BLE while this fails. Surface it
            // once via onError after a few misses, then again only if it
            // keeps failing after a recovery.
            var consecFails = 0
            while (isActive) {
                val nowSec = System.currentTimeMillis() / 1000
                val bucket = Presence.currentBucket(nowSec, rotationWindowSec)
                val token = Presence.deriveToken(prsCopy, bucket)
                // Do NOT restart the advertiser while a peer is connected.
                //
                // Stopping and starting an advertising set makes Android hand
                // out a fresh resolvable private address. Doing that every 60 s
                // (and again on every characteristic subscribe) meant the
                // laptop's cached address was ALWAYS dead by the time it tried
                // to reconnect, so its "connect straight to the last address"
                // fast path could never once succeed — every reconnect paid a
                // full 15 s scan, and six such failures in a row used to make
                // the laptop power-cycle its whole Bluetooth adapter.
                //
                // The rotation exists to stop a passive observer linking our
                // advertisements over time. A connected peer is not that
                // observer: it already knows exactly who we are, and while the
                // link is up nobody is scanning for us. So rotate when it
                // matters — between sessions — and hold still while connected.
                if (isConnected() && activePayload != null) {
                    Log.d(TAG, "presence rotation held: peer connected (keeping this RPA)")
                    val intoBucket = nowSec % rotationWindowSec
                    withTimeoutOrNull((rotationWindowSec - intoBucket + 5L) * 1000) {
                        rotationKick.receive()
                    }
                    continue
                }
                stop()
                startWith(AdvPayload.trustedPresence(token)) { result ->
                    when (result) {
                        is StartResult.Started -> consecFails = 0
                        is StartResult.Failed -> {
                            consecFails++
                            Log.w(TAG, "trusted-presence advertise failed (${consecFails}x): ${result.reason}")
                            if (consecFails == PRESENCE_FAIL_ALERT_AT) onError(result.reason)
                        }
                    }
                }
                // Sleep until ~5s past the next bucket boundary so we
                // refresh just inside the new window — OR until a kick
                // (connect/disconnect edge) asks for an immediate
                // re-advertise with a re-evaluated mode. Receivers
                // tolerate ±1 bucket so a small drift is fine.
                val secondsIntoBucket = nowSec % rotationWindowSec
                val sleepSec = rotationWindowSec - secondsIntoBucket + 5L
                withTimeoutOrNull(sleepSec * 1000) { rotationKick.receive() }
            }
        }
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

        /** Consecutive trusted-presence start failures before [startTrustedPresence]'s
         *  onError fires (the loop itself keeps retrying every bucket). */
        private const val PRESENCE_FAIL_ALERT_AT = 3
    }
}

private fun ByteArray.toHexString(): String =
    joinToString("") { "%02x".format(it) }
