package com.vortex.a3.service

import android.app.Service
import android.content.Context
import android.os.Build
import android.util.Log
import com.vortex.a3.core.ble.Advertiser
import com.vortex.a3.core.ble.GattServer
import android.bluetooth.BluetoothManager
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.identity.Platform
import com.vortex.a3.core.lan.LanServer
import com.vortex.a3.core.pairing.PairingOrchestrator
import com.vortex.a3.core.pairing.ReconnectOrchestrator
import com.vortex.a3.core.storage.EncryptedPrefsIdentityStore
import com.vortex.a3.core.storage.EncryptedPrefsPeerStore
import com.vortex.a3.core.storage.PeerStore
import com.vortex.a3.core.storage.loadOrGenerate
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.launch

/**
 * The Vortex network stack — everything [VortexService] used to wire inline:
 * the BLE GATT server + advertiser + reconnect responder, the LAN mDNS +
 * TCP IK listener and its AppState sync, and the Phase 2/3 call + media
 * hand-off orchestrators. Pulled out so VortexService is a thin lifecycle +
 * notification shell.
 *
 * The stack also implements [VortexNotification.Host]: it owns the live
 * state (who holds the buds, the latest peer AppState) the notification
 * renders, so the service can hand it straight to the notifier.
 *
 * Coupling back to the service is deliberately narrow: a [Service] for
 * Context / receiver registration, and an `onStateChanged` callback (wired
 * to the notification refresh) invoked when fresh peer state arrives.
 */
class VortexStack(internal val service: Service) : VortexNotification.Host {

    internal val ctx: Context get() = service.applicationContext
    internal val scope: CoroutineScope = CoroutineScope(SupervisorJob())

    private var advertiser: Advertiser? = null
    internal var gattServer: GattServer? = null
    /** Buffers phone→laptop notifications that fail to send while BLE is down;
     *  flushed when the peer re-subscribes to AUDIO_SIGNAL. */
    internal val notificationOutbox = com.vortex.a3.core.notif.NotificationOutbox()
    /** Packages whose icon we've already pushed to the laptop this BLE session
     *  (one icon per app; cleared on re-subscribe so it re-sends after a drop
     *  until the laptop has it cached). */
    internal val sentIconPkgs = java.util.Collections.synchronizedSet(HashSet<String>())
    private var clipboardListener: com.vortex.a3.core.clipboard.ClipboardListener? = null
    internal var wifiDirectTeardownJob: kotlinx.coroutines.Job? = null
    /** Icon PNG bytes per ICON frame chunk (kept under the BLE notify MTU
     *  once the appId header + AEAD tag + frame header are added). */
    internal val ICON_CHUNK = 180
    /** Bytes of JSON per bulk-companion frame (contacts/call-log/SMS). The link
     *  negotiates the max BLE MTU (~517 → 514-byte notifies observed), so a 450-
     *  byte payload (+4 chunk header +16 AEAD +4 frame header ≈ 474) fits safely.
     *  Big chunks mean ~2.5× fewer notifies than the old 180, which is what keeps
     *  the Noise receive cipher in sync — one lost notify desyncs the whole run,
     *  so fewer notifies = far fewer desync drops with three datasets in flight. */
    internal val CONTACTS_CHUNK = 450
    /** Reads the phone's contacts + observes changes; emits to contactsBus. */
    /** Pending mirror-burst refresh, armed on AUDIO_SIGNAL subscribe and
     *  cancelled on disconnect — so only a session stable for
     *  [MIRROR_REFRESH_SETTLE_MS] pays the ~37-chunk BLE storm. */
    private var mirrorRefreshJob: kotlinx.coroutines.Job? = null

    internal var contactsProvider: com.vortex.a3.core.contacts.ContactsProvider? = null
    /** Reads the phone's recent call log + observes changes; emits to callLogBus. */
    internal var callLogProvider: com.vortex.a3.core.calllog.CallLogProvider? = null
    /** Reads the phone's recent SMS + observes changes; emits to smsBus. */
    internal var smsProvider: com.vortex.a3.core.sms.SmsProvider? = null
    /** Serializes the bulk companion transfers (contacts + call log). They both
     *  fire on BLE re-subscribe; sending two chunk bursts at once doubles the
     *  notify rate and overruns the BLE link → lost notifies desync the Noise
     *  receive cipher. One-at-a-time keeps each burst at the proven 12ms rate. */
    internal val companionSendMutex = kotlinx.coroutines.sync.Mutex()
    internal var lanServer: LanServer? = null
    private var pairingOrchestrator: PairingOrchestrator? = null
    private var reconnectOrchestrator: ReconnectOrchestrator? = null
    internal var callFlowOrchestrator: com.vortex.a3.core.call.CallFlowOrchestrator? = null
    /** Acts on the current call for the laptop's call banner (Accept/Decline/
     *  End/Mute over BLE → TelecomManager/AudioManager). Lazy: `ctx` resolves
     *  to `service.applicationContext`, which is null until the Service's base
     *  context is attached (after construction). */
    internal val callController by lazy { com.vortex.a3.core.call.CallController(ctx) }
    /** Highest call-control `seq` already handled — dedups the SAME command
     *  arriving over both transports (BLE CALL_CONTROL frame + the AppState
     *  `call_control` LAN fallback). A fresh click always has a larger seq. */
    @Volatile internal var lastHandledCallControlSeq: Long = 0L

    /** Phase 3 — smart audio-follow: grabs the buds to the phone when media
     *  starts here. */
    internal var mediaHandoff: com.vortex.a3.core.media.MediaHandoffCoordinator? = null
    /** Latest local media-playing state, published on the AppState heartbeat
     *  (advisory; the auto-grab decision is local). */
    @Volatile internal var localMediaPlaying: Boolean = false
    /** Last-seen peer (laptop) media-playing value, for firing the
     *  buds-release on the peer's not-playing → playing edge. */
    @Volatile internal var lastPeerMediaPlaying: Boolean = false
    /** Bluetooth handle used to release the buds when the laptop starts
     *  playing (symmetric to the Linux release-on-peer-media). */
    internal var audioCtl: com.vortex.a3.core.earbuds.AudioDeviceController? = null
    /** Latest peer (laptop) AppState — its battery + earbuds info feed the
     *  foreground notification's battery readout + owner indicator. */
    @Volatile internal var latestPeerState: com.vortex.a3.core.appstate.AppState? = null
    /** elapsedRealtime() when [latestPeerState] last arrived — used to tell
     *  whether the peer link is still alive (state is never cleared on
     *  disconnect, so freshness, not presence, is the liveness signal). */
    @Volatile internal var latestPeerStateAtMs: Long = 0L
    internal var fakeCallReceiver: android.content.BroadcastReceiver? = null
    internal var wifiDirectReceiver: android.content.BroadcastReceiver? = null
    /** Stashed for the BT-restart path (see [startBleComponents]). */
    private var identity: IdentityRecord? = null
    internal lateinit var peerStore: PeerStore
    /** Invoked when fresh peer state arrives, so the notification refreshes. */
    internal var onStateChanged: () -> Unit = {}

    /** True once [start] has wired the stack (advertiser is up). */
    fun isStarted(): Boolean = advertiser != null

    // ---- VortexNotification.Host ----
    override fun phoneOwnsBuds(): Boolean {
        val mac = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx)?.address
        return mac != null && audioCtl?.isConnected(mac) == true
    }
    override fun peerState(): com.vortex.a3.core.appstate.AppState? = latestPeerState
    override fun peerStateAgeMs(): Long {
        val at = latestPeerStateAtMs
        return if (at == 0L) Long.MAX_VALUE
        else android.os.SystemClock.elapsedRealtime() - at
    }
    override fun phoneEarbudsBattery(): Int? = try {
        com.vortex.a3.core.earbuds.EarbudsDetector.readConnectedEarbuds(ctx)?.battery
    } catch (_: Throwable) { null }

    /**
     * Bring the whole stack up. `onStateChanged` is called when inbound peer
     * AppState arrives (wired to the notification refresh). Returns false if
     * the GATT server couldn't open — the caller should stopSelf().
     */
    fun start(onStateChanged: () -> Unit): Boolean {
        this.onStateChanged = onStateChanged
        // Identity + peers — same stores as MainActivity (Keystore-backed).
        val identityStore = EncryptedPrefsIdentityStore(ctx)
        peerStore = EncryptedPrefsPeerStore(ctx)
        val identity = identityStore.loadOrGenerate(Platform.Android)

        // Earbuds-switch orchestrator (Phase 1). Idempotent — also
        // initialised by MainActivity for the foreground path. The first
        // caller wins; both pass the same peerStore so the orchestrator
        // state is consistent regardless of who's hosting.
        com.vortex.a3.core.earbuds.EarbudsSwitchHolder.init(ctx, peerStore)

        // Pairing orchestrator is INTENTIONALLY NOT wired in the background
        // service. Once trust exists the service's job is strictly to keep
        // the trusted-presence advertiser + reconnect path alive; opening a
        // new pairing requires the user to bring MainActivity to the
        // foreground. Wiring a PairingOrchestrator here would let any nearby
        // attacker connect to our GATT server, push a Noise XX msg1, and
        // harvest our static public key from the encrypted msg2 — a
        // "Pre-Trust Privacy" leak per the pairing-model notes.
        pairingOrchestrator = null

        // BLE components FIRST (reconnect IK responder + GATT server +
        // trusted-presence advertiser). If the GATT server can't open — BT is
        // off or mid-cycle — bail out BEFORE starting call-flow / media-follow
        // / the fake-call receiver, so the service's auto-retry re-runs start()
        // cleanly instead of leaking a half-registered receiver. Factored out
        // so the BT-state receiver can also rebuild them after a BT off/on.
        this.identity = identity
        if (!startBleComponents(identity)) {
            return false
        }

        val callFlow = startCallFlow()        // Phase 2 — call hand-off
        startMediaFollow()                    // Phase 3 — smart audio-follow
        registerFakeCallReceiver(callFlow)    // debug-only test hook
        registerWifiDirectReceiver()          // debug-only Wi-Fi Direct validation
        watchSwitchStateForCall(callFlow)     // cancel speakerphone on connect
        forwardNotifications()                // phone notifications → laptop over BLE
        startClipboardOutbound()              // clipboard text/image + shared file → laptop
        forwardLiveActivities()               // ride/nav ETA pills → laptop top bar
        forwardCallEvents()                   // call banner + in-call pill → laptop
        forwardHandoff()                      // shared page (Handoff) → laptop opens it
        startContacts()                       // phone contacts → laptop Contacts page
        startCallLog()                        // phone call log → laptop Recents page
        startSms()                            // phone SMS → laptop Messages page

        // Laptop→phone screen mirror: when the user toggles "view laptop screen"
        // (or closes the viewer), ship our AppState NOW over both transports so
        // the request reaches the laptop within ~1s instead of a heartbeat away.
        com.vortex.a3.core.mirror.LaptopMirror.onRequestChanged = {
            lanServer?.nudge()
            pushStateViaBle()
        }
        // The laptop could not cast: say so. Without this the user taps, nothing
        // appears, and the reason sits in a log on the other machine — which is
        // exactly how "Extended display" failing on a non-GNOME desktop looked
        // like the app doing nothing at all.
        com.vortex.a3.core.mirror.LaptopMirror.onCastFailed = { reason ->
            android.os.Handler(android.os.Looper.getMainLooper()).post {
                try {
                    // English only: the app localizes through `ui/Strings.kt`,
                    // whose `str()` is @Composable and so unavailable here, and a
                    // service has no locale context to pick with. Worth moving
                    // into the UI layer if this message becomes prominent.
                    android.widget.Toast.makeText(
                        ctx,
                        "Can't show the laptop screen: $reason",
                        android.widget.Toast.LENGTH_LONG,
                    ).show()
                } catch (t: Throwable) {
                    // A toast is best-effort (blocked in the background on some
                    // ROMs); the request is cleared either way, which is the part
                    // that matters — the UI is usable again.
                    Log.w(TAG, "cast-failure toast suppressed: ${t.message}")
                }
            }
        }

        startLanServer(identity)              // mDNS + TCP IK + AppState sync
        return true
    }

    /** Tear the stack down (service onDestroy). */
    fun stop() {
        clipboardListener?.stop()
        wifiDirectTeardownJob?.cancel()
        com.vortex.a3.core.lan.WifiDirect.stop()
        scope.cancel()
        advertiser?.stopAll()
        gattServer?.stop()
        lanServer?.stop()
        callFlowOrchestrator?.stop()
        mediaHandoff?.stop()
        fakeCallReceiver?.let { try { service.unregisterReceiver(it) } catch (_: Exception) {} }
        wifiDirectReceiver?.let { try { service.unregisterReceiver(it) } catch (_: Exception) {} }
        if (VortexService.liveLan === lanServer) VortexService.liveLan = null
        if (VortexService.liveStack === this) VortexService.liveStack = null
    }

    /** Re-register the telephony listener after READ_PHONE_STATE is granted
     *  post-launch (idempotent). */
    fun refreshCallFlow() {
        if (callFlowOrchestrator?.start() == true) {
            Log.i(TAG, "call-flow refreshed; telephony listener now active")
        }
    }

    /**
     * Phase 3 — smart audio-follow. When media starts playing on this phone
     * while the buds are elsewhere, pull them over automatically (same
     * orchestrator seam the call-flow uses). A call always wins.
     */
    private fun startMediaFollow() {
        val ctl = com.vortex.a3.core.earbuds.AudioDeviceController(ctx)
        audioCtl = ctl
        val media = com.vortex.a3.core.media.MediaHandoffCoordinator(
            context = ctx,
            weOwnBuds = {
                val mac = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx)?.address
                mac != null && ctl.isConnected(mac)
            },
            // A grab only makes sense if the laptop is reachable RIGHT NOW
            // (fresh AppState = live link) AND it currently holds the buds.
            // Otherwise (no laptop, or buds in their case) there is nothing
            // to pull, so we must not pause the phone's media.
            peerHoldsBuds = {
                val fresh = android.os.SystemClock.elapsedRealtime() - latestPeerStateAtMs < PEER_FRESH_MS
                fresh && latestPeerState?.earbuds?.connected == true
            },
            // The shared gate — covers calls taken with the buds already on
            // the phone too (pendingCallPhase alone misses those).
            isCallActive = { VortexService.callGateActive() },
            // Smart-switch grab + return reuse the EXACT same primitives as
            // the call hand-off so every direction behaves identically.
            requestGrab = { grabBudsToPhone() },
            requestReturnToPeer = { handBudsToLaptop() },
            onMediaPlayingChanged = { playing ->
                localMediaPlaying = playing
                // Push the advisory flag to the peer promptly so its owner
                // indicator tracks reality instead of waiting the 12 s tick.
                lanServer?.nudge()
            },
        )
        media.start()
        mediaHandoff = media
        // Seed + follow the shared smart-switch setting. The coordinator's
        // live flag tracks it; the UI toggle and peer-LWW adoption both flow
        // through SmartSwitchSetting (single source of truth, persisted).
        com.vortex.a3.core.media.SmartSwitchSetting.init(ctx)
        com.vortex.a3.core.notif.NotificationMirrorSetting.init(ctx)
        com.vortex.a3.core.clipboard.ClipboardSyncSetting.init(ctx)
        scope.launch {
            com.vortex.a3.core.media.SmartSwitchSetting.enabled.collect { on ->
                media.smartSwitchEnabled = on
            }
        }
        // Automatic phone→laptop clipboard capture (needs the READ_CLIPBOARD
        // AppOp for background reads; degrades to the QS tile otherwise). Armed
        // / disarmed with the same local toggle that gates the rest of sync.
        val clipListener = com.vortex.a3.core.clipboard.ClipboardListener(ctx)
        clipboardListener = clipListener
        scope.launch {
            com.vortex.a3.core.clipboard.ClipboardSyncSetting.enabled.collect { on ->
                if (on) clipListener.start() else clipListener.stop()
            }
        }
    }

    /**
     * Watch the switch state — when the buds physically connect mid-call,
     * cancel the speakerphone fallback so audio routes through the buds.
     */
    private fun watchSwitchStateForCall(callFlow: com.vortex.a3.core.call.CallFlowOrchestrator) {
        scope.launch {
            com.vortex.a3.core.earbuds.EarbudsSwitchHolder.state.collect { s ->
                if (s is com.vortex.a3.core.earbuds.SwitchState.AlmostDone ||
                    s == com.vortex.a3.core.earbuds.SwitchState.Idle) {
                    callFlow.notifyBudsConnected()
                }
            }
        }
    }

    /** Latest contacts snapshot (JSON bytes + sha256-hex) for the LAN
     *  bulk-sync responder, refreshed by the contactsBus collector. */
    @Volatile internal var latestContactsJson: ByteArray? = null
    @Volatile internal var latestContactsHash: String? = null

    /** Hash last confirmed delivered to the laptop over LAN bulk-sync
     *  ("sent" or "match") — the BLE-burst skip condition. */
    @Volatile internal var lanDeliveredContactsHash: String? = null

    /** Latest call-log snapshot + LAN-delivered hash — see the contacts
     *  twins above for the model. */
    @Volatile internal var latestCallLogJson: ByteArray? = null
    @Volatile internal var latestCallLogHash: String? = null
    @Volatile internal var lanDeliveredCallLogHash: String? = null

    /** Latest recent-SMS snapshot + LAN-delivered hash — see the contacts
     *  twins above for the model. Bodies live only in memory. */
    @Volatile internal var latestSmsJson: ByteArray? = null
    @Volatile internal var latestSmsHash: String? = null
    @Volatile internal var lanDeliveredSmsHash: String? = null

    /**
     * Create + wire the BLE stack (reconnect IK responder, GATT server,
     * trusted-presence advertiser). Re-invocable: [start] calls it once, and
     * [restartBleComponents] again after the BT adapter cycles OFF→ON. A
     * fresh ReconnectOrchestrator each call so listeners don't accumulate.
     * Returns false if the GATT server fails to open.
     */
    private fun startBleComponents(identity: IdentityRecord): Boolean {
        val reconnect = ReconnectOrchestrator(identity, peerStore)
        reconnectOrchestrator = reconnect

        val server = GattServer(
            ctx,
            pairingOrchestrator = null,
            reconnectOrchestrator = reconnect,
        )
        if (!server.start()) {
            Log.e(TAG, "failed to start GATT server")
            return false
        }
        gattServer = server
        startNotesSync() // notes/todos bidirectional sync (NOTES_SYNC)

        // BLE-WRITE reverse channel: when the laptop AEAD-seals an
        // AudioOpFrame and WRITEs it to AUDIO_SIGNAL, the GattServer decrypts
        // and calls back here; we feed it through the SwitchOrchestrator's
        // normal onIncoming path. Malformed JSON just logs + drops.
        server.onAudioOpReceived = { peerPub, jsonBytes ->
            val frame = com.vortex.a3.core.earbuds.AudioOpFrame.fromJsonBytes(jsonBytes)
            if (frame == null) {
                Log.w(TAG, "BLE-write AudioOp: malformed payload from ${peerPub.toHexPrefix()}")
            } else {
                scope.launch {
                    try {
                        com.vortex.a3.core.earbuds.EarbudsSwitchHolder.onIncoming(peerPub, frame)
                    } catch (e: Exception) {
                        Log.w(TAG, "BLE-write AudioOp dispatch failed: ${e.message}")
                    }
                }
            }
        }
        // Laptop→phone notification mirroring: the laptop writes a
        // NOTIFICATION frame; show it as an Android notification, gated by
        // the phone's local "show laptop notifications" toggle.
        server.onNotificationReceived = { _, jsonBytes ->
            val m = com.vortex.a3.core.notif.NotificationMirror.fromJsonBytes(jsonBytes)
            if (m == null) {
                Log.w(TAG, "BLE-write NOTIFICATION: malformed payload")
            } else if (m.resync) {
                // Laptop dropped a BLE notify (or (re)connected) → re-send any
                // active notification it's missing. BLE Notify is fire-and-forget,
                // so a dropped notification is otherwise lost forever. knownKeys =
                // what the laptop already has, so we only re-send the rest.
                com.vortex.a3.core.media.MediaNotificationListenerService
                    .resendMissing(m.knownKeys.toHashSet())
            } else if (m.dismiss) {
                // Laptop cleared its mirror of one of OUR notifications →
                // dismiss the original on this phone.
                if (m.key.isNotEmpty()) {
                    com.vortex.a3.core.media.MediaNotificationListenerService.dismissByKey(m.key)
                }
            } else if (m.invokeIndex >= 0) {
                // Laptop clicked an action button → fire it (seq-deduped so the
                // LAN backstop copy doesn't double-fire).
                handleNotifInvoke(m)
            } else if (com.vortex.a3.core.notif.NotificationMirrorSetting.isShowPeer()) {
                com.vortex.a3.core.notif.IncomingNotificationDisplay.show(ctx, m)
            }
        }

        // Laptop→phone clipboard sync: the laptop copied text → set it on this
        // phone's clipboard. Setting the clipboard from a foreground service is
        // permitted (the read restriction doesn't apply to writes).
        server.onClipboardReceived = { _, jsonBytes ->
            if (com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) {
                try {
                    val o = org.json.JSONObject(String(jsonBytes, Charsets.UTF_8))
                    val text = o.optString("text").trim()
                    if (text.isNotEmpty()) {
                        // Loop guard BEFORE applying, so the auto-listener
                        // recognises this as just-applied and won't bounce it.
                        com.vortex.a3.core.clipboard.ClipboardSyncGuard.markApplied(
                            com.vortex.a3.core.clipboard.ClipboardSyncGuard.sig(text)
                        )
                        val cm = ctx.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                            as? android.content.ClipboardManager
                        cm?.setPrimaryClip(
                            android.content.ClipData.newPlainText("Vortex", text)
                        )
                        Log.i(TAG, "clipboard: synced from laptop (${text.length} chars)")
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "onClipboardReceived parse/apply failed: ${e.message}")
                }
            }
        }

        // Laptop→phone LONG clipboard text: reassemble CLIPBOARD_TEXT chunks
        // (byte-blind assembler), UTF-8-decode, then setPrimaryClip. Content
        // not logged (only length).
        val clipboardTextAsm = com.vortex.a3.core.clipboard.ClipboardImageAssembler()
        server.onClipboardTextChunk = { _, chunk ->
            if (com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) {
                try {
                    val bytes = clipboardTextAsm.add(chunk)
                    if (bytes != null) {
                        val text = String(bytes, Charsets.UTF_8).trim()
                        if (text.isNotEmpty()) {
                            com.vortex.a3.core.clipboard.ClipboardSyncGuard.markApplied(
                                com.vortex.a3.core.clipboard.ClipboardSyncGuard.sig(text)
                            )
                            val cm = ctx.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                                as? android.content.ClipboardManager
                            cm?.setPrimaryClip(
                                android.content.ClipData.newPlainText("Vortex", text)
                            )
                            Log.i(TAG, "clipboard: long text synced from laptop (${text.length} chars)")
                        }
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "onClipboardTextChunk apply failed: ${e.message}")
                }
            }
        }

        // Laptop→phone clipboard IMAGE: reassemble chunks, then write the PNG
        // to a cache file and put it on the clipboard via a FileProvider URI
        // (image ClipData needs a content:// URI; writing the clipboard from a
        // foreground service is allowed).
        val clipboardImageAsm = com.vortex.a3.core.clipboard.ClipboardImageAssembler()
        server.onClipboardImageChunk = { _, chunk ->
            if (com.vortex.a3.core.clipboard.ClipboardSyncSetting.isEnabled()) {
                try {
                    val png = clipboardImageAsm.add(chunk)
                    if (png != null) {
                        val dir = java.io.File(ctx.cacheDir, "clipboard").apply { mkdirs() }
                        // Unique filename per image. A fixed "incoming.png" was
                        // overwritten in place, so a second synced image
                        // corrupted the first one's STILL-PENDING paste:
                        // ClipData.newUri stores only the content:// reference
                        // and the pasting app reads the bytes lazily at paste
                        // time, by which point the file had changed underneath.
                        // nanoTime is monotonic in-process → no collisions.
                        val file = java.io.File(dir, "in_${System.nanoTime()}.png")
                        file.writeBytes(png)
                        // Keep the cache bounded: drop all but the newest few
                        // incoming images (older ones are unlikely to still be
                        // the pending clipboard content).
                        dir.listFiles { f -> f.name.startsWith("in_") }
                            ?.sortedByDescending { it.lastModified() }
                            ?.drop(8)
                            ?.forEach { it.delete() }
                        val uri = androidx.core.content.FileProvider.getUriForFile(
                            ctx, "${ctx.packageName}.clipboard", file
                        )
                        val cm = ctx.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                            as? android.content.ClipboardManager
                        val clip = android.content.ClipData.newUri(ctx.contentResolver, "Vortex", uri)
                        cm?.setPrimaryClip(clip)
                        Log.i(TAG, "clipboard: image synced from laptop (${png.size} bytes)")
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "onClipboardImageChunk apply failed: ${e.message}")
                }
            }
        }

        server.onStateReceived = { peerPub, jsonBytes ->
            // Laptop→phone app-state over BLE → same handler as the LAN
            // heartbeat, so the phone shows the laptop connected (battery /
            // charging / earbuds) even when Wi-Fi blocks device-to-device.
            val state = com.vortex.a3.core.appstate.AppState.fromJsonBytes(jsonBytes)
            if (state == null) {
                Log.w(TAG, "BLE-write STATE: malformed AppState payload")
            } else {
                handlePeerAppState(peerPub, state)
            }
        }

        server.onCallControlReceived = { _, jsonBytes ->
            // Laptop's call banner/pill clicked Accept/Decline/End/Mute → act on
            // the current call (BLE CALL_CONTROL frame — the fast path).
            val ctrl = com.vortex.a3.core.call.CallControl.fromJsonBytes(jsonBytes)
            if (ctrl == null) {
                Log.w(TAG, "BLE-write CALL_CONTROL: malformed payload")
            } else {
                handleCallControl(ctrl)
            }
        }

        // BLE notify path just became deliverable (the laptop (re)subscribed to
        // AUDIO_SIGNAL) → flush any notifications buffered during the outage,
        // so none are lost across BLE drops / reconnects.
        // BLE session gone → mDNS discovery matters again; re-hold the
        // multicast lock so the laptop can find us over LAN. Also drop any
        // pending mirror burst — a flapping session must not queue storms.
        server.onPeerDisconnected = { _ ->
            mirrorRefreshJob?.cancel()
            lanServer?.setBleLinked(false)
            // Engage the reconnect-seeking LOW_LATENCY advertising NOW —
            // waiting for the next 60s rotation cost the whole first
            // reconnect window after a walk-away.
            advertiser?.kickRotation()
        }

        server.onAudioSignalSubscribed = { _ ->
            // BLE fast-path is up → the Wi-Fi multicast lock buys nothing
            // (laptop reaches us via BLE + cached-IP heartbeat). Release it
            // to let the Wi-Fi radio sleep — it returns on disconnect.
            lanServer?.setBleLinked(true)
            // Connected → drop advertising back to BALANCED right away
            // (the boost only matters while the link is down).
            advertiser?.kickRotation()
            // Announce our current battery/charging/earbuds the instant the
            // laptop subscribes, so it shows us CONNECTED over BLE alone — vital
            // on Wi-Fi that blocks device-to-device traffic (AP isolation),
            // where the LAN heartbeat never completes.
            pushStateViaBle()
            // Push our notes/todos set on (re)connect so the laptop merges +
            // replies — converges both sides after an offline edit. Debounced.
            com.vortex.a3.core.notes.NoteSync.markDirty()
            // Re-send app icons after a reconnect (until the laptop has them
            // cached) and flush any notifications buffered during the outage.
            sentIconPkgs.clear()
            // Re-push every active live activity so its laptop tray reappears
            // after a reconnect even if its content didn't change meanwhile.
            for (la in com.vortex.a3.core.media.MediaNotificationListenerService.activeLiveActivities()) {
                VortexService.liveActivityBus.tryEmit(la)
            }
            // Re-send the companion mirrors (contacts/recents/SMS) so the
            // laptop's pages repopulate after a reconnect — but only once the
            // session has been STABLE for a few seconds. Firing the ~37-chunk
            // storm on every subscribe fed a live-observed feedback loop: a
            // notify lost mid-burst desyncs the receive cipher → the laptop's
            // 3-fail rule drops the session → re-IK → re-subscribe → another
            // storm. A flapping session now skips the burst entirely (the
            // laptop renders from its disk caches meanwhile).
            mirrorRefreshJob?.cancel()
            mirrorRefreshJob = scope.launch {
                kotlinx.coroutines.delay(MIRROR_REFRESH_SETTLE_MS)
                contactsProvider?.refresh()
                callLogProvider?.refresh()
                smsProvider?.refresh()
            }
            scope.launch {
                for (peer in peerStore.list()) {
                    val peerPub = peer.peerStaticPub
                    notificationOutbox.flush(peerPub.notifHex()) { mirror ->
                        gattServer?.sendNotificationEncrypted(peerPub, mirror.toJsonBytes()) ?: false
                    }
                }
            }
        }

        // P2.13 — every successful BLE-IK reconnect carries a fresh Noise
        // transport cipher pair. Register both ciphers + a BLE-NOTIFY writer
        // with the earbuds-switch orchestrator so AUDIO_OP frames can ride
        // the persistent GATT link instead of waiting for a LAN TCP+IK session.
        reconnect.addListener { outcome ->
            server.registerAudioSession(
                outcome.peerStaticPub,
                outcome.device,
                outcome.ciphers.sender,
                outcome.ciphers.receiver,
            )
            val peerPub = outcome.peerStaticPub.copyOf()
            val bleWriter: suspend (com.vortex.a3.core.earbuds.AudioOpFrame) -> Result<Unit> = { f ->
                val ok = server.sendAudioOpEncrypted(peerPub, f.toJsonBytes())
                if (ok) Result.success(Unit)
                else Result.failure(IllegalStateException("BLE audio-signal not ready (no cipher / no subscriber)"))
            }
            com.vortex.a3.core.earbuds.EarbudsSwitchHolder.setBleWriter(peerPub, bleWriter)
            Log.i(TAG, "P2.13: BLE audio writer registered for peer=${peerPub.take(4).joinToString("") { "%02x".format(it) }}…")
            // The IK handshake itself proves the laptop is alive RIGHT NOW —
            // refresh the UI's last-seen immediately (re-emitting the last
            // known snapshot) instead of waiting ~1.5s for the laptop's
            // first state heartbeat. Without this the laptop UI flips to
            // "connected" visibly earlier than the phone after a reconnect.
            latestPeerState?.let { st ->
                VortexService.peerStateBus.tryEmit(peerPub.toHex() to st)
            }
        }

        // Advertiser: trusted-presence with rotating token if trust exists;
        // pairable mode is opened explicitly by MainActivity via an intent.
        val adv = Advertiser(ctx)
        // Reconnect-seeking boost: while the laptop link is down and was
        // lost within the last 10 minutes (walk-away/walk-back cycles, app
        // restarts), advertise LOW_LATENCY so the laptop's connect lands in
        // ~1-2s instead of ~11s (MIUI throttles screen-off advertising).
        // After 10 minutes away it falls back to BALANCED — advertising
        // fast all day with no laptop around would just burn battery.
        adv.fastModeProvider = provider@{
            val srv = gattServer ?: return@provider false
            !srv.hasActiveConnection() &&
                android.os.SystemClock.elapsedRealtime() - srv.lastDisconnectAtMs <
                FAST_ADV_WINDOW_MS
        }
        val firstPeer = peerStore.list().firstOrNull()
        if (firstPeer != null) {
            adv.startTrustedPresence(
                prs = firstPeer.prs,
                scope = scope,
                rotationWindowSec = 60L,
                onError = { reason -> Log.w(TAG, "presence adv error: $reason") },
            )
            Log.i(TAG, "trusted-presence advertising started (have ${peerStore.list().size} peer(s))")
        } else {
            Log.i(TAG, "no trust — service idle, awaiting pairing")
        }
        advertiser = adv
        return true
    }

    /**
     * Re-create the BLE stack after the BT adapter has come back ON. Tears
     * down the now-invalid advertiser/GATT handles first then rebuilds. LAN
     * is untouched (rides Wi-Fi). Called by VortexReceivers on STATE_ON.
     */
    fun restartBleComponents() {
        val id = identity ?: run {
            Log.w(TAG, "BT re-enabled but no identity yet; skipping BLE restart")
            return
        }
        Log.i(TAG, "Bluetooth re-enabled — restarting BLE advertiser + GATT server")
        try { advertiser?.stopAll() } catch (_: Exception) {}
        try { gattServer?.stop() } catch (_: Exception) {}
        advertiser = null
        gattServer = null
        if (!startBleComponents(id)) {
            Log.e(TAG, "BLE restart failed to reopen GATT server")
        }
    }

    /**
     * Hand the earbuds back to the laptop — the ONE return path, shared by
     * call-end + smart-switch media-stop: announce a BLE `Claim` (~200 ms)
     * and release locally in parallel, with the LAN pendingAudioClaim + nudge
     * as the BLE-down fallback. False if there's no trusted peer / saved buds.
     */
    fun handBudsToLaptop(): Boolean {
        val firstPeer = peerStore.list().firstOrNull() ?: return false
        val saved = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx) ?: return false
        com.vortex.a3.core.earbuds.EarbudsSwitchHolder.claim(firstPeer.peerStaticPub, saved.address)
        VortexService.pendingAudioClaim.set(true)
        lanServer?.nudge()
        return true
    }

    /**
     * Pull the earbuds to this phone — the ONE grab path, shared by call-start
     * + smart-switch media-play. Runs the AudioOp initiator; the laptop
     * releases and our connect-retry lands the buds. True only if the switch
     * actually started (orchestrator idle); false if BT is off / unconfigured.
     */
    fun grabBudsToPhone(): Boolean {
        if (!isBluetoothOn()) {
            Log.i(TAG, "grab skipped — phone Bluetooth is OFF")
            return false
        }
        val firstPeer = peerStore.list().firstOrNull() ?: return false
        val saved = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx) ?: return false
        return com.vortex.a3.core.earbuds.EarbudsSwitchHolder
            .request(firstPeer.peerStaticPub, saved.address)
    }

    /**
     * Push the current AppState to the trusted peer over the persistent BLE
     * link as a STATE frame — instant (~200 ms, works in-pocket). No-op if
     * BLE isn't up; the LAN nudge alongside (in the caller) is the fallback.
     */
    fun pushStateViaBle() {
        val peerPub = peerStore.list().firstOrNull()?.peerStaticPub ?: return
        val server = gattServer ?: return
        scope.launch {
            try {
                val json = buildLocalAppState().toJsonBytes()
                if (server.sendStateEncrypted(peerPub, json)) {
                    Log.i(TAG, "state pushed over BLE")
                }
            } catch (e: Exception) {
                Log.w(TAG, "pushStateViaBle failed: ${e.message}")
            }
        }
    }

    /**
     * LAN: mDNS publish + TCP listener for IK reconnect, wired to the
     * AppState sync. VortexService runs only when trust exists, so mDNS
     * publishes the trusted-runtime service type with a rotating
     * presence-token instance name (spec §5.4).
     */
    private fun startLanServer(identity: IdentityRecord) {
        val lan = LanServer(ctx, identity, peerStore).also {
            it.start(com.vortex.a3.core.lan.LanServerMode.TrustedRuntime)
        }
        VortexService.liveLan = lan
        VortexService.liveStack = this
        lan.localAppStateProvider = { buildLocalAppState() }
        lan.onPeerAppState = { peerPub, state -> handlePeerAppState(peerPub, state) }
        // LAN bulk-sync responder: hand the latest mirror snapshot to a
        // laptop whose cached hash is stale; null = its cache is current.
        lan.bulkProvider = provider@{ key, peerHash ->
            when (key) {
                "contacts" -> {
                    val json = latestContactsJson ?: return@provider null
                    val hash = latestContactsHash ?: return@provider null
                    if (hash == peerHash) null else Pair(json, hash)
                }
                "call_log" -> {
                    val json = latestCallLogJson ?: return@provider null
                    val hash = latestCallLogHash ?: return@provider null
                    if (hash == peerHash) null else Pair(json, hash)
                }
                "sms" -> {
                    val json = latestSmsJson ?: return@provider null
                    val hash = latestSmsHash ?: return@provider null
                    if (hash == peerHash) null else Pair(json, hash)
                }
                "sms_ids" -> {
                    // Deletion reconcile: full id list, computed on demand
                    // (one indexed column — cheap). Canonical form matches
                    // the laptop's: compact JSON array, ids ascending.
                    if (!com.vortex.a3.core.sms.SmsMirrorSetting.isEnabled()) return@provider null
                    val ids = smsProvider?.readAllIds() ?: return@provider null
                    val json = org.json.JSONArray(ids).toString().toByteArray(Charsets.UTF_8)
                    val hash = sha256Hex(json)
                    if (hash == peerHash) null else Pair(json, hash)
                }
                else -> null
            }
        }
        lan.onBulkDelivered = { key, hash ->
            when (key) {
                "contacts" -> lanDeliveredContactsHash = hash
                "call_log" -> lanDeliveredCallLogHash = hash
                "sms" -> lanDeliveredSmsHash = hash
            }
        }
        // Watermark datasets: full history backfills, capped per session —
        // the laptop's advancing watermark self-paginates the remainder.
        lan.historyProvider = { key, since ->
            when {
                key == "sms_history" && com.vortex.a3.core.sms.SmsMirrorSetting.isEnabled() ->
                    smsProvider?.readHistorySince(since, 5000)
                        ?.takeIf { it.isNotEmpty() }
                        ?.let { com.vortex.a3.core.sms.smsToJsonBytes(it) }
                key == "call_log_history" && com.vortex.a3.core.calllog.CallLogMirrorSetting.isEnabled() ->
                    callLogProvider?.readHistorySince(since, 5000)
                        ?.takeIf { it.isNotEmpty() }
                        ?.let { com.vortex.a3.core.calllog.callLogToJsonBytes(it) }
                else -> null
            }
        }
        // Screen-mirror: a dedicated mirror session (first frame = SCREEN_MIRROR
        // START) is handed here. We build a MirrorSession that, on START, starts
        // the capture/encode service streaming sealed H.264 over UDP back to the
        // laptop — but only if the user already granted MediaProjection consent
        // (armed via MirrorConsentActivity). Input injection is wired in M3.
        lan.mirrorHandler = { sock, input, output, pair, handshakeHash, firstFrame ->
            val laptopIp = sock.inetAddress?.hostAddress
            if (laptopIp == null) {
                Log.w(TAG, "mirror: no laptop IP; dropping session")
            } else {
                com.vortex.a3.core.lan.MirrorSession(
                    sock, input, output, pair, handshakeHash,
                    onStart = onStart@{ start ->
                        // The laptop's START frame IS the consent trigger: pop a
                        // fresh MediaProjection dialog (the token is single-use)
                        // and begin capturing only once the user taps "Start now".
                        // Debounce: a duplicate START (laptop briefly opens two
                        // control sessions) must NOT pop a second dialog.
                        if (!MirrorConsent.beginPrompt()) {
                            Log.i(TAG, "mirror: duplicate START — consent already pending, ignoring")
                            return@onStart
                        }
                        val beginStream = {
                            val token = MirrorConsent.resultData
                            if (token != null) {
                                val key = com.vortex.a3.core.mirror.MirrorUdp.deriveMediaKey(handshakeHash)
                                ScreenMirrorService.start(
                                    ctx, MirrorConsent.resultCode, token,
                                    laptopIp, start.udpPort, start.w, start.h, start.fps, start.bitrate, key,
                                )
                                // ScreenMirrorService keeps its own copy via the
                                // intent; drop ours so the next session re-prompts.
                                MirrorConsent.clear()
                            } else {
                                Log.w(TAG, "mirror START but consent denied; no stream")
                            }
                        }
                        MirrorConsent.onResult = { granted -> if (granted) beginStream() }
                        // Background-safe: opens the consent directly when an
                        // activity is visible, else via a full-screen-intent
                        // notification (a background startActivity is blocked, so
                        // the request used to silently do nothing when Vortex was
                        // closed).
                        com.vortex.a3.core.mirror.MirrorRequestNotification.prompt(ctx)
                    },
                    onInput = { pkt -> VortexInputService.instance?.onPacket(pkt) },
                    onRequestKeyframe = { ScreenMirrorService.requestKeyframe(ctx) },
                    onStop = { ScreenMirrorService.stop(ctx) },
                ).run(firstFrame)
            }
        }
        lanServer = lan
    }

    /**
     * Toggle which device holds the buds (foreground-notification "Switch"
     * action). [onTarget] is called with the side we're switching TO so the
     * notifier can tint the row + fire quick refreshes.
     */
    fun toggleAudio(onTarget: (String) -> Unit) {
        val mac = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx)?.address ?: return
        val firstPeer = peerStore.list().firstOrNull() ?: return
        mediaHandoff?.noteManualSwitch()
        if (audioCtl?.isConnected(mac) == true) {
            // Phone holds them → hand to the laptop.
            Log.i(TAG, "notification: switch buds phone → laptop")
            onTarget("laptop")
            scope.launch { audioCtl?.disconnect(mac) }
            VortexService.pendingAudioClaim.set(true)
            lanServer?.nudge()
        } else {
            // Laptop holds them → grab to the phone.
            Log.i(TAG, "notification: switch buds laptop → phone")
            onTarget("phone")
            com.vortex.a3.core.earbuds.EarbudsSwitchHolder.request(firstPeer.peerStaticPub, mac)
        }
    }

    /** True if this phone's Bluetooth adapter is on — i.e. it can actually
     *  connect to the earbuds. */
    internal fun isBluetoothOn(): Boolean =
        service.getSystemService(BluetoothManager::class.java)?.adapter?.isEnabled == true

    companion object {
        internal const val TAG = "VortexStack"
        /** How long after losing the laptop link the phone keeps
         *  advertising in LOW_LATENCY (reconnect-seeking) mode. */
        internal const val FAST_ADV_WINDOW_MS = 10 * 60_000L

        /** How long an AUDIO_SIGNAL subscription must stay up before the
         *  companion mirror burst (contacts/recents/SMS, ~37 chunks) fires.
         *  Guards the desync feedback loop — see onAudioSignalSubscribed. */
        internal const val MIRROR_REFRESH_SETTLE_MS = 3_000L
        /** How long a peer AppState stays "fresh" before we treat the link
         *  as down for hand-off purposes. The heartbeat lands every ~12 s,
         *  so 30 s tolerates one missed beat without false-positives. */
        internal const val PEER_FRESH_MS = 30_000L
    }
}
