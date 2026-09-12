package com.vortex.a3.core.handoff

import org.json.JSONObject

/**
 * A page the phone hands off to the laptop (continuity style). Mirrors the
 * Rust `core::handoff::HandoffEvent`; rides the Noise-sealed AUDIO_SIGNAL stream
 * as frame [com.vortex.a3.core.ble.FrameType.HANDOFF].
 *
 * @param url      the page URL (empty = "stop handing off" → clears the laptop pill)
 * @param title    page/tab title for the pill label
 * @param appId    source app package (e.g. com.android.chrome) for its icon
 * @param openNow  true = an explicit Share → the laptop opens it immediately;
 *                 false = the live accessibility read → a "continue" pill
 * @param id       identity of an [openNow] request, so the laptop opens it
 *                 exactly once. The event also rides the AppState snapshot as a
 *                 backstop for a dead BLE link, and that snapshot is
 *                 republished on every heartbeat — without an identity the
 *                 laptop cannot tell a re-assert from a fresh share, and
 *                 re-opened the browser every ~12s forever. Left empty on the
 *                 live-read path (the laptop only dedups the open path).
 */
data class HandoffEvent(
    val url: String,
    val title: String = "",
    val appId: String = "",
    val openNow: Boolean = false,
    val id: String = "",
) {
    fun toJsonBytes(): ByteArray {
        val o = JSONObject()
        o.put("url", url)
        if (title.isNotEmpty()) o.put("title", title)
        if (appId.isNotEmpty()) o.put("app_id", appId)
        o.put("open_now", openNow)
        if (id.isNotEmpty()) o.put("id", id)
        return o.toString().toByteArray(Charsets.UTF_8)
    }
}
