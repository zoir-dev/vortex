package com.vortex.a3.core.notif

import android.content.Context
import android.util.Log

/**
 * Tell the user, once per feature per process, that Android is refusing a
 * permission Vortex needs — and take them to the screen that fixes it.
 *
 * These failures used to be a swallowed `SecurityException` and a debug log
 * line. On MIUI they are routine: it revokes runtime permissions on reinstall
 * and after a spell of disuse, and it can report a permission as granted while
 * the appop behind it denies the read. The visible result was a laptop showing
 * "No contacts yet" or "No messages" forever, with both devices displaying a
 * healthy connection and nothing, anywhere, explaining why.
 *
 * The notification lands on the phone deliberately: that is where the setting
 * lives, so it is the one place where being told is also being able to act.
 */
object FeatureBlocked {
    private const val TAG = "VortexFeatureBlocked"
    private const val CHANNEL_ID = "vortex_broken"

    /** Features already reported this process, so a polling provider that
     *  fails every tick does not become a notification stream. */
    private val reported = java.util.Collections.synchronizedSet(HashSet<String>())

    /**
     * @param feature short user-facing name, e.g. "Contacts" or "Messages".
     * @param permission the Android permission that was refused.
     */
    fun report(context: Context, feature: String, permission: String) {
        if (!reported.add(feature)) return
        Log.w(TAG, "$feature unavailable: $permission is denied")
        try {
            val nm = context.getSystemService(android.app.NotificationManager::class.java) ?: return
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.O &&
                nm.getNotificationChannel(CHANNEL_ID) == null
            ) {
                nm.createNotificationChannel(
                    android.app.NotificationChannel(
                        CHANNEL_ID,
                        "Vortex problems",
                        android.app.NotificationManager.IMPORTANCE_DEFAULT,
                    ).apply {
                        description = "Tells you when a Vortex feature has stopped working."
                    },
                )
            }
            val intent = android.content.Intent(
                android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                android.net.Uri.fromParts("package", context.packageName, null),
            ).addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)
            val pi = android.app.PendingIntent.getActivity(
                context,
                feature.hashCode(),
                intent,
                android.app.PendingIntent.FLAG_UPDATE_CURRENT or
                    android.app.PendingIntent.FLAG_IMMUTABLE,
            )
            val text = "$feature can't reach your laptop because Android is blocking " +
                "permission. Tap to grant it."
            val n = androidx.core.app.NotificationCompat.Builder(context, CHANNEL_ID)
                .setSmallIcon(android.R.drawable.stat_sys_warning)
                .setContentTitle("$feature is unavailable")
                .setContentText(text)
                .setStyle(androidx.core.app.NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(pi)
                .setAutoCancel(true)
                .build()
            androidx.core.app.NotificationManagerCompat.from(context)
                .notify(NOTIF_BASE + (feature.hashCode() and 0xFF), n)
        } catch (t: Throwable) {
            Log.w(TAG, "could not report $feature: ${t.message}")
        }
    }

    private const val NOTIF_BASE = 0x701F0
}
