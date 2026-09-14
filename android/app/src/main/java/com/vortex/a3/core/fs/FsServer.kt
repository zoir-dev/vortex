package com.vortex.a3.core.fs

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.os.ParcelFileDescriptor
import android.util.Log
import java.io.File
import org.json.JSONObject

/**
 * Serves the ranged-filesystem protocol against this phone's SAF-granted
 * folders. The phone half of what Rust `fs_server` does for the laptop, and
 * deliberately the same shape so the two can be read side by side.
 *
 * [FsProto] is the wire format, [FsRoots] is the policy gate, and this is the
 * I/O. Everything here is synchronous and must run off the main thread: reads
 * are bounded to [MAX_READ_LEN] so no single call is long, but a document
 * provider backed by a cloud account can stall arbitrarily and must never block
 * the BLE callback thread.
 */
class FsServer(
    private val context: Context,
    private val roots: FsRoots,
    private val handles: FsHandles,
) {

    /**
     * Called with a share token when the peer CLOSEs a share-sheet file it was
     * reading — the one unambiguous "the laptop has the bytes" moment on this
     * device, and what advances the share queue's progress.
     *
     * The old transfer got this from having just written the whole file onto
     * the socket. A ranged pull has no such moment, so CLOSE stands in for it.
     */
    @Volatile
    var onShareDelivered: (token: String) -> Unit = {}

    /** What one served op produced. The caller turns this into frames — this
     *  class knows nothing about framing or transports. */
    sealed class Served {
        data class Meta(val reply: FsReply) : Served()
        data class Data(val bytes: ByteArray) : Served()
        data class Err(val err: FsErr) : Served()
    }

    private fun err(id: Int, code: Int, msg: String) = Served.Err(FsErr(id, code, msg))

    /**
     * Serve one request.
     *
     * Every branch answers. An op we do not implement is refused explicitly,
     * never dropped: on the far side a silent op is indistinguishable from a
     * dead link, and a file manager blocked on it hangs rather than reporting
     * anything the user can act on.
     */
    fun serve(op: Byte, payload: ByteArray): Served {
        val json = { JSONObject(String(payload, Charsets.UTF_8)) }
        return try {
            when (op) {
                FsOp.LIST -> doList(ListReq.from(json()))
                FsOp.STAT -> doStat(StatReq.from(json()))
                FsOp.OPEN -> doOpen(OpenReq.from(json()))
                FsOp.READ -> doRead(ReadReq.from(json()))
                FsOp.CLOSE -> {
                    val r = CloseReq.from(json())
                    handles.remove(r.handle)?.let { token ->
                        ShareGrants.revoke(token)
                        try {
                            onShareDelivered(token)
                        } catch (e: Exception) {
                            Log.w(TAG, "onShareDelivered threw: ${e.message}")
                        }
                    }
                    // Not BADF for an unknown handle: we expire handles
                    // ourselves, so "already gone" is the state the caller
                    // asked for.
                    Served.Meta(FsReply.Ok(r.id))
                }
                FsOp.WRITE, FsOp.SETMETA -> {
                    // Read-only in v1 (design doc §3). Defined, wired, and
                    // honestly refused.
                    val id = try {
                        decodeWrite(payload)?.first?.id ?: json().optInt("id")
                    } catch (_: Exception) {
                        0
                    }
                    err(id, FsCode.NOTSUP, "this device serves read-only")
                }
                else -> {
                    Log.d(TAG, "fs: unsupported op 0x${"%02x".format(op)}")
                    err(0, FsCode.NOTSUP, "unsupported op")
                }
            }
        } catch (e: Exception) {
            // A malformed request cannot be answered against its own id (we
            // may have failed before reading one), so id 0 — which the
            // protocol reserves for "no particular request".
            Log.w(TAG, "fs: malformed op 0x${"%02x".format(op)}: ${e.message}")
            err(0, FsCode.INVAL, "malformed request")
        }
    }

    // -----------------------------------------------------------------------
    // Ops
    // -----------------------------------------------------------------------

    private fun doList(r: ListReq): Served {
        // The empty path is the synthetic root: it lists the granted folders
        // themselves, so the laptop discovers what it may see instead of being
        // told out of band. Same convention as the Rust server.
        if (r.path.isEmpty() || r.path == "/") {
            val all = roots.roots()
            if (all.size != 1) {
                return Served.Meta(
                    FsReply.ListPage(
                        r.id,
                        all.map {
                            FsEntry(
                                name = it.name,
                                isDir = true,
                                readonly = !it.writable,
                                path = rootPath(it),
                            )
                        },
                        cursor = null,
                    ),
                )
            }
            // With exactly one folder, a synthetic level above it would be a
            // directory the user clicks through every time for no information.
            return when (val only = all[0]) {
                is FsRoots.Root.Local -> listLocal(r.id, only.dir, r.cursor)
                is FsRoots.Root.Tree ->
                    listChildren(
                        r.id,
                        treeRootUri(only.treeUri) ?: return err(r.id, FsCode.IO, "bad tree"),
                        r.cursor,
                    )
            }
        }
        return when (val res = roots.resolve(r.path, forWrite = false)) {
            is FsRoots.Result.Err -> err(r.id, res.code, "refused")
            is FsRoots.Result.Ok -> when (val t = res.target) {
                is FsRoots.Target.Doc -> listChildren(r.id, t.uri, r.cursor)
                is FsRoots.Target.Local -> listLocal(r.id, t.file, r.cursor)
                // A shared file is one file, by construction.
                is FsRoots.Target.Shared -> err(r.id, FsCode.INVAL, "not a directory")
            }
        }
    }

    /** Directory listing over the all-files root. */
    private fun listLocal(id: Int, dir: File, cursor: Int): Served {
        if (!dir.isDirectory) return err(id, FsCode.INVAL, "not a directory")
        // Sorted so paging is stable: listFiles has no defined order, and an
        // unstable one would drop or repeat entries across pages.
        val all = try {
            dir.listFiles()?.sortedBy { it.name } ?: return err(id, FsCode.IO, "cannot list")
        } catch (e: SecurityException) {
            return err(id, FsCode.ACCES, "not granted")
        } catch (e: Exception) {
            return err(id, FsCode.IO, e.message ?: "list failed")
        }
        val page = all.drop(cursor).take(LIST_PAGE)
        val next = if (cursor + page.size < all.size) cursor + page.size else null
        return Served.Meta(FsReply.ListPage(id, page.map { localEntry(it) }, next))
    }

    private fun listChildren(id: Int, dirUri: Uri, cursor: Int): Served {
        val docId = try {
            if (DocumentsContract.isDocumentUri(context, dirUri)) {
                DocumentsContract.getDocumentId(dirUri)
            } else {
                DocumentsContract.getTreeDocumentId(dirUri)
            }
        } catch (e: Exception) {
            return err(id, FsCode.INVAL, "not a document: ${e.message}")
        }
        val childrenUri = try {
            DocumentsContract.buildChildDocumentsUriUsingTree(dirUri, docId)
        } catch (e: Exception) {
            return err(id, FsCode.INVAL, "cannot address children: ${e.message}")
        }

        val entries = ArrayList<FsEntry>()
        var next: Int? = null
        try {
            context.contentResolver.query(childrenUri, PROJECTION, null, null, null)?.use { c ->
                // Skip to the cursor. SAF has no offset query, so paging means
                // re-walking — acceptable because pages are large and deep
                // paging is rare, and it keeps a 10,000-entry folder off a
                // single frame either way.
                if (cursor > 0 && !c.moveToPosition(cursor - 1)) return@use
                while (c.moveToNext()) {
                    if (entries.size >= LIST_PAGE) {
                        // Non-null cursor always means "call again" — never a
                        // guess, so the consumer can trust it as a terminator.
                        next = cursor + entries.size
                        break
                    }
                    entries.add(entryOf(c, dirUri))
                }
            } ?: return err(id, FsCode.IO, "provider returned no cursor")
        } catch (e: SecurityException) {
            return err(id, FsCode.ACCES, "not granted")
        } catch (e: Exception) {
            return err(id, FsCode.IO, e.message ?: "query failed")
        }
        return Served.Meta(FsReply.ListPage(id, entries, next))
    }

    private fun doStat(r: StatReq): Served {
        if (r.path.isEmpty() || r.path == "/") {
            // The synthetic root is a directory that exists but has no
            // document behind it; answer without touching a provider.
            return Served.Meta(FsReply.Stat(r.id, FsEntry(name = "/", isDir = true, readonly = true, path = "/")))
        }
        return when (val res = roots.resolve(r.path, forWrite = false)) {
            is FsRoots.Result.Err -> err(r.id, res.code, "refused")
            is FsRoots.Result.Ok -> when (val t = res.target) {
                is FsRoots.Target.Shared -> Served.Meta(
                    FsReply.Stat(
                        r.id,
                        FsEntry(name = t.name, isDir = false, size = t.size, readonly = true, path = r.path),
                    ),
                )
                is FsRoots.Target.Local ->
                    if (!t.file.exists()) err(r.id, FsCode.NOENT, "no such file")
                    else Served.Meta(FsReply.Stat(r.id, localEntry(t.file)))
                is FsRoots.Target.Doc -> {
                    val docUri = asDocumentUri(t.uri)
                        ?: return err(r.id, FsCode.INVAL, "not a document")
                    try {
                        context.contentResolver.query(docUri, PROJECTION, null, null, null)?.use { c ->
                            if (!c.moveToFirst()) return err(r.id, FsCode.NOENT, "no such document")
                            Served.Meta(FsReply.Stat(r.id, entryOf(c, t.uri)))
                        } ?: err(r.id, FsCode.NOENT, "no such document")
                    } catch (e: SecurityException) {
                        err(r.id, FsCode.ACCES, "not granted")
                    } catch (e: Exception) {
                        err(r.id, FsCode.IO, e.message ?: "stat failed")
                    }
                }
            }
        }
    }

    private fun doOpen(r: OpenReq): Served {
        if (r.write) return err(r.id, FsCode.ROFS, "this device serves read-only")
        val target = when (val res = roots.resolve(r.path, forWrite = false)) {
            is FsRoots.Result.Err -> return err(r.id, res.code, "refused")
            is FsRoots.Result.Ok -> res.target
        }
        return when (target) {
            is FsRoots.Target.Local -> openLocal(r.id, target.file)
            is FsRoots.Target.Doc -> openDoc(r.id, target.uri)
            is FsRoots.Target.Shared ->
                openShared(r.id, target.uri, target.size, target.token)
        }
    }

    /**
     * Open a share-sheet file. Straight to the resolver: no document query
     * first, because the URI may be a MediaStore or FileProvider one that
     * answers none of the Document columns.
     */
    private fun openShared(id: Int, uri: Uri, declaredSize: Long, token: String): Served {
        val pfd = try {
            context.contentResolver.openFileDescriptor(uri, "r")
        } catch (e: SecurityException) {
            // The one-off grant the share gave us has lapsed — Android drops it
            // when the sharing task finishes.
            return err(id, FsCode.ACCES, "share permission expired")
        } catch (e: java.io.FileNotFoundException) {
            return err(id, FsCode.NOENT, "shared file is gone")
        } catch (e: Exception) {
            return err(id, FsCode.IO, e.message ?: "open failed")
        } ?: return err(id, FsCode.IO, "provider returned no descriptor")
        // Prefer what the descriptor says over what the provider claimed at
        // share time: statSize is the length we will actually be able to read.
        val size = try { pfd.statSize.coerceAtLeast(0) } catch (_: Exception) { 0 }
        return finishOpen(id, pfd, if (size > 0) size else declaredSize.coerceAtLeast(0), token)
    }

    private fun openLocal(id: Int, f: File): Served {
        if (f.isDirectory) return err(id, FsCode.ISDIR, "is a directory")
        if (!f.exists()) return err(id, FsCode.NOENT, "no such file")
        val pfd = try {
            ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY)
        } catch (e: SecurityException) {
            return err(id, FsCode.ACCES, "not granted")
        } catch (e: java.io.FileNotFoundException) {
            return err(id, FsCode.NOENT, "no such file")
        } catch (e: Exception) {
            return err(id, FsCode.IO, e.message ?: "open failed")
        }
        return finishOpen(id, pfd, f.length())
    }

    private fun openDoc(id: Int, uri: Uri): Served {
        val docUri = asDocumentUri(uri) ?: return err(id, FsCode.INVAL, "not a document")
        var size = 0L
        try {
            context.contentResolver.query(docUri, PROJECTION, null, null, null)?.use { c ->
                if (c.moveToFirst()) {
                    if (isDir(c)) return err(id, FsCode.ISDIR, "is a directory")
                    size = c.getLong(IDX_SIZE)
                }
            }
        } catch (_: Exception) {
            // Size is advisory — the open below is the real test.
        }
        val pfd = try {
            context.contentResolver.openFileDescriptor(docUri, "r")
        } catch (e: SecurityException) {
            return err(id, FsCode.ACCES, "not granted")
        } catch (e: java.io.FileNotFoundException) {
            return err(id, FsCode.NOENT, "no such document")
        } catch (e: Exception) {
            return err(id, FsCode.IO, e.message ?: "open failed")
        } ?: return err(id, FsCode.IO, "provider returned no descriptor")

        if (size <= 0) size = try { pfd.statSize.coerceAtLeast(0) } catch (_: Exception) { 0 }
        return finishOpen(id, pfd, size)
    }

    private fun finishOpen(
        id: Int,
        pfd: ParcelFileDescriptor,
        size: Long,
        shareToken: String? = null,
    ): Served {
        val handle = handles.insert(pfd, size, shareToken)
        if (handle == null) {
            // Close what we just opened: refusing the request must not also
            // leak the descriptor that made us refuse it.
            try { pfd.close() } catch (_: Exception) {}
            return err(id, FsCode.IO, "too many open handles")
        }
        return Served.Meta(FsReply.Open(id, handle, size, readonly = true))
    }

    private fun doRead(r: ReadReq): Served {
        if (r.len < 0 || r.offset < 0) return err(r.id, FsCode.INVAL, "negative read")
        val (pfd, size) = handles.get(r.handle)
            ?: return err(r.id, FsCode.BADF, "unknown or expired handle")

        val want = minOf(r.len, MAX_READ_LEN)
        if (want == 0) return Served.Data(encodeData(r.id, r.offset, eof = r.offset >= size, ByteArray(0)))
        if (size > 0 && r.offset >= size) {
            // Reading at or past the end is a normal way to discover EOF, not
            // an error — answer with an empty, EOF-flagged frame.
            return Served.Data(encodeData(r.id, r.offset, eof = true, ByteArray(0)))
        }

        val buf = ByteArray(want)
        val n = try {
            // Positional read: pread does not disturb a shared file offset, so
            // concurrent ranged reads on one handle cannot interleave into each
            // other's bytes. A thumbnailer firing parallel reads makes that a
            // real case, not a theoretical one.
            Os.pread(pfd.fileDescriptor, buf, 0, want, r.offset)
        } catch (e: ErrnoException) {
            return if (e.errno == OsConstants.ESPIPE) {
                // Some providers (cloud-backed documents) hand back a pipe,
                // which cannot seek. Honest refusal beats silently returning
                // the wrong bytes.
                err(r.id, FsCode.IO, "document is not seekable")
            } else {
                err(r.id, FsCode.IO, "pread: ${e.message}")
            }
        } catch (e: Exception) {
            return err(r.id, FsCode.IO, e.message ?: "read failed")
        }

        if (n <= 0) return Served.Data(encodeData(r.id, r.offset, eof = true, ByteArray(0)))
        val eof = if (size > 0) r.offset + n >= size else n < want
        return Served.Data(encodeData(r.id, r.offset, eof, buf, n))
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /** A tree URI addressed as the document it stands for, so children can be
     *  enumerated from it. */
    private fun treeRootUri(treeUri: Uri): Uri? = try {
        DocumentsContract.buildDocumentUriUsingTree(
            treeUri,
            DocumentsContract.getTreeDocumentId(treeUri),
        )
    } catch (_: Exception) {
        null
    }

    /** The address a peer should send back to enter this root. A document URI
     *  for a SAF tree; an ordinary absolute path for the all-files root. */
    private fun rootPath(root: FsRoots.Root): String = when (root) {
        is FsRoots.Root.Tree -> (treeRootUri(root.treeUri) ?: root.treeUri).toString()
        is FsRoots.Root.Local -> root.dir.absolutePath
    }

    private fun localEntry(f: File): FsEntry = FsEntry(
        name = f.name,
        isDir = f.isDirectory,
        size = if (f.isDirectory) 0 else f.length(),
        // The protocol carries seconds; File reports milliseconds.
        mtime = f.lastModified() / 1000,
        readonly = true,
        path = f.absolutePath,
    )

    /** Peer-supplied URIs are already tree-document URIs (we only ever emit
     *  those), but a bare tree URI is accepted too so the laptop can address a
     *  root by the path it was given. */
    private fun asDocumentUri(uri: Uri): Uri? =
        if (DocumentsContract.isDocumentUri(context, uri)) uri else treeRootUri(uri)

    private fun isDir(c: android.database.Cursor): Boolean =
        c.getString(IDX_MIME) == DocumentsContract.Document.MIME_TYPE_DIR

    private fun entryOf(c: android.database.Cursor, parent: Uri): FsEntry {
        val docId = c.getString(IDX_ID)
        val dir = isDir(c)
        return FsEntry(
            name = c.getString(IDX_NAME) ?: docId ?: "?",
            isDir = dir,
            size = if (dir) 0 else c.getLong(IDX_SIZE),
            // The protocol carries seconds; SAF reports milliseconds.
            mtime = c.getLong(IDX_MTIME) / 1000,
            readonly = true,
            path = try {
                DocumentsContract.buildDocumentUriUsingTree(parent, docId).toString()
            } catch (_: Exception) {
                ""
            },
        )
    }

    companion object {
        private const val TAG = "VortexFs"

        private val PROJECTION = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_SIZE,
            DocumentsContract.Document.COLUMN_LAST_MODIFIED,
        )
        private const val IDX_ID = 0
        private const val IDX_NAME = 1
        private const val IDX_MIME = 2
        private const val IDX_SIZE = 3
        private const val IDX_MTIME = 4
    }
}
