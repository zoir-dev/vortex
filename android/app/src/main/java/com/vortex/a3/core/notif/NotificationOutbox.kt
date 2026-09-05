package com.vortex.a3.core.notif

import android.os.SystemClock
import android.util.Log

/**
 * Per-peer queue of notifications that couldn't be delivered because the BLE
 * link was down (no subscriber / no cipher). Notification mirroring is
 * BLE-only, and BLE drops briefly and often (RPA rotation, A2DP radio
 * contention, reconnect after the daemon restarts) — without a buffer, every
 * notification posted during a down window is lost forever.
 *
 * The capture path enqueues here whenever an immediate send fails; the stack
 * flushes a peer's queue the instant that peer re-subscribes to AUDIO_SIGNAL
 * (i.e. the notify path is deliverable again). Bounded + TTL'd so a long
 * outage can't grow it without limit or replay ancient notifications.
 *
 * Thread-safe: the capture coroutine enqueues while the GATT callback thread
 * triggers a flush.
 */
class NotificationOutbox {

    private data class Entry(val mirror: NotificationMirror, val queuedAtMs: Long)

    /** peer-static-pub hex → FIFO of pending notifications (oldest first). */
    private val queues = HashMap<String, ArrayDeque<Entry>>()
    private val lock = Any()

    /**
     * Queue a notification that failed to send to [peerHex]. A DISMISS for a
     * notification still sitting in the queue (never delivered) cancels that
     * entry instead of queuing — the laptop never saw the original, so there's
     * nothing to dismiss there.
     */
    fun enqueue(peerHex: String, mirror: NotificationMirror) {
        synchronized(lock) {
            val q = queues.getOrPut(peerHex) { ArrayDeque() }
            if (mirror.dismiss && mirror.key.isNotEmpty()) {
                val before = q.size
                q.removeAll { it.mirror.key == mirror.key && !it.mirror.dismiss }
                if (q.size != before) return // original never went out → drop both
            }
            pruneExpired(q)
            q.addLast(Entry(mirror, SystemClock.elapsedRealtime()))
            while (q.size > MAX_PER_PEER) q.removeFirst() // shed oldest under pressure
            Log.i(TAG, "buffered notification for ${peerHex.take(8)}… (queue=${q.size})")
        }
    }

    /**
     * Try to deliver everything queued for [peerHex], oldest first, via
     * [send] (returns true on a successful BLE notify). Stops at the first
     * failure and keeps the remainder queued in order, so a flush fired a hair
     * too early (before the subscription fully settles) simply retries on the
     * next trigger. No-op when the queue is empty.
     */
    suspend fun flush(peerHex: String, send: suspend (NotificationMirror) -> Boolean) {
        val pending: List<Entry> = synchronized(lock) {
            val q = queues[peerHex] ?: return
            pruneExpired(q)
            val snapshot = q.toList()
            q.clear()
            snapshot
        }
        if (pending.isEmpty()) return
        Log.i(TAG, "flushing ${pending.size} buffered notification(s) to ${peerHex.take(8)}…")
        val leftover = ArrayList<Entry>()
        var stalled = false
        var first = true
        for (e in pending) {
            if (stalled) { leftover.add(e); continue }
            // Pace the backlog like every other burst on this link. The whole
            // queue used to go out back-to-back, which is the shape that
            // overruns the BLE stack, loses a notify, and desyncs the receive
            // cipher — the very failure the 12 ms discipline exists for
            // everywhere else, and the reason the contacts burst has a settle
            // delay in front of it.
            if (!first) kotlinx.coroutines.delay(12)
            first = false
            val ok = try { send(e.mirror) } catch (ex: Exception) {
                Log.w(TAG, "flush send threw: ${ex.message}"); false
            }
            if (!ok) { stalled = true; leftover.add(e) }
        }
        if (leftover.isNotEmpty()) synchronized(lock) {
            // Put the undelivered ones back at the FRONT (they're the oldest).
            val q = queues.getOrPut(peerHex) { ArrayDeque() }
            for (e in leftover.asReversed()) q.addFirst(e)
            while (q.size > MAX_PER_PEER) q.removeFirst()
            Log.i(TAG, "${leftover.size} still pending for ${peerHex.take(8)}… (link not ready)")
        }
    }

    private fun pruneExpired(q: ArrayDeque<Entry>) {
        val now = SystemClock.elapsedRealtime()
        while (q.isNotEmpty() && now - q.first().queuedAtMs > TTL_MS) q.removeFirst()
    }

    companion object {
        private const val TAG = "VortexNotifOutbox"
        /** Cap per peer — a burst during a long outage sheds the oldest. */
        private const val MAX_PER_PEER = 50
        /** Drop notifications older than this. Generous on purpose ("hech
         *  narsa yo'qolmasin"): a workday away from the laptop must still
         *  deliver on return. Not spammy in practice — reading/dismissing a
         *  notification on the phone cancels its queued entry (see
         *  [enqueue]), so only UNSEEN ones replay, and the cap sheds the
         *  rest. */
        private const val TTL_MS = 12 * 60 * 60 * 1000L
    }
}
