package com.vortex.a3.core.fs

import org.json.JSONArray
import org.json.JSONObject

/**
 * Ranged-filesystem protocol — the Kotlin mirror of Rust
 * `core::fs_proto` (see `docs/design/file-browsing.md`).
 *
 * One primitive sits underneath both file browsing and large-file transfer:
 *
 * ```
 * READ(handle, offset, len) -> bytes
 * ```
 *
 * Every transfer today buffers a whole file in memory (the old blob store
 * holds bytes keyed by a content token), which is what made an 835 MB share an
 * `OutOfMemoryError` and why a 64 MB cap exists. Ranged reads remove the cap as
 * a side effect rather than as a separate change.
 *
 * The protocol is **symmetric**: this phone serves these ops so the laptop can
 * browse its storage, and sends them so it can browse the laptop's. Nothing
 * here names a side.
 *
 * Wire types must stay byte-identical to the Rust module — the field names are
 * the contract.
 */

/** Op codes, carried in the frame's `sub` byte. Mirrors Rust `fs_proto::op`. */
object FsOp {
    const val LIST: Byte = 0x01
    const val STAT: Byte = 0x02
    const val OPEN: Byte = 0x03
    const val READ: Byte = 0x04
    const val WRITE: Byte = 0x05
    const val CLOSE: Byte = 0x06
    const val SETMETA: Byte = 0x07
}

/**
 * Error codes. Errno-shaped on purpose: the laptop's mount adapters (FUSE,
 * ProjFS) turn these straight back into OS errors, and a private vocabulary
 * would mean two lossy translations instead of none.
 *
 * Mirrors Rust `fs_proto::code`.
 */
object FsCode {
    const val NOENT = 2
    const val ACCES = 13
    const val IO = 5
    const val BADF = 9
    const val INVAL = 22

    /**
     * Defined and wired, deliberately not implemented. Answered explicitly,
     * never dropped: a stub that looks like a timeout is worse than an honest
     * refusal.
     */
    const val NOTSUP = 95
    const val ISDIR = 21
    const val ROFS = 30
}

/**
 * Bytes per READ. Bounded so memory stays flat on both sides regardless of file
 * size — the consumer issues many ranged reads rather than one huge one, which
 * is the entire point.
 */
const val MAX_READ_LEN = 48 * 1024

/** Entries per LIST page. A 10,000-entry folder must not be one frame. */
const val LIST_PAGE = 256

/** Binary header on an FS_DATA payload: id(4) + offset(8) + flags(1). */
const val DATA_HEADER_LEN = 13

/** FS_DATA flag: this reply reaches end-of-file. */
const val FLAG_EOF: Int = 0x01

// ---------------------------------------------------------------------------
// Requests — parsed from JSON when serving, built when consuming
// ---------------------------------------------------------------------------

data class ListReq(val id: Int, val path: String, val cursor: Int = 0) {
    fun toJson(): JSONObject =
        JSONObject().put("id", id).put("path", path).put("cursor", cursor)

    companion object {
        fun from(o: JSONObject) =
            ListReq(o.optInt("id"), o.optString("path"), o.optInt("cursor", 0))
    }
}

data class StatReq(val id: Int, val path: String) {
    fun toJson(): JSONObject = JSONObject().put("id", id).put("path", path)

    companion object {
        fun from(o: JSONObject) = StatReq(o.optInt("id"), o.optString("path"))
    }
}

data class OpenReq(val id: Int, val path: String, val write: Boolean = false) {
    fun toJson(): JSONObject =
        JSONObject().put("id", id).put("path", path).put("write", write)

    companion object {
        fun from(o: JSONObject) =
            OpenReq(o.optInt("id"), o.optString("path"), o.optBoolean("write", false))
    }
}

/**
 * Read [len] bytes at [offset]. A short reply is normal — end of file, or the
 * server chose a smaller slice — and is not necessarily EOF; check [FLAG_EOF].
 */
data class ReadReq(val id: Int, val handle: Long, val offset: Long, val len: Int) {
    fun toJson(): JSONObject = JSONObject()
        .put("id", id).put("handle", handle).put("offset", offset).put("len", len)

    companion object {
        fun from(o: JSONObject) = ReadReq(
            o.optInt("id"), o.optLong("handle"), o.optLong("offset"), o.optInt("len"),
        )
    }
}

/** Write at [offset]; the bytes ride a binary tail (see [encodeWrite]). */
data class WriteReq(val id: Int, val handle: Long, val offset: Long) {
    fun toJson(): JSONObject =
        JSONObject().put("id", id).put("handle", handle).put("offset", offset)

    companion object {
        fun from(o: JSONObject) =
            WriteReq(o.optInt("id"), o.optLong("handle"), o.optLong("offset"))
    }
}

data class CloseReq(val id: Int, val handle: Long) {
    fun toJson(): JSONObject = JSONObject().put("id", id).put("handle", handle)

    companion object {
        fun from(o: JSONObject) = CloseReq(o.optInt("id"), o.optLong("handle"))
    }
}

data class SetMetaReq(
    val id: Int,
    val path: String,
    val mtime: Long? = null,
    /** A bare NAME within the same directory, never a path. */
    val renameTo: String? = null,
) {
    fun toJson(): JSONObject = JSONObject().put("id", id).put("path", path).also {
        if (mtime != null) it.put("mtime", mtime)
        if (renameTo != null) it.put("rename_to", renameTo)
    }

    companion object {
        fun from(o: JSONObject) = SetMetaReq(
            o.optInt("id"),
            o.optString("path"),
            if (o.has("mtime") && !o.isNull("mtime")) o.optLong("mtime") else null,
            if (o.has("rename_to") && !o.isNull("rename_to")) o.optString("rename_to") else null,
        )
    }
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/**
 * One directory entry, or a stat result.
 *
 * Deliberately minimal: a file manager needs name, kind, size and mtime to draw
 * a row, and every extra field is bytes on a link that may be BLE.
 *
 * [path] is an opaque, server-defined addressing token — a SAF document URI
 * here, an absolute path on the laptop. The far side must send it back verbatim
 * and must never build a child address by joining [name] onto its parent:
 * under SAF a name is simply not addressable.
 */
data class FsEntry(
    val name: String,
    val isDir: Boolean = false,
    val size: Long = 0,
    val mtime: Long = 0,
    val readonly: Boolean = false,
    val path: String = "",
) {
    fun toJson(): JSONObject = JSONObject()
        .put("name", name)
        .put("is_dir", isDir)
        .put("size", size)
        .put("mtime", mtime)
        .put("readonly", readonly)
        .also { if (path.isNotEmpty()) it.put("path", path) }

    companion object {
        fun from(o: JSONObject) = FsEntry(
            o.optString("name"),
            o.optBoolean("is_dir", false),
            o.optLong("size", 0),
            o.optLong("mtime", 0),
            o.optBoolean("readonly", false),
            o.optString("path", ""),
        )
    }
}

/**
 * A successful non-data reply, carried as JSON in an FS_META frame. Serialised
 * with a `kind` tag to match Rust's `#[serde(tag = "kind")]`.
 */
sealed class FsReply {
    abstract val id: Int

    data class ListPage(
        override val id: Int,
        val entries: List<FsEntry>,
        /** Resume point for the next page, or null when complete. Non-null
         *  always means "call again" — never a guess. */
        val cursor: Int? = null,
    ) : FsReply()

    data class Stat(override val id: Int, val entry: FsEntry) : FsReply()

    data class Open(
        override val id: Int,
        val handle: Long,
        /** Size at open time, so the consumer can plan reads without a
         *  follow-up stat. */
        val size: Long,
        val readonly: Boolean = false,
    ) : FsReply()

    data class Wrote(override val id: Int, val bytes: Int) : FsReply()

    /** Generic success for ops with nothing to report (CLOSE, SETMETA). */
    data class Ok(override val id: Int) : FsReply()

    fun toJsonBytes(): ByteArray = toJson().toString().toByteArray(Charsets.UTF_8)

    fun toJson(): JSONObject = when (this) {
        is ListPage -> JSONObject()
            .put("kind", "list")
            .put("id", id)
            .put("entries", JSONArray().also { a -> entries.forEach { a.put(it.toJson()) } })
            .also { if (cursor != null) it.put("cursor", cursor) }
        is Stat -> JSONObject().put("kind", "stat").put("id", id).put("entry", entry.toJson())
        is Open -> JSONObject().put("kind", "open").put("id", id)
            .put("handle", handle).put("size", size).put("readonly", readonly)
        is Wrote -> JSONObject().put("kind", "wrote").put("id", id).put("bytes", bytes)
        is Ok -> JSONObject().put("kind", "ok").put("id", id)
    }

    companion object {
        fun from(o: JSONObject): FsReply? {
            val id = o.optInt("id")
            return when (o.optString("kind")) {
                "list" -> {
                    val arr = o.optJSONArray("entries") ?: JSONArray()
                    val out = ArrayList<FsEntry>(arr.length())
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.let { out.add(FsEntry.from(it)) }
                    }
                    ListPage(
                        id,
                        out,
                        if (o.has("cursor") && !o.isNull("cursor")) o.optInt("cursor") else null,
                    )
                }
                "stat" -> o.optJSONObject("entry")?.let { Stat(id, FsEntry.from(it)) }
                "open" -> Open(
                    id, o.optLong("handle"), o.optLong("size"), o.optBoolean("readonly", false),
                )
                "wrote" -> Wrote(id, o.optInt("bytes"))
                "ok" -> Ok(id)
                else -> null
            }
        }
    }
}

/**
 * A definite failure. Every failing op sends one — silence is never an answer,
 * because the far side cannot tell it from a lost frame.
 */
data class FsErr(val id: Int, val code: Int, val msg: String = "") {
    fun toJsonBytes(): ByteArray = JSONObject()
        .put("id", id).put("code", code).put("msg", msg)
        .toString().toByteArray(Charsets.UTF_8)

    companion object {
        fun from(o: JSONObject) =
            FsErr(o.optInt("id"), o.optInt("code"), o.optString("msg", ""))
    }
}

// ---------------------------------------------------------------------------
// Binary framings
// ---------------------------------------------------------------------------

/** Build an FS_DATA payload: `[id u32 BE][offset u64 BE][flags u8][bytes]`. */
fun encodeData(id: Int, offset: Long, eof: Boolean, bytes: ByteArray, count: Int = bytes.size): ByteArray {
    val out = ByteArray(DATA_HEADER_LEN + count)
    out[0] = (id ushr 24).toByte()
    out[1] = (id ushr 16).toByte()
    out[2] = (id ushr 8).toByte()
    out[3] = id.toByte()
    for (i in 0 until 8) out[4 + i] = (offset ushr (56 - 8 * i)).toByte()
    out[12] = if (eof) FLAG_EOF.toByte() else 0
    System.arraycopy(bytes, 0, out, DATA_HEADER_LEN, count)
    return out
}

/** Parsed FS_DATA payload. [bytes] is a copy, safe to retain. */
data class FsData(val id: Int, val offset: Long, val eof: Boolean, val bytes: ByteArray) {
    // ByteArray in a data class: identity equals/hashCode would be wrong and
    // silently break any set/map use, so both are content-based.
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is FsData) return false
        return id == other.id && offset == other.offset && eof == other.eof &&
            bytes.contentEquals(other.bytes)
    }

    override fun hashCode(): Int {
        var h = id
        h = 31 * h + offset.hashCode()
        h = 31 * h + eof.hashCode()
        h = 31 * h + bytes.contentHashCode()
        return h
    }
}

/** Parse an FS_DATA payload, or null if truncated. */
fun decodeData(p: ByteArray): FsData? {
    if (p.size < DATA_HEADER_LEN) return null
    var id = 0
    for (i in 0 until 4) id = (id shl 8) or (p[i].toInt() and 0xFF)
    var off = 0L
    for (i in 0 until 8) off = (off shl 8) or (p[4 + i].toLong() and 0xFF)
    val eof = (p[12].toInt() and FLAG_EOF) != 0
    return FsData(id, off, eof, p.copyOfRange(DATA_HEADER_LEN, p.size))
}

/** Build an FS_REQ/WRITE payload: `[json_len u16 BE][json][bytes]`. */
fun encodeWrite(req: WriteReq, bytes: ByteArray, count: Int = bytes.size): ByteArray {
    val json = req.toJson().toString().toByteArray(Charsets.UTF_8)
    val out = ByteArray(2 + json.size + count)
    out[0] = (json.size ushr 8).toByte()
    out[1] = json.size.toByte()
    System.arraycopy(json, 0, out, 2, json.size)
    System.arraycopy(bytes, 0, out, 2 + json.size, count)
    return out
}

/** Parse an FS_REQ/WRITE payload, or null if truncated / malformed. */
fun decodeWrite(p: ByteArray): Pair<WriteReq, ByteArray>? {
    if (p.size < 2) return null
    val n = ((p[0].toInt() and 0xFF) shl 8) or (p[1].toInt() and 0xFF)
    val end = 2 + n
    // A peer can claim any length; a lying header must decode to null rather
    // than throw out of the frame handler.
    if (end > p.size) return null
    val req = try {
        WriteReq.from(JSONObject(String(p, 2, n, Charsets.UTF_8)))
    } catch (_: Exception) {
        return null
    }
    return req to p.copyOfRange(end, p.size)
}
