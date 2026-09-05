package com.vortex.a3.core.media

import android.app.Notification
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import com.vortex.a3.core.notif.LiveActivity
import com.vortex.a3.core.notif.NotificationMirror
import com.vortex.a3.core.notif.NotificationMirrorSetting
import com.vortex.a3.service.VortexService

/**
 * Notification-listener component, used for TWO things:
 *
 *  1. **MediaSessionManager access** — the platform only lets
 *     [android.media.session.MediaSessionManager.getActiveSessions] work
 *     when the caller owns an *enabled* notification-listener (or holds
 *     the system-only MEDIA_CONTENT_CONTROL permission). The user enables
 *     it once in Settings → Notification access.
 *
 *  2. **Notification mirroring** — forward posted notifications to the
 *     paired laptop for desktop display. Content rides the Noise-sealed
 *     BLE link (frame type NOTIFICATION) and is never logged verbatim.
 *
 * Captured notifications are published to [VortexService.notificationBus];
 * [com.vortex.a3.service.VortexStack] consumes them and sends over BLE.
 * Heavy filtering keeps noise out (ongoing/foreground, group summaries,
 * our own, empty).
 */
class MediaNotificationListenerService : NotificationListenerService() {

    /** Recent (key → last-sent elapsedRealtime) for dedup. Bounded. */
    private val recentSends = LinkedHashMap<String, Long>()

    /** SBN keys we've mirrored to the laptop — so we sync ONLY their
     *  dismissals, exactly once (also our loop guard for cancelNotification). */
    private val mirroredKeys = HashSet<String>()

    /** SBN keys currently treated as LIVE ACTIVITIES (ongoing progress pills),
     *  with the last-sent timestamp + content so we rate-limit the update
     *  stream and emit an `ended` when the activity is removed. Guarded by
     *  [liveKeys]'s monitor. */
    private val liveKeys = HashSet<String>()
    private val liveRecentMs = HashMap<String, Long>()
    private val liveContent = HashMap<String, String>()

    // Re-push every still-active live activity on a timer so the laptop's
    // staleness sweeper keeps seeing it even when the app's notification content
    // hasn't changed for a while (steady navigation between turn instructions —
    // otherwise the pill vanishes mid-trip). Bypasses handleLiveActivity's
    // content-dedup on purpose; stops when no live activity is active.
    private val liveHeartbeatHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private val liveHeartbeat = object : Runnable {
        override fun run() {
            if (NotificationMirrorSetting.isEnabled()) {
                for (live in activeLive.values) {
                    VortexService.liveActivityBus.tryEmit(live)
                }
            }
            liveHeartbeatHandler.postDelayed(this, LIVE_HEARTBEAT_MS)
        }
    }

    // Outgoing-call connect poll: onNotificationPosted doesn't reliably re-fire
    // when the dialer flips its notification to the connected (chronometer)
    // state, so while an outgoing call is active-but-not-connected we re-scan
    // the LIVE notifications every ~1.5s and run the same detection. Stops the
    // moment the call connects or ends.
    private val callPollHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private val callPoll = object : Runnable {
        override fun run() {
            val cur = VortexService.currentCall
            if (cur == null || cur.phase != com.vortex.a3.core.call.CallEvent.PHASE_ACTIVE ||
                !cur.outgoing || cur.connected
            ) {
                return // nothing to poll for → stop (don't reschedule)
            }
            try {
                activeNotifications?.firstOrNull { isCallNotification(it) }
                    ?.let { trackCallNotification(it) }
            } catch (_: Throwable) {
            }
            val now = VortexService.currentCall
            if (now != null && now.phase == com.vortex.a3.core.call.CallEvent.PHASE_ACTIVE &&
                now.outgoing && !now.connected
            ) {
                callPollHandler.postDelayed(this, 1500)
            }
        }
    }

    /** Kick the connect poll while an outgoing call is dialing/un-connected. */
    private fun maybeStartCallPoll() {
        val cur = VortexService.currentCall ?: return
        if (cur.phase == com.vortex.a3.core.call.CallEvent.PHASE_ACTIVE && cur.outgoing && !cur.connected) {
            callPollHandler.removeCallbacks(callPoll)
            callPollHandler.postDelayed(callPoll, 1500)
        }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification?) {
        val sbn = sbn ?: return
        try {
            // Outgoing-call connect detection (side-effect; the call banner/pill
            // rides the separate CALL frame, this only refines its timer).
            trackCallNotification(sbn)
            maybeStartCallPoll()
            // Live activity (ongoing + progress/category) → top-bar pill path,
            // routed separately from one-shot notification mirroring.
            if (handleLiveActivity(sbn)) return
            val mirror = buildMirror(sbn) ?: return
            // Dedup / rate-limit: collapse the same content re-posted within
            // DEDUP_WINDOW_MS (chatty apps re-post on every minor update —
            // progress bars, typing, repeated lines). Keyed on app+title+text.
            val key = "${mirror.app}|${mirror.title}|${mirror.text}"
            val now = android.os.SystemClock.elapsedRealtime()
            val last = recentSends[key]
            if (last != null && now - last < DEDUP_WINDOW_MS) return
            recentSends[key] = now
            pruneRecent(now)
            if (mirror.key.isNotEmpty()) {
                synchronized(mirroredKeys) { mirroredKeys.add(mirror.key) }
                persistMirroredKeys()
            }
            VortexService.notificationBus.tryEmit(mirror)
        } catch (t: Throwable) {
            // Never let a malformed notification crash the listener.
            Log.w(TAG, "onNotificationPosted: ${t.message}")
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification?) {
        val key = sbn?.key ?: return
        // Dialer call notification gone → the call ended; emit END at once.
        handleCallRemoved(key)
        // A live activity ended → clear its pill on the laptop, exactly once.
        val wasLive = synchronized(liveKeys) {
            val had = liveKeys.remove(key)
            if (had) { liveRecentMs.remove(key); liveContent.remove(key) }
            had
        }
        activeLive.remove(key)
        if (wasLive) {
            if (NotificationMirrorSetting.isEnabled()) {
                VortexService.liveActivityBus.tryEmit(LiveActivity(key = key, ended = true))
            }
            return
        }
        // Only sync the dismissal of notifications WE mirrored — and only
        // once. Removing from the set first also makes our own
        // cancelNotification() (from a laptop-driven dismiss) a no-op here,
        // so there's no echo loop.
        val wasMirrored = synchronized(mirroredKeys) { mirroredKeys.remove(key) }
        if (wasMirrored) persistMirroredKeys()
        if (!wasMirrored) return
        if (!NotificationMirrorSetting.isEnabled()) return
        // Tell the laptop to close its mirrored copy.
        VortexService.notificationBus.tryEmit(
            NotificationMirror(app = "", title = "", text = "", ts = 0L, key = key, dismiss = true),
        )
    }

    override fun onListenerConnected() {
        instance = this
        // Restore the mirrored-keys set persisted before the listener/process
        // was killed (MIUI does this freely). Without it, a notification mirrored
        // before the gap wouldn't be recognized on its later removal → its
        // dismissal would never sync → the laptop keeps a notification the user
        // already cleared. Loaded BEFORE catch-up so catch-up correctly skips
        // already-mirrored notifications instead of re-showing them.
        try {
            val restored = com.vortex.a3.core.notif.MirroredKeysStore.load(this)
            if (restored.isNotEmpty()) {
                synchronized(mirroredKeys) { mirroredKeys.addAll(restored) }
                Log.i(TAG, "restored ${restored.size} mirrored keys from disk")
            }
        } catch (t: Throwable) {
            Log.w(TAG, "restore mirrored keys: ${t.message}")
        }
        // Reconcile the mirrored-keys set against what is actually on screen.
        //
        // Restored keys were never checked, so a notification the user cleared
        // while we were unbound stayed in the set for the life of the process —
        // and its eventual real removal was then read as a dismissal to sync,
        // or swallowed as already-removed. Keep only keys that still exist.
        try {
            val liveKeysNow = activeNotifications?.map { it.key }?.toSet().orEmpty()
            val dropped = synchronized(mirroredKeys) {
                val before = mirroredKeys.size
                mirroredKeys.retainAll(liveKeysNow)
                before - mirroredKeys.size
            }
            if (dropped > 0) {
                persistMirroredKeys()
                Log.i(TAG, "dropped $dropped mirrored key(s) that are no longer active")
            }
        } catch (t: Throwable) {
            Log.w(TAG, "reconcile mirrored keys: ${t.message}")
        }
        // Seed from notifications that are ALREADY active: their
        // onNotificationPosted fired before we (re)bound — e.g. a navigation
        // that was already running — so without this its live-activity pill
        // would never appear on the laptop until the app next posts an update.
        try {
            // Re-seed from what is ACTUALLY live, and drop anything we were
            // still tracking that is not — the reconciliation the old code
            // never did: it only ever added, so a key removed while we were
            // unbound stayed in the map for the life of the process.
            val live = activeNotifications?.toList().orEmpty()
            val stillLive = live.map { it.key }.toSet()
            val gone = activeLive.keys.filter { it !in stillLive }
            for (key in gone) {
                activeLive.remove(key)
                if (NotificationMirrorSetting.isEnabled()) {
                    VortexService.liveActivityBus.tryEmit(LiveActivity(key = key, ended = true))
                }
            }
            if (gone.isNotEmpty()) {
                Log.i(TAG, "cleared ${gone.size} live activity(ies) that ended while unbound")
            }
            live.forEach { handleLiveActivity(it) }
        } catch (t: Throwable) {
            Log.w(TAG, "seed active live activities: ${t.message}")
        }
        // Seed call-connect detection: if an outgoing call connected while the
        // listener was dead (MIUI kills the binding freely), its dialer
        // chronometer notification is ALREADY active and no fresh
        // onNotificationPosted will fire for it — detect it now + start the poll
        // so the laptop pill gets its timer instead of being stuck on "Calling…".
        try {
            activeNotifications?.firstOrNull { isCallNotification(it) }?.let { trackCallNotification(it) }
            maybeStartCallPoll()
        } catch (t: Throwable) {
            Log.w(TAG, "seed call connect: ${t.message}")
        }
        // Catch-up for ONE-SHOT notifications: anything posted while the
        // listener was dead (MIUI kills the binding freely; Android rebinds
        // later) was never captured and would otherwise be lost forever.
        // Mirror the still-active, recent ones we haven't sent. Bounded to
        // the freshest few so a process restart doesn't replay the user's
        // whole status bar onto the laptop. DELAYED a few seconds: on a
        // fresh process the bus consumer (VortexStack) races this callback,
        // and notificationBus has replay=0 — an instant emit would vanish
        // unheard. The delay also dodges the "activeNotifications empty
        // right at onListenerConnected" framework quirk.
        liveHeartbeatHandler.postDelayed({
            try {
                val now = System.currentTimeMillis()
                activeNotifications
                    ?.filter { sbn ->
                        now - sbn.postTime < CATCHUP_WINDOW_MS &&
                            synchronized(mirroredKeys) { sbn.key !in mirroredKeys }
                    }
                    ?.sortedBy { it.postTime }
                    ?.takeLast(CATCHUP_MAX)
                    ?.forEach { sbn ->
                        val mirror = buildMirror(sbn) ?: return@forEach
                        if (mirror.key.isNotEmpty()) {
                            synchronized(mirroredKeys) { mirroredKeys.add(mirror.key) }
                            persistMirroredKeys()
                        }
                        Log.i(TAG, "catch-up mirror after listener gap: ${mirror.app}")
                        VortexService.notificationBus.tryEmit(mirror)
                    }
            } catch (t: Throwable) {
                Log.w(TAG, "catch-up mirror: ${t.message}")
            }
        }, CATCHUP_DELAY_MS)
        liveHeartbeatHandler.removeCallbacks(liveHeartbeat)
        liveHeartbeatHandler.postDelayed(liveHeartbeat, LIVE_HEARTBEAT_MS)
    }

    override fun onListenerDisconnected() {
        liveHeartbeatHandler.removeCallbacks(liveHeartbeat)
        callPollHandler.removeCallbacks(callPoll)
        if (instance === this) instance = null
        // Forget the live activities we were tracking.
        //
        // While unbound we cannot be told that one ended, so anything left here
        // is a claim we can no longer stand behind. Keeping them meant a
        // navigation that finished during the gap was re-sent by the 25s
        // heartbeat forever — and because every re-send refreshed it, the
        // laptop's own 90-second staleness sweeper never got the chance to
        // reap the pill. It sat on the top bar until the app was killed.
        // `onListenerConnected` re-seeds from `activeNotifications`, so
        // anything genuinely still running comes straight back.
        activeLive.clear()
        // MIUI kills the listener binding freely (after a process restart it
        // often never rebinds on its own) — without the listener NOTHING mirrors
        // and call connect/end detection dies. Ask the system to rebind us.
        try {
            requestRebind(android.content.ComponentName(this, MediaNotificationListenerService::class.java))
        } catch (_: Throwable) {
        }
    }

    /** Persist a snapshot of the mirrored-keys set so it survives a listener/
     *  process kill (MIUI). Snapshot under the lock, write off-lock (apply()). */
    private fun persistMirroredKeys() {
        val snapshot = synchronized(mirroredKeys) { mirroredKeys.toSet() }
        com.vortex.a3.core.notif.MirroredKeysStore.save(this, snapshot)
    }

    /** Drop stale + over-cap dedup entries so the map can't grow unbounded. */
    private fun pruneRecent(now: Long) {
        val it = recentSends.entries.iterator()
        while (it.hasNext()) {
            if (now - it.next().value > DEDUP_WINDOW_MS) it.remove()
        }
        while (recentSends.size > MAX_RECENT) {
            val first = recentSends.entries.iterator()
            if (first.hasNext()) { first.next(); first.remove() } else break
        }
    }

    /** Map a posted notification to a [NotificationMirror], or null if it
     *  should be dropped (noise / not worth mirroring). */
    private fun buildMirror(sbn: StatusBarNotification): NotificationMirror? {
        // Never mirror our own notifications (the foreground-service one).
        if (sbn.packageName == packageName) return null

        val n = sbn.notification ?: return null
        val flags = n.flags
        // Drop ongoing / non-clearable (foreground services, media
        // transport controls, persistent system bars) — not real alerts.
        if (!sbn.isClearable) return null
        if (flags and Notification.FLAG_ONGOING_EVENT != 0) return null
        // Drop the group SUMMARY — its children carry the real content;
        // mirroring the summary would duplicate or show a vague roll-up.
        if (flags and Notification.FLAG_GROUP_SUMMARY != 0) return null

        val extras = n.extras ?: return null
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()?.trim().orEmpty()
        // Body: prefer the FULL conversation (MessagingStyle / InboxStyle
        // multiple lines) so a chat's recent messages show stacked, like the
        // phone — not just the latest line. Falls back to big-text / text.
        val text = extractBody(extras)
        if (title.isEmpty() && text.isEmpty()) return null

        // Action-button labels (Mark as read, Reply, …), capped. The phone
        // keeps the PendingIntents; the laptop only shows labels and asks us
        // to fire one by index. Keep the FULL action objects in step so we
        // can flag which one accepts inline text (RemoteInput) — the laptop
        // pops a text entry for that one (GNOME has no inline reply).
        val rawActions = n.actions?.filter { it.title?.toString()?.trim()?.isNotEmpty() == true }
            ?.take(MAX_ACTIONS)
            ?: emptyList()
        // rawActions already filtered to non-empty titles; the safe access keeps
        // this robust if that filter ever changes (Action.title is nullable).
        val actions = rawActions.map { it.title?.toString()?.trim().orEmpty() }
        // First action that carries a RemoteInput = the reply action. -1 if
        // none. Messaging apps expose exactly one.
        val replyIndex = rawActions.indexOfFirst { !it.remoteInputs.isNullOrEmpty() }

        return NotificationMirror(
            app = appLabel(sbn.packageName),
            appId = sbn.packageName,
            title = normalize(title).take(MAX_TITLE),
            text = text.take(MAX_TEXT), // already per-line normalized, keeps newlines
            ts = System.currentTimeMillis(),
            key = sbn.key ?: "",
            actions = actions,
            replyIndex = replyIndex,
        )
    }

    /** If [sbn] is a live activity (ongoing progress/navigation/delivery),
     *  emit it to the laptop's top-bar pill and return true (consuming it so
     *  it isn't also one-shot-mirrored). Rate-limited per key so a per-second
     *  update stream doesn't flood BLE, but a real change always passes. */
    private fun handleLiveActivity(sbn: StatusBarNotification): Boolean {
        val live = buildLiveActivity(sbn)
        if (live == null) {
            // The notification updated but no longer qualifies as a live
            // activity (nav finished → the foreground notification demoted to a
            // plain "running" one). If this key WAS live, end it now so the
            // laptop pill clears immediately instead of waiting for the sweeper.
            val key = sbn.key
            if (key != null && activeLive.remove(key) != null) {
                synchronized(liveKeys) {
                    liveKeys.remove(key); liveRecentMs.remove(key); liveContent.remove(key)
                }
                if (NotificationMirrorSetting.isEnabled()) {
                    VortexService.liveActivityBus.tryEmit(LiveActivity(key = key, ended = true))
                }
                return true
            }
            return false
        }
        // Remember the LATEST state per key (even if the send is rate-limited)
        // so we can re-push it when the laptop reconnects — otherwise a live
        // activity whose content didn't change during the BLE outage would be
        // swallowed by the dedup below and the laptop's tray would never appear.
        activeLive[live.key] = live
        if (!NotificationMirrorSetting.isEnabled()) return true // consumed, just not forwarded
        val key = live.key
        val content = "${live.title}|${live.text}|${live.progress}|${live.playing}"
        val now = android.os.SystemClock.elapsedRealtime()
        val emit = synchronized(liveKeys) {
            liveKeys.add(key)
            val last = liveRecentMs[key]
            when {
                liveContent[key] == content -> false // unchanged → swallow
                // ONLY the playing flag flipped (track/progress unchanged) —
                // a user-driven play/pause edge. Exempt from the rate limit
                // so the laptop's ⏸/▶ button updates instantly.
                live.playing != null &&
                    liveContent[key]?.substringBeforeLast('|') ==
                    content.substringBeforeLast('|') -> {
                    liveRecentMs[key] = now
                    liveContent[key] = content
                    true
                }
                // Changed but too soon: DON'T store the new content, so the
                // next post after the window still counts as a change and
                // sends the latest state.
                last != null && now - last < LIVE_UPDATE_MS -> false
                else -> {
                    liveRecentMs[key] = now
                    liveContent[key] = content
                    true
                }
            }
        }
        if (emit) VortexService.liveActivityBus.tryEmit(live)
        return true
    }

    /** Map an ongoing, progress/category-bearing notification to a
     *  [LiveActivity], or null if it isn't one. */
    private fun buildLiveActivity(sbn: StatusBarNotification): LiveActivity? {
        if (sbn.packageName == packageName) return null
        val n = sbn.notification ?: return null
        val flags = n.flags
        if (flags and Notification.FLAG_GROUP_SUMMARY != 0) return null
        val extras = n.extras ?: return null
        // Media playback → its own now-playing pill (title/artist + playing
        // flag; the laptop draws transport buttons). Checked BEFORE the
        // ongoing gate on purpose: a PAUSED player's notification usually
        // turns clearable, but the pill must stay so the track can be
        // resumed from the laptop; it clears when the notification goes.
        if (extras.containsKey(Notification.EXTRA_MEDIA_SESSION)) {
            return buildMediaLiveActivity(sbn, extras)
        }
        // Live activities are ongoing (persistent, non-dismissable).
        val ongoing = !sbn.isClearable || (flags and Notification.FLAG_ONGOING_EVENT != 0)
        if (!ongoing) return null
        val max = extras.getInt(Notification.EXTRA_PROGRESS_MAX, 0)
        val cur = extras.getInt(Notification.EXTRA_PROGRESS, 0)
        val indeterminate = extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE, false)
        val hasProgress = max > 0 || indeterminate
        val liveCategory = n.category in LIVE_CATEGORIES
        if (!hasProgress && !liveCategory) return null
        val appLabel = appLabel(sbn.packageName)
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()?.trim().orEmpty()
        val text = extras.getCharSequence(Notification.EXTRA_TEXT)?.toString()?.trim().orEmpty()
        // sub-text → an extra stage line (when the app exposes it as a standard
        // field; apps with fully-custom layouts like Yandex Maps don't).
        val sub = normalize(extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString()?.trim().orEmpty())
        if (title.isEmpty() && text.isEmpty() && sub.isEmpty()) return null
        val progress = if (max > 0) (cur.toLong() * 100 / max).toInt().coerceIn(0, 100) else -1
        val key = sbn.key ?: return null
        return LiveActivity(
            key = key,
            app = appLabel,
            appId = sbn.packageName,
            title = normalize(title).take(MAX_TITLE),
            text = normalize(text).take(MAX_TEXT),
            sub = sub.take(MAX_TITLE),
            progress = progress,
        )
    }

    /** Map a media-playback notification to a now-playing [LiveActivity]:
     *  track title + artist from the app's [android.media.session.MediaController]
     *  (falling back to the notification's own title/text lines) and the live
     *  playing/paused flag the laptop's transport buttons key off. */
    private fun buildMediaLiveActivity(
        sbn: StatusBarNotification,
        extras: android.os.Bundle,
    ): LiveActivity? {
        val key = sbn.key ?: return null
        // Session lookup by package — we ARE the enabled notification
        // listener, so getActiveSessions works. Never throws out of here.
        val controller = try {
            val msm = getSystemService(MEDIA_SESSION_SERVICE)
                as? android.media.session.MediaSessionManager
            msm?.getActiveSessions(
                android.content.ComponentName(this, MediaNotificationListenerService::class.java),
            )?.firstOrNull { it.packageName == sbn.packageName }
        } catch (_: Exception) {
            null
        }
        val md = controller?.metadata
        val title = md?.getString(android.media.MediaMetadata.METADATA_KEY_TITLE)?.trim()
            ?.takeIf { it.isNotEmpty() }
            ?: extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()?.trim().orEmpty()
        val artist = md?.getString(android.media.MediaMetadata.METADATA_KEY_ARTIST)?.trim()
            ?.takeIf { it.isNotEmpty() }
            ?: extras.getCharSequence(Notification.EXTRA_TEXT)?.toString()?.trim().orEmpty()
        if (title.isEmpty() && artist.isEmpty()) return null
        val state = controller?.playbackState?.state
        val playing = state == android.media.session.PlaybackState.STATE_PLAYING ||
            state == android.media.session.PlaybackState.STATE_BUFFERING
        return LiveActivity(
            key = key,
            app = appLabel(sbn.packageName),
            appId = sbn.packageName,
            title = normalize(title).take(MAX_TITLE),
            text = normalize(artist).take(MAX_TITLE),
            playing = playing,
        )
    }

    /** Build the mirrored body, preferring a chat's recent messages
     *  (MessagingStyle / InboxStyle) joined as separate lines so they show
     *  stacked like the phone, else the single big-text / text line. Each
     *  line is whitespace-normalised but the newlines between lines are kept. */
    @Suppress("DEPRECATION")
    private fun extractBody(extras: android.os.Bundle): String {
        // InboxStyle — multiple summary lines.
        extras.getCharSequenceArray(Notification.EXTRA_TEXT_LINES)?.let { lines ->
            val out = lines.mapNotNull { normalize(it.toString()).takeIf { s -> s.isNotEmpty() } }
            if (out.isNotEmpty()) return out.takeLast(MAX_LINES).joinToString("\n")
        }
        // MessagingStyle — array of message bundles (text under key "text").
        extras.getParcelableArray(Notification.EXTRA_MESSAGES)?.let { msgs ->
            val out = msgs.mapNotNull {
                (it as? android.os.Bundle)?.getCharSequence("text")
                    ?.let { t -> normalize(t.toString()) }
                    ?.takeIf { s -> s.isNotEmpty() }
            }
            if (out.isNotEmpty()) return out.takeLast(MAX_LINES).joinToString("\n")
        }
        val single = (extras.getCharSequence(Notification.EXTRA_BIG_TEXT)
            ?: extras.getCharSequence(Notification.EXTRA_TEXT))?.toString().orEmpty()
        return normalize(single)
    }

    /** Collapse newlines + runs of whitespace into single spaces so the
     *  mirrored line reads cleanly and stays compact on the wire. */
    private fun normalize(s: String): String =
        s.replace(Regex("\\s+"), " ").trim()

    /** Friendly app label for a package, falling back to the package name. */
    private fun appLabel(pkg: String): String = try {
        val pm = packageManager
        pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
    } catch (_: Throwable) {
        pkg
    }

    companion object {
        private const val TAG = "VortexNotifListener"

        /** Active listener instance, for cancelNotification from elsewhere. */
        @Volatile
        private var instance: MediaNotificationListenerService? = null

        /** Is the system actually BOUND to us right now?
         *
         *  Not the same question as "is notification access granted", and the
         *  difference is the whole bug: MIUI's OneKeyClean kills this process,
         *  Android restarts the foreground services but does NOT rebind the
         *  listener, and the permission still reads as granted. Nothing mirrors,
         *  while BLE and LAN keep working — so both devices show "connected"
         *  and the user has no way to tell. Observed here: killed at 09:20,
         *  still unbound 1h51m later, and only a manual re-toggle in Settings
         *  brought it back. `getEnabledListenerPackages()` cannot see this;
         *  only whether we ever received onListenerConnected can. */
        fun isBound(): Boolean = instance != null

        /** Key of the dialer's ongoing-call notification while a call is on
         *  screen — so its REMOVAL is a definitive "call ended" signal. */
        @Volatile
        private var callNotifKey: String? = null

        /** True if [sbn] is the system/dialer ongoing-call notification. Works
         *  even when `sbn.notification` is null (common on removal), via package. */
        private fun isCallNotification(sbn: StatusBarNotification): Boolean {
            if (sbn.notification?.category == android.app.Notification.CATEGORY_CALL) return true
            val p = sbn.packageName
            return p.contains("dialer") || p.contains("incallui") ||
                p == "com.android.server.telecom"
        }

        /** Track the dialer call notification: remember its key (for end
         *  detection) and, when its chronometer reports the callee answered,
         *  stamp the real connect time so the laptop pill switches "Calling…" →
         *  a live timer. The TelephonyCallback can't tell dialing from connected,
         *  so this notification chronometer is the only reliable connect signal. */
        private fun trackCallNotification(sbn: StatusBarNotification) {
            if (!isCallNotification(sbn)) return
            val cur = VortexService.currentCall ?: return
            if (cur.phase == com.vortex.a3.core.call.CallEvent.PHASE_ENDED) return
            callNotifKey = sbn.key
            val n = sbn.notification ?: return
            // Fill in who is calling when telephony would not say.
            //
            // On Android 12+ the modern `TelephonyCallback.CallStateListener`
            // is handed only a state — no number — and the deprecated listener
            // that did carry one is blanked by the platform without
            // READ_CALL_LOG. So every incoming banner read "Unknown caller",
            // contact lookup had nothing to look up, and a missed call left
            // nothing behind because that notification needs a non-empty
            // number. The dialer's own notification knows perfectly well who is
            // calling, and we are already reading it for the connect timer.
            if (cur.name.isEmpty() && cur.number.isEmpty()) {
                val who = n.extras?.getCharSequence(android.app.Notification.EXTRA_TITLE)
                    ?.toString()
                    ?.trim()
                    .orEmpty()
                if (who.isNotEmpty()) {
                    // A title of digits and dialling punctuation is a number;
                    // anything else is the contact name the dialer resolved.
                    val looksNumeric = who.any { it.isDigit() } &&
                        who.all { it.isDigit() || it in "+ -()." }
                    val filled = if (looksNumeric) cur.copy(number = who) else cur.copy(name = who)
                    VortexService.currentCall = filled
                    VortexService.callEventBus.tryEmit(filled)
                    Log.i(TAG, "call notif: filled caller identity from the dialer notification")
                }
            }
            val chrono = n.extras?.getBoolean(android.app.Notification.EXTRA_SHOW_CHRONOMETER, false) ?: false
            val whenMs = n.`when`
            Log.i(TAG, "call notif: chrono=$chrono when=$whenMs phase=${cur.phase} outgoing=${cur.outgoing} connected=${cur.connected}")
            if (chrono && whenMs > 0L &&
                cur.phase == com.vortex.a3.core.call.CallEvent.PHASE_ACTIVE &&
                !(cur.connected && cur.startedAt == whenMs)
            ) {
                val updated = cur.copy(connected = true, startedAt = whenMs)
                VortexService.currentCall = updated
                VortexService.callEventBus.tryEmit(updated)
                Log.i(TAG, "call connected (dialer chronometer) → started_at=$whenMs")
            }
        }

        /** The tracked dialer call notification was removed → emit a definitive
         *  END so the laptop clears the pill at once (TelephonyCallback IDLE +
         *  BLE delivery can lag, leaving the pill's timer ticking on). */
        private fun handleCallRemoved(key: String) {
            if (key != callNotifKey) return
            callNotifKey = null
            val cur = VortexService.currentCall ?: return
            if (cur.phase == com.vortex.a3.core.call.CallEvent.PHASE_ENDED) return
            // A vanished notification is a HINT that the call ended, not proof.
            //
            // `callNotifKey` follows any notification from the dialer package,
            // and dialers move those around constantly: MIUI cancels the ringing
            // notification the moment you answer (the in-call screen takes
            // over), and a "missed call from A" card can be re-posted, tracked,
            // and then swiped away while call B is live. Both looked exactly
            // like a hang-up from here, so the laptop's pill vanished mid-call
            // and its Mute/End buttons went with it — and the real end event
            // that followed was discarded as a duplicate.
            //
            // Telephony knows the truth, and READ_PHONE_STATE is already
            // required for the call mirror to work at all.
            if (telephonyBusy()) {
                Log.i(TAG, "dialer notification removed but telephony is still busy — not ending")
                return
            }
            val ended = cur.copy(phase = com.vortex.a3.core.call.CallEvent.PHASE_ENDED)
            VortexService.currentCall = null
            VortexService.callEventBus.tryEmit(ended)
            Log.i(TAG, "dialer call notification removed → END")
        }

        /** Is a call still up according to the telephony stack?
         *
         *  Errs towards "no" when it cannot tell (permission gone, no service):
         *  the old behaviour ended the call on notification removal alone, so an
         *  unreadable state is no worse than before, while a readable one is
         *  strictly better. */
        private fun telephonyBusy(): Boolean {
            val ctx = instance ?: return false
            return try {
                val tm = ctx.getSystemService(android.telephony.TelephonyManager::class.java)
                    ?: return false
                @Suppress("DEPRECATION")
                tm.callState != android.telephony.TelephonyManager.CALL_STATE_IDLE
            } catch (t: Throwable) {
                Log.w(TAG, "telephony state unreadable: ${t.message}")
                false
            }
        }

        /** Latest state of every CURRENTLY-active live activity, keyed by SBN
         *  key. Lets the stack re-push them when the laptop reconnects (the
         *  per-key dedup would otherwise swallow an unchanged re-send). */
        private val activeLive = java.util.concurrent.ConcurrentHashMap<String, LiveActivity>()

        /** Snapshot of active live activities — re-pushed on BLE re-subscribe. */
        fun activeLiveActivities(): List<LiveActivity> = activeLive.values.toList()

        /** Re-run now-playing detection over the active media notifications
         *  NOW. Called on play/pause edges (from the media coordinator, and
         *  after a laptop-driven transport command): apps don't always
         *  repost their media notification on a state flip, but the
         *  MediaController state is already correct — a rescan re-reads it
         *  so the laptop pill's ⏸/▶ tracks reality. */
        fun rescanMediaPills() {
            val svc = instance ?: return
            try {
                svc.activeNotifications?.forEach { sbn ->
                    if (sbn.notification?.extras
                            ?.containsKey(Notification.EXTRA_MEDIA_SESSION) == true
                    ) {
                        svc.handleLiveActivity(sbn)
                    }
                }
            } catch (t: Throwable) {
                Log.w(TAG, "rescanMediaPills: ${t.message}")
            }
        }

        /** Laptop catch-up request: re-send any active mirrorable notification the
         *  laptop is MISSING (its notify was dropped in-air, or it arrived while the
         *  laptop was disconnected). [knownKeys] = what the laptop already displayed,
         *  so we skip those — no re-popup of what the user has seen. Bounded to the
         *  recent few (same window as the listener-gap catch-up) and gated by the
         *  mirror toggle. Emits to [VortexService.notificationBus]; the stack's
         *  forwarder sends each over BLE exactly as a live post would. */
        fun resendMissing(knownKeys: Set<String>) {
            if (!com.vortex.a3.core.notif.NotificationMirrorSetting.isEnabled()) return
            val svc = instance ?: return
            try {
                val now = System.currentTimeMillis()
                svc.activeNotifications
                    ?.filter { sbn ->
                        sbn.key !in knownKeys && now - sbn.postTime < CATCHUP_WINDOW_MS
                    }
                    ?.sortedBy { it.postTime }
                    ?.takeLast(CATCHUP_MAX)
                    ?.forEach { sbn ->
                        val mirror = svc.buildMirror(sbn) ?: return@forEach
                        if (mirror.key.isNotEmpty()) {
                            synchronized(svc.mirroredKeys) { svc.mirroredKeys.add(mirror.key) }
                            svc.persistMirroredKeys()
                        }
                        Log.i(TAG, "resend (laptop catch-up): ${mirror.app}")
                        VortexService.notificationBus.tryEmit(mirror)
                    }
            } catch (t: Throwable) {
                Log.w(TAG, "resendMissing: ${t.message}")
            }
        }

        /** Fire a notification's action (the laptop clicked an action button).
         *  Finds the live notification by key, fires `actions[index]`'s
         *  PendingIntent — filling its RemoteInput with `reply` when the
         *  action accepts inline text and `reply` is non-empty. */
        fun invokeAction(key: String, index: Int, reply: String) {
            val svc = instance ?: return
            fun fail(why: String) {
                // Say so out loud. All of these used to be a silent `return`:
                // the user typed a reply on their laptop, pressed send, GNOME
                // closed the notification — and the words went nowhere, with
                // nothing anywhere recording that they had.
                Log.w(TAG, "invokeAction($index) failed: $why")
            }
            try {
                val sbn = svc.activeNotifications?.firstOrNull { it.key == key }
                if (sbn == null) {
                    fail("the notification is no longer active")
                    return
                }
                // Index against the SAME filtered list the laptop was shown.
                //
                // `buildMirror` sends actions with empty titles removed and the
                // list capped at three, then the laptop sends back a position in
                // THAT list — but this indexed the raw array, so a blank-titled
                // or reordered action shifted everything: "Mark as read" could
                // fire the reply intent with no text attached, or the reverse.
                val actions = sbn.notification.actions
                    ?.filter { it.title?.toString()?.trim()?.isNotEmpty() == true }
                    ?.take(MAX_ACTIONS)
                val action = actions?.getOrNull(index)
                if (action == null) {
                    fail("action $index is gone (the app re-posted with a different set)")
                    return
                }
                val pi = action.actionIntent
                if (pi == null) {
                    fail("action has no intent")
                    return
                }
                val inputs = action.remoteInputs
                if (reply.isNotEmpty() && inputs.isNullOrEmpty()) {
                    fail("a reply was typed but this action takes no text")
                    return
                }
                if (reply.isNotEmpty()) {
                    val intent = android.content.Intent()
                    val bundle = android.os.Bundle()
                    for (ri in inputs!!) bundle.putCharSequence(ri.resultKey, reply)
                    android.app.RemoteInput.addResultsToIntent(inputs, intent, bundle)
                    pi.send(svc, 0, intent)
                } else {
                    pi.send()
                }
                Log.i(TAG, "invokeAction($index) fired${if (reply.isEmpty()) "" else " with a reply"}")
            } catch (e: android.app.PendingIntent.CanceledException) {
                fail("the app cancelled this action (notification already handled?)")
            } catch (e: Exception) {
                fail(e.message ?: e.toString())
            }
        }

        /** Answer / hang-up the current phone call by firing the DIALER's own
         *  notification action (its Answer / Decline / Hang-up PendingIntent).
         *  This is the laptop call banner's reliable accept/end path: it works
         *  where `TelecomManager.acceptRingingCall()` is blocked (MIUI and other
         *  ROMs gate it to the default dialer). Returns true if an action fired.
         *
         *  Identifies the call notification by category CALL or a known dialer
         *  package, then picks the action whose title matches the answer /
         *  decline keyword set (multilingual). Returns false (so the caller can
         *  fall back to TelecomManager) if nothing matched. */
        fun fireCallAction(wantAnswer: Boolean): Boolean {
            val svc = instance ?: return false
            return try {
                val ns = svc.activeNotifications ?: return false
                fun isCallSbn(sbn: StatusBarNotification): Boolean {
                    val n = sbn.notification ?: return false
                    val isCall = n.category == android.app.Notification.CATEGORY_CALL ||
                        sbn.packageName.contains("dialer") ||
                        sbn.packageName.contains("incallui") ||
                        sbn.packageName == "com.android.server.telecom"
                    return isCall && (n.actions?.isNotEmpty() == true)
                }
                // Prefer the notification we are ACTUALLY mirroring.
                //
                // Taking "the first call notification with actions" is wrong
                // during call waiting: with a call in progress and a second one
                // ringing, both are present, and the ringing one usually sorts
                // first. So End — meant for the call the laptop's pill is
                // showing — pressed "Decline" on the incoming call instead, and
                // the call the user wanted to hang up carried on. `callNotifKey`
                // is the one we have been tracking all along.
                val callSbn = callNotifKey?.let { key -> ns.firstOrNull { it.key == key && isCallSbn(it) } }
                    ?: ns.firstOrNull { isCallSbn(it) }
                    ?: return false
                val actions = callSbn.notification.actions ?: return false
                val answerKw = listOf(
                    "answer", "accept", "pick up", "javob", "qabul",
                    "ответить", "принять", "відповісти",
                )
                val declineKw = listOf(
                    "decline", "reject", "dismiss", "hang", "end call", "end",
                    "rad", "bekor", "tugat", "отклонить", "сбросить", "завершить",
                    "відхилити", "відхилення",
                )
                val kws = if (wantAnswer) answerKw else declineKw
                val match = actions.firstOrNull { a ->
                    val t = a.title?.toString()?.lowercase()?.trim().orEmpty()
                    t.isNotEmpty() && kws.any { t.contains(it) } && a.actionIntent != null
                }
                // Titles are logged so a real-call test can confirm / refine the
                // keyword match if a locale's labels differ.
                Log.i(TAG, "fireCallAction(answer=$wantAnswer): actions=${actions.mapNotNull { it.title?.toString() }} matched=${match?.title}")
                val pi = match?.actionIntent ?: return false
                pi.send()
                true
            } catch (e: Exception) {
                Log.w(TAG, "fireCallAction: ${e.message}")
                false
            }
        }

        /** Dismiss a mirrored notification by key — invoked when the laptop
         *  clears its copy. Removes from our mirrored set first so the
         *  resulting onNotificationRemoved doesn't echo a dismiss back. */
        fun dismissByKey(key: String) {
            val svc = instance ?: return
            val removed = synchronized(svc.mirroredKeys) { svc.mirroredKeys.remove(key) }
            if (removed) svc.persistMirroredKeys()
            try {
                svc.cancelNotification(key)
            } catch (e: Exception) {
                Log.w(TAG, "dismissByKey: ${e.message}")
            }
        }
        /** Field caps so a mirrored notification fits one BLE notify and
         *  we never ship a runaway payload. */
        private const val MAX_TITLE = 120
        private const val MAX_TEXT = 280
        /** Max action buttons to mirror (keeps the payload small). */
        private const val MAX_ACTIONS = 3
        /** Max conversation lines to stack in a mirrored chat body. */
        private const val MAX_LINES = 6
        /** Collapse the same content re-posted within this window (chatty
         *  apps re-post on every minor update). Matches the ecosystem's 4 s. */
        private const val DEDUP_WINDOW_MS = 4_000L
        /** Min gap between live-activity updates pushed for one key — caps a
         *  per-second ETA stream while still passing each real change. */
        private const val LIVE_UPDATE_MS = 1_200L
        // Re-push cadence for active live activities (keeps the laptop pill alive
        // when content is steady). Must be well under the laptop's staleness
        // sweep window so a missed beat doesn't drop the pill.
        private const val LIVE_HEARTBEAT_MS = 25_000L
        /** Notification categories that qualify as live activities (ride,
         *  navigation, delivery progress, timer, workout, location share).
         *  Media (transport + position) is excluded via EXTRA_MEDIA_SESSION. */
        private val LIVE_CATEGORIES = setOf(
            Notification.CATEGORY_NAVIGATION,
            Notification.CATEGORY_TRANSPORT,
            Notification.CATEGORY_PROGRESS,
            Notification.CATEGORY_STOPWATCH,
            Notification.CATEGORY_WORKOUT,
            Notification.CATEGORY_LOCATION_SHARING,
        )
        /** Hard cap on the dedup map size (belt-and-suspenders vs unbounded). */
        private const val MAX_RECENT = 64
        /** Listener-gap catch-up: only notifications posted within this
         *  window are replayed on (re)connect — older ones were almost
         *  certainly mirrored before the gap (the mirrored-keys set just
         *  didn't survive the process restart). */
        private const val CATCHUP_WINDOW_MS = 30 * 60 * 1000L
        /** Max catch-up mirrors per (re)connect. */
        private const val CATCHUP_MAX = 10
        /** Wait for the stack's bus consumer before emitting catch-ups
         *  (replay=0 — an emit with no subscriber is gone). */
        private const val CATCHUP_DELAY_MS = 4_000L
    }
}
