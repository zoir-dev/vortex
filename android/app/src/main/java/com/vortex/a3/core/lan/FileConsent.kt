package com.vortex.a3.core.lan

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * Instant-share receive consent: when the laptop offers a file batch
 * (FILE_PUSH_OFFER), pop a notification with Accept / Decline and BLOCK the
 * LAN handler thread until the user chooses (or a timeout → decline). The
 * laptop only streams the bytes after an accept.
 *
 * [FileAutoAcceptSetting] short-circuits the whole prompt when the user has
 * opted in — see [request].
 */
object FileConsent {
    private const val TAG = "VortexFileConsent"
    private const val CHANNEL = "vortex_files"
    private const val ACTION = "com.vortex.a3.FILE_CONSENT"

    private val pending = ConcurrentHashMap<Int, ArrayBlockingQueue<Boolean>>()
    private val nextId = AtomicInteger(7000)

    @Volatile private var registered = false

    /**
     * Show the prompt and wait for the user's decision. Returns true = accept,
     * false = decline/timeout. Blocks the calling thread up to [timeoutMs].
     */
    fun request(
        ctx: Context,
        label: String,
        count: Int,
        totalBytes: Long,
        timeoutMs: Long = 45_000,
    ): Boolean {
        val app = ctx.applicationContext
        // Auto-accept: skip the prompt entirely and answer immediately, so the
        // laptop starts streaming without the 45 s wait. `init` is idempotent —
        // this call also covers the case where the LAN handler races ahead of
        // the UI/service having loaded the setting.
        FileAutoAcceptSetting.init(app)
        if (FileAutoAcceptSetting.isEnabled()) {
            Log.i(TAG, "auto-accept on → '$label' accepted without asking")
            return true
        }
        ensureReceiver(app)
        val id = nextId.getAndIncrement()
        val q = ArrayBlockingQueue<Boolean>(1)
        pending[id] = q
        showPrompt(app, id, label, count, totalBytes)
        return try {
            val decision = q.poll(timeoutMs, TimeUnit.MILLISECONDS)
            val accepted = decision == true
            Log.i(TAG, "consent '$label' → ${if (accepted) "accept" else "decline/timeout"}")
            accepted
        } catch (e: InterruptedException) {
            false
        } finally {
            pending.remove(id)
            cancel(app, id)
        }
    }

    private fun ensureReceiver(app: Context) {
        if (registered) return
        synchronized(this) {
            if (registered) return
            val rcv = object : BroadcastReceiver() {
                override fun onReceive(c: Context, i: Intent) {
                    val id = i.getIntExtra("id", -1)
                    val accept = i.getBooleanExtra("accept", false)
                    pending[id]?.offer(accept)
                }
            }
            val filter = IntentFilter(ACTION)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                app.registerReceiver(rcv, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                @Suppress("UnspecifiedRegisterReceiverFlag")
                app.registerReceiver(rcv, filter)
            }
            registered = true
        }
    }

    private fun showPrompt(app: Context, id: Int, label: String, count: Int, totalBytes: Long) {
        try {
            val nm = app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                nm.createNotificationChannel(
                    NotificationChannel(
                        CHANNEL,
                        "File transfers",
                        NotificationManager.IMPORTANCE_HIGH,
                    ),
                )
            }
            val accept = decisionIntent(app, id, true)
            val decline = decisionIntent(app, id, false)
            val title = if (count > 1) "Laptop wants to send $count files" else "Laptop wants to send a file"
            val text = "$label · ${fmtBytes(totalBytes)}"
            val n = NotificationCompat.Builder(app, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_sys_download)
                .setContentTitle(title)
                .setContentText(text)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .setCategory(NotificationCompat.CATEGORY_RECOMMENDATION)
                .setOngoing(true)
                .addAction(0, "Accept", accept)
                .addAction(0, "Decline", decline)
                .build()
            nm.notify(id, n)
        } catch (e: Exception) {
            Log.w(TAG, "showPrompt failed: ${e.message}")
            // No UI → fail safe: auto-decline by signalling the waiter.
            pending[id]?.offer(false)
        }
    }

    private fun decisionIntent(app: Context, id: Int, accept: Boolean): PendingIntent {
        val intent = Intent(ACTION).apply {
            setPackage(app.packageName)
            putExtra("id", id)
            putExtra("accept", accept)
        }
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        return PendingIntent.getBroadcast(app, id * 2 + if (accept) 1 else 0, intent, flags)
    }

    private fun cancel(app: Context, id: Int) {
        try {
            (app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).cancel(id)
        } catch (_: Exception) {
        }
    }

    private fun fmtBytes(n: Long): String = when {
        n < 1024 -> "$n B"
        n < 1024 * 1024 -> "${n / 1024} KB"
        n < 1024L * 1024 * 1024 -> "%.1f MB".format(n.toDouble() / 1024 / 1024)
        else -> "%.2f GB".format(n.toDouble() / 1024 / 1024 / 1024)
    }
}
