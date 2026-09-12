package com.vortex.a3.service

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.net.Uri
import android.util.Log
import androidx.core.app.NotificationCompat
import com.vortex.a3.R
import com.vortex.a3.core.clipboard.ClipboardFileReader

/**
 * Paces a multi-file share instead of capping it.
 *
 * Sharing 150 files used to deliver about 20 and claim success for all of them:
 * the outgoing bus dropped its overflow and the blob store evicted the rest
 * before the laptop's one-at-a-time pull reached them. Capping the batch made
 * that honest but refused work the user had asked for, which is worse UX than
 * simply taking longer.
 *
 * So the whole list is accepted and fed through a window: at most
 * [WINDOW] files are in flight, and the next is read only when one is
 * confirmed delivered. Two properties fall out of that:
 *
 *  * **Memory stays flat.** Files are read one at a time, on their turn — the
 *    queue holds URIs, not bytes. Reading 150 files up front is what makes an
 *    850 MB share an OutOfMemoryError.
 *  * **Nothing is evicted unread.** In-flight never exceeds what the blob store
 *    holds, so a queued file's bytes are still there when its turn comes.
 *
 * Progress is a single updating notification rather than a toast per file —
 * 150 toasts is its own bug.
 */
class ShareQueue(
    private val context: Context,
    /** Hand a read file to the existing offer path. Returns false if it could
     *  not be accepted, in which case the queue retries it later. */
    private val emit: (com.vortex.a3.core.clipboard.ClipboardOutgoingFile) -> Boolean,
    /** How many offers are awaiting collection right now. The window is
     *  measured against this, so pacing follows real delivery rather than a
     *  timer. */
    private val inFlight: () -> Int,
) {
    private val pending = ArrayDeque<Uri>()
    private var total = 0
    private var done = 0
    private var failed = 0

    /** Add [uris] to the queue and start (or continue) draining it. */
    @Synchronized
    fun enqueue(uris: List<Uri>) {
        if (uris.isEmpty()) return
        pending.addAll(uris)
        total += uris.size
        Log.i(TAG, "queued ${uris.size} file(s); $total total, ${pending.size} waiting")
        showProgress()
        pump()
    }

    /** A file reached the laptop. Advance progress and start the next one. */
    @Synchronized
    fun noteServed(name: String) {
        done++
        Log.i(TAG, "delivered '$name' ($done/$total)")
        showProgress()
        pump()
    }

    /** Read and hand off files until the in-flight window is full. */
    @Synchronized
    fun pump() {
        while (pending.isNotEmpty() && inFlight() < WINDOW) {
            val uri = pending.removeFirst()
            when (val outcome = ClipboardFileReader.read(context, uri)) {
                is ClipboardFileReader.Outcome.Ok -> {
                    if (!emit(outcome.file)) {
                        // Downstream is momentarily full — put it back and stop;
                        // the next delivery re-enters here.
                        pending.addFirst(uri)
                        return
                    }
                }
                is ClipboardFileReader.Outcome.TooLarge -> {
                    failed++
                    Log.w(TAG, "skipping $uri: over the size cap (${outcome.bytes} bytes)")
                    showProgress()
                }
                is ClipboardFileReader.Outcome.Unreadable -> {
                    failed++
                    Log.w(TAG, "skipping $uri: ${outcome.why}")
                    showProgress()
                }
            }
        }
        if (pending.isEmpty() && inFlight() == 0) finish()
    }

    private fun showProgress() {
        // A single share of one file already gets the share-sheet toast; a
        // progress notification on top of that is noise.
        if (total <= 1) return
        val nm = context.getSystemService(NotificationManager::class.java) ?: return
        ensureChannel(nm)
        val settled = done + failed
        val n = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.vortex_logo)
            .setContentTitle("Sending files to laptop")
            .setContentText(
                if (failed == 0) "$done of $total" else "$done of $total · $failed skipped",
            )
            .setProgress(total, settled, false)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()
        nm.notify(NOTIF_ID, n)
    }

    private fun finish() {
        val nm = context.getSystemService(NotificationManager::class.java) ?: return
        if (total <= 1) {
            nm.cancel(NOTIF_ID)
            reset()
            return
        }
        ensureChannel(nm)
        val n = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.vortex_logo)
            .setContentTitle(
                if (failed == 0) "Sent $done files" else "Sent $done of $total files",
            )
            .setContentText(
                if (failed == 0) "All files reached the laptop"
                else "$failed couldn't be sent (too large or unreadable)",
            )
            .setOngoing(false)
            .setAutoCancel(true)
            .build()
        nm.notify(NOTIF_ID, n)
        Log.i(TAG, "batch finished: $done sent, $failed skipped of $total")
        reset()
    }

    private fun reset() {
        total = 0
        done = 0
        failed = 0
    }

    private fun ensureChannel(nm: NotificationManager) {
        if (nm.getNotificationChannel(CHANNEL_ID) != null) return
        nm.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                "File transfers",
                // LOW: a progress bar that pings on every one of 150 updates
                // would be worse than the toasts it replaces.
                NotificationManager.IMPORTANCE_LOW,
            ).apply { description = "Progress while sending files to the laptop" },
        )
    }

    companion object {
        private const val TAG = "VortexShareQueue"
        private const val CHANNEL_ID = "vortex_transfer"
        private const val NOTIF_ID = 0x701E6

        /**
         * Files in flight at once.
         *
         * Must stay well under `ClipboardBlobStore.MAX_ENTRIES` so a queued
         * file's bytes cannot be evicted before the laptop collects them, and
         * small enough that the OFFER burst does not overrun the BLE notify
         * path (the same reason the offer sender paces itself).
         */
        const val WINDOW = 8
    }
}
