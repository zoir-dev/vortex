package com.vortex.a3.core.contacts

import android.content.Context
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.os.Handler
import android.os.Looper
import android.provider.ContactsContract
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * Reads the phone's contacts (name + numbers) and emits the full list whenever
 * it changes, so the laptop companion's Contacts page can mirror it. One query
 * over `Phone.CONTENT_URI` (one row per number) grouped by contact id; a
 * `ContentObserver` re-emits on any add/edit/delete. READ_CONTACTS gated.
 */
class ContactsProvider(
    private val context: Context,
    private val onContacts: (List<Contact>) -> Unit,
) {
    private val tag = "ContactsProvider"
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var observer: ContentObserver? = null

    companion object {
        /** Most contacts to mirror. ~120 entries is about 18 chunks — inside
         *  the burst length this link handles without dropping notifies. */
        private const val MAX_CONTACTS = 120

        /** Quiet window before re-reading + re-sending after an onChange burst. */
        private const val EMIT_DEBOUNCE_MS = 2_000L
    }

    fun start() {
        if (!hasPermission()) {
            Log.i(tag, "READ_CONTACTS missing; contacts mirror disabled")
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
                ContactsContract.Contacts.CONTENT_URI, true, obs,
            )
            observer = obs
        } catch (e: Exception) {
            Log.w(tag, "registerContentObserver: ${e.message}")
        }
    }

    /** Battery: each emit ships the full list as a BLE chunk burst, so collapse
     *  observer bursts (a contact import fires onChange per row) into one
     *  re-read after a quiet window. */
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

    /** Re-read + emit now (e.g. after the user grants READ_CONTACTS post-launch). */
    fun refresh() {
        if (hasPermission()) scope.launch { emitSnapshot() }
    }

    private fun emitSnapshot() {
        try {
            onContacts(readContacts())
        } catch (e: SecurityException) {
            // Not a log line: the laptop would otherwise show an empty Contacts
            // page forever with nothing saying why.
            com.vortex.a3.core.notif.FeatureBlocked.report(
                context, "Contacts", android.Manifest.permission.READ_CONTACTS,
            )
            Log.w(tag, "emitSnapshot: ${e.message}")
        } catch (e: Exception) {
            Log.w(tag, "emitSnapshot: ${e.message}")
        }
    }

    private fun readContacts(): List<Contact> {
        // Preserve insertion order (DISPLAY_NAME ASC) and group numbers per id.
        val byId = LinkedHashMap<String, Pair<String, MutableList<String>>>()
        val cols = arrayOf(
            ContactsContract.CommonDataKinds.Phone.CONTACT_ID,
            ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME,
            ContactsContract.CommonDataKinds.Phone.NUMBER,
        )
        context.contentResolver.query(
            ContactsContract.CommonDataKinds.Phone.CONTENT_URI,
            cols,
            null,
            null,
            ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME + " COLLATE NOCASE ASC",
        )?.use { c ->
            val idIdx = c.getColumnIndex(ContactsContract.CommonDataKinds.Phone.CONTACT_ID)
            val nameIdx = c.getColumnIndex(ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME)
            val numIdx = c.getColumnIndex(ContactsContract.CommonDataKinds.Phone.NUMBER)
            if (idIdx < 0 || numIdx < 0) return emptyList()
            while (c.moveToNext()) {
                val id = c.getString(idIdx) ?: continue
                val name = (if (nameIdx >= 0) c.getString(nameIdx) else null)?.trim().orEmpty()
                val num = c.getString(numIdx)?.trim().orEmpty()
                if (num.isEmpty()) continue
                val entry = byId.getOrPut(id) { name to mutableListOf() }
                if (num !in entry.second) entry.second.add(num)
                // Cap the set that goes over BLE.
                //
                // The read was unbounded and the whole thing was re-sent on
                // every change AND every reconnect. A 2000-contact address book
                // is roughly 300 chunks at 450 bytes — about eight times the
                // ~36-chunk run this link is documented as handling reliably.
                // One lost notify in that burst desyncs the receive cipher,
                // which drops the session, taking any CALL frame in flight with
                // it; the laptop then renders a stale disk cache with no sign
                // anything failed. A truncated list that arrives beats a
                // complete one that never does.
                if (byId.size >= MAX_CONTACTS) break
            }
        }
        if (byId.size >= MAX_CONTACTS) {
            Log.i(tag, "contacts capped at $MAX_CONTACTS for the BLE burst")
        }
        return byId.map { (id, v) ->
            Contact(
                id = id,
                name = v.first.ifEmpty { v.second.firstOrNull().orEmpty() },
                numbers = v.second,
            )
        }
    }

    private fun hasPermission(): Boolean =
        ContextCompat.checkSelfPermission(
            context,
            android.Manifest.permission.READ_CONTACTS,
        ) == PackageManager.PERMISSION_GRANTED
}
