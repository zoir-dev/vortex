package com.vortex.a3.core.calllog

import android.content.Context
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.os.Handler
import android.os.Looper
import android.provider.CallLog
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * Reads the phone's recent call log and emits the list whenever it changes, so
 * the laptop companion's Recents page can mirror it. Bounded to the most recent
 * [LIMIT] calls (DATE DESC); a `ContentObserver` re-emits on any new/changed
 * call. READ_CALL_LOG gated (already granted for the call-mirror caller lookup).
 */
class CallLogProvider(
    private val context: Context,
    private val onCallLog: (List<CallLogEntry>) -> Unit,
) {
    private val tag = "CallLogProvider"
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var observer: ContentObserver? = null

    companion object {
        // Kept modest on purpose: the whole list ships as one chunked BLE burst,
        // and a big burst risks lost notifies (which desync the Noise cipher).
        // 60 recent calls is plenty for a Recents view and stays well within the
        // proven-reliable chunk count.
        private const val LIMIT = 60

        /** Quiet window before re-reading + re-sending after an onChange burst. */
        private const val EMIT_DEBOUNCE_MS = 2_000L
    }

    fun start() {
        if (!hasPermission()) {
            Log.i(tag, "READ_CALL_LOG missing; call-log mirror disabled")
            return
        }
        scope.launch { emitSnapshot() }
        val obs = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) {
                scheduleEmit()
            }
        }
        try {
            context.contentResolver.registerContentObserver(
                CallLog.Calls.CONTENT_URI, true, obs,
            )
            observer = obs
        } catch (e: Exception) {
            Log.w(tag, "registerContentObserver: ${e.message}")
        }
    }

    /** Battery: each emit ships the full list as a BLE chunk burst, so collapse
     *  observer bursts (one call updates several rows) into one re-read after a
     *  quiet window. */
    private fun scheduleEmit() {
        pendingEmit?.cancel()
        pendingEmit = scope.launch {
            kotlinx.coroutines.delay(EMIT_DEBOUNCE_MS)
            emitSnapshot()
        }
    }

    private var pendingEmit: Job? = null

    fun stop() {
        observer?.let {
            try { context.contentResolver.unregisterContentObserver(it) } catch (_: Exception) {}
        }
        observer = null
        scope.coroutineContext[Job]?.cancel()
    }

    fun refresh() {
        if (hasPermission()) scope.launch { emitSnapshot() }
    }

    private fun emitSnapshot() {
        try {
            onCallLog(readCallLog())
        } catch (e: Exception) {
            Log.w(tag, "emitSnapshot: ${e.message}")
        }
    }

    /**
     * The most recent [LIMIT] calls, newest-first.
     *
     * The row cap is [queryCallLog]'s Kotlin `cap`, NOT a SQL `LIMIT` appended
     * to the sort order: the call-log provider validates `sortOrder` and throws
     * `IllegalArgumentException("Invalid token LIMIT")` (seen on Android 16 /
     * OnePlus), which made every read fail — silently here, and as
     * `call_log_history: "error"` on the laptop's bulk-sync. Don't re-add it.
     */
    private fun readCallLog(): List<CallLogEntry> =
        queryCallLog(null, null, "${CallLog.Calls.DATE} DESC", LIMIT)

    /**
     * Read calls NEWER than [sinceMs] (oldest-first, up to [limit]) for the
     * laptop's LAN bulk-sync history backfill — the watermark twin of
     * [com.vortex.a3.core.sms.SmsProvider.readHistorySince].
     *
     * [limit] is enforced in Kotlin, not as a SQL `LIMIT` in the sort order —
     * same provider rejection as [readCallLog].
     */
    fun readHistorySince(sinceMs: Long, limit: Int): List<CallLogEntry> {
        if (!hasPermission()) return emptyList()
        val cap = limit.coerceIn(1, 5000)
        return queryCallLog(
            "${CallLog.Calls.DATE} > ?",
            arrayOf(sinceMs.toString()),
            "${CallLog.Calls.DATE} ASC",
            cap,
        )
    }

    private fun queryCallLog(
        selection: String?,
        args: Array<String>?,
        sortOrder: String,
        cap: Int,
    ): List<CallLogEntry> {
        val out = ArrayList<CallLogEntry>(minOf(cap, LIMIT))
        val cols = arrayOf(
            CallLog.Calls._ID,
            CallLog.Calls.NUMBER,
            CallLog.Calls.CACHED_NAME,
            CallLog.Calls.TYPE,
            CallLog.Calls.DATE,
            CallLog.Calls.DURATION,
        )
        context.contentResolver.query(
            CallLog.Calls.CONTENT_URI,
            cols,
            selection,
            args,
            sortOrder,
        )?.use { c ->
            val idIdx = c.getColumnIndex(CallLog.Calls._ID)
            val numIdx = c.getColumnIndex(CallLog.Calls.NUMBER)
            val nameIdx = c.getColumnIndex(CallLog.Calls.CACHED_NAME)
            val typeIdx = c.getColumnIndex(CallLog.Calls.TYPE)
            val dateIdx = c.getColumnIndex(CallLog.Calls.DATE)
            val durIdx = c.getColumnIndex(CallLog.Calls.DURATION)
            while (c.moveToNext()) {
                if (out.size >= cap) break // sole row limit: see readCallLog
                val id = if (idIdx >= 0) c.getString(idIdx).orEmpty() else ""
                out.add(
                    CallLogEntry(
                        id = id,
                        number = (if (numIdx >= 0) c.getString(numIdx) else null)?.trim().orEmpty(),
                        name = (if (nameIdx >= 0) c.getString(nameIdx) else null)?.trim().orEmpty(),
                        type = if (typeIdx >= 0) c.getInt(typeIdx) else 0,
                        date = if (dateIdx >= 0) c.getLong(dateIdx) else 0L,
                        duration = if (durIdx >= 0) c.getLong(durIdx) else 0L,
                    ),
                )
            }
        }
        return out
    }

    private fun hasPermission(): Boolean =
        ContextCompat.checkSelfPermission(
            context,
            android.Manifest.permission.READ_CALL_LOG,
        ) == PackageManager.PERMISSION_GRANTED
}
