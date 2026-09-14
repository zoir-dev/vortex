package com.vortex.a3.core.clipboard

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log

/** A file the phone is sending to the laptop. */
data class ClipboardOutgoingFile(
    /** What to read when the laptop pulls it. Held as a URI, never as bytes:
     *  the file is streamed in ranges on demand, so its size no longer has to
     *  fit in the heap. */
    val uri: Uri,
    val name: String,
    val mime: String,
    /** Best known size, or -1 when the provider will not say. Advisory only —
     *  the open is what decides. */
    val size: Long,
)

/**
 * Describes an arbitrary clipboard / shared `content://` URI for phone→laptop
 * FILE sync. Used by both the Quick Settings quick-send and the share-sheet
 * target.
 *
 * It no longer READS the file. The old version buffered the whole thing to
 * compute a content hash for the token and to hand the bytes to the offer path,
 * which is what made an 835 MB share allocate 876 MB against a 256 MB heap
 * growth limit and throw `OutOfMemoryError` — an `Error`, so the surrounding
 * `catch (Exception)` missed it and the process died, taking the BLE/LAN
 * service with it. The 64 MB cap existed to keep that from happening.
 *
 * Now the laptop pulls the file through the ranged-read protocol
 * ([com.vortex.a3.core.fs.FsServer]), one bounded chunk at a time, so nothing
 * on either side holds more than a chunk and the cap is gone.
 */
object ClipboardFileReader {

    private const val TAG = "ClipboardFileOut"

    /** Outcome of a describe, so the caller can tell the user something true
     *  instead of a generic "couldn't read the shared file". */
    sealed class Outcome {
        data class Ok(val file: ClipboardOutgoingFile) : Outcome()
        /** Unreadable or empty. There is no longer a "too large". */
        data class Unreadable(val why: String) : Outcome()
    }

    /**
     * Describe [uri] without reading it, or explain why it cannot be sent.
     *
     * The only I/O here is opening the stream briefly to prove it is readable.
     * Discovering at pull time that a file was never readable would mean the
     * user sees a share succeed and a transfer fail minutes later, so the cheap
     * check is worth one open.
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
        val size = reportedSize(context, uri)

        return try {
            val readable = cr.openFileDescriptor(uri, "r")?.use { pfd ->
                // A zero-length file is not worth a transfer, and an empty
                // provider read is the usual symptom of a URI we cannot really
                // open. `statSize` is -1 when the provider will not say, which
                // is not itself a failure.
                val st = try { pfd.statSize } catch (_: Exception) { -1L }
                st != 0L
            } ?: return Outcome.Unreadable("no file descriptor")
            if (!readable) return Outcome.Unreadable("empty file")
            Outcome.Ok(ClipboardOutgoingFile(uri, name, mime, size))
        } catch (e: Exception) {
            Log.w(TAG, "file not readable: ${e.message}")
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
