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
                    com.vortex.a3.core.handoff.HandoffEvent(url = url, title = title, openNow = true),
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

        var sent = 0
        for (uri in uris) {
            val file = ClipboardFileReader.read(this, uri)
            if (file != null) {
                VortexService.clipboardFileBus.tryEmit(file)
                Log.i(TAG, "share: forwarded file '${file.name}' (${file.bytes.size} bytes)")
                sent++
            } else {
                Log.w(TAG, "share: couldn't read $uri")
            }
        }
        val msg = when {
            sent == 0 -> "Couldn't read the shared file(s)"
            sent == 1 -> "Sending file to laptop…"
            else -> "Sending $sent files to laptop…"
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
