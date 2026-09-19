package com.vortex.a3.core.clipboard

/**
 * Clipboard text that could not go out over BLE, held for the LAN sync.
 *
 * Phone→laptop clipboard text is a BLE notify and nothing else. When the GATT
 * link is down the notify fails at its first check and the text is gone — no
 * retry, no fallback, no message. That is invisible in exactly the case it
 * matters: a laptop can sit for hours with a perfectly healthy LAN session and
 * a BLE link that never connected, and every copy the user makes is dropped
 * while the UI says "Sending text to laptop…".
 *
 * File and image offers already solved this: they ride the bulk-sync done
 * frame (see `LanServer.pendingOffersProvider`), so the link that works is the
 * one that delivers. This is the same escape hatch for text.
 *
 * **One slot, latest wins.** The clipboard holds one primary item at a time,
 * so a queue would only ever deliver stale content the user has already
 * replaced — the same reasoning that makes [ClipboardSyncGuard] a single slot.
 *
 * **Bounded by age.** Text is only worth delivering while it is plausibly
 * still what the user wants to paste; a copy from this morning arriving when
 * the laptop finally reconnects is a surprise, not a feature.
 */
object ClipboardOutbox {

    private val lock = Any()
    private var text: String? = null
    private var stashedAt: Long = 0L

    /** Hold text the BLE path could not deliver. Replaces anything older. */
    fun stash(value: String) {
        if (value.isEmpty()) return
        synchronized(lock) {
            text = value
            stashedAt = System.currentTimeMillis()
        }
    }

    /**
     * Take the pending text if there is any and it is still fresh, clearing
     * the slot. Returns null otherwise.
     *
     * Taking on read (rather than after an ack) matches what the content is:
     * the done frame carrying it is the last frame of a round that has already
     * proved itself, and a clipboard is transient enough that re-announcing it
     * on every subsequent round would be worse than dropping it once.
     */
    fun take(): String? = synchronized(lock) {
        val pending = text ?: return@synchronized null
        text = null
        if (System.currentTimeMillis() - stashedAt > MAX_AGE_MS) null else pending
    }

    /** Drop anything pending — the content was delivered another way. */
    fun clear() {
        synchronized(lock) { text = null }
    }

    /** Beyond this, held text is stale enough that delivering it would surprise. */
    private const val MAX_AGE_MS = 5 * 60 * 1000L
}
