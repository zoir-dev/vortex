package com.vortex.a3.core.clipboard

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.widget.Toast
import com.vortex.a3.service.VortexService

/**
 * Share-sheet target: the user picks "Vortex" when sharing to the laptop —
 * instant-share style. Three kinds of share, in priority order:
 *
 *  1. text containing a URL  → browsing handoff (the laptop opens the page),
 *  2. any attachment         → FILE to the laptop's download folder,
 *  3. plain text             → the laptop's CLIPBOARD.
 *
 * Files arrive as granted `content://` URIs in the intent (no focus trick
 * needed) and are handed to [VortexService] as FILEs. Handles both single
 * (`ACTION_SEND`) and multi (`ACTION_SEND_MULTIPLE`) shares — file managers use
 * the latter for a multi-selection, which is why a SEND-only filter never
 * appeared.
 */
class ShareReceiverActivity : Activity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        overridePendingTransition(0, 0)

        // Shared TEXT (a URL from Chrome's "Share") → browsing handoff: the
        // laptop opens it in the browser. Handled before files (a text/plain
        // share carries EXTRA_TEXT, not a stream).
        if (intent?.action == Intent.ACTION_SEND) {
            val url = intent.getStringExtra(Intent.EXTRA_TEXT)?.let { extractUrl(it) }
            if (url != null) {
                val title = intent.getStringExtra(Intent.EXTRA_SUBJECT)
                    ?.takeIf { it.isNotBlank() } ?: ""
                VortexService.handoffBus.tryEmit(
                    com.vortex.a3.core.handoff.HandoffEvent(
                        url = url,
                        title = title,
                        openNow = true,
                        // Identifies THIS share so the laptop opens it once, no
                        // matter how many transports and heartbeats deliver it.
                        // A fresh id per share is what keeps re-sharing the same
                        // page working (the laptop dedups on the id, not the URL).
                        id = java.util.UUID.randomUUID().toString(),
                    ),
                )
                Log.i(TAG, "share: forwarded a page to the laptop")
                Toast.makeText(this, "Opening on laptop…", Toast.LENGTH_SHORT).show()
                finish()
                overridePendingTransition(0, 0)
                return
            }
        }

        val uris: List<Uri> = when (intent?.action) {
            Intent.ACTION_SEND -> listOfNotNull(streamExtra())
            Intent.ACTION_SEND_MULTIPLE -> streamListExtra()
            else -> emptyList()
        }

        // Shared plain TEXT (no URL in it, so the handoff above passed) → the
        // laptop's CLIPBOARD, via the same bus the Quick Settings tile uses, so
        // it inherits the cap + chunking + per-peer send. Without this, a text
        // share fell through to the file loop below with nothing to read and
        // died on "Couldn't read the shared file(s)" — the manifest advertises
        // text/plain, so Vortex offers itself for text and must honour it.
        //
        // Guarded on `uris.isEmpty()`: a share can carry a caption ALONGSIDE an
        // attachment (EXTRA_TEXT + EXTRA_STREAM), and there the file is the
        // payload the user meant.
        if (uris.isEmpty() && intent?.action == Intent.ACTION_SEND) {
            val text = intent.getStringExtra(Intent.EXTRA_TEXT)?.trim()
            if (!text.isNullOrEmpty()) {
                VortexService.clipboardBus.tryEmit(text)
                Log.i(TAG, "share: forwarded ${text.length} chars to the laptop clipboard")
                Toast.makeText(this, "Sending text to laptop…", Toast.LENGTH_SHORT).show()
                finish()
                overridePendingTransition(0, 0)
                return
            }
        }

        // Hand the whole list to the service and let it pace itself.
        //
        // Deliberately NOT read here: reading every file up front is what made
        // an 835 MB share an OutOfMemoryError, and what made a 150-file share
        // lose most of its files to buffer overflow. The service reads each
        // file on its turn (see ShareQueue), so memory is flat and the batch is
        // bounded by real delivery instead of a cap that refuses work.
        //
        // ClipData + FLAG_GRANT_READ_URI_PERMISSION is what carries the share
        // sheet's read grant across to the service; plain extras would not.
        if (uris.isEmpty()) {
            // Nothing readable in the share and the text path above did not
            // claim it. `uris.first()` below would throw.
            Log.w(TAG, "share: no URIs and no text — nothing to do")
            Toast.makeText(this, "Nothing to send", Toast.LENGTH_SHORT).show()
            finish()
            overridePendingTransition(0, 0)
            return
        }
        val clip = android.content.ClipData.newUri(contentResolver, "vortex-share", uris.first())
        for (u in uris.drop(1)) clip.addItem(android.content.ClipData.Item(u))
        val svc = android.content.Intent(this, VortexService::class.java).apply {
            action = VortexService.ACTION_ENQUEUE_SHARE
            clipData = clip
            addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        try {
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.O) {
                startForegroundService(svc)
            } else {
                startService(svc)
            }
        } catch (e: Exception) {
            Log.w(TAG, "couldn't hand the share to the service: ${e.message}")
            Toast.makeText(this, "Couldn't start the transfer", Toast.LENGTH_SHORT).show()
            finish()
            overridePendingTransition(0, 0)
            return
        }
        Log.i(TAG, "share: handed ${uris.size} file(s) to the queue")
        // One toast. Per-file progress is the notification the queue maintains —
        // a toast per file meant 150 toasts for a 150-file share.
        val msg = if (uris.size == 1) {
            "Sending file to laptop…"
        } else {
            "Queued ${uris.size} files for the laptop"
        }
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

        finish()
        overridePendingTransition(0, 0)
    }

    /** Pull the first http(s) URL out of shared text (Chrome may share "Title
     *  https://…" or "Look: https://…"). Returns null if there's no web URL. */
    private fun extractUrl(text: String): String? =
        Regex("""https?://\S+""").find(text)?.value?.trimEnd('.', ',', ')', ']', '"', '\'')

    private fun streamExtra(): Uri? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
    } else {
        @Suppress("DEPRECATION") intent.getParcelableExtra(Intent.EXTRA_STREAM)
    }

    private fun streamListExtra(): List<Uri> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION") intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)
        } ?: emptyList()

    companion object {
        private const val TAG = "VortexShare"
    }
}
