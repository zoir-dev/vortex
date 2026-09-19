package com.vortex.a3.core.clipboard

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.util.Log
import android.widget.Toast
import com.vortex.a3.service.VortexService

/**
 * "Vortex" in the text-selection toolbar, beside Copy and Share.
 *
 * Select text anywhere, tap Vortex, and it lands on the laptop's clipboard —
 * one tap, no share sheet and no app picker.
 *
 * This exists because AUTOMATIC capture is not reachable. Reading the clipboard
 * in the background needs the caller to be the default IME, the focused window,
 * or a holder of `READ_CLIPBOARD_IN_BACKGROUND` — which is `signature|role` and
 * cannot be granted to a sideloaded app by any route (see [ClipboardAccess] for
 * the measurements). So [ClipboardListener]'s callback is never delivered and
 * every working path has to be one the user triggers.
 *
 * `ACTION_PROCESS_TEXT` is the cheapest of those by a wide margin: the system
 * hands us the selected text directly in the intent, so there is no clipboard
 * read to be refused and no invisible foreground trampoline to bounce through
 * (unlike the Quick Settings tile, which needs [ClipboardQuickSendActivity] to
 * take focus first).
 */
class ProcessTextActivity : Activity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        overridePendingTransition(0, 0)

        val selected = intent
            ?.getCharSequenceExtra(Intent.EXTRA_PROCESS_TEXT)
            ?.toString()
            ?.trim()

        if (selected.isNullOrEmpty()) {
            Log.w(TAG, "process-text: empty selection — nothing to do")
            Toast.makeText(this, "Nothing to send", Toast.LENGTH_SHORT).show()
        } else {
            // Same bus as the tile and the share sheet, so this inherits the
            // cap, the chunking, the per-peer send and the LAN fallback for
            // free. Length only in the log — never the content.
            VortexService.clipboardBus.tryEmit(selected)
            Log.i(TAG, "process-text: forwarded ${selected.length} chars to the laptop clipboard")
            Toast.makeText(this, "Sending text to laptop…", Toast.LENGTH_SHORT).show()
        }

        // Deliberately NO setResult: returning EXTRA_PROCESS_TEXT would make the
        // host app REPLACE the user's selection with whatever we sent back.
        // Sending a copy to the laptop must never edit what they were reading —
        // and in a read-only view (EXTRA_PROCESS_TEXT_READONLY) it would be
        // silently discarded anyway, so the two cases behave the same.
        finish()
        overridePendingTransition(0, 0)
    }

    companion object {
        private const val TAG = "VortexProcessText"
    }
}
