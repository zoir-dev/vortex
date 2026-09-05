package com.vortex.a3.ui

import android.Manifest
import android.content.Context
import android.os.Build
import android.os.PowerManager
import androidx.core.app.NotificationManagerCompat

// Standalone system / permission checks pulled out of MainActivity — pure
// context-only helpers (no Activity state).

internal fun Context.isIgnoringBatteryOptimizations(): Boolean {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return true
    val pm = getSystemService(PowerManager::class.java) ?: return true
    return pm.isIgnoringBatteryOptimizations(packageName)
}

/** True when our NotificationListenerService is enabled — the signal
 *  that the smart-switch can use precise MediaSession detection instead
 *  of the coarse isMusicActive fallback. Checked on each [onResume] to
 *  raise the "grant notification access" prompt (and to clear it once
 *  the user has granted). */
internal fun Context.isNotificationAccessGranted(): Boolean =
    androidx.core.app.NotificationManagerCompat
        .getEnabledListenerPackages(this)
        .contains(packageName)

/**
 * The permissions WITHOUT which pairing cannot happen at all.
 *
 * Everything else in [requiredPermissions] is a companion feature — the call
 * mirror, contacts, SMS — and the comments there already say so ("All optional
 * — the mirror degrades without them"). The permission RESULT handler did not
 * agree: one denial anywhere in that list and advertising never started, so a
 * user who declined SMS access could not pair at all, and the screen told them
 * only "permissions denied".
 *
 * Bluetooth is genuinely different. Without it there is no radio to advertise
 * on, so there is nothing to degrade to.
 */
internal fun pairingCriticalPermissions(): List<String> = buildList {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        add(Manifest.permission.BLUETOOTH_ADVERTISE)
        add(Manifest.permission.BLUETOOTH_CONNECT)
    }
}

internal fun requiredPermissions(): List<String> = buildList {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        add(Manifest.permission.BLUETOOTH_ADVERTISE)
        add(Manifest.permission.BLUETOOTH_CONNECT)
    }
    // Real-call hand-off: the CallFlowOrchestrator registers a
    // telephony listener which silently no-ops without this runtime
    // permission. Declared in the manifest but, like all dangerous
    // permissions, must be granted at runtime.
    add(Manifest.permission.READ_PHONE_STATE)
    // Call mirroring → the laptop banner. ANSWER_PHONE_CALLS lets its
    // Accept/Decline answer/hang up via TelecomManager (API 26+);
    // READ_CALL_LOG surfaces the caller number; READ_CONTACTS resolves it
    // to a name. All optional — the mirror degrades without them.
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        add(Manifest.permission.ANSWER_PHONE_CALLS)
    }
    add(Manifest.permission.READ_CALL_LOG)
    add(Manifest.permission.READ_CONTACTS)
    // Dial from the laptop (Contacts/Recents): place the outgoing call.
    add(Manifest.permission.CALL_PHONE)
    // Mirror recent SMS to the laptop's Messages page.
    add(Manifest.permission.READ_SMS)
    // Send SMS the laptop composed (Contacts/Messages).
    add(Manifest.permission.SEND_SMS)
}

/**
 * Connectivity + notification permissions the app needs to advertise, pair and
 * post its foreground notification. Requested UP FRONT on first launch so the
 * user grants "Nearby devices" once and pairing then just works — instead of a
 * dialog interrupting the pairing tap (and, if missed, a silent BLE failure).
 */
internal fun essentialPermissions(): List<String> = buildList {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        add(Manifest.permission.BLUETOOTH_ADVERTISE)
        add(Manifest.permission.BLUETOOTH_CONNECT)
        add(Manifest.permission.BLUETOOTH_SCAN)
    } else {
        // Pre-12, BLE scanning is gated on location.
        add(Manifest.permission.ACCESS_FINE_LOCATION)
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        add(Manifest.permission.NEARBY_WIFI_DEVICES)
        add(Manifest.permission.POST_NOTIFICATIONS)
    }
}

internal fun pickerRequiredPermissions(): List<String> = buildList {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        add(Manifest.permission.BLUETOOTH_SCAN)
        add(Manifest.permission.BLUETOOTH_CONNECT)
    } else {
        add(Manifest.permission.ACCESS_FINE_LOCATION)
    }
}

/**
 * Heuristic: detect OEM ROMs that aggressively kill background apps and/or block
 * BOOT_COMPLETED until the user whitelists the app (autostart / "never-sleeping
 * apps"). Used only to surface the setup hint and pick the deep-link target; the
 * rest of the app is OEM-agnostic. Stock Android / Pixel return false (no hint
 * needed — the standard receiver + battery-opt whitelist suffice).
 */
internal fun isAggressiveOemRom(): Boolean {
    val m = Build.MANUFACTURER.lowercase()
    return AGGRESSIVE_OEMS.any { m.contains(it) }
}

private val AGGRESSIVE_OEMS = listOf(
    "xiaomi", "redmi", "poco",            // MIUI / HyperOS
    "huawei", "honor",                    // EMUI / MagicOS
    "samsung",                            // One UI — "never-sleeping apps"
    "oppo", "realme", "oneplus",          // ColorOS / OxygenOS
    "vivo", "iqoo",                       // Funtouch / OriginOS
    "meizu", "asus", "letv",
)
