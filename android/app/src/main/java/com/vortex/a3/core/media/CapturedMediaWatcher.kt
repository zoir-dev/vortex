package com.vortex.a3.core.media

import android.content.ContentUris
import android.content.Context
import android.database.ContentObserver
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.MediaStore
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** What kind of picture the gallery just gained — decides the toggle that
 *  gates it and the folder it lands in on the laptop. */
enum class CapturedKind(
    /** The value carried in the file OFFER's `kind` field; the laptop maps it
     *  to a subfolder itself, so the phone never names a path. */
    val wire: String,
) {
    SCREENSHOT("screenshot"),
    PHOTO("photo"),
    SCREEN_RECORDING("screen_recording"),
    VIDEO("video"),
}

/** One finished picture in the gallery that a switched-on toggle covers. */
data class CapturedMedia(
    val uri: Uri,
    val id: Long,
    val name: String,
    val mime: String,
    val bytes: Long,
    val kind: CapturedKind,
    /** Which MediaStore collection the row lives in ("images" / "video").
     *  `_id` is only unique within one, so anything that looks the row up
     *  again later needs this too. */
    val collection: String,
)

/**
 * Watches the gallery for a NEW screenshot or camera photo and reports each
 * one once, so the laptop can be offered it — the trigger half of
 * "screenshots and photos land on the laptop by themselves". The delivery
 * half is the share-sheet pipeline, untouched: this only replaces the tap on
 * "Share → Vortex".
 *
 * Same shape as [com.vortex.a3.core.calllog.CallLogProvider]: a
 * `ContentObserver` on `MediaStore.Images`, a quiet window to collapse the
 * burst of change notices one picture produces, then a query. What differs
 * is that a picture is not a snapshot to re-send whole — it is a row to
 * report exactly once, which is what the watermark and the seen-set below
 * are for:
 *
 *  - **`_id` watermark.** The observer says only "something changed", so each
 *    scan asks for rows with `_id` above the highest one already handled.
 *    `_id` is the media table's autoincrement key, so it climbs and never
 *    reuses. Seeded with the CURRENT maximum at start: the pictures already on
 *    the phone are not a backlog to ship, and a switch turned on tonight must
 *    not empty the camera roll onto the laptop.
 *  - **Pending rows.** On Android 10 the screenshot service inserts the row
 *    with `IS_PENDING=1` and only clears it once the bytes are written — the
 *    same flag [com.vortex.a3.core.lan.IncomingFile] sets on the files IT
 *    writes. Reading such a row yields a truncated file, so a pending row
 *    holds the watermark and the scan comes back for it. A row that stays
 *    pending past [PENDING_GIVE_UP_MS] is a capture that was abandoned
 *    (the editor cancelled, the app died); it is stepped over so it can't
 *    block everything after it for ever.
 *  - **Seen-set.** Rows are handled in `_id` order, but a finished row can sit
 *    ABOVE a pending one, in which case it goes out while the watermark stays
 *    below it — and would be found again next scan. The set says which ids
 *    already went. It also absorbs MIUI's gallery, which rewrites a file after
 *    insert (EXIF, thumbnail) and fires more change notices for the same id.
 *
 * Which pictures count is a bucket test, `Screenshots` or `Camera` — the
 * folder names every Android ROM uses for its own captures — and a freshness
 * test on `DATE_ADDED`: a scan runs within seconds of the change it answers,
 * so a row added more than [FRESHNESS_MARGIN_SEC] ago is not something the
 * user just took. That is what keeps a re-index or a restore (OLD pictures
 * under NEW ids) off the laptop, and what bounds a late permission grant to
 * the capture the user made to try it, not the album behind it.
 *
 * Needs READ_EXTERNAL_STORAGE (READ_MEDIA_IMAGES from Android 13), granted at
 * runtime and optional: without it the query is silently empty on Android 10,
 * so the grant is checked explicitly and its absence logged once, and the
 * rest of the app is unaffected.
 *
 * Lives and dies with the service the observer is registered from. MIUI kills
 * that service freely; while it is down nothing is watched, and pictures taken
 * then are NOT sent later (the watermark re-seeds on start). That is the
 * intended shape — "lands seconds later" is a live feature, not a sync — and
 * it means this class never depends on the observer having survived.
 */class CapturedMediaWatcher(
    private val context: Context,
    private val onCaptured: (CapturedMedia) -> Unit,
) {
    private val tag = "CapturedMedia"
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /**
     * One MediaStore collection, with the state the scan keeps for it.
     *
     * Images and video are separate tables with separate `_id` sequences, so
     * every piece of per-collection bookkeeping — watermark, seen-set, pending
     * rows, the seeded-with-permission flag — has to be per-collection too. A
     * shared watermark would let a busy camera roll hide new screen recordings
     * behind an id that means nothing in the other table.
     */
    private inner class Source(
        /** For the log, so two collections' lines can be told apart. */
        val label: String,
        val collection: Uri,
        /** The runtime grant this collection needs; checked per scan. */
        val permission: String,
        val classify: (bucket: String?, relPath: String?) -> CapturedKind?,
    ) {
        var observer: ContentObserver? = null
        var seedJob: Job? = null
        var pendingScan: Job? = null

        /** Highest `_id` every row up to which has been handled. */
        @Volatile var watermark: Long = -1L

        /** Ids handled ABOVE the watermark, and ids MIUI re-announces. */
        val seen = LinkedHashSet<Long>()

        /** When each still-pending row was first noticed. */
        val pendingSince = HashMap<Long, Long>()

        @Volatile var permissionWarned = false

        /** True once a scan has run WITH the grant, so the seed can be redone
         *  against the real collection. */
        @Volatile var seededWithPermission = false
    }

    private val sources: List<Source> = buildList {
        add(
            Source(
                label = "images",
                collection = MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
                permission = mediaImagesPermission(),
                classify = ::classifyCapture,
            ),
        )
        add(
            Source(
                label = "video",
                collection = MediaStore.Video.Media.EXTERNAL_CONTENT_URI,
                permission = mediaVideoPermission(),
                classify = ::classifyVideoCapture,
            ),
        )
    }

    companion object {
        /** Quiet window after a change notice before the scan. One screenshot
         *  produces several notices (insert, the pending clear, MIUI's
         *  rewrite); this folds them into one query. Short, because "seconds
         *  later" is the promise. */
        private const val SCAN_DEBOUNCE_MS = 1_000L

        /** Re-scan interval while a row is still pending. */
        private const val PENDING_RECHECK_MS = 2_000L

        /** How long a row may stay pending before it is stepped over. A video
         *  is written while it records, so this is also the longest recording
         *  that can still be picked up — generous on purpose. */
        private const val PENDING_GIVE_UP_MS = 600_000L

        /** How old (by `DATE_ADDED`, wall-clock seconds) a row may be when a
         *  scan reaches it and still count as "just taken". Wide enough for a
         *  pending row that took its full [PENDING_GIVE_UP_MS] to finish. */
        private const val FRESHNESS_MARGIN_SEC = 660L

        /** Bound on each source's seen-set. Far above any plausible burst. */
        private const val SEEN_MAX = 512

        /** Rows per scan. Bounds a burst-mode session; anything past this is
         *  picked up by the next notice. */
        private const val SCAN_LIMIT = 50

        private val PROJECTION = arrayOf(
            MediaStore.MediaColumns._ID,
            MediaStore.MediaColumns.DISPLAY_NAME,
            MediaStore.MediaColumns.BUCKET_DISPLAY_NAME,
            MediaStore.MediaColumns.RELATIVE_PATH,
            MediaStore.MediaColumns.MIME_TYPE,
            MediaStore.MediaColumns.SIZE,
            MediaStore.MediaColumns.IS_PENDING,
            MediaStore.MediaColumns.DATE_ADDED,
        )
    }

    fun start() {
        if (sources.any { it.observer != null }) return
        MediaAutoShareSetting.init(context)
        // Loads what has already been sent, so a deletion made while the app
        // was down is still noticed on the next round.
        CaptureLedger.init(context)
        for (source in sources) start(source)
    }

    private fun start(source: Source) {
        // Seed the watermark. Done on IO — it is a query — and joined by every
        // scan (see [Source.seedJob]), so registering the observer right away
        // is safe.
        source.seedJob = scope.launch {
            source.watermark = currentMaxId(source)
            Log.i(tag, "watching ${source.label} from _id=${source.watermark}")
        }
        val obs = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean, uri: Uri?) {
                scheduleScan(source, SCAN_DEBOUNCE_MS)
            }
        }
        try {
            // Descendants too: the notice for a single row arrives on
            // `…/media/<id>`, not on the collection itself.
            context.contentResolver.registerContentObserver(source.collection, true, obs)
            source.observer = obs
        } catch (e: Exception) {
            Log.w(tag, "registerContentObserver(${source.label}): ${e.message}")
        }
    }

    fun stop() {
        for (source in sources) {
            source.observer?.let {
                try { context.contentResolver.unregisterContentObserver(it) } catch (_: Exception) {}
            }
            source.observer = null
            source.pendingScan?.cancel()
        }
        scope.coroutineContext[Job]?.cancel()
    }

    private fun scheduleScan(source: Source, afterMs: Long) {
        // Nothing switched on for this collection → no query at all. The
        // observer stays registered (it costs nothing) so a toggle flipped
        // later needs no restart.
        if (!anyEnabledFor(source)) return
        source.pendingScan?.cancel()
        source.pendingScan = scope.launch {
            delay(afterMs)
            source.seedJob?.join()
            scan(source)
        }
    }

    /** Is any toggle this collection can produce switched on? */
    private fun anyEnabledFor(source: Source): Boolean = when (source.label) {
        "video" -> MediaAutoShareSetting.screenRecordingsEnabled() ||
            MediaAutoShareSetting.videosEnabled()
        else -> MediaAutoShareSetting.screenshotsEnabled() ||
            MediaAutoShareSetting.photosEnabled()
    }

    /** The toggle that gates one kind. */
    private fun enabledFor(kind: CapturedKind): Boolean = when (kind) {
        CapturedKind.SCREENSHOT -> MediaAutoShareSetting.screenshotsEnabled()
        CapturedKind.PHOTO -> MediaAutoShareSetting.photosEnabled()
        CapturedKind.SCREEN_RECORDING -> MediaAutoShareSetting.screenRecordingsEnabled()
        CapturedKind.VIDEO -> MediaAutoShareSetting.videosEnabled()
    }

    private fun hasPermission(source: Source): Boolean =
        androidx.core.content.ContextCompat.checkSelfPermission(
            context,
            source.permission,
        ) == android.content.pm.PackageManager.PERMISSION_GRANTED

    /** MAX(_id) right now, or -1 on an empty/unreadable collection. Without the
     *  grant Android 10 answers with only OUR rows, which is fine for a seed:
     *  the watermark is re-seeded on the first permitted scan (see [scan]) so a
     *  grant given later doesn't unleash the whole collection. */
    private fun currentMaxId(source: Source): Long = try {
        context.contentResolver.query(
            source.collection,
            arrayOf(MediaStore.MediaColumns._ID),
            null,
            null,
            "${MediaStore.MediaColumns._ID} DESC LIMIT 1",
        )?.use { c -> if (c.moveToFirst()) c.getLong(0) else -1L } ?: -1L
    } catch (e: Exception) {
        Log.w(tag, "seed query (${source.label}): ${e.message}")
        -1L
    }

    private fun scan(source: Source) {
        if (!hasPermission(source)) {
            if (!source.permissionWarned) {
                source.permissionWarned = true
                Log.i(tag, "${source.label}: permission not granted; captures stay on the phone")
            }
            return
        }
        if (!source.seededWithPermission) {
            // The seed may have run unprivileged (see currentMaxId), in which
            // case it sits far below the real collection. Everything present
            // now is history, not a capture — except the one the user just made
            // to see the feature work, which is what scheduled this scan.
            // Re-seed to just below the oldest FRESH row, so that one goes and
            // the library behind it does not.
            source.seededWithPermission = true
            source.permissionWarned = false
            source.watermark = maxOf(source.watermark, freshWatermark(source))
        }
        var stillPending = false
        try {
            context.contentResolver.query(
                source.collection,
                PROJECTION,
                "${MediaStore.MediaColumns._ID} > ?",
                arrayOf(source.watermark.toString()),
                "${MediaStore.MediaColumns._ID} ASC LIMIT $SCAN_LIMIT",
            )?.use { c ->
                val idIdx = c.getColumnIndexOrThrow(MediaStore.MediaColumns._ID)
                val nameIdx = c.getColumnIndex(MediaStore.MediaColumns.DISPLAY_NAME)
                val bucketIdx = c.getColumnIndex(MediaStore.MediaColumns.BUCKET_DISPLAY_NAME)
                val relIdx = c.getColumnIndex(MediaStore.MediaColumns.RELATIVE_PATH)
                val mimeIdx = c.getColumnIndex(MediaStore.MediaColumns.MIME_TYPE)
                val sizeIdx = c.getColumnIndex(MediaStore.MediaColumns.SIZE)
                val pendIdx = c.getColumnIndex(MediaStore.MediaColumns.IS_PENDING)
                val addedIdx = c.getColumnIndex(MediaStore.MediaColumns.DATE_ADDED)
                // The watermark only advances over a CONTIGUOUS run of handled
                // rows from the bottom; the first pending row stops it.
                var advanceTo = source.watermark
                var blocked = false
                while (c.moveToNext()) {
                    val id = c.getLong(idIdx)
                    val pending = pendIdx >= 0 && c.getInt(pendIdx) == 1
                    if (pending) {
                        val now = android.os.SystemClock.elapsedRealtime()
                        val since = source.pendingSince.getOrPut(id) { now }
                        if (now - since < PENDING_GIVE_UP_MS) {
                            stillPending = true
                            blocked = true
                            continue
                        }
                        Log.w(tag, "${source.label} _id=$id pending too long; stepping over it")
                        source.pendingSince.remove(id)
                        remember(source, id)
                        if (!blocked) advanceTo = id
                        continue
                    }
                    source.pendingSince.remove(id)
                    if (!blocked) advanceTo = id
                    if (id in source.seen) continue
                    remember(source, id)

                    val kind = source.classify(
                        if (bucketIdx >= 0) c.getString(bucketIdx) else null,
                        if (relIdx >= 0) c.getString(relIdx) else null,
                    ) ?: continue
                    if (!enabledFor(kind)) continue
                    val added = if (addedIdx >= 0) c.getLong(addedIdx) else 0L
                    if (added < nowSec() - FRESHNESS_MARGIN_SEC) {
                        Log.i(tag, "_id=$id is old under a new id (re-index?); not sent")
                        continue
                    }
                    val size = if (sizeIdx >= 0) c.getLong(sizeIdx) else 0L
                    // No size cap any more. There used to be one because the
                    // reader pulled the whole file into the service's heap to
                    // send it — which is exactly what the ranged-read protocol
                    // removed: an offer now carries a grant, and the laptop
                    // streams the bytes on demand. A screen recording is
                    // offered like anything else, and nothing here holds it.
                    val name = (if (nameIdx >= 0) c.getString(nameIdx) else null)
                        ?.takeIf { it.isNotBlank() } ?: "capture-$id"
                    val mime = (if (mimeIdx >= 0) c.getString(mimeIdx) else null)
                        ?.takeIf { it.isNotBlank() } ?: "application/octet-stream"
                    val media = CapturedMedia(
                        uri = ContentUris.withAppendedId(source.collection, id),
                        id = id,
                        name = name,
                        mime = mime,
                        bytes = size,
                        kind = kind,
                        collection = source.label,
                    )
                    Log.i(tag, "new ${kind.name.lowercase()} _id=$id ($size bytes)")
                    try {
                        onCaptured(media)
                    } catch (e: Exception) {
                        Log.w(tag, "onCaptured threw: ${e.message}")
                    }
                }
                if (advanceTo > source.watermark) source.watermark = advanceTo
            }
        } catch (e: SecurityException) {
            // The grant was revoked mid-run (Settings → Permissions). Same
            // outcome as never granted; say so once.
            if (!source.permissionWarned) {
                source.permissionWarned = true
                Log.i(tag, "${source.label}: permission revoked; captures stay on the phone")
            }
        } catch (e: Exception) {
            Log.w(tag, "scan(${source.label}): ${e.message}")
        }
        if (stillPending) scheduleScan(source, PENDING_RECHECK_MS)
    }

    private fun nowSec(): Long = System.currentTimeMillis() / 1000L

    /** The `_id` just below the oldest row that still counts as fresh — so a
     *  first permitted scan handles the capture that triggered it and nothing
     *  older. Falls back to MAX(_id) when nothing is fresh. */
    private fun freshWatermark(source: Source): Long = try {
        val cutoff = nowSec() - FRESHNESS_MARGIN_SEC
        context.contentResolver.query(
            source.collection,
            arrayOf(MediaStore.MediaColumns._ID),
            "${MediaStore.MediaColumns.DATE_ADDED} >= ?",
            arrayOf(cutoff.toString()),
            "${MediaStore.MediaColumns._ID} ASC LIMIT 1",
        )?.use { c -> if (c.moveToFirst()) c.getLong(0) - 1 else currentMaxId(source) }
            ?: currentMaxId(source)
    } catch (e: Exception) {
        currentMaxId(source)
    }

    private fun remember(source: Source, id: Long) {
        source.seen += id
        while (source.seen.size > SEEN_MAX) {
            val oldest = source.seen.iterator().next()
            source.seen.remove(oldest)
        }
    }
}

/** Screenshot or camera photo, by the folder the ROM put it in; anything
 *  else (a download, a messenger's saved image, an edit) is not ours. The
 *  bucket is the folder's own name; RELATIVE_PATH's last segment is checked
 *  too, because a ROM may localise the bucket label while the path on disk
 *  stays `DCIM/Screenshots/`. Pure, so it is unit-tested without a provider. */
internal fun classifyCapture(bucket: String?, relPath: String?): CapturedKind? {
    val names = folderNames(bucket, relPath)
    return when {
        "screenshots" in names -> CapturedKind.SCREENSHOT
        "camera" in names -> CapturedKind.PHOTO
        else -> null
    }
}

/**
 * The same test for the video collection: a screen recording or a camera clip.
 *
 * Only folders a ROM dedicates to recording count. `Movies/` is deliberately
 * NOT one of them even though stock Android 11+ records there — it is also
 * where downloaded films and anything a video app saves end up, and shipping
 * someone's film collection to their laptop because they once recorded their
 * screen is not a mistake worth risking. ROMs with a dedicated folder (MIUI's
 * `DCIM/ScreenRecorder`) are matched; a ROM that only uses `Movies/` gets
 * nothing from this toggle, which is the safe direction to be wrong in.
 */
internal fun classifyVideoCapture(bucket: String?, relPath: String?): CapturedKind? {
    val names = folderNames(bucket, relPath)
    return when {
        names.any { it in RECORDER_FOLDERS } -> CapturedKind.SCREEN_RECORDING
        "camera" in names -> CapturedKind.VIDEO
        else -> null
    }
}

/** Folder names ROMs dedicate to screen recording. */
private val RECORDER_FOLDERS = setOf(
    "screenrecorder", "screenrecords", "screen recordings",
    "screen_recordings", "recordings", "screenshots",
)

/** Both names a row's folder can go by, lowercased: the bucket label AND the
 *  last segment of RELATIVE_PATH. Either may match — a ROM can localise the
 *  label while the path on disk stays `DCIM/Screenshots/`, and it can equally
 *  report a bucket for a path this cannot parse. */
private fun folderNames(bucket: String?, relPath: String?): List<String> = listOfNotNull(
    bucket?.trim()?.lowercase()?.takeIf { it.isNotEmpty() },
    relPath?.trim()?.trimEnd('/')?.substringAfterLast('/')?.lowercase()?.takeIf { it.isNotEmpty() },
)

/** The runtime permission that lets us read other apps' pictures: the
 *  granular one from Android 13, the legacy storage one below it (where the
 *  granular one does not exist). One place, so the checks and the request
 *  can't disagree. */
fun mediaImagesPermission(): String =
    if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
        android.Manifest.permission.READ_MEDIA_IMAGES
    } else {
        @Suppress("DEPRECATION")
        android.Manifest.permission.READ_EXTERNAL_STORAGE
    }

/** The same for video. Below Android 13 it is the very same legacy grant, so
 *  the two collections share one permission there and asking twice is free. */
fun mediaVideoPermission(): String =
    if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
        android.Manifest.permission.READ_MEDIA_VIDEO
    } else {
        @Suppress("DEPRECATION")
        android.Manifest.permission.READ_EXTERNAL_STORAGE
    }

/** Every media grant the auto-share toggles can need, deduplicated — one
 *  entry below Android 13, two from it. What the permission request asks for
 *  and what the first-run check lists. */
fun mediaReadPermissions(): List<String> =
    listOf(mediaImagesPermission(), mediaVideoPermission()).distinct()

/** Back-compat alias for the images grant, which is the one the UI's "is the
 *  media toggle usable" hint has always meant. */
fun mediaReadPermission(): String = mediaImagesPermission()
