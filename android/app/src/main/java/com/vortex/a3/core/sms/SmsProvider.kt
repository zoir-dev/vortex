package com.vortex.a3.core.sms

import android.content.Context
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.Telephony
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * Reads the phone's recent SMS and emits the list whenever it changes, so the
 * laptop companion's Messages page can mirror it (the laptop groups them into
 * conversations). Bounded to the most recent [LIMIT] messages (DATE DESC); a
 * `ContentObserver` re-emits on any new/changed message. READ_SMS gated.
 */
class SmsProvider(
    private val context: Context,
    private val onSms: (List<SmsMessage>) -> Unit,
) {
    private val tag = "SmsProvider"
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var observer: ContentObserver? = null

    companion object {
        // Kept small: the whole list ships as one chunked BLE burst, and a big
        // burst risks lost notifies (which desync the Noise cipher). 80 recent
        // messages is plenty for the conversation view.
        // Reliability ceiling for the chunked BLE burst: one lost notify desyncs
        // the Noise cipher and drops the transfer, and contacts + call log are in
        // flight in the same session. ~80 messages ≈ 15 chunks at 450 B keeps the
        // session's total in the proven-reliable range (≈30-36 chunks). True
        // "all" (the full history) needs LAN-for-bulk or on-demand pagination.
        private const val LIMIT = 80

        /** Quiet window before re-reading + re-sending after an onChange burst. */
        private const val EMIT_DEBOUNCE_MS = 2_000L
    }

    fun start() {
        // We attempt the read even when checkSelfPermission() reports denied:
        // some ROMs (MIUI) gate SMS reads on the READ_SMS *appop* rather than
        // the runtime grant flag. readSms() catches SecurityException and yields
        // an empty list if it's genuinely blocked.
        Log.i(tag, "sms: READ_SMS runtime-granted=${hasPermission()}; starting")
        scope.launch { emitSnapshot() }
        val obs = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) {
                scheduleEmit()
            }
        }
        try {
            context.contentResolver.registerContentObserver(
                Telephony.Sms.CONTENT_URI, true, obs,
            )
            observer = obs
        } catch (e: Exception) {
            Log.w(tag, "registerContentObserver: ${e.message}")
        }
    }

    fun stop() {
        observer?.let {
            try { context.contentResolver.unregisterContentObserver(it) } catch (_: Exception) {}
        }
        observer = null
        scope.coroutineContext[Job]?.cancel()
    }

    fun refresh() {
        scope.launch { emitSnapshot() }
    }

    /** Battery: each emit ships the full list as a ~1.5 s BLE chunk burst, so
     *  collapse observer bursts (an active conversation fires onChange per
     *  message AND per status update) into one re-read after a quiet window. */
    private fun scheduleEmit() {
        pendingEmit?.cancel()
        pendingEmit = scope.launch {
            kotlinx.coroutines.delay(EMIT_DEBOUNCE_MS)
            emitSnapshot()
        }
    }

    private var pendingEmit: Job? = null

    /**
     * Read ONE conversation's messages for the laptop's infinite scroll: the
     * [thread] (preferred) or [address] (fallback), newest-first, skipping the
     * first [offset] and returning up to [limit]. The laptop requests page 0 on
     * open, then older pages as the user scrolls up. A page is small (≈[limit]
     * messages) so it ships as one short, reliable BLE burst — unlike the full
     * history, which would desync the cipher. Empty / short page ⇒ no more older.
     */
    fun loadThread(thread: Long, address: String, offset: Int, limit: Int): List<SmsMessage> {
        val cols = arrayOf(
            Telephony.Sms._ID,
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.TYPE,
            Telephony.Sms.DATE,
            Telephony.Sms.THREAD_ID,
            Telephony.Sms.READ,
        )
        val (sel, args) = when {
            thread > 0L -> "${Telephony.Sms.THREAD_ID}=?" to arrayOf(thread.toString())
            address.isNotEmpty() -> "${Telephony.Sms.ADDRESS}=?" to arrayOf(address)
            else -> return emptyList()
        }
        val cap = limit.coerceIn(1, 200)
        val skip = offset.coerceAtLeast(0)
        return querySms(
            cols, sel, args,
            "${Telephony.Sms.DATE} DESC LIMIT $cap OFFSET $skip",
            // Some ROMs leave ADDRESS blank on sent rows; fall back to the
            // requested address so the laptop's thread merge (keyed by
            // address) doesn't silently drop them.
            fallbackAddress = address.trim(),
            logTag = "loadThread",
        )
    }

    /**
     * Read messages NEWER than [sinceMs] (oldest-first, up to [limit]) for the
     * laptop's LAN bulk-sync history backfill: the laptop names its watermark,
     * we ship only what it's missing. Oldest-first so a capped batch advances
     * the watermark contiguously — the next heartbeat picks up where this one
     * stopped (self-paginating).
     */
    fun readHistorySince(sinceMs: Long, limit: Int): List<SmsMessage> {
        val cols = arrayOf(
            Telephony.Sms._ID,
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.TYPE,
            Telephony.Sms.DATE,
            Telephony.Sms.THREAD_ID,
            Telephony.Sms.READ,
        )
        val cap = limit.coerceIn(1, 5000)
        return querySms(
            cols,
            "${Telephony.Sms.DATE} > ?",
            arrayOf(sinceMs.toString()),
            "${Telephony.Sms.DATE} ASC LIMIT $cap",
            logTag = "readHistorySince",
        )
    }

    /**
     * Every SMS row id, ascending — the deletion-reconcile dataset: the
     * laptop diffs this against its history store and prunes entries the
     * phone no longer has. One indexed column, cheap even at 10k rows.
     */
    fun readAllIds(): List<String> {
        val out = ArrayList<String>()
        try {
            context.contentResolver.query(
                Uri.parse("content://sms"),
                arrayOf(Telephony.Sms._ID),
                null,
                null,
                "${Telephony.Sms._ID} ASC",
            )?.use { c ->
                val idIdx = c.getColumnIndex(Telephony.Sms._ID)
                while (c.moveToNext()) {
                    if (idIdx >= 0) out.add(c.getString(idIdx).orEmpty())
                }
            }
        } catch (e: Exception) {
            Log.w(tag, "readAllIds: ${e.message}")
        }
        return out
    }

    /** Shared cursor loop for the thread/history readers. */
    private fun querySms(
        cols: Array<String>,
        selection: String,
        args: Array<String>,
        sortOrder: String,
        fallbackAddress: String = "",
        logTag: String,
    ): List<SmsMessage> {
        val out = ArrayList<SmsMessage>()
        try {
            context.contentResolver.query(
                Uri.parse("content://sms"), cols, selection, args, sortOrder,
            )?.use { c ->
                val idIdx = c.getColumnIndex(Telephony.Sms._ID)
                val addrIdx = c.getColumnIndex(Telephony.Sms.ADDRESS)
                val bodyIdx = c.getColumnIndex(Telephony.Sms.BODY)
                val typeIdx = c.getColumnIndex(Telephony.Sms.TYPE)
                val dateIdx = c.getColumnIndex(Telephony.Sms.DATE)
                val threadIdx = c.getColumnIndex(Telephony.Sms.THREAD_ID)
                val readIdx = c.getColumnIndex(Telephony.Sms.READ)
                while (c.moveToNext()) {
                    out.add(
                        SmsMessage(
                            id = if (idIdx >= 0) c.getString(idIdx).orEmpty() else "",
                            address = (if (addrIdx >= 0) c.getString(addrIdx) else null)
                                ?.trim().orEmpty().ifEmpty { fallbackAddress },
                            body = (if (bodyIdx >= 0) c.getString(bodyIdx) else null).orEmpty(),
                            type = if (typeIdx >= 0) c.getInt(typeIdx) else 0,
                            date = if (dateIdx >= 0) c.getLong(dateIdx) else 0L,
                            thread = if (threadIdx >= 0) c.getLong(threadIdx) else 0L,
                            read = if (readIdx >= 0) c.getInt(readIdx) else 1,
                        ),
                    )
                }
            }
        } catch (e: Exception) {
            Log.w(tag, "$logTag: ${e.message}")
        }
        return out
    }

    private fun emitSnapshot() {
        try {
            onSms(readSms())
        } catch (e: SecurityException) {
            com.vortex.a3.core.notif.FeatureBlocked.report(
                context, "Messages", android.Manifest.permission.READ_SMS,
            )
            Log.w(tag, "emitSnapshot: ${e.message}")
        } catch (e: Exception) {
            Log.w(tag, "emitSnapshot: ${e.message}")
        }
    }

    private fun readSms(): List<SmsMessage> {
        val out = ArrayList<SmsMessage>(LIMIT)
        val cols = arrayOf(
            Telephony.Sms._ID,
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.TYPE,
            Telephony.Sms.DATE,
            Telephony.Sms.THREAD_ID,
            Telephony.Sms.READ,
        )
        context.contentResolver.query(
            Uri.parse("content://sms"),
            cols,
            null,
            null,
            "${Telephony.Sms.DATE} DESC LIMIT $LIMIT",
        )?.use { c ->
            val idIdx = c.getColumnIndex(Telephony.Sms._ID)
            val addrIdx = c.getColumnIndex(Telephony.Sms.ADDRESS)
            val bodyIdx = c.getColumnIndex(Telephony.Sms.BODY)
            val typeIdx = c.getColumnIndex(Telephony.Sms.TYPE)
            val dateIdx = c.getColumnIndex(Telephony.Sms.DATE)
            val threadIdx = c.getColumnIndex(Telephony.Sms.THREAD_ID)
            val readIdx = c.getColumnIndex(Telephony.Sms.READ)
            while (c.moveToNext()) {
                if (out.size >= LIMIT) break // guard if the SQL LIMIT is ignored
                out.add(
                    SmsMessage(
                        id = if (idIdx >= 0) c.getString(idIdx).orEmpty() else "",
                        address = (if (addrIdx >= 0) c.getString(addrIdx) else null)?.trim().orEmpty(),
                        body = (if (bodyIdx >= 0) c.getString(bodyIdx) else null).orEmpty(),
                        type = if (typeIdx >= 0) c.getInt(typeIdx) else 0,
                        date = if (dateIdx >= 0) c.getLong(dateIdx) else 0L,
                        thread = if (threadIdx >= 0) c.getLong(threadIdx) else 0L,
                        read = if (readIdx >= 0) c.getInt(readIdx) else 1,
                    ),
                )
            }
        }
        return out
    }

    private fun hasPermission(): Boolean =
        ContextCompat.checkSelfPermission(
            context,
            android.Manifest.permission.READ_SMS,
        ) == PackageManager.PERMISSION_GRANTED
}
