package com.vortex.a3.service

import android.util.Log
import kotlinx.coroutines.launch

/**
 * Phone→laptop clipboard + instant-share outbound (text / image / file offers) —
 * split out of [VortexStack]. The QS tile / quick-send / share-sheet reads the
 * clipboard or a shared blob (foreground, per Android's rules) and emits on a
 * VortexService bus; these collectors push it to the laptop (small text inline,
 * long text chunked, images/files as an OFFER the laptop pulls over LAN).
 * All gated by the local clipboard-sync toggle; content is never logged.
 */

/** Wire the three outbound collectors (clipboard text, clipboard image, shared
 *  file). Called once from [VortexStack.start]. */
internal fun VortexStack.startClipboardOutbound() {
    // Clipboard sync (phone→laptop): the Quick Settings tile / quick-send
    // activity reads the clipboard (foreground, per Android's rule) and
    // emits its text here; push it to the laptop as a CLIPBOARD frame.
    scope.launch {
        VortexService.clipboardBus.collect { text ->
            if (!com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) return@collect
            val trimmed = text.trim()
            if (trimmed.isEmpty()) return@collect
            val capped = if (trimmed.length > com.vortex.a3.core.clipboard.ClipboardText.MAX_TEXT_CHARS) {
                trimmed.take(com.vortex.a3.core.clipboard.ClipboardText.MAX_TEXT_CHARS)
            } else {
                trimmed
            }
            val utf8Len = capped.toByteArray(Charsets.UTF_8).size
            if (utf8Len <= com.vortex.a3.core.clipboard.ClipboardText.MAX_SINGLE_FRAME_TEXT_BYTES) {
                val json = clipboardJsonBytes(capped)
                for (peer in peerStore.list()) {
                    gattServer?.sendClipboardEncrypted(peer.peerStaticPub, json)
                }
            } else {
                // Long text → chunk over CLIPBOARD_TEXT, paced so the BLE
                // notify queue doesn't drop frames (same 12ms as images).
                val chunks = com.vortex.a3.core.clipboard.ClipboardText.buildChunks(capped)
                for (peer in peerStore.list()) {
                    for (chunk in chunks) {
                        gattServer?.sendClipboardTextChunkEncrypted(peer.peerStaticPub, chunk)
                        kotlinx.coroutines.delay(12)
                    }
                }
                Log.i(VortexStack.TAG, "clipboard: long text sent chunked ($utf8Len bytes, ${chunks.size} chunks)")
            }
        }
    }

    // Clipboard / shared IMAGE (phone→laptop): stash the PNG and signal the
    // laptop with a small OFFER frame; the laptop PULLS it over the reliable
    // LAN bulk-sync (BLE notify is too lossy for hundreds of image chunks).
    scope.launch {
        VortexService.clipboardImageBus.collect { png ->
            if (!com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) return@collect
            if (png.isEmpty()) return@collect
            val token = com.vortex.a3.core.clipboard.ClipboardImageStore.stash(png)
            val o = org.json.JSONObject()
            o.put("token", token)
            o.put("bytes", png.size)
            val offer = o.toString().toByteArray(Charsets.UTF_8)
            var delivered = false
            for (peer in peerStore.list()) {
                if (gattServer?.sendClipboardImageOfferEncrypted(peer.peerStaticPub, offer) == true) {
                    delivered = true
                }
            }
            // Not retried, unlike a file: a clipboard image is transient, and by
            // the time the link is back the user has copied something else. But
            // don't claim it was offered when it wasn't.
            if (delivered) {
                Log.i(VortexStack.TAG, "clipboard image offered to laptop (${png.size} bytes, token=$token)")
            } else {
                Log.w(VortexStack.TAG, "clipboard image offer couldn't go out (BLE link down?)")
            }
        }
    }

    // Clipboard / shared FILE (phone→laptop): same offer+LAN-pull path as
    // images, but the OFFER carries name+mime so the laptop writes a real
    // file and makes it pasteable. Bytes ride the same store/pull as images.
    scope.launch {
        VortexService.clipboardFileBus.collect { file ->
            if (!com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) return@collect
            if (file.bytes.isEmpty()) return@collect
            val token = com.vortex.a3.core.clipboard.ClipboardBlobStore.stash(file.bytes)
            val o = org.json.JSONObject()
            o.put("token", token)
            o.put("bytes", file.bytes.size)
            o.put("name", file.name)
            o.put("mime", file.mime)
            val offer = o.toString().toByteArray(Charsets.UTF_8)
            Log.i(VortexStack.TAG, "clipboard file offered to laptop ('${file.name}', ${file.bytes.size} bytes, token=$token)")
            // Tracked until the laptop has actually FETCHED the bytes: the OFFER
            // is a fire-and-forget BLE notify that goes nowhere on a dead link,
            // and even a delivered one can sit unfetched. Retries, warms the LAN
            // path on delivery, and toasts here if it ends up nowhere.
            offerFileToLaptop(token, file.name, offer)
            // Big file → bring up Wi-Fi Direct for a high-speed direct pull. Small
            // files stay on the router path (the ~6s Wi-Fi switch isn't worth it).
            if (file.bytes.size >= 4 * 1024 * 1024) maybeStartWifiDirect()
        }
    }
}

/** Serialise clipboard text to the `{text, ts}` wire JSON (matches the
 *  Rust `ClipboardMirror`). Capped to keep it inside one BLE frame. */
internal fun VortexStack.clipboardJsonBytes(text: String): ByteArray {
    val capped = if (text.length > 4096) text.take(4096) else text
    val o = org.json.JSONObject()
    o.put("text", capped)
    o.put("ts", System.currentTimeMillis())
    return o.toString().toByteArray(Charsets.UTF_8)
}
