package com.vortex.a3.core.fs

import android.os.ParcelFileDescriptor
import android.util.Log

/**
 * Open read handles for one peer. Mirrors Rust `fs_server::FsHandles`, and for
 * the same reasons.
 *
 * Per-peer rather than global: with several paired laptops, one peer's handle
 * ids must not address another's open files. Multi-peer made "whose statement
 * is this" load-bearing throughout the codebase and a handle table is no
 * different.
 */
class FsHandles {

    private class Handle(
        val pfd: ParcelFileDescriptor,
        val size: Long,
        var lastUsed: Long,
        /** Set when this handle is reading a share-sheet file: its CLOSE is
         *  what tells the sender the laptop actually has the bytes. */
        val shareToken: String?,
    )

    private val lock = Any()
    private var next: Long = 0
    private val open = HashMap<Long, Handle>()

    /**
     * Register an open descriptor, returning its handle id, or `null` when we
     * are already holding as many as we will.
     *
     * A peer that opens in a loop and never closes must not be able to exhaust
     * the process's descriptors, so the table is bounded and idle entries are
     * pruned first.
     */
    fun insert(pfd: ParcelFileDescriptor, size: Long, shareToken: String? = null): Long? = synchronized(lock) {
        prune()
        if (open.size >= MAX_HANDLES) {
            Log.w(TAG, "fs: handle table full ($MAX_HANDLES); refusing OPEN")
            return null
        }
        // Start at 1: 0 is never valid, because it is the value a buggy
        // consumer is most likely to send by accident.
        next += 1
        if (next <= 0) next = 1
        val id = next
        open[id] = Handle(pfd, size, System.nanoTime(), shareToken)
        id
    }

    /** Look a handle up and mark it fresh, or `null` if unknown/expired. */
    fun get(id: Long): Pair<ParcelFileDescriptor, Long>? = synchronized(lock) {
        prune()
        val h = open[id] ?: return null
        h.lastUsed = System.nanoTime()
        Pair(h.pfd, h.size)
    }

    /**
     * Close and forget a handle, returning its share token if it had one.
     *
     * Only an EXPLICIT close reports a token — [prune] deliberately does not.
     * An expired handle means the reader went away mid-file, which is the
     * opposite of delivery, and counting it would tell the user a transfer
     * succeeded when it was abandoned.
     */
    fun remove(id: Long): String? = synchronized(lock) {
        val h = open.remove(id) ?: return null
        close(h)
        h.shareToken
    }

    /** Close everything. Called when the link drops: handles cannot outlive
     *  the session that owns them. */
    fun clear() = synchronized(lock) {
        open.values.forEach { close(it) }
        open.clear()
    }

    fun size(): Int = synchronized(lock) { open.size }

    /**
     * Drop handles nothing has touched for [IDLE_TIMEOUT_NS].
     *
     * A consumer that dies mid-copy — a file manager killed, a mount
     * unmounted — never sends CLOSE, and one leaked descriptor per abandoned
     * read would eventually exhaust us. Reopening is cheap; leaking is not.
     */
    private fun prune() {
        val now = System.nanoTime()
        val dead = open.filterValues { now - it.lastUsed > IDLE_TIMEOUT_NS }
        if (dead.isEmpty()) return
        dead.forEach { (id, h) ->
            Log.i(TAG, "fs: expiring idle handle $id")
            close(h)
            open.remove(id)
        }
    }

    private fun close(h: Handle) {
        try {
            h.pfd.close()
        } catch (_: Exception) {
            // Closing twice, or after the provider died, is not worth a log
            // line — the descriptor is gone either way.
        }
    }

    companion object {
        private const val TAG = "VortexFs"
        private const val MAX_HANDLES = 64
        private const val IDLE_TIMEOUT_NS = 300_000_000_000L // 300 s
    }
}
