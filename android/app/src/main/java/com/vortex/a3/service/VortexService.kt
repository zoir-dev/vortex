package com.vortex.a3.service

import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log
import com.vortex.a3.core.lan.LanServer

/**
 * Long-running foreground service that owns the Vortex network stack.
 *
 * This class is now a thin lifecycle + notification shell: the actual stack
 * (BLE GATT server + advertiser + reconnect, LAN mDNS + IK + AppState sync,
 * call + media hand-off) lives in [VortexStack]; the foreground notification
 * in [VortexNotification]; the BT/battery receivers in [VortexReceivers].
 *
 * Pairing (which needs the SAS approval UI) stays in MainActivity — the
 * service only handles post-pair operation: discovery + reconnect.
 *
 * Lifecycle: START_STICKY (OS restarts after low-memory kill); startForeground
 * with a persistent notification; BootReceiver re-starts it after reboot when
 * trust exists.
 */
class VortexService : Service() {

    private val tag = "VortexService"

    /** The network stack. Also the notification's data source ([VortexNotification.Host]). */
    private val stack = VortexStack(this)
    private val notification = VortexNotification(this, stack)
    /** BT-adapter-state + battery receivers. On BT re-enable rebuild the BLE
     *  stack; on a battery event push state to the peer (BLE + LAN). */
    private val receivers = VortexReceivers(
        context = this,
        onBluetoothReenabled = { onBluetoothReenabled() },
        onBatteryChanged = {
            // Fast path over the already-open BLE link (~200 ms, works
            // in-pocket); LAN nudge is the fallback. Both fire; the laptop
            // dedups by latest state.
            //
            // Guarded: `pushStateViaBle` reads `peerStore`, which is a
            // `lateinit` initialised by the stack. A trigger that fires before
            // the stack is up therefore throws, and a throw here happens on a
            // system callback thread — which takes the whole service down. The
            // network callback added below fires the instant it registers, so
            // it hit this immediately; the battery receiver only ever avoided
            // it by luck of timing.
            if (stack.isStarted()) {
                stack.pushStateViaBle()
                liveLan?.nudge()
            }
        },
    )

    override fun onCreate() {
        super.onCreate()
        Log.i(tag, "service onCreate")
        notification.startInForeground()
        // Ask the system to (re)bind our notification listener NOW. After a
        // process kill (MIUI OneKeyClean / force-stop) Android can leave the
        // listener unbound for MINUTES — a window where no notification is
        // captured at all. requestRebind closes that window to seconds and
        // is a safe no-op when already bound; the listener's catch-up then
        // replays whatever was posted during the gap.
        try {
            android.service.notification.NotificationListenerService.requestRebind(
                android.content.ComponentName(
                    this,
                    com.vortex.a3.core.media.MediaNotificationListenerService::class.java,
                ),
            )
        } catch (e: Exception) {
            Log.w(tag, "listener requestRebind: ${e.message}")
        }
        startListenerWatchdog()
        cleanStaleListenerEntry()
        ensureStackStarted()
    }

    /** How often to check that the notification listener is actually bound. */
    private val listenerCheckMs = 60_000L
    private val listenerHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private var listenerMisses = 0
    private var listenerWarned = false

    /**
     * Keep asking until the notification listener is really bound.
     *
     * The single `requestRebind` in [onCreate] is not enough, because the
     * failure it is meant to cover does not always happen at create time. MIUI's
     * OneKeyClean kills this process while the user is elsewhere; Android brings
     * the foreground services back but leaves the listener unbound, and there is
     * no callback for that — the app simply never hears about notifications
     * again. On this device it stayed that way for nearly two hours, twice in
     * one morning, while both ends still displayed "connected".
     *
     * So: check on a timer. `requestRebind` is cheap and a no-op when bound. If
     * it still has not taken after a few minutes, the binding is not something
     * the app can repair — notification access has to be toggled by hand — so
     * say so once, plainly, instead of leaving the user with silence.
     */
    private fun startListenerWatchdog() {
        listenerHandler.removeCallbacksAndMessages(null)
        listenerHandler.postDelayed(
            object : Runnable {
                override fun run() {
                    try {
                        checkListenerBound()
                    } catch (t: Throwable) {
                        Log.w(tag, "listener watchdog: ${t.message}")
                    }
                    listenerHandler.postDelayed(this, listenerCheckMs)
                }
            },
            listenerCheckMs,
        )
    }

    private fun checkListenerBound() {
        val granted = try {
            androidx.core.app.NotificationManagerCompat
                .getEnabledListenerPackages(this)
                .contains(packageName)
        } catch (t: Throwable) {
            true // can't tell — assume granted and let the rebind attempt speak
        }
        if (com.vortex.a3.core.media.MediaNotificationListenerService.isBound()) {
            listenerMisses = 0
            listenerWarned = false
            return
        }
        listenerMisses++
        if (!granted) {
            // Access itself is off: rebinding cannot help, only the user can.
            warnListenerDead(accessRevoked = true)
            return
        }
        Log.w(tag, "notification listener not bound (miss #$listenerMisses) — requesting rebind")
        try {
            android.service.notification.NotificationListenerService.requestRebind(
                android.content.ComponentName(
                    this,
                    com.vortex.a3.core.media.MediaNotificationListenerService::class.java,
                ),
            )
        } catch (e: Exception) {
            Log.w(tag, "listener requestRebind: ${e.message}")
        }
        // Three misses ≈ three minutes of asking with no bind. Past that it is
        // not coming back on its own.
        if (listenerMisses >= 3) {
            warnListenerDead(accessRevoked = false)
        }
    }

    /**
     * Remove the obsolete `VortexService` entry from the system's enabled
     * notification-listener list.
     *
     * It is a leftover from when the service itself was the listener. It does
     * nothing — the system logs "Not registering … it does not require the
     * permission" on every boot — but it sits in the same setting as the real
     * listener and makes the list read as though two components are enabled,
     * which is exactly the wrong impression while diagnosing a listener that is
     * not bound. Best-effort: on most builds this setting is not writable
     * without a privileged permission, so a failure is expected and silent.
     */
    private fun cleanStaleListenerEntry() {
        try {
            val key = "enabled_notification_listeners"
            val cur = android.provider.Settings.Secure.getString(contentResolver, key) ?: return
            val stale = "$packageName/${VortexService::class.java.name}"
            if (!cur.split(':').contains(stale)) return
            val cleaned = cur.split(':').filter { it.isNotEmpty() && it != stale }.joinToString(":")
            if (android.provider.Settings.Secure.putString(contentResolver, key, cleaned)) {
                Log.i(tag, "removed the stale VortexService notification-listener entry")
            }
        } catch (t: Throwable) {
            Log.d(tag, "stale listener entry not removable: ${t.message}")
        }
    }

    private fun warnListenerDead(accessRevoked: Boolean) {
        if (listenerWarned) return
        listenerWarned = true
        Log.w(tag, "notification mirroring is dead (accessRevoked=$accessRevoked)")
        try {
            notification.postListenerBroken(accessRevoked)
        } catch (t: Throwable) {
            Log.w(tag, "listener warning notification: ${t.message}")
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i(tag, "onStartCommand flags=$flags startId=$startId action=${intent?.action}")
        if (!stack.isStarted()) {
            ensureStackStarted()
        } else when (intent?.action) {
            // READ_PHONE_STATE granted after the stack was already running
            // (the common trusted-launch path never asked for it).
            ACTION_REFRESH_CALLFLOW -> stack.refreshCallFlow()
            // User tapped the earbuds row / Switch in the notification.
            ACTION_TOGGLE_AUDIO -> toggleAudio()
            // User tapped the lock glyph in the notification. Both directions
            // are one-tap, no biometric: the laptop only honours "unlock" while
            // this phone reports itself unlocked (owner-present gate), so reaching
            // an unlocked phone to tap IS the authentication — same as the
            // in-app button and the proximity auto-unlock.
            ACTION_LOCK_LAPTOP -> requestLaptopLock(applicationContext, "lock")
            ACTION_UNLOCK_LAPTOP -> requestLaptopLock(applicationContext, "unlock")
        }
        return START_STICKY
    }

    override fun onDestroy() {
        Log.i(tag, "service onDestroy")
        retryHandler.removeCallbacks(retryStart)
        notification.stop()
        stack.stop()
        receivers.unregister()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private val retryHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private val retryStart = Runnable { ensureStackStarted() }

    /** Bring the stack up (idempotent). Registers the infra receivers UP FRONT
     *  so the BT-state receiver can retry even if THIS attempt fails, and on
     *  failure (the GATT server can't open — BT off or mid-cycle) keeps the
     *  service alive and retries instead of dying. A transient BT blip must
     *  never leave the phone permanently invisible to the laptop. */
    private fun ensureStackStarted() {
        if (stack.isStarted()) return
        receivers.register() // up front: arms the BT-on retry even on failure
        if (stack.start(onStateChanged = { notification.refresh() })) {
            retryHandler.removeCallbacks(retryStart)
            Log.i(tag, "stack started")
        } else {
            Log.w(tag, "stack start failed (Bluetooth down?); staying up, retry in ${STACK_RETRY_MS}ms")
            retryHandler.removeCallbacks(retryStart)
            retryHandler.postDelayed(retryStart, STACK_RETRY_MS)
        }
    }

    /** BT adapter turned back on. If the stack never came up (start failed
     *  while BT was off) do a full start; if it's already up just rebuild the
     *  BLE advertiser + GATT server, which Android tears down on BT off. */
    private fun onBluetoothReenabled() {
        if (stack.isStarted()) stack.restartBleComponents() else ensureStackStarted()
    }

    /** Notification "Switch" action — toggle which device holds the buds; the
     *  stack does the switch and tells us the target side so the notifier can
     *  tint the row during the gap. */
    private fun toggleAudio() {
        stack.toggleAudio { owner -> notification.noteSwitchTarget(owner) }
    }

    companion object {
        /** Intent action: (re)register the telephony call listener after
         *  READ_PHONE_STATE is granted post-launch. See [startOrRefreshCallFlow]. */
        /** How long to wait before retrying a failed stack start (BT down). */
        private const val STACK_RETRY_MS = 3000L

        const val ACTION_REFRESH_CALLFLOW = "com.vortex.a3.REFRESH_CALLFLOW"
        /** Intent action: the user tapped the earbuds row / Switch button in
         *  the foreground notification — toggle which device holds the buds. */
        const val ACTION_TOGGLE_AUDIO = "com.vortex.a3.TOGGLE_AUDIO"
        /** Intent action: the user tapped the lock glyph in the foreground
         *  notification while the laptop was unlocked — lock it. */
        const val ACTION_LOCK_LAPTOP = "com.vortex.a3.LOCK_LAPTOP"
        /** Intent action: tapped the glyph while the laptop was locked — unlock
         *  it. One-tap (no biometric); the laptop gates on this phone being
         *  unlocked (owner-present gate). */
        const val ACTION_UNLOCK_LAPTOP = "com.vortex.a3.UNLOCK_LAPTOP"

        /**
         * Latest peer AppState snapshot, keyed by peer_static_pub hex.
         * replay=1 so the UI can subscribe any time and see the most recent
         * value. Drop-oldest is safe — state is monotonic, UI wants latest.
         */
        val peerStateBus: kotlinx.coroutines.flow.MutableSharedFlow<
            Pair<String, com.vortex.a3.core.appstate.AppState>
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 1,
            extraBufferCapacity = 32,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * Phone notifications captured by [com.vortex.a3.core.media.MediaNotificationListenerService]
         * for mirroring to the laptop. [VortexStack] subscribes and sends each
         * over BLE. replay=0 (events, not state); drop-oldest under a burst so
         * a notification storm can't wedge the listener.
         */
        val notificationBus: kotlinx.coroutines.flow.MutableSharedFlow<
            com.vortex.a3.core.notif.NotificationMirror
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 0,
            extraBufferCapacity = 64,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * Clipboard text captured on THIS phone (via the Quick Settings tile /
         * quick-send activity — Android forbids background clipboard reads, so
         * it's user-triggered) for sending to the laptop. [VortexStack]
         * subscribes and pushes each over BLE. replay=0; small buffer.
         */
        val clipboardBus: kotlinx.coroutines.flow.MutableSharedFlow<String> =
            kotlinx.coroutines.flow.MutableSharedFlow(
                replay = 0,
                extraBufferCapacity = 8,
                onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
            )

        /** Clipboard IMAGE (PNG bytes) captured on THIS phone for sending to
         *  the laptop (user-triggered via the tile / quick-send). */
        val clipboardImageBus: kotlinx.coroutines.flow.MutableSharedFlow<ByteArray> =
            kotlinx.coroutines.flow.MutableSharedFlow(
                replay = 0,
                extraBufferCapacity = 4,
                onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
            )

        /** A FILE (any non-image content) captured on THIS phone for sending to
         *  the laptop — same offer+LAN-pull path as images, but the laptop
         *  writes it to disk and makes it pasteable. */
        val clipboardFileBus: kotlinx.coroutines.flow.MutableSharedFlow<
            com.vortex.a3.core.clipboard.ClipboardOutgoingFile> =
            kotlinx.coroutines.flow.MutableSharedFlow(
                replay = 0,
                // Sized for a real multi-select share. The share sheet hands us
                // every file at once in a tight loop, while the collector hashes
                // each one with SHA-256 and sends a BLE offer per file — far
                // slower than the emit. With four slots and DROP_OLDEST, a
                // ten-photo share silently forwarded only some of them, and the
                // toast still said "Sending 10 files to laptop…". Dropping the
                // user's files is not a reasonable answer to back-pressure, and
                // a queue of offers costs almost nothing to hold.
                extraBufferCapacity = 64,
                onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.SUSPEND,
            )

        /** Browsing HANDOFF (seamless-continuity): a page the phone wants to continue
         *  on the laptop. The Share sheet (or, later, the accessibility live read)
         *  emits here; [VortexStack] forwards each to the laptop over BLE. */
        val handoffBus: kotlinx.coroutines.flow.MutableSharedFlow<
            com.vortex.a3.core.handoff.HandoffEvent> =
            kotlinx.coroutines.flow.MutableSharedFlow(
                replay = 0,
                extraBufferCapacity = 4,
                onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
            )

        /**
         * Live activities (ongoing, in-place-updating notifications: ride ETA,
         * navigation, delivery, timer) captured by the notification listener.
         * [VortexStack] subscribes and pushes each to the laptop's top-bar pill
         * over BLE. replay=0; drop-oldest so a rapid update stream can't wedge.
         */
        val liveActivityBus: kotlinx.coroutines.flow.MutableSharedFlow<
            com.vortex.a3.core.notif.LiveActivity
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 0,
            extraBufferCapacity = 64,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * Phone-call events (ringing → active → ended) from [CallFlowOrchestrator].
         * [VortexStack] forwards each to the laptop over BLE (CALL frame) to drive
         * the continuity-style call banner + in-call pill. replay=1 so a
         * late BLE re-subscribe still sees the current call's latest phase.
         */
        val callEventBus: kotlinx.coroutines.flow.MutableSharedFlow<
            com.vortex.a3.core.call.CallEvent
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 1,
            extraBufferCapacity = 16,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * The phone's full contacts list, mirrored to the laptop companion's
         * Contacts page. [VortexStack] chunks + forwards it over BLE. replay=1
         * so a late BLE re-subscribe re-sends the current snapshot.
         */
        val contactsBus: kotlinx.coroutines.flow.MutableSharedFlow<
            List<com.vortex.a3.core.contacts.Contact>
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 1,
            extraBufferCapacity = 4,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * The phone's recent call log, mirrored to the laptop companion's
         * Recents page. [VortexStack] chunks + forwards it over BLE. replay=1
         * so a late BLE re-subscribe re-sends the current snapshot.
         */
        val callLogBus: kotlinx.coroutines.flow.MutableSharedFlow<
            List<com.vortex.a3.core.calllog.CallLogEntry>
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 1,
            extraBufferCapacity = 4,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * The phone's recent SMS, mirrored to the laptop companion's Messages
         * page. [VortexStack] chunks + forwards it over BLE. replay=1 so a late
         * BLE re-subscribe re-sends the current snapshot.
         */
        val smsBus: kotlinx.coroutines.flow.MutableSharedFlow<
            List<com.vortex.a3.core.sms.SmsMessage>
        > = kotlinx.coroutines.flow.MutableSharedFlow(
            replay = 1,
            extraBufferCapacity = 4,
            onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
        )

        /**
         * Fired when the peer pushes AppState.revoked = true. UI clears the
         * paired-device cards immediately instead of waiting for a navigate.
         */
        val revokedByPeerBus: kotlinx.coroutines.flow.MutableSharedFlow<String> =
            kotlinx.coroutines.flow.MutableSharedFlow(
                replay = 0,
                extraBufferCapacity = 16,
                onBufferOverflow = kotlinx.coroutines.channels.BufferOverflow.DROP_OLDEST,
            )

        /**
         * Pending revocation for a peer we're about to forget — kept alive
         * until the laptop has connected once and seen our `revoked=true`
         * AppState, then deleted locally. Set of peer_pub hex strings.
         */
        val pendingRevokes: java.util.concurrent.ConcurrentHashMap.KeySetView<String, Boolean> =
            java.util.concurrent.ConcurrentHashMap.newKeySet()

        /**
         * One-shot "you claim the buds" flag. Set by the UI when the user
         * taps swap on the side holding the buds; the next outgoing AppState
         * carries the bit and the peer starts its initiator flow. getAndSet
         * at encode time so a single heartbeat carries it.
         */
        val pendingAudioClaim: java.util.concurrent.atomic.AtomicBoolean =
            java.util.concurrent.atomic.AtomicBoolean(false)

        /** Phase 2 — current call phase shipped on every outgoing AppState
         *  until cleared. Linux pauses MPRIS on `null` → `ringing`/`active`
         *  and resumes on `*` → `null`. Volatile so the writer
         *  (CallFlowOrchestrator) and reader (AppState builder) don't race. */
        @Volatile
        var pendingCallPhase: String? = null

        /** The current call mirrored to the laptop, carried on every outgoing
         *  AppState (BLE+LAN) so the banner/pill survives a BLE drop mid-call.
         *  Set on each [com.vortex.a3.core.call.CallEvent], cleared on `ended`.
         *  Volatile — written by CallFlowOrchestrator, read by the AppState
         *  builder. */
        @Volatile
        var currentCall: com.vortex.a3.core.call.CallEvent? = null

        /** The ONE in-call gate for every audio-switch path (media-follow
         *  return timer, peer-media release, incoming-request acceptance).
         *  [pendingCallPhase] alone is NOT enough — it is only set when the
         *  call GRABBED the buds off the laptop, so gating on it misses calls
         *  taken with the buds already on the phone (the buds then got yanked
         *  mid-call / at call end). [currentCall] is stamped for EVERY call by
         *  the mirror, and its brief post-`ended` linger doubles as a settle
         *  window so media auto-resume wins the race against a hand-back. */
        fun callGateActive(): Boolean = currentCall != null || pendingCallPhase != null

        /** Current browsing-handoff page (null when not browsing) — included in
         *  the outgoing AppState as the LAN backstop for the laptop's "continue"
         *  pill. Set by the handoff forwarder from [handoffBus]. */
        @Volatile
        var currentHandoff: com.vortex.a3.core.handoff.HandoffEvent? = null

        /** Continuity Camera: set while THIS phone is streaming its camera to the
         *  laptop — advertised in the AppState so the laptop dials it. null = off. */
        @Volatile
        var cameraOffer: com.vortex.a3.core.appstate.CameraOffer? = null

        /** Currently-running LanServer ref (set by [VortexStack] while alive).
         *  internal so the stack can publish/clear it for the UI entrypoints. */
        @Volatile
        internal var liveLan: LanServer? = null

        /** Currently-running stack ref (set by [VortexStack] while alive) so
         *  UI entrypoints can trigger an immediate BLE AppState push. */
        @Volatile
        internal var liveStack: VortexStack? = null

        /** Set the audio-claim flag + trigger an immediate mDNS re-announce so
         *  the laptop pulls the next AppState within ~1 s. */
        fun requestPeerToClaim() {
            pendingAudioClaim.set(true)
            liveLan?.nudge()
        }

        /**
         * Push the current AppState to the peer NOW, preferring the BLE
         * fast-path (~200 ms, low energy, works in-pocket) with a LAN
         * re-announce as the fallback — both fire and the peer dedups. Use
         * after a state change the peer should learn immediately (e.g. the
         * smart-switch toggle) instead of waiting for the next heartbeat.
         */
        fun requestStatePush() {
            liveStack?.pushStateViaBle()
            liveLan?.nudge()
        }

        /**
         * Pending remote lock/unlock command for the laptop. NOT a one-shot:
         * it stays attached to EVERY outgoing snapshot until [expiresAtMs]
         * (the laptop dedups by [seq], so repeats are no-ops). The first
         * implementation consumed it at the first snapshot build, and a
         * single lost BLE write silently ate the user's tap — live-observed
         * as seq gaps on the laptop and "works on the second try".
         */
        data class PendingLock(val op: String, val seq: Long, val expiresAtMs: Long)

        val pendingLock =
            java.util.concurrent.atomic.AtomicReference<PendingLock?>(null)

        /** How long a lock/unlock command keeps riding outgoing snapshots.
         *  Long enough to span a BLE push failure + the next LAN pull. */
        private const val LOCK_CMD_TTL_MS = 20_000L

        private val lockPushHandler =
            android.os.Handler(android.os.Looper.getMainLooper())

        /** UI entrypoint: queue a laptop lock/unlock and push state NOW.
         *  The seq is allocated here (persisted, monotonic) so every
         *  snapshot in the TTL window carries the SAME command identity.
         *  Two delayed re-pushes cover a failed first BLE write — without
         *  them the command would wait for a slow periodic beat. */
        fun requestLaptopLock(context: Context, op: String) {
            val seq = com.vortex.a3.core.appstate.LockCommandSeq.next(context)
            pendingLock.set(
                PendingLock(
                    op = op,
                    seq = seq,
                    expiresAtMs = android.os.SystemClock.elapsedRealtime() + LOCK_CMD_TTL_MS,
                ),
            )
            requestStatePush()
            for (delayMs in longArrayOf(2_000L, 6_000L)) {
                lockPushHandler.postDelayed(
                    { if (pendingLock.get()?.seq == seq) requestStatePush() },
                    delayMs,
                )
            }
        }

        /**
         * Pending media transport command for the laptop (the laptop-media
         * notification's ⏮⏯⏭ buttons). The [PendingLock] model: NOT one-shot,
         * it rides every outgoing snapshot until [expiresAtMs] (the laptop
         * dedups by [seq]) so a single lost BLE write can't eat the tap.
         */
        data class PendingMediaControl(val op: String, val seq: Long, val expiresAtMs: Long)

        val pendingMediaControl =
            java.util.concurrent.atomic.AtomicReference<PendingMediaControl?>(null)

        /** Shorter TTL than the lock command — transport taps come in bursts
         *  and a >8s-late "next track" is worse than a dropped one. */
        private const val MEDIA_CMD_TTL_MS = 8_000L

        /** Notification-button entrypoint: queue a laptop media transport
         *  command and push state NOW (+ one delayed re-push covering a
         *  failed first BLE write). Reuses the persisted [LockCommandSeq]
         *  counter — the laptop's media dedup only needs monotonicity. */
        fun requestLaptopMedia(context: Context, op: String) {
            val seq = com.vortex.a3.core.appstate.LockCommandSeq.next(context)
            pendingMediaControl.set(
                PendingMediaControl(
                    op = op,
                    seq = seq,
                    expiresAtMs = android.os.SystemClock.elapsedRealtime() + MEDIA_CMD_TTL_MS,
                ),
            )
            requestStatePush()
            lockPushHandler.postDelayed(
                { if (pendingMediaControl.get()?.seq == seq) requestStatePush() },
                2_000L,
            )
        }

        /**
         * Ask whichever LanServer is running to re-announce its NSD service.
         * UI calls this after committing a fact (locale, theme, …) the peer
         * should learn now instead of on the next heartbeat. No-op when down.
         */
        fun requestLanNudge() {
            liveLan?.nudge()
        }

        /** Public entrypoint: start the service from anywhere. Idempotent. */
        fun start(context: Context) {
            val intent = Intent(context, VortexService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        /** Public entrypoint: stop the service (used by Forget-all). */
        fun stop(context: Context) {
            context.stopService(Intent(context, VortexService::class.java))
        }

        /** Start the service if down, or — if up — nudge it to re-register the
         *  telephony call listener. Used right after the user grants
         *  READ_PHONE_STATE (the trusted-launch path never requests it).
         *  Idempotent. */
        fun startOrRefreshCallFlow(context: Context) {
            val intent = Intent(context, VortexService::class.java)
                .setAction(ACTION_REFRESH_CALLFLOW)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }
    }
}
