package com.vortex.a3.core.fs

import android.net.Uri
import android.util.Log

/**
 * Files the user has explicitly shared with Vortex, addressable by an opaque
 * token so the laptop can pull them through the ranged-read protocol.
 *
 * This exists because a shared file is authorised differently from a browsed
 * one. Browsing is gated by [FsRoots]: a SAF tree the user picked, or all-files
 * access. A share-sheet file is neither — it arrives as a one-off URI grant to
 * this process, and the *act of sharing* is the authorisation. So it gets its
 * own, deliberately narrow gate: exactly the files the user sent, addressed by
 * a token they cannot be guessed from, and nothing else.
 *
 * Tokens are random rather than a content hash. The old store keyed blobs by
 * sha256 of the bytes, which meant hashing — and therefore reading — the whole
 * file before it could be offered. That is the buffering this change removes,
 * so the token cannot depend on the content.
 */
object ShareGrants {

    /** One shared file: what to read, and what to call it. */
    data class Grant(val uri: Uri, val name: String, val mime: String, val size: Long)

    /**
     * How many shares stay addressable. Matches the old blob store's ceiling,
     * and for the same reason: the laptop pulls one file at a time, so a grant
     * evicted before its turn is a file that silently never arrives. Callers
     * cap a batch at this (see ShareReceiverActivity.MAX_SHARE_FILES).
     *
     * Unlike the old store, holding this many costs a URI each rather than a
     * file each — 32 entries used to mean up to 2 GB of heap.
     */
    const val MAX_ENTRIES = 32

    private const val TAG = "VortexFs"

    // Insertion-ordered so eviction drops the oldest first.
    private val grants = LinkedHashMap<String, Grant>()

    /** Register [uri] as shared; returns the token the laptop pulls it by. */
    @Synchronized
    fun grant(uri: Uri, name: String, mime: String, size: Long): String {
        val token = randomToken()
        grants[token] = Grant(uri, name, mime, size)
        while (grants.size > MAX_ENTRIES) {
            val oldest = grants.keys.iterator().next()
            grants.remove(oldest)
        }
        return token
    }

    /** The grant for [token], or null when unknown or evicted. */
    @Synchronized
    fun get(token: String): Grant? = if (token.isEmpty()) null else grants[token]

    /** Forget a grant once the laptop has the file. */
    @Synchronized
    fun revoke(token: String) {
        if (grants.remove(token) != null) Log.i(TAG, "share grant spent")
    }

    @Synchronized
    fun size(): Int = grants.size

    /**
     * 16 bytes of randomness, hex. Unguessable on purpose: this token is the
     * only thing standing between a paired laptop and a file the user shared
     * with it, and the peer supplies it verbatim.
     */
    private fun randomToken(): String {
        val b = ByteArray(16)
        java.security.SecureRandom().nextBytes(b)
        return b.joinToString("") { "%02x".format(it) }
    }
}
