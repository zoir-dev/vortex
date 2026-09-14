package com.vortex.a3.core.fs

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Environment
import android.provider.DocumentsContract
import android.util.Log
import java.io.File

/**
 * What this phone serves to a paired laptop, and the gate every path-taking op
 * passes through.
 *
 * The counterpart of Rust `fs_proto::Roots`, with one structural difference the
 * platform forces: on Linux a root is a filesystem path and the allowlist lives
 * in a config file, whereas here a root is a **SAF tree the user picked** and
 * the allowlist IS the set of persisted URI permissions Android already holds
 * for us. Keeping the grant as the single source of truth means there is no way
 * for our own bookkeeping to drift from what the OS will actually let us read —
 * a second list could only ever be wrong in the dangerous direction.
 *
 * Per design doc §5, SAF trees are the default and the alarming
 * `MANAGE_EXTERNAL_STORAGE` all-files permission is deliberately NOT requested:
 * a paired phone is not thereby a phone that has handed over its whole disk.
 *
 * # Why paths are opaque URIs
 *
 * The protocol's `path` is a string the consumer only ever echoes back — it
 * addresses children by the `path` in the [FsEntry] we sent. So a document URI
 * serves directly as the path, and the phone needs no virtual namespace and no
 * URI↔path map to keep in sync. The laptop shows `name`; it never has to parse
 * `path`.
 */
class FsRoots(private val context: Context) {

    /**
     * One shared location.
     *
     * Two kinds, because the two grant models are genuinely different: a SAF
     * tree is addressed by document URI and enumerated through a provider,
     * while all-files access hands back the ordinary filesystem. Modelling them
     * as one and converting would mean inventing a fake URI for real paths, or
     * vice versa — both lossy.
     */
    sealed class Root {
        abstract val name: String
        abstract val writable: Boolean

        data class Tree(
            val treeUri: Uri,
            override val name: String,
            override val writable: Boolean,
        ) : Root()

        /** The whole of shared storage, available only while the user has
         *  granted all-files access. */
        data class Local(
            val dir: File,
            override val name: String,
            override val writable: Boolean,
        ) : Root()
    }

    /** What a resolved path turned out to address. */
    sealed class Target {
        data class Doc(val uri: Uri) : Target()
        data class Local(val file: File) : Target()
        /**
         * A share-sheet file. Kept apart from [Doc] because it is usually NOT
         * a SAF document URI at all — a share commonly hands over a MediaStore
         * or FileProvider URI, which has no document id and cannot be asked
         * for children. Only opening and stat'ing it makes sense.
         */
        data class Shared(
            val uri: Uri,
            val name: String,
            val size: Long,
            /** Echoed back on CLOSE so the sender learns the file landed. */
            val token: String,
        ) : Target()
    }

    /**
     * Whether the user has granted all-files access.
     *
     * Read from the OS, never cached and never mirrored into a preference of
     * our own: the user can revoke it in system settings at any time, and a
     * stale "on" would mean offering the laptop a root we can no longer read.
     * The permission IS the setting — same rule as the SAF grants above.
     */
    fun allFilesGranted(): Boolean = try {
        Environment.isExternalStorageManager()
    } catch (_: Exception) {
        false
    }

    /**
     * The trees the user has granted us, newest first.
     *
     * Read live from the OS on every call rather than cached: a user can revoke
     * a grant in system settings at any moment, and a cache would keep serving
     * a folder that has actually been withdrawn.
     */
    fun roots(): List<Root> {
        val out = ArrayList<Root>()
        // All-files access supersedes the individual trees: offering both would
        // show the same photo twice under two different paths, and the laptop
        // has no way to know they are the same file.
        if (allFilesGranted()) {
            val shared = try {
                @Suppress("DEPRECATION")
                Environment.getExternalStorageDirectory()
            } catch (_: Exception) {
                null
            }
            if (shared != null && shared.isDirectory) {
                out.add(Root.Local(shared, "Phone storage", writable = false))
                return out
            }
        }
        context.contentResolver.persistedUriPermissions
            .filter { it.isReadPermission }
            .forEach { perm ->
                val uri = perm.uri
                // Only tree grants: a single-document grant cannot be browsed
                // and has no children to enumerate.
                if (!DocumentsContract.isTreeUri(uri)) return@forEach
                out.add(
                    Root.Tree(
                        treeUri = uri,
                        name = displayNameOf(uri),
                        // v1 is read-only end to end; the flag is carried so the
                        // laptop can show the folder as read-only rather than
                        // discovering it by failing a write.
                        writable = false,
                    ),
                )
            }
        return out
    }

    fun isEmpty(): Boolean = roots().isEmpty()

    /**
     * Resolve a peer-supplied path to a document URI, or refuse it.
     *
     * This is the only place a peer-supplied string becomes something we will
     * open, so it is the whole security boundary. A paired laptop must not be
     * able to hand us an arbitrary `content://` URI and have us read it on its
     * behalf — that would make this app a confused deputy for every provider it
     * can reach, including other apps' and MediaStore's.
     *
     * The gate is the SAF tree id: a URI is acceptable only when it carries the
     * same authority AND the same tree id as a grant we actually hold. That is
     * exactly the boundary Android itself keys permissions on, so we can never
     * reach past what the user picked. Containment *within* the tree is then
     * enforced by the provider — a document id that is not really a child fails
     * with SecurityException, which surfaces as [FsCode.ACCES].
     */
    fun resolve(path: String, forWrite: Boolean): Result {
        if (path.isEmpty()) return Result.Err(FsCode.INVAL)
        // An absolute path means the all-files root; anything else must be a
        // content URI. Dispatching on the first character rather than trying
        // both keeps the two gates separate, so neither can be reached by a
        // path shaped for the other.
        // A file the user shared through the share sheet, pulled by token. Not
        // a browse: it is not under any root and never will be, because the
        // authorisation is the share itself rather than a folder grant.
        if (path.startsWith(SHARE_PREFIX)) {
            if (forWrite) return Result.Err(FsCode.ROFS)
            val token = path.removePrefix(SHARE_PREFIX)
            val g = ShareGrants.get(token) ?: return Result.Err(FsCode.NOENT)
            return Result.Ok(Target.Shared(g.uri, g.name, g.size, token))
        }
        if (path.startsWith("/")) return resolveLocal(path, forWrite)

        val uri = try {
            Uri.parse(path)
        } catch (_: Exception) {
            return Result.Err(FsCode.INVAL)
        }
        // A path that is not a content URI is not merely absent — it is a
        // request we will never honour, so INVAL rather than NOENT.
        if (uri.scheme != "content") return Result.Err(FsCode.INVAL)

        val requestedTree = try {
            DocumentsContract.getTreeDocumentId(uri)
        } catch (_: Exception) {
            null
        } ?: return Result.Err(FsCode.ACCES)

        for (root in roots()) {
            if (root !is Root.Tree) continue
            val grantedTree = try {
                DocumentsContract.getTreeDocumentId(root.treeUri)
            } catch (_: Exception) {
                continue
            }
            if (uri.authority != root.treeUri.authority) continue
            if (requestedTree != grantedTree) continue
            if (forWrite && !root.writable) return Result.Err(FsCode.ROFS)
            return Result.Ok(Target.Doc(uri))
        }
        // ACCES, not NOENT, and deliberately so: answering "no such file" for a
        // path outside every root would let a paired peer probe for the
        // existence of documents it is not allowed to see.
        return Result.Err(FsCode.ACCES)
    }

    /**
     * Resolve a real filesystem path under the all-files root.
     *
     * Canonicalises before comparing, so `..` traversal and symlinks pointing
     * out of shared storage are rejected rather than merely discouraged. Being
     * granted all-files access is not the same as agreeing to serve `/data` —
     * the user turned on "any files" meaning their files, and this app can read
     * a great deal more than that.
     */
    private fun resolveLocal(path: String, forWrite: Boolean): Result {
        if (!allFilesGranted()) return Result.Err(FsCode.ACCES)
        val canonical = try {
            File(path).canonicalFile
        } catch (_: Exception) {
            return Result.Err(FsCode.NOENT)
        }
        for (root in roots()) {
            if (root !is Root.Local) continue
            val croot = try {
                root.dir.canonicalFile
            } catch (_: Exception) {
                continue
            }
            val inside = canonical == croot ||
                canonical.path.startsWith(croot.path + File.separator)
            if (!inside) continue
            if (forWrite && !root.writable) return Result.Err(FsCode.ROFS)
            return Result.Ok(Target.Local(canonical))
        }
        return Result.Err(FsCode.ACCES)
    }

    /** Take a tree the user just picked, persisting the grant across reboots. */
    fun grant(uri: Uri): Boolean = try {
        context.contentResolver.takePersistableUriPermission(
            uri,
            Intent.FLAG_GRANT_READ_URI_PERMISSION,
        )
        Log.i(TAG, "fs: now serving tree ${displayNameOf(uri)}")
        true
    } catch (e: Exception) {
        Log.w(TAG, "fs: could not persist tree grant: ${e.message}")
        false
    }

    /** Stop serving a tree. */
    fun revoke(uri: Uri) {
        try {
            context.contentResolver.releasePersistableUriPermission(
                uri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION,
            )
            Log.i(TAG, "fs: stopped serving a tree")
        } catch (e: Exception) {
            Log.w(TAG, "fs: could not release tree grant: ${e.message}")
        }
    }

    /**
     * A human name for a tree.
     *
     * Falls back to the last path segment, which is ugly but never empty —
     * the laptop shows this, and a blank folder name in a file manager is
     * worse than a technical one.
     */
    private fun displayNameOf(treeUri: Uri): String {
        val docUri = try {
            DocumentsContract.buildDocumentUriUsingTree(
                treeUri,
                DocumentsContract.getTreeDocumentId(treeUri),
            )
        } catch (_: Exception) {
            null
        }
        if (docUri != null) {
            try {
                context.contentResolver.query(
                    docUri,
                    arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME),
                    null,
                    null,
                    null,
                )?.use { c ->
                    if (c.moveToFirst()) {
                        val n = c.getString(0)
                        if (!n.isNullOrBlank()) return n
                    }
                }
            } catch (_: Exception) {
                // Fall through to the segment fallback.
            }
        }
        return treeUri.lastPathSegment?.substringAfterLast(':')?.takeIf { it.isNotBlank() }
            ?: "Shared folder"
    }

    sealed class Result {
        data class Ok(val target: Target) : Result()
        data class Err(val code: Int) : Result()
    }

    companion object {
        private const val TAG = "VortexFs"

        /** Addresses a share-sheet file rather than a browsable path. */
        const val SHARE_PREFIX = "share:"
    }
}
