package com.vortex.a3.service

import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.coroutines.launch

/**
 * Wi-Fi Direct (direct-link fast pull) — split out of [VortexStack]. A big
 * shared file brings up a P2P group owner on the phone and tells the laptop to
 * join for an ~20 MB/s direct link; the group is torn down after an idle window.
 * Also wires a debug-only broadcast hook for manual GO validation.
 */

/** Create the Wi-Fi Direct GO (idempotent) and, once it's up, tell the laptop
 *  to join; tear it down after an idle window (reset on each big file). */
internal fun VortexStack.maybeStartWifiDirect() {
    com.vortex.a3.core.lan.WifiDirect.start(ctx) {
        // COALESCE the offer. `WifiDirect.start` is idempotent for the group but
        // re-invokes this callback on every call (`if (isUp) onReady()`), and
        // this runs once per big file in a share. A 65-file batch therefore sent
        // 65 offers in about a second, and each one makes the laptop join the P2P
        // group and then restore its normal Wi-Fi — a single adapter, so that is
        // a full disconnect/reconnect cycle per offer. The user got a storm of
        // Wi-Fi notifications and pulls kept getting cut off mid-transfer, which
        // is also where the duplicate files came from.
        //
        // Not "only on transition": a LATER batch, arriving while the group is
        // still up but after the laptop restored its Wi-Fi, does need telling
        // again. A time gap satisfies both, and sits well inside the idle
        // teardown window so a genuinely new batch is never starved.
        val now = android.os.SystemClock.elapsedRealtime()
        val since = now - lastWifiDirectOfferAtMs
        if (lastWifiDirectOfferAtMs != 0L && since < VortexStack.WIFI_DIRECT_OFFER_MIN_GAP_MS) {
            Log.d(VortexStack.TAG, "wifi-direct: offer suppressed (sent ${since}ms ago)")
            return@start
        }
        lastWifiDirectOfferAtMs = now
        val o = org.json.JSONObject()
        o.put("ssid", com.vortex.a3.core.lan.WifiDirect.SSID)
        o.put("pass", com.vortex.a3.core.lan.WifiDirect.PASS)
        val offer = o.toString().toByteArray(Charsets.UTF_8)
        for (peer in peerStore.list()) {
            gattServer?.sendWifiDirectOfferEncrypted(peer.peerStaticPub, offer)
        }
        Log.i(VortexStack.TAG, "wifi-direct: GO ready → offer sent to laptop")
    }
    wifiDirectTeardownJob?.cancel()
    wifiDirectTeardownJob = scope.launch {
        kotlinx.coroutines.delay(60_000)
        com.vortex.a3.core.lan.WifiDirect.stop()
        // Group is gone, so the next batch must be free to offer at once
        // rather than waiting out the coalescing gap.
        lastWifiDirectOfferAtMs = 0L
        Log.i(VortexStack.TAG, "wifi-direct: GO torn down (idle)")
    }
}

/**
 * Debug-only Wi-Fi Direct validation hook:
 *   adb shell am broadcast -a com.vortex.a3.WIFI_DIRECT --es mode on
 *   adb shell am broadcast -a com.vortex.a3.WIFI_DIRECT --es mode off
 * `on` brings up the GO (logs SSID/passphrase/IP); the laptop joins manually
 * to measure the direct-link speed. EXPORTED so adb can reach it (debug only).
 */
internal fun VortexStack.registerWifiDirectReceiver() {
    if (!com.vortex.a3.BuildConfig.DEBUG) return
    val receiver = object : android.content.BroadcastReceiver() {
        override fun onReceive(c: android.content.Context?, intent: android.content.Intent?) {
            when (intent?.getStringExtra("mode")?.lowercase()) {
                "on" -> {
                    Log.i(VortexStack.TAG, "WIFI_DIRECT broadcast → on")
                    com.vortex.a3.core.lan.WifiDirect.start(ctx)
                }
                "off" -> {
                    Log.i(VortexStack.TAG, "WIFI_DIRECT broadcast → off")
                    com.vortex.a3.core.lan.WifiDirect.stop()
                }
                else -> Log.w(VortexStack.TAG, "WIFI_DIRECT: --es mode on|off required")
            }
        }
    }
    val filter = android.content.IntentFilter("com.vortex.a3.WIFI_DIRECT")
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        service.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
    } else {
        @Suppress("UnspecifiedRegisterReceiverFlag")
        service.registerReceiver(receiver, filter)
    }
    wifiDirectReceiver = receiver
}
