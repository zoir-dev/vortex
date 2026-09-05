package com.vortex.a3.core.notif

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import java.util.concurrent.atomic.AtomicInteger

/**
 * Posts a mirrored LAPTOP desktop notification on THIS phone (the
 * laptop→phone direction). Self-contained — its own channel, incrementing
 * id, BigText body. Best-effort: if POST_NOTIFICATIONS (Android 13+) isn't
 * granted, notify() throws SecurityException which we swallow.
 */
object IncomingNotificationDisplay {
    private const val CHANNEL_ID = "vortex_mirror"
    private val seq = AtomicInteger(20_000)

    /** Notification id per laptop-side key, so an update replaces its previous
     *  copy in place. Bounded LRU: one session does not produce unbounded
     *  distinct conversations, and a stale entry costs one integer. */
    private val idsByKey = object : LinkedHashMap<String, Int>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Int>?): Boolean =
            size > 200
    }
    @Volatile private var channelReady = false

    fun show(context: Context, m: NotificationMirror) {
        val ctx = context.applicationContext
        ensureChannel(ctx)
        val title = when {
            m.app.isNotBlank() && m.title.isNotBlank() -> "${m.app} · ${m.title}"
            m.title.isNotBlank() -> m.title
            else -> m.app.ifBlank { "Notification" }
        }
        // Reuse the id for a given key, so an updated laptop notification
        // REPLACES its previous copy instead of stacking a new one. A chat that
        // updated five times used to leave five notifications on the phone.
        val id = if (m.key.isNotEmpty()) {
            idsByKey.getOrPut(m.key) { seq.incrementAndGet() }
        } else {
            seq.incrementAndGet()
        }
        // A no-op contentIntent so a TAP dismisses the mirror. We can't run a
        // laptop notification's action from here (we're not its owner), but
        // setAutoCancel only fires when there IS a contentIntent — this gives
        // the user "tap to clear" instead of a notification that ignores taps.
        val tapIntent = android.content.Intent("com.vortex.a3.MIRROR_TAP")
            .setPackage(ctx.packageName)
        val piFlags = android.app.PendingIntent.FLAG_UPDATE_CURRENT or
            android.app.PendingIntent.FLAG_IMMUTABLE
        val tapPi = android.app.PendingIntent.getBroadcast(ctx, id, tapIntent, piFlags)
        val n = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setContentTitle(title)
            .setContentText(m.text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(m.text))
            .setAutoCancel(true)
            .setContentIntent(tapPi)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .build()
        try {
            NotificationManagerCompat.from(ctx).notify(id, n)
        } catch (_: SecurityException) {
            // POST_NOTIFICATIONS not granted — nothing to show.
        }
    }

    private fun ensureChannel(ctx: Context) {
        if (channelReady) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID,
                "Laptop notifications",
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply { description = "Notifications mirrored from the paired laptop" }
            ctx.getSystemService(NotificationManager::class.java)
                ?.createNotificationChannel(ch)
        }
        channelReady = true
    }
}
