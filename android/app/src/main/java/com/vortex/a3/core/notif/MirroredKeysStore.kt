package com.vortex.a3.core.notif

import android.content.Context

/**
 * Durable backing for the notification listener's "keys we mirrored to the
 * laptop" set. The in-memory set is the working copy, but MIUI freely kills /
 * rebinds the listener (and the whole process); without persistence the set is
 * lost, so after a restart [MediaNotificationListenerService.onNotificationRemoved]
 * can't tell a key was mirrored → it never syncs the dismissal → the laptop
 * keeps showing a notification the user already cleared on the phone.
 *
 * Bounded so a busy status bar can't grow it without limit. Order in a
 * SharedPreferences StringSet is undefined, so over-cap eviction is arbitrary —
 * fine here: the oldest keys are the least likely to still be on screen.
 */
object MirroredKeysStore {
    private const val PREFS = "vortex_notif_mirrored"
    private const val FIELD = "keys"
    private const val MAX = 300

    private fun prefs(ctx: Context) =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The persisted keys (empty set if none / on any error). */
    fun load(ctx: Context): Set<String> =
        try {
            prefs(ctx).getStringSet(FIELD, emptySet())?.toSet() ?: emptySet()
        } catch (_: Throwable) {
            emptySet()
        }

    /** Persist a snapshot of the current in-memory set (bounded to [MAX]).
     *  Uses apply() (async): this is called on the listener's main thread on
     *  every notification post/removal, so a synchronous disk write would risk
     *  jank/ANR. Durability here is best-effort dismiss-sync, not security — the
     *  worst case of losing the last write is one stale notification, which the
     *  reconnect catch-up already mitigates. */
    fun save(ctx: Context, keys: Set<String>) {
        // `take` on a HashSet takes an ARBITRARY subset — hash order, not
        // recency — so trimming could equally well drop a notification that is
        // on screen right now and keep one from hours ago. The dismissal of the
        // dropped one then never syncs. An Android SBN key ends with the post
        // ID, which increases over time, so sorting by key keeps the newest.
        val bounded = if (keys.size > MAX) keys.sorted().takeLast(MAX).toSet() else keys
        try {
            prefs(ctx).edit().putStringSet(FIELD, bounded).apply()
        } catch (_: Throwable) {
            // Best-effort; a failed persist only costs us the restart guarantee.
        }
    }
}
