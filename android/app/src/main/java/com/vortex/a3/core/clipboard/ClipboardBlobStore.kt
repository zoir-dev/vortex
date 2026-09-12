package com.vortex.a3.core.clipboard

import java.security.MessageDigest

/**
 * Holds recent outgoing FILE blobs (instant-share style) keyed by a content
 * token, so MULTIPLE files selected in one share all survive until the laptop
 * pulls each over LAN. Unlike [ClipboardImageStore] (single most-recent image),
 * this keeps several; the oldest is evicted past [MAX_ENTRIES].
 *
 * Each entry is a SOURCE of bytes, not always the bytes. A shared file is held
 * in memory as before — it came in as bytes and a re-share of it is free. A
 * picture the gallery watcher sends by itself is held as its `content://` URI
 * and read again when the laptop comes for it: with photos going out
 * unprompted, 32 held JPEGs at 5 MB each is a heap this service cannot carry
 * on a 4 GB MIUI phone, and the file is on disk anyway. The token is the hash
 * of the bytes either way (the laptop dedups on it), so a media entry is
 * hashed once from a read that is then dropped.
 */
object ClipboardBlobStore {
    /**
     * Blobs kept before the oldest is evicted — and therefore the hard ceiling
     * on how many files ONE share can deliver: the laptop pulls them one at a
     * time over LAN, so a blob evicted before its turn is a file that silently
     * never arrives. Callers cap their batch at this, so the two numbers cannot
     * drift apart (see ShareReceiverActivity.MAX_SHARE_FILES).
     */
    const val MAX_ENTRIES = 32

    // Insertion-ordered so eviction drops the oldest first.
    private val blobs = LinkedHashMap<String, () -> ByteArray?>()

    /** Stash [bytes]; returns the content token the laptop pulls it by. */
    @Synchronized
    fun stash(bytes: ByteArray): String = put(sha256Hex(bytes).take(16)) { bytes }

    /** Stash a blob by its already-hashed [bytes] but keep only [reload], which
     *  re-reads it on demand. Returns the token. [reload] returning null (the
     *  picture was deleted meanwhile) makes the pull a "nomatch", which the
     *  laptop already handles. */
    @Synchronized
    fun stashLazy(bytes: ByteArray, reload: () -> ByteArray?): String =
        put(sha256Hex(bytes).take(16), reload)

    private fun put(token: String, source: () -> ByteArray?): String {
        blobs.remove(token) // re-insert to refresh recency
        blobs[token] = source
        while (blobs.size > MAX_ENTRIES) {
            val oldest = blobs.keys.iterator().next()
            blobs.remove(oldest)
        }
        return token
    }

    /** The bytes for [token], or null if unknown/evicted (or, for a lazy
     *  entry, no longer readable). */
    fun getByToken(token: String): ByteArray? {
        if (token.isEmpty()) return null
        // Take the source under the lock, read it outside: a lazy read is a
        // multi-megabyte file read and must not hold up a concurrent stash.
        val source = synchronized(this) { blobs[token] } ?: return null
        return try { source() } catch (_: Exception) { null }
    }

    private fun sha256Hex(data: ByteArray): String {
        val d = MessageDigest.getInstance("SHA-256").digest(data)
        val sb = StringBuilder(d.size * 2)
        for (b in d) sb.append("%02x".format(b))
        return sb.toString()
    }
}
