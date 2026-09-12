package com.vortex.a3.core.clipboard

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log

/** A file the phone is sending to the laptop (bytes + display name + MIME). */
data class ClipboardOutgoingFile(val bytes: ByteArray, val name: String, val mime: String)

/**
 * Reads an arbitrary clipboard / shared `content://` URI into a
 * [ClipboardOutgoingFile] for phone→laptop FILE sync. Used by both the Quick
 * Settings quick-send and the share-sheet target. Returns null if it isn't
 * readable or exceeds the LAN size cap.
 */
object ClipboardFileReader {
    /** Mirrors the Rust `clipboard_mirror::MAX_FILE_BYTES`. */
    const val MAX_FILE_BYTES = 64L * 1024 * 1024

    private const val TAG = "ClipboardFileOut"

    /** Outcome of a read, so the caller can tell the user something true
     *  instead of a generic "couldn't read the shared file". */
    sealed class Outcome {
        data class Ok(val file: ClipboardOutgoingFile) : Outcome()
        /** Bigger than [MAX_FILE_BYTES]; [bytes] is the best size we know. */
        data class TooLarge(val bytes: Long) : Outcome()
        /** Unreadable, empty, or it would not fit in memory. */
        data class Unreadable(val why: String) : Outcome()
    }

    /**
     * Read [uri] into memory, or explain why not.
     *
     * **The size is checked BEFORE the bytes are read.** It used to be checked
     * after `readBytes()`, which made the guard unreachable for exactly the
     * files it existed to stop: an 835 MB share allocated 876 MB against a
     * 256 MB heap growth limit and threw `OutOfMemoryError` at the read. That
     * is an `Error`, not an `Exception`, so the old `catch (e: Exception)` did
     * not catch it — it escaped `ShareReceiverActivity.onCreate` and killed the
     * whole process, taking the BLE/LAN service down with it. The user saw a
     * crash and no explanation.
     *
     * `OutOfMemoryError` is still caught below, because a pre-check can only
     * use the size the provider *reports*: `OpenableColumns.SIZE` is absent or
     * -1 for plenty of providers, and a wrong one must not be able to kill the
     * app either.
     */
    /** The file, or null if it could not be read or was over the cap.
     *
     *  For callers with nowhere to put the reason — a MediaStore auto-send, a
     *  file-browser fetch. Anything facing the user should call [read] and say
     *  which of the two it was: "too large" and "unreadable" are different
     *  problems and only one of them is the user's to fix. */
    fun readOrNull(context: Context, uri: Uri): ClipboardOutgoingFile? =
        (read(context, uri) as? Outcome.Ok)?.file

    fun read(context: Context, uri: Uri): Outcome {
        val cr = context.contentResolver
        val mime = cr.getType(uri) ?: "application/octet-stream"
        val name = displayName(context, uri) ?: "file"

        // Pre-flight: refuse before allocating anything. A negative or absent
        // size means "provider doesn't know" — fall through and let the
        // bounded read below decide.
        val reported = reportedSize(context, uri)
        if (reported > MAX_FILE_BYTES) {
            Log.i(TAG, "file too large ($reported bytes > $MAX_FILE_BYTES) — not sent")
            return Outcome.TooLarge(reported)
        }

        return try {
            // Bounded even when the provider lied about (or omitted) the size:
            // read at most the cap + 1 byte, so an oversized stream is detected
            // without ever buffering it whole.
            val bytes = cr.openInputStream(uri)?.use { it.readAtMost(MAX_FILE_BYTES + 1) }
            when {
                bytes == null -> Outcome.Unreadable("no input stream")
                bytes.isEmpty() -> Outcome.Unreadable("empty file")
                bytes.size > MAX_FILE_BYTES -> {
                    Log.i(TAG, "file exceeds cap (provider reported $reported) — not sent")
                    Outcome.TooLarge(maxOf(reported, bytes.size.toLong()))
                }
                else -> Outcome.Ok(ClipboardOutgoingFile(bytes, name, mime))
            }
        } catch (e: OutOfMemoryError) {
            // Reachable only when the reported size was wrong/absent. Catching
            // an Error is deliberate and narrow: the alternative is the process
            // dying and every Vortex feature with it.
            Log.w(TAG, "file read ran out of memory: ${e.message}")
            Outcome.Unreadable("too large to buffer")
        } catch (e: Exception) {
            Log.w(TAG, "file read failed: ${e.message}")
            Outcome.Unreadable(e.message ?: "read failed")
        }
    }

    /** `OpenableColumns.SIZE`, or -1 when the provider does not report one. */
    private fun reportedSize(context: Context, uri: Uri): Long = try {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)
            ?.use { c ->
                val idx = c.getColumnIndex(OpenableColumns.SIZE)
                if (c.moveToFirst() && idx >= 0 && !c.isNull(idx)) c.getLong(idx) else -1L
            } ?: -1L
    } catch (_: Exception) {
        -1L
    }

    /** Read at most [limit] bytes. Unlike `readBytes()` this never allocates
     *  more than the caller is prepared to accept. */
    private fun java.io.InputStream.readAtMost(limit: Long): ByteArray {
        val cap = limit.coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
        val out = java.io.ByteArrayOutputStream(minOf(cap, 64 * 1024))
        val buf = ByteArray(64 * 1024)
        var total = 0
        while (total < cap) {
            val n = read(buf, 0, minOf(buf.size, cap - total))
            if (n <= 0) break
            out.write(buf, 0, n)
            total += n
        }
        return out.toByteArray()
    }

    private fun displayName(context: Context, uri: Uri): String? = try {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { c ->
                if (c.moveToFirst()) {
                    val idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (idx >= 0) c.getString(idx) else null
                } else {
                    null
                }
            }
            ?: uri.lastPathSegment
    } catch (_: Exception) {
        uri.lastPathSegment
    }
}
