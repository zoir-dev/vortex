package com.vortex.a3.core.mirror

import android.content.Context
import android.content.Intent
import android.util.Log
import com.vortex.a3.ui.LaptopMirrorActivity

/**
 * Phone-side coordinator for the LAPTOP→phone screen mirror (view-only).
 *
 * Flow:
 *  1. User taps "view laptop screen" → [requestView] flips [requestActive] on and
 *     nudges the heartbeat. The AppState builder ships `laptopMirrorReq = true`.
 *  2. The laptop sees the request, pops its screen-share consent and, on accept,
 *     starts casting + advertises `{ip, port, key}` in ITS AppState.
 *  3. The phone's AppState handler calls [onLaptopOffer], which launches
 *     [LaptopMirrorActivity] once to connect + decode.
 *  4. Closing the viewer calls [onViewerClosed], dropping the request so the
 *     laptop releases its capture.
 *
 * One viewer at a time; all state is process-global (a single user, a single
 * laptop). The 32-byte media key is the laptop→phone key, delivered over the
 * Noise-sealed control channel — never derived or logged here.
 */
object LaptopMirror {
    private const val TAG = "LaptopMirror"

    /** Consecutive offer-less heartbeats before we tear the viewer down (the
     *  laptop sends its AppState every ~1-2s, so ~5 ≈ several seconds of
     *  confirmed "not casting" — well past any start-up race). */
    private const val MISS_LIMIT = 5

    /** Heartbeats to wait for an offer before giving up on a request that has
     *  produced neither a cast nor an error. Higher than [MISS_LIMIT]: the
     *  laptop legitimately takes a moment here (screen-share consent, portal
     *  session, encoder start), and giving up while the user is still reading
     *  the consent dialog would be worse than waiting. */
    private const val SILENT_LIMIT = 10

    /** True while the user wants to see the laptop screen. Read by the AppState
     *  builder → `laptopMirrorReq`; the laptop casts only while it's set. */
    @Volatile
    var requestActive: Boolean = false
        private set

    /** Set once the viewer is on screen, so a re-sent offer doesn't relaunch it. */
    @Volatile
    private var viewerOpen: Boolean = false

    /** Consecutive heartbeats seen WITHOUT a `laptop_cast` offer while the viewer
     *  is open. We only tear the viewer down after several in a row — a single
     *  transient null (a heartbeat built the instant the cast was (re)starting)
     *  must NOT close it, or it thrashes (close→req off→cast stop→restart→new
     *  key→AEAD mismatch→black). Real "Stop sharing" yields a sustained null. */
    @Volatile
    private var castMisses: Int = 0

    /** Set by VortexStack: ship the local AppState NOW (BLE + LAN) so a request
     *  flip reaches the laptop within ~1s instead of waiting a heartbeat. */
    @Volatile
    var onRequestChanged: (() -> Unit)? = null

    /** Set by the live viewer Activity to finish itself; cleared on close. Lets
     *  the stack tear the viewer down when the LAPTOP stops casting. */
    @Volatile
    var viewerCloser: (() -> Unit)? = null

    /** Which kind of screen the current request is for: a second monitor to drag
     *  windows onto (`true`), or a view of the screen the laptop already has
     *  (`false`). Shipped as `laptop_mirror_extend` so the laptop obeys the
     *  choice made HERE — it used to be the laptop's own setting, which meant
     *  walking over to it to change what the phone was about to show. */
    @Volatile
    var extendWanted: Boolean = false
        private set

    /** User tapped the screen button and picked a kind. */
    fun requestView(extend: Boolean) {
        if (requestActive) return
        extendWanted = extend
        requestActive = true
        Log.i(TAG, "view-laptop requested (extend=$extend)")
        onRequestChanged?.invoke()
    }

    /** The laptop is casting: launch the viewer once. The phone is the video
     *  SERVER (the laptop dials us — only laptop→phone connections survive real
     *  networks), so we just need the port to listen on + the key to open with. */
    fun onLaptopOffer(ctx: Context, port: Int, key: ByteArray) {
        castMisses = 0 // an offer is present → reset the teardown debounce
        if (!requestActive || viewerOpen) return
        if (port == 0 || key.size != 32) {
            Log.w(TAG, "ignoring malformed laptop cast offer")
            return
        }
        viewerOpen = true
        Log.i(TAG, "laptop cast offer → launching viewer (server :$port)")
        val intent = Intent(ctx, LaptopMirrorActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            putExtra(LaptopMirrorActivity.EXTRA_PORT, port)
            putExtra(LaptopMirrorActivity.EXTRA_KEY, key)
        }
        try {
            ctx.startActivity(intent)
        } catch (t: Throwable) {
            Log.w(TAG, "viewer launch failed: ${t.message}")
            viewerOpen = false
        }
    }

    /** Viewer closed → stop requesting so the laptop releases its capture. */
    fun onViewerClosed(@Suppress("UNUSED_PARAMETER") ctx: Context) {
        viewerOpen = false
        if (!requestActive) return
        requestActive = false
        Log.i(TAG, "viewer closed → cast request cleared")
        onRequestChanged?.invoke()
    }

    /** The laptop reported that it CANNOT cast (AppState `laptop_cast_error`).
     *
     *  Clears the request, so we stop re-asserting something the laptop will
     *  keep failing, and so [requestView] stops early-returning — its
     *  `requestActive` guard is what made every further tap a no-op. Also hands
     *  the reason to the UI: previously the user tapped, nothing happened, and
     *  the explanation existed only in the laptop's log.
     *
     *  Idempotent: the laptop re-sends the same reason on every heartbeat until
     *  it sees us stop asking, so only the first one does anything. */
    fun onLaptopCastFailed(reason: String) {
        if (!requestActive) return
        requestActive = false
        castMisses = 0
        Log.w(TAG, "laptop cannot cast: $reason → request cleared")
        onCastFailed?.invoke(reason)
        viewerCloser?.invoke() // no-op when no viewer is up
        onRequestChanged?.invoke() // tell the laptop at once, don't wait a heartbeat
    }

    /** Set by the UI to surface a cast failure (toast/dialog). */
    @Volatile
    var onCastFailed: ((String) -> Unit)? = null

    /** A request that produced NO offer and NO error — the laptop never answered
     *  at all (out of range, killed mid-request, an older build with no
     *  `laptop_cast_error`). Give up after several heartbeats.
     *
     *  [onLaptopCastEnded] cannot cover this: it returns early unless a viewer is
     *  already open, so a request that never got as far as a viewer had no
     *  timeout whatsoever and stayed latched indefinitely. */
    fun onLaptopCastSilent() {
        if (!requestActive || viewerOpen) return
        if (++castMisses < SILENT_LIMIT) return
        castMisses = 0
        requestActive = false
        Log.w(TAG, "no cast offer after $SILENT_LIMIT heartbeats → request cleared")
        onCastFailed?.invoke("The laptop did not respond")
        onRequestChanged?.invoke()
    }

    /** The LAPTOP stopped casting (its AppState `laptop_cast` cleared — user hit
     *  "Stop sharing" on the laptop, or the capture errored). Tear the viewer
     *  down on this side too so the screens stay in sync. No-op if no viewer. */
    fun onLaptopCastEnded() {
        if (!viewerOpen) return
        // Debounce: only tear down after a SUSTAINED absence of the offer, so a
        // lone transient null doesn't kill a healthy session.
        if (++castMisses < MISS_LIMIT) return
        Log.i(TAG, "laptop stopped casting ($castMisses misses) → closing viewer")
        castMisses = 0
        viewerCloser?.invoke() // Activity.finish() → onViewerClosed() clears state
    }
}
