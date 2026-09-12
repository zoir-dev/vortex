package com.vortex.a3.core.files

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * The folders on this phone the laptop may look inside, and the reading of
 * them.
 *
 * Browsing, deliberately, rather than another thing that copies itself. The
 * laptop asks for a folder, gets its entries, and nothing moves until someone
 * picks a file — which is also how iCloud Drive and Huawei's Multi-Screen
 * behave, and for the same reason: a phone's Download folder is mostly things
 * nobody wants a second copy of, and the laptop has no way to guess which few
 * are the exception. Automatic transfer is for what the phone GENERATES
 * (see [com.vortex.a3.core.media.CapturedMediaWatcher]); everything else is
 * asked for.
 *
 * **Access is granted per folder, by the user, once.** `ACTION_OPEN_DOCUMENT_TREE`
 * plus `takePersistableUriPermission`, so this reads exactly the folders that
 * were handed to it and nothing else. Deliberately not `MANAGE_EXTERNAL_STORAGE`:
 * "all files" is the wrong shape of permission to hold for a feature whose
 * whole job is to show a list, and Play restricts it heavily. Also deliberately
 * not `MediaStore.Downloads`, which under scoped storage returns only files
 * this app itself wrote — which is to say, nothing the user is looking for.
 *
 * The laptop addresses a file by its document URI. That is a capability, so
 * [resolve] refuses any URI that does not sit under a tree the user granted —
 * the peer is authenticated, but "authenticated" is not "may read any path it
 * can name".
 */
object PhoneFiles {
    private const val TAG = "PhoneFiles"

    /** Columns one listing row needs. */
    private val PROJECTION = arrayOf(
        DocumentsContract.Document.COLUMN_DOCUMENT_ID,
        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_MIME_TYPE,
        DocumentsContract.Document.COLUMN_SIZE,
        DocumentsContract.Document.COLUMN_LAST_MODIFIED,
    )

    /** Entries per listing. A Download folder with thousands of files must not
     *  produce one enormous frame; the laptop shows what it gets. */
    private const val MAX_ENTRIES = 500

    /** The intent that asks the user for a folder. */
    fun pickFolderIntent(): Intent =
        Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
            addFlags(
                Intent.FLAG_GRANT_READ_URI_PERMISSION or
                    Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION,
            )
        }

    /** Keep [treeUri] readable across restarts. Called with the picker's result. */
    fun persistGrant(context: Context, treeUri: Uri) {
        try {
            context.contentResolver.takePersistableUriPermission(
                treeUri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION,
            )
            Log.i(TAG, "folder granted: ${treeUri.lastPathSegment}")
        } catch (e: Exception) {
            Log.w(TAG, "could not persist the folder grant: ${e.message}")
        }
    }

    /** Stop reading [treeUri]. */
    fun releaseGrant(context: Context, treeUri: Uri) {
        try {
            context.contentResolver.releasePersistableUriPermission(
                treeUri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION,
            )
        } catch (e: Exception) {
            Log.w(TAG, "could not release the folder grant: ${e.message}")
        }
    }

    /** Every folder the user has granted, newest first. */
    fun grantedTrees(context: Context): List<Uri> = try {
        context.contentResolver.persistedUriPermissions
            .filter { it.isReadPermission }
            .sortedByDescending { it.persistedTime }
            .map { it.uri }
    } catch (e: Exception) {
        Log.w(TAG, "could not read the folder grants: ${e.message}")
        emptyList()
    }

    /**
     * One folder's entries as JSON, or the granted roots when [docUri] is blank.
     *
     * Shape: `{"at": "<uri or empty>", "entries": [{id, name, mime, bytes,
     * modified, dir}]}`. `at` is echoed so a reply that overtakes another
     * cannot be mistaken for the folder the laptop is currently showing.
     */
    fun list(context: Context, docUri: String): ByteArray {
        val out = JSONObject().put("at", docUri)
        val entries = JSONArray()
        try {
            if (docUri.isBlank()) {
                for (tree in grantedTrees(context)) {
                    val id = DocumentsContract.getTreeDocumentId(tree)
                    entries.put(
                        JSONObject()
                            .put("id", DocumentsContract.buildDocumentUriUsingTree(tree, id).toString())
                            .put("name", prettyRootName(id))
                            .put("mime", DocumentsContract.Document.MIME_TYPE_DIR)
                            .put("bytes", 0)
                            .put("dir", true),
                    )
                }
            } else {
                val uri = resolve(context, docUri) ?: return refusal(docUri, "not a granted folder")
                val children = DocumentsContract.buildChildDocumentsUriUsingTree(
                    uri,
                    DocumentsContract.getDocumentId(uri),
                )
                context.contentResolver.query(children, PROJECTION, null, null, null)?.use { c ->
                    var n = 0
                    while (c.moveToNext() && n < MAX_ENTRIES) {
                        val id = c.getString(0) ?: continue
                        val mime = c.getString(2).orEmpty()
                        entries.put(
                            JSONObject()
                                .put("id", DocumentsContract.buildDocumentUriUsingTree(uri, id).toString())
                                .put("name", c.getString(1) ?: id)
                                .put("mime", mime)
                                .put("bytes", if (c.isNull(3)) 0L else c.getLong(3))
                                .put("modified", if (c.isNull(4)) 0L else c.getLong(4))
                                .put("dir", mime == DocumentsContract.Document.MIME_TYPE_DIR),
                        )
                        n++
                    }
                    if (n >= MAX_ENTRIES) out.put("truncated", true)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "listing failed: ${e.message}")
            return refusal(docUri, "could not read this folder")
        }
        return out.put("entries", entries).toString().toByteArray(Charsets.UTF_8)
    }

    /** A listing the laptop can show as an explanation instead of an empty folder. */
    private fun refusal(at: String, why: String): ByteArray =
        JSONObject().put("at", at).put("error", why).put("entries", JSONArray())
            .toString().toByteArray(Charsets.UTF_8)

    /**
     * The document URI for [raw], but only if it sits under a folder the user
     * granted. Null otherwise — and null is the right answer for a URI that
     * merely looks wrong, too: this is the only thing standing between "the
     * peer may browse the Download folder" and "the peer may name any path the
     * provider will open".
     */
    fun resolve(context: Context, raw: String): Uri? {
        val uri = try { Uri.parse(raw) } catch (_: Exception) { return null }
        val granted = grantedTrees(context)
        if (granted.isEmpty()) return null
        val treeId = try { DocumentsContract.getTreeDocumentId(uri) } catch (_: Exception) { null }
            ?: return null
        val ok = granted.any { tree ->
            tree.authority == uri.authority &&
                runCatching { DocumentsContract.getTreeDocumentId(tree) }.getOrNull() == treeId
        }
        if (!ok) {
            Log.w(TAG, "refused a document outside every granted folder")
            return null
        }
        return uri
    }

    /**
     * The bytes of [raw], subject to the same size cap every phone→laptop file
     * goes through, or null when it is not readable, not granted, or too big.
     */
    fun read(context: Context, raw: String): com.vortex.a3.core.clipboard.ClipboardOutgoingFile? {
        val uri = resolve(context, raw) ?: return null
        return com.vortex.a3.core.clipboard.ClipboardFileReader.readOrNull(context, uri)
    }

    /** `primary:Download` reads better as `Download`. */
    private fun prettyRootName(documentId: String): String =
        documentId.substringAfterLast(':').substringAfterLast('/').ifBlank { documentId }
}
