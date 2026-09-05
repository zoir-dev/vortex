package com.vortex.a3.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import androidx.core.app.NotificationCompat
import com.vortex.a3.core.appstate.AppState
import com.vortex.a3.ui.MainActivity

/**
 * Owns the Vortex foreground-service notification: channel creation, the
 * custom RemoteViews battery row (laptop + earbuds, tinted by which device
 * holds the buds), the periodic battery-refresh ticker, and the owner-hold
 * display state that keeps the earbuds row from blinking out mid-switch.
 *
 * Pulled out of [VortexService] so that file stays focused on the network
 * stack. The service supplies the live state it renders + reacts to the
 * toggle action via [Host]; this class only renders + schedules.
 */
class VortexNotification(
    private val service: Service,
    private val host: Host,
) {
    /** Live state the notification renders, supplied by the service. */
    interface Host {
        /** True when this phone currently holds the buds (A2DP connected). */
        fun phoneOwnsBuds(): Boolean
        /** Latest peer (laptop) AppState — battery / charging / earbuds. */
        fun peerState(): AppState?
        /** Earbuds battery to show when the phone owns them (null if unknown). */
        fun phoneEarbudsBattery(): Int?
        /** Millis since the last peer AppState arrived (MAX_VALUE = never).
         *  Gates the lock glyph: a STALE locked state must not render a
         *  tappable lock — a tap queued while the laptop is unreachable
         *  fires much later (sticky command TTL) with surprising effects
         *  (live-observed: re-locked the laptop seconds after the user
         *  had just unlocked it at the keyboard). */
        fun peerStateAgeMs(): Long
    }

    private val handler = Handler(Looper.getMainLooper())
    // Owner shown in the row, held across the brief mid-switch gap where
    // neither device reports the buds. lastOwner = last real owner;
    // targetOwner = side a manual tap is switching TO; lastOwnerAtMs = when
    // we last had a real owner.
    @Volatile private var lastOwner: String? = null
    @Volatile private var targetOwner: String? = null
    @Volatile private var lastOwnerAtMs: Long = 0L

    private val ticker = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, REFRESH_MS)
        }
    }

    /** Enter the foreground with the notification + start the refresh tick. */
    fun startInForeground() {
        createChannel()
        val notification = build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // Android 14+ requires foregroundServiceType at the API call.
            service.startForeground(
                NOTIF_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
            )
        } else {
            service.startForeground(NOTIF_ID, notification)
        }
        handler.removeCallbacks(ticker)
        handler.postDelayed(ticker, REFRESH_MS)
    }

    /** Signature of what the ongoing notification currently shows. */
    @Volatile private var lastRendered: String? = null

    /**
     * Re-render the notification — but only when something in it CHANGED.
     *
     * The ticker runs every two seconds and used to re-post unconditionally:
     * 288 posts in a morning, almost all of them identical. That is expensive
     * for nothing, and on MIUI it is worse than that — a foreground
     * notification updating that often looks like a misbehaving app in the
     * battery accounting, which is what OneKeyClean goes hunting for. The kill
     * it then performs is what leaves the notification listener unbound.
     */
    fun refresh() {
        try {
            val sig = renderSignature()
            if (sig == lastRendered) return
            lastRendered = sig
            service.getSystemService(NotificationManager::class.java)
                ?.notify(NOTIF_ID, build())
        } catch (_: Exception) {}
    }

    /** Everything the rendered notification depends on, as one comparable
     *  string. Cheap: all of it is already in memory. */
    private fun renderSignature(): String {
        val peer = host.peerState()
        val fresh = peer != null && host.peerStateAgeMs() < PEER_FRESH_MS
        return listOf(
            host.phoneOwnsBuds(),
            host.phoneEarbudsBattery(),
            fresh,
            if (fresh) peer?.battery else null,
            if (fresh) peer?.charging else null,
            if (fresh) peer?.earbuds?.connected else null,
            if (fresh) peer?.earbuds?.battery else null,
            if (fresh) peer?.locked else null,
            lastOwner,
            targetOwner,
        ).joinToString("|")
    }

    /** The service's toggle handler calls this after kicking off a switch:
     *  remember the side we're switching TO so the row shows that owner's
     *  colour during the gap, then fire a few quick refreshes so it updates
     *  the moment the buds land (rather than waiting for the slow tick). */
    fun noteSwitchTarget(owner: String) {
        targetOwner = owner
        refresh()
        handler.postDelayed({ refresh() }, 700)
        handler.postDelayed({ refresh() }, 1600)
        handler.postDelayed({ refresh() }, 2800)
    }

    /** Stop the refresh tick (service onDestroy). */
    fun stop() {
        handler.removeCallbacks(ticker)
    }

    /** Foreground notification, ecosystem-style: a custom row with the
     *  laptop battery + the earbuds battery, the earbuds tinted by which
     *  device holds them (green = phone, blue = laptop) and tappable to
     *  switch. No phone battery, no title text. */
    private fun build(): Notification {
        val phoneOwns = host.phoneOwnsBuds()
        val peer = host.peerState()
        // Only trust the peer's battery/charging while the link is FRESH — a
        // stale AppState left over from before a disconnect must not keep
        // showing the laptop's last battery % or a ⚡ (the bug: it read
        // "charging" while actually disconnected). Same gate the lock/clipboard
        // buttons already use below.
        val laptopFresh = peer != null && host.peerStateAgeMs() < PEER_FRESH_MS
        // Gate "laptop holds the buds" on freshness too — same as the grab
        // logic's `peerHoldsBuds`. Without it, a stale AppState left over from
        // before a disconnect kept the earbuds row pinned showing the laptop's
        // last battery % (blue, ⚡) forever — the bug seen after the laptop was
        // powered off: the phone still advertised the wireless buds as a
        // charged laptop device. Now it ages out within PEER_FRESH_MS like the
        // laptop battery/lock rows.
        val laptopOwns = !phoneOwns && laptopFresh && peer?.earbuds?.connected == true
        val laptopBatt = if (laptopFresh) peer?.battery else null
        val budsBatt = if (phoneOwns) host.phoneEarbudsBattery()
            else if (laptopFresh) peer?.earbuds?.battery else null
        fun pct(v: Int?) = v?.let { "$it%" } ?: "--"

        // Resolve the owner, but HOLD it across the brief mid-switch gap
        // where neither device reports the buds — otherwise the earbuds row
        // blinks out every switch. During the gap we show "…" in the
        // target (manual) / last (auto) owner's colour.
        val realOwner = when {
            phoneOwns -> "phone"
            laptopOwns -> "laptop"
            else -> null
        }
        val now = SystemClock.elapsedRealtime()
        if (realOwner != null) {
            lastOwner = realOwner
            lastOwnerAtMs = now
            if (targetOwner == realOwner) targetOwner = null // switch landed
        }
        val pending = realOwner == null
        val displayOwner = realOwner ?: targetOwner ?: lastOwner
        val showRow = displayOwner != null &&
            (realOwner != null || now - lastOwnerAtMs < OWNER_HOLD_MS)

        val rv = android.widget.RemoteViews(
            service.packageName,
            com.vortex.a3.R.layout.notification_vortex,
        )
        // Laptop (peer) battery — turns blue with a ⚡ when the laptop is
        // plugged in / charging, otherwise the usual white.
        val laptopCharging = laptopFresh && peer?.charging == true
        val laptopColor = if (laptopCharging) {
            service.getColor(com.vortex.a3.R.color.notification_audio_linux)
        } else {
            service.getColor(android.R.color.white)
        }
        rv.setTextViewText(
            com.vortex.a3.R.id.notification_laptop_battery,
            if (laptopCharging) pct(laptopBatt) + " ⚡" else pct(laptopBatt),
        )
        rv.setTextColor(com.vortex.a3.R.id.notification_laptop_battery, laptopColor)
        rv.setInt(com.vortex.a3.R.id.notification_laptop_icon, "setColorFilter", laptopColor)
        if (showRow) {
            // showRow already implies displayOwner != null.
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_audio_group,
                android.view.View.VISIBLE,
            )
            rv.setTextViewText(
                com.vortex.a3.R.id.notification_audio_battery,
                if (pending) "…" else pct(budsBatt),
            )
            val color = if (displayOwner == "phone") {
                service.getColor(com.vortex.a3.R.color.notification_audio_android)
            } else {
                service.getColor(com.vortex.a3.R.color.notification_audio_linux)
            }
            rv.setTextColor(com.vortex.a3.R.id.notification_audio_battery, color)
            rv.setInt(com.vortex.a3.R.id.notification_audio_icon, "setColorFilter", color)
            val togglePi = PendingIntent.getService(
                service,
                3,
                Intent(service, VortexService::class.java).setAction(VortexService.ACTION_TOGGLE_AUDIO),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            rv.setOnClickPendingIntent(com.vortex.a3.R.id.notification_audio_group, togglePi)
        } else {
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_audio_group,
                android.view.View.GONE,
            )
        }
        // Remote-lock glyph: tap locks the laptop directly, or — when it's
        // locked — bounces through the translucent biometric trampoline to
        // unlock. Hidden when the laptop doesn't report a lock state
        // (pre-lock build) OR the state is stale (laptop unreachable) —
        // see Host.peerStateAgeMs.
        val laptopLocked = peer?.locked?.takeIf { host.peerStateAgeMs() < PEER_FRESH_MS }
        if (laptopLocked != null) {
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_lock_group,
                android.view.View.VISIBLE,
            )
            rv.setImageViewResource(
                com.vortex.a3.R.id.notification_lock_icon,
                if (laptopLocked) {
                    com.vortex.a3.R.drawable.ic_notification_lock
                } else {
                    com.vortex.a3.R.drawable.ic_notification_lock_open
                },
            )
            val lockColor = if (laptopLocked) {
                service.getColor(com.vortex.a3.R.color.notification_audio_linux)
            } else {
                service.getColor(android.R.color.white)
            }
            rv.setInt(com.vortex.a3.R.id.notification_lock_icon, "setColorFilter", lockColor)
            // Both directions are one-tap service actions now (no biometric
            // trampoline): the laptop only honours "unlock" while this phone is
            // unlocked, so a pocketed/locked phone can't open the laptop.
            val lockAction = if (laptopLocked) {
                VortexService.ACTION_UNLOCK_LAPTOP
            } else {
                VortexService.ACTION_LOCK_LAPTOP
            }
            val lockPi = PendingIntent.getService(
                service,
                if (laptopLocked) 4 else 5,
                Intent(service, VortexService::class.java).setAction(lockAction),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            rv.setOnClickPendingIntent(com.vortex.a3.R.id.notification_lock_group, lockPi)
        } else {
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_lock_group,
                android.view.View.GONE,
            )
        }
        // Send-clipboard button (top-right of the custom row). Shown only when
        // sync is on AND the laptop is actually connected (fresh peer state) —
        // tapping it with no peer would just read the clipboard and fail to
        // deliver. Same freshness gate as the lock button above.
        val laptopConnected = peer != null && host.peerStateAgeMs() < PEER_FRESH_MS
        if (laptopConnected && com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) {
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_clipboard_group,
                android.view.View.VISIBLE,
            )
            val clipPi = PendingIntent.getActivity(
                service,
                6,
                Intent(service, com.vortex.a3.core.clipboard.ClipboardQuickSendActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            rv.setOnClickPendingIntent(com.vortex.a3.R.id.notification_clipboard_group, clipPi)
        } else {
            rv.setViewVisibility(
                com.vortex.a3.R.id.notification_clipboard_group,
                android.view.View.GONE,
            )
        }

        val launchPi = PendingIntent.getActivity(
            service,
            0,
            Intent(service, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(service, CHANNEL_ID)
            .setSmallIcon(com.vortex.a3.R.drawable.ic_notification_vortex)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setContentIntent(launchPi)
            .setCustomContentView(rv)
            .setStyle(NotificationCompat.DecoratedCustomViewStyle())
            .build()
    }

    /**
     * Tell the user, once, that notification mirroring has stopped and only
     * they can restart it.
     *
     * This is deliberately a real, tappable, IMPORTANCE_DEFAULT notification
     * rather than a log line or a badge inside the app. The whole failure is
     * that everything LOOKS fine — both devices say connected, the toggle says
     * on — while nothing mirrors, so a signal the user has to go looking for is
     * no signal at all. Tapping it lands on the exact system screen that fixes
     * it.
     */
    fun postListenerBroken(accessRevoked: Boolean) {
        createBrokenChannel()
        val intent = android.content.Intent(
            android.provider.Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS,
        ).addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)
        val pi = android.app.PendingIntent.getActivity(
            service,
            0,
            intent,
            android.app.PendingIntent.FLAG_UPDATE_CURRENT or
                android.app.PendingIntent.FLAG_IMMUTABLE,
        )
        val text = if (accessRevoked) {
            "Notification access is off, so nothing reaches your laptop. Tap to turn it back on."
        } else {
            "Android stopped delivering notifications to Vortex. Tap, then switch Vortex off and " +
                "on again to restore mirroring."
        }
        val n = NotificationCompat.Builder(service, BROKEN_CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_warning)
            .setContentTitle("Notification mirroring stopped")
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(pi)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        try {
            androidx.core.app.NotificationManagerCompat.from(service).notify(BROKEN_NOTIF_ID, n)
        } catch (_: SecurityException) {
            // POST_NOTIFICATIONS denied — nothing more we can do from here.
        }
    }

    private fun createBrokenChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val nm = service.getSystemService(NotificationManager::class.java) ?: return
        if (nm.getNotificationChannel(BROKEN_CHANNEL_ID) != null) return
        nm.createNotificationChannel(
            NotificationChannel(
                BROKEN_CHANNEL_ID,
                "Vortex problems",
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply {
                description = "Tells you when a Vortex feature has stopped working."
            },
        )
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val nm = service.getSystemService(NotificationManager::class.java) ?: return
        if (nm.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Vortex background",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Keeps Vortex reachable to your paired devices."
            setShowBadge(false)
        }
        nm.createNotificationChannel(channel)
    }

    companion object {
        private const val CHANNEL_ID = "vortex_bg"
        private const val NOTIF_ID = 0x701E5
        /** Separate channel: the ongoing one is IMPORTANCE_LOW and silent by
         *  design, which is wrong for something the user must act on. */
        private const val BROKEN_CHANNEL_ID = "vortex_broken"
        private const val BROKEN_NOTIF_ID = 0x701E6
        /** How often the foreground notification re-reads batteries. */
        private const val REFRESH_MS = 2_000L
        /** Keep the earbuds row visible (showing "…") this long after the
         *  buds momentarily belong to neither device, so it doesn't blink
         *  out during a switch. */
        private const val OWNER_HOLD_MS = 12_000L
        /** Peer state older than this = laptop unreachable → hide the lock
         *  glyph (mirrors MainActivity's LAPTOP_STALE_MS). */
        private const val PEER_FRESH_MS = 30_000L
    }
}
