package com.vortex.a3.service

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.os.BatteryManager
import android.util.Log

/**
 * The infrastructure broadcast receivers VortexService relies on,
 * pulled out so the service file stays focused on the stack itself:
 *
 *  - **BT adapter state** — Android tears down all BLE advertising + the
 *    GATT server when the adapter turns off and never restores them, so we
 *    rebuild on STATE_ON (via [onBluetoothReenabled]).
 *  - **Battery** — edge-detected: a charging flip pushes instantly, a level
 *    change only on a ≥2-point delta, so a slow drain doesn't spam the peer
 *    (via [onBatteryChanged]).
 *  - **BT Pairing Auto-Confirm** — auto-confirms pairing requests from the
 *    trusted laptop without prompting user for PIN/confirmation.
 *
 * The service supplies the reactions as callbacks; this class owns the
 * receiver instances, the battery edge-detect state, and register/unregister
 * lifecycle. (The debug-only FAKE_CALL receiver stays in the service — it is
 * wired directly to the call-flow orchestrator.)
 */
class VortexReceivers(
    private val context: Context,
    private val onBluetoothReenabled: () -> Unit,
    private val onBatteryChanged: () -> Unit,
) {
    private var btStateReceiver: BroadcastReceiver? = null
    private var batteryReceiver: BroadcastReceiver? = null
    private var pairingRequestReceiver: BroadcastReceiver? = null
    private var networkCallback: android.net.ConnectivityManager.NetworkCallback? = null
    @Volatile private var lastWifiIp: String? = null
    @Volatile private var lastBattCharging: Boolean? = null
    @Volatile private var lastBattLevel: Int = -1

    fun register() {
        registerBtState()
        registerBattery()
        registerPairingRequest()
        registerNetwork()
    }

    fun unregister() {
        btStateReceiver?.let { safeUnregister(it) }
        batteryReceiver?.let { safeUnregister(it) }
        pairingRequestReceiver?.let { safeUnregister(it) }
        networkCallback?.let { cb ->
            try {
                context.getSystemService(android.net.ConnectivityManager::class.java)
                    ?.unregisterNetworkCallback(cb)
            } catch (_: Exception) {}
        }
        btStateReceiver = null
        batteryReceiver = null
        pairingRequestReceiver = null
        networkCallback = null
    }

    /**
     * Tell the laptop as soon as this phone's Wi-Fi address changes.
     *
     * There was no network listener at all. The phone's `wifiIp` only rode
     * whatever push happened next — a battery-level change, a connect, a
     * mirror toggle — so a phone sitting on the charger at 100% produced no
     * push for a long time. Meanwhile, with BLE up the phone releases its
     * multicast lock, so mDNS is empty and the laptop falls back to the cached
     * address: a router reboot handing out a new lease left every LAN feature
     * dead for hours, 15 seconds of timeout at a time, while BLE cheerfully
     * reported "connected". It happened here (192.168.1.147 → .113).
     *
     * A new address is the one thing the laptop cannot discover on its own, so
     * it is worth a push of its own.
     */
    private fun registerNetwork() {
        if (networkCallback != null) return
        val cm = context.getSystemService(android.net.ConnectivityManager::class.java) ?: return
        val cb = object : android.net.ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(
                network: android.net.Network,
                props: android.net.LinkProperties,
            ) {
                // First non-loopback IPv4 on this link — the same thing
                // `currentWifiIp()` reports to the peer.
                val ip = props.linkAddresses
                    .mapNotNull { it.address }
                    .firstOrNull { !it.isLoopbackAddress && it is java.net.Inet4Address }
                    ?.hostAddress
                if (ip == null || ip == lastWifiIp) return
                val seeding = lastWifiIp == null
                lastWifiIp = ip
                // registerNetworkCallback delivers the CURRENT link straight
                // away. That first delivery is not a change — it is the
                // starting point — so record it and say nothing.
                if (seeding) return
                Log.i(TAG, "wifi address changed → pushing state")
                onBatteryChanged() // the state-push trigger; name predates this use
            }

            override fun onLost(network: android.net.Network) {
                lastWifiIp = null
            }
        }
        try {
            val req = android.net.NetworkRequest.Builder()
                .addTransportType(android.net.NetworkCapabilities.TRANSPORT_WIFI)
                .build()
            cm.registerNetworkCallback(req, cb)
            networkCallback = cb
        } catch (e: Exception) {
            Log.w(TAG, "network callback: ${e.message}")
        }
    }

    private fun registerBtState() {
        if (btStateReceiver != null) return
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context?, intent: Intent?) {
                if (intent?.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
                when (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                    BluetoothAdapter.STATE_ON -> onBluetoothReenabled()
                    BluetoothAdapter.STATE_OFF ->
                        Log.i(TAG, "Bluetooth turned off — BLE down until re-enabled")
                }
            }
        }
        registerReceiverCompat(receiver, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))
        btStateReceiver = receiver
    }

    /**
     * Event-driven battery push. ACTION_BATTERY_CHANGED fires on every
     * charging flip (plug/unplug) and level tick. We edge-detect: a
     * charging-state flip fires [onBatteryChanged] instantly; a level change
     * only when it moves ≥2 points so a slow drain doesn't spam the peer.
     */
    private fun registerBattery() {
        if (batteryReceiver != null) return
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context?, intent: Intent?) {
                if (intent?.action != Intent.ACTION_BATTERY_CHANGED) return
                val status = intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
                val charging = status == BatteryManager.BATTERY_STATUS_CHARGING ||
                    status == BatteryManager.BATTERY_STATUS_FULL
                val rawLevel = intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
                val scale = intent.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
                val level = if (rawLevel >= 0 && scale > 0) rawLevel * 100 / scale else -1

                val chargingFlipped = lastBattCharging != null && lastBattCharging != charging
                val levelMoved = lastBattLevel >= 0 && level >= 0 &&
                    kotlin.math.abs(level - lastBattLevel) >= 2
                val firstSeen = lastBattCharging == null
                lastBattCharging = charging
                lastBattLevel = level
                // Skip the very first sticky broadcast (just seeds state).
                if (firstSeen) return
                if (chargingFlipped || levelMoved) {
                    Log.i(TAG, "battery event (charging=$charging level=$level) → push")
                    onBatteryChanged()
                }
            }
        }
        registerReceiverCompat(receiver, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        batteryReceiver = receiver
    }

    private fun registerPairingRequest() {
        if (pairingRequestReceiver != null) return
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context?, intent: Intent?) {
                if (intent?.action != BluetoothDevice.ACTION_PAIRING_REQUEST) return
                try {
                    val device = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE, BluetoothDevice::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE)
                    }
                    val pairingVariant = intent.getIntExtra("android.bluetooth.device.extra.PAIRING_VARIANT", 0)
                    Log.i(TAG, "Bluetooth pairing request from device: ${device?.name ?: device?.address}, variant: $pairingVariant")
                    // Automatically confirm pairing without prompting the user
                    device?.setPairingConfirmation(true)
                    abortBroadcast()
                } catch (e: Exception) {
                    Log.w(TAG, "Error auto-confirming pairing request: ${e.message}")
                }
            }
        }
        val filter = IntentFilter(BluetoothDevice.ACTION_PAIRING_REQUEST).apply {
            priority = IntentFilter.SYSTEM_HIGH_PRIORITY
        }
        registerReceiverCompat(receiver, filter)
        pairingRequestReceiver = receiver
    }

    private fun registerReceiverCompat(receiver: BroadcastReceiver, filter: IntentFilter) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }
    }

    private fun safeUnregister(receiver: BroadcastReceiver) {
        try { context.unregisterReceiver(receiver) } catch (_: Exception) {}
    }

    companion object {
        private const val TAG = "VortexReceivers"
    }
}
