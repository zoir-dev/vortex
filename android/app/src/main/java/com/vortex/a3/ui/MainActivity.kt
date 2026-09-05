package com.vortex.a3.ui

import android.util.Log
import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material.icons.outlined.Laptop
import androidx.compose.material3.AlertDialog
import com.vortex.a3.BuildConfig
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import android.provider.Settings
import kotlinx.coroutines.launch
import kotlinx.coroutines.isActive
import kotlinx.coroutines.delay
import com.vortex.a3.core.ble.Advertiser
import com.vortex.a3.core.ble.GattServer
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.identity.Platform
import com.vortex.a3.core.lan.LanServer
import com.vortex.a3.core.pairing.PairingOrchestrator
import com.vortex.a3.core.pairing.ReconnectOrchestrator
import com.vortex.a3.core.storage.EncryptedPrefsIdentityStore
import com.vortex.a3.core.storage.EncryptedPrefsPeerStore
import com.vortex.a3.core.storage.PeerStore
import com.vortex.a3.core.storage.TrustedPeer
import com.vortex.a3.core.storage.loadOrGenerate
import com.vortex.a3.service.VortexService
import com.vortex.a3.core.appstate.AppState
import com.vortex.a3.ui.components.EarbudsCard
import com.vortex.a3.ui.components.PeerDeviceCard
import com.vortex.a3.ui.components.toHex
import com.vortex.a3.ui.screens.HomeScreen
import com.vortex.a3.ui.screens.SettingsScreen
import com.vortex.a3.core.earbuds.EarbudsStore
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update

class MainActivity : ComponentActivity() {

    internal val advertiser by lazy { Advertiser(applicationContext) }
    internal val gattServer by lazy { GattServer(applicationContext) }
    internal val identityStore by lazy { EncryptedPrefsIdentityStore(applicationContext) }
    internal val peerStore: PeerStore by lazy { EncryptedPrefsPeerStore(applicationContext) }
    internal var pairingOrchestrator: PairingOrchestrator? = null
    internal var lanServer: LanServer? = null

    /** 8-byte instance ID shared between the BLE pairable advertise
     *  and the mDNS `_vortex-pair._tcp.` record so a single discoverer
     *  can correlate both transports as the same device. Computed
     *  once when the LanServer enters PairingWindow mode. */
    internal var pairingInstanceId: ByteArray? = null

    internal val state = MutableStateFlow<AdvertiseState>(AdvertiseState.Idle)
    internal val identityState = MutableStateFlow<IdentityRecord?>(null)
    internal val handshakeState = MutableStateFlow<PairingOrchestrator.HandshakeOutcome?>(null)
    internal val reconnectState = MutableStateFlow<ReconnectOrchestrator.ReconnectOutcome?>(null)
    internal val peerCountState = MutableStateFlow(0)
    internal val peerListState = MutableStateFlow<List<TrustedPeer>>(emptyList())

    /** Latest peer AppState snapshots from VortexService (peer pub hex → state). */
    internal val peerStatesState = MutableStateFlow<Map<String, AppState>>(emptyMap())

    /**
     * Wall-clock ms when we last received any traffic from each peer.
     * Drives the laptop card's green/grey status dot — Linux heartbeats
     * every ~12-15s, so a gap longer than [LAPTOP_STALE_MS] means the
     * daemon is gone (laptop closed, daemon killed, network dropped).
     */
    internal val peerLastSeenState = MutableStateFlow<Map<String, Long>>(emptyMap())

    /**
     * Ticks every few seconds so the laptop status dot can re-evaluate
     * staleness without waiting for the next peer message.
     */
    internal val nowTickState = MutableStateFlow(System.currentTimeMillis())

    /** Locally-connected wireless audio (BluetoothA2dp); polled while UI is alive. */
    internal val localEarbudsState =
        MutableStateFlow<com.vortex.a3.core.appstate.EarbudsInfo?>(null)
    internal var earbudsPollJob: kotlinx.coroutines.Job? = null

    // ---- In-app BT picker state ----
    internal val earbudsPickerState = MutableStateFlow(PickerState())
    internal var pickerScanJob: kotlinx.coroutines.Job? = null

    /** Mirrors `EarbudsStore.load() != null` reactively — updated on
     *  save/clear so the long-press menu shows the right actions. */
    internal val savedEarbudsExists = MutableStateFlow(false)

    /**
     * Permission launcher dedicated to the earbuds picker. We can't
     * reuse the advertising launcher because it auto-starts advertising
     * once permissions resolve, which is unrelated. On grant, we kick
     * off the scan; on deny we leave the picker open with an empty row
     * list so the user sees the empty state.
     */
    internal val pickerPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { granted ->
        val ok = granted.values.all { it }
        if (ok) {
            startPickerScan()
        } else {
            earbudsPickerState.value = PickerState(open = true, scanning = false, rows = emptyList())
        }
    }

    // ------- Persisted per-device UI settings -------
    // Shared prefs handle, also used for the MIUI-Autostart flags below.
    internal val settingsPrefs by lazy {
        getSharedPreferences("vortex_ui_settings", MODE_PRIVATE)
    }
    /** Locale + theme: persistence + observable state (see UiSettingsStore).
     *  Lazy — its ctor opens SharedPreferences, which needs the Activity's
     *  base context attached (null during field init → crash). First touch
     *  is in onCreate, by which point the context is ready. */
    internal val uiSettings by lazy { UiSettingsStore(this) }

    /**
     * Pending pairing awaiting the user's SAS decision. Drives the
     * approval AlertDialog. Cleared when the orchestrator transitions out
     * of `WaitingForPeerApproval` (BothApproved or PeerRejected).
     */
    internal val pendingApproval =
        MutableStateFlow<PairingOrchestrator.HandshakeOutcome?>(null)

    /** Drives the "grant notification access" dialog. Set on entry
     *  (onResume) whenever a peer is paired but notification-listener access
     *  isn't granted yet — like the call-permission ask, it re-appears every
     *  time the user opens the app until they grant it. */
    internal val showNotifAccessDialog = MutableStateFlow(false)

    /** Drives the one-time "enable Autostart" dialog. Unlike notification
     *  access, MIUI Autostart can't be read back programmatically, so we
     *  can't tell when it's done — re-prompting every entry would nag
     *  forever. We prompt ONCE (persisted flag) and leave the passive hint
     *  card ([autostartHintDismissed]) as the standing reminder. */
    internal val showAutostartDialog = MutableStateFlow(false)

    /** Whether the user has dismissed the standing MIUI-Autostart hint card.
     *  Since Autostart state can't be read back, the card can't auto-clear;
     *  the user taps "Got it" once they've enabled it and we persist that so
     *  it stays hidden across launches. Seeded from prefs. */
    internal val autostartHintDismissed by lazy {
        MutableStateFlow(settingsPrefs.getBoolean("autostart_hint_dismissed", false))
    }

    /** True when the Bluetooth adapter is off (or absent). Drives the home
     *  "Bluetooth is off" banner — without BT there is no BLE pairing or
     *  trusted reconnect, so the whole app silently does nothing; we surface
     *  it instead. Refreshed on [onResume] and kept live while the app is in
     *  front by [btStateReceiver]; the banner clears itself once BT comes
     *  back on. */
    internal val bluetoothOff = MutableStateFlow(false)

    /** True while [btStateReceiver] is registered, so onPause unregisters
     *  exactly once (double-unregister throws). */
    private var btReceiverRegistered = false

    /** Live adapter-state updates while the app is foregrounded, so the
     *  banner appears/disappears the instant the user toggles BT from the
     *  quick settings without leaving Vortex. */
    private val btStateReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            bluetoothOff.value = isBluetoothOff()
        }
    }

    /** Adapter null (no BT hardware) or disabled → treat as "off" so the
     *  banner shows; a device with no BT can't run Vortex anyway. */
    private fun isBluetoothOff(): Boolean {
        val adapter = getSystemService(BluetoothManager::class.java)?.adapter
        return adapter == null || !adapter.isEnabled
    }

    /** Result-aware launcher for the system "turn on Bluetooth?" dialog. We
     *  don't rely on the result code (the user may enable BT from the shade
     *  instead) — we just re-read the adapter state when control returns. */
    internal val enableBluetoothLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult(),
    ) { bluetoothOff.value = isBluetoothOff() }

    internal val permissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { granted ->
        val denied = granted.filterValues { !it }.keys
        // Only a denied BLUETOOTH permission can stop us: without the radio
        // there is nothing to advertise on. Declining SMS or call-log access
        // costs the user those features, not the ability to pair — which is
        // what the old "any denial is an error" check took away from them.
        val blocking = denied.intersect(pairingCriticalPermissions().toSet())
        if (blocking.isEmpty()) {
            if (denied.isNotEmpty()) {
                Log.i(
                    "MainActivity",
                    "pairing on without optional permissions: ${denied.joinToString()}",
                )
            }
            startAdvertising()
        } else {
            state.value = AdvertiseState.Error("permissions denied: ${blocking.joinToString()}")
        }
    }

    /** First-launch grant of the connectivity/notification essentials (see
     *  [essentialPermissions]). Result is informational only — the pairing and
     *  picker flows re-check what they need; this just gets "Nearby devices"
     *  out of the way up front so pairing isn't blocked by an unasked grant. */
    internal val essentialPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { /* no-op */ }

    /** Dedicated READ_PHONE_STATE request used on the trusted-launch path
     *  (which starts the service directly, bypassing the BLE permission
     *  flow). On any result we nudge the service to (re)register the
     *  telephony listener — granted means real-call hand-off now works;
     *  denied leaves it disabled (logged in the service). */
    internal val callPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { _ ->
        VortexService.startOrRefreshCallFlow(applicationContext)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Dev-only: keep the screen on so the lab tester can read the
        // generated identity. Production removes this.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.addFlags(WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON)
        window.addFlags(WindowManager.LayoutParams.FLAG_DISMISS_KEYGUARD)
        uiSettings.load()                               // saved locale + theme
        val identity = identityStore.loadOrGenerate(Platform.Android)
        identityState.value = identity
        applyDevHooks()                                 // CI/dev intent hooks (debug only)
        refreshPeerList()
        refreshSavedEarbudsFlag()
        // Earbuds-switch orchestrator (Phase 1). Idempotent — also init'd by
        // VortexService; we init here so the UI can drive it even before the
        // service is up (fresh launch with the activity in front).
        com.vortex.a3.core.earbuds.EarbudsSwitchHolder.init(applicationContext, peerStore)
        wirePairingOrchestrator(identity)               // Phase 5c/5d + manual approval
        wireReconnectOrchestrator(identity)             // Phase 6
        startPairingWindowLanIfUntrusted(identity)
        wireRuntimeObservers()                          // peer state / staleness / earbuds poll
        val ui = buildUiState()
        val actions = buildActions()
        setContent {
            VortexRoot(activity = this, settings = uiSettings, ui = ui, actions = actions)
        }
        maybeRequestEssentialPermissions()              // ask "Nearby devices" etc. up front
        selectLaunchMode()                              // trusted → service; else pairing window
    }

    /** CI / dev intent hooks, DEBUG-only (production never honours the
     *  extras — otherwise any installed app could wipe trust / drop bonds):
     *   - `--ez clear_trust true` wipes trusted peers before init.
     *   - `--es remove_bond <MAC>` drops a stale one-sided LE bond that
     *     Android hides from Settings (see BondCleaner). */
    private fun applyDevHooks() {
        if (!BuildConfig.DEBUG) return
        if (intent.getBooleanExtra("clear_trust", false)) {
            for (peer in peerStore.list()) peerStore.forget(peer.peerStaticPub)
        }
        intent.getStringExtra("remove_bond")?.let { mac ->
            val adapter = getSystemService(android.bluetooth.BluetoothManager::class.java)?.adapter
            // Result is not shown on screen: BondCleaner already logs it
            // ("VortexBondCleaner: removeBond(<mac>) -> <ok>"), and a toast on
            // top of the pairing UI got in the way while live-debugging bonds.
            adapter?.let { com.vortex.a3.core.ble.BondCleaner.removeBond(it, mac) }
        }
    }




    /**
     * Long-lived UI observers tied to the Activity lifecycle: peer AppState
     * pushes, laptop-staleness ticks, bidirectional-forget, the 4s local
     * earbuds poll, and a switch-completion refresh burst.
     */
    private fun wireRuntimeObservers() {
        // Peer AppState pushes from VortexService → composable-visible state.
        lifecycleScope.launch {
            VortexService.peerStateBus.collect { (hex, state) ->
                // `update { }` atomically RMW's the StateFlow — `.value =
                // .value + …` would be a TOCTOU race when the BLE and LAN
                // collectors run concurrently.
                peerStatesState.update { it + (hex to state) }
                peerLastSeenState.update { it + (hex to System.currentTimeMillis()) }
            }
        }
        // Periodic staleness re-eval so the laptop dot flips to grey even
        // when no further peer message arrives.
        lifecycleScope.launch {
            while (isActive) {
                nowTickState.value = System.currentTimeMillis()
                delay(3_000)
            }
        }
        // Bidirectional forget: peer revoked us → clear UI immediately.
        lifecycleScope.launch {
            VortexService.revokedByPeerBus.collect {
                refreshPeerList()
                if (peerStore.list().isEmpty()) {
                    state.value = AdvertiseState.Idle
                }
            }
        }
        // Poll local earbuds every 4s so plugging/unplugging reflects in the
        // UI without waiting for the next peer reconnect.
        earbudsPollJob = lifecycleScope.launch {
            while (isActive) {
                try {
                    localEarbudsState.value =
                        com.vortex.a3.core.earbuds.EarbudsDetector
                            .readConnectedEarbuds(applicationContext)
                } catch (t: Throwable) {
                    android.util.Log.e("VortexEarbuds", "poll threw", t)
                }
                delay(4000)
            }
        }
        // When a switch flow finishes, kick a quick burst of local refreshes
        // so the card shows the new ownership immediately (the 4s poll would
        // otherwise leave it on a stale "Switching…" caption for a beat).
        lifecycleScope.launch {
            var wasActive = false
            com.vortex.a3.core.earbuds.EarbudsSwitchHolder.state.collect { s ->
                val active = s !is com.vortex.a3.core.earbuds.SwitchState.Idle &&
                    s !is com.vortex.a3.core.earbuds.SwitchState.Failed
                if (wasActive && !active) {
                    listOf(50L, 400L, 1000L).forEach { ms ->
                        lifecycleScope.launch {
                            delay(ms)
                            try {
                                localEarbudsState.value =
                                    com.vortex.a3.core.earbuds.EarbudsDetector
                                        .readConnectedEarbuds(applicationContext)
                            } catch (_: Throwable) { /* best-effort */ }
                        }
                    }
                    // Nudge LAN so the laptop pulls our fresh AppState.
                    VortexService.requestLanNudge()
                }
                wasActive = active
            }
        }
    }

    /** Bundle the activity's StateFlows for the root composable. */
    private fun buildUiState(): MainUiState = MainUiState(
        advertise = state,
        identity = identityState,
        handshake = handshakeState,
        reconnect = reconnectState,
        peers = peerListState,
        peerStates = peerStatesState,
        peerLastSeen = peerLastSeenState,
        nowTick = nowTickState,
        localEarbuds = localEarbudsState,
        hasSavedEarbuds = savedEarbudsExists,
        picker = earbudsPickerState,
        pendingApproval = pendingApproval,
        autostartHintDismissed = autostartHintDismissed,
        showNotifAccessDialog = showNotifAccessDialog,
        showAutostartDialog = showAutostartDialog,
        bluetoothOff = bluetoothOff,
    )

    /** Bundle the activity's callbacks for the root composable. */
    private fun buildActions(): VortexActions = VortexActions(
        onForgetPeer = ::onForgetPeerClicked,
        onOpenAutostart = ::onOpenAutostartSettings,
        onDismissAutostartHint = ::dismissAutostartHint,
        onRequestBatteryWhitelist = ::onRequestBatteryWhitelist,
        onOpenEarbudsPicker = ::openEarbudsPicker,
        onPickEarbud = ::pickEarbud,
        onRescanEarbuds = ::rescanEarbudsPicker,
        onClosePicker = ::closeEarbudsPicker,
        onRemoveSavedEarbuds = ::removeSavedEarbuds,
        onToggleLaptopLock = ::toggleLaptopLock,
        onApprove = ::onApproveClicked,
        onReject = ::onRejectClicked,
        onOpenNotificationAccess = ::onOpenNotificationAccess,
        onOpenScreenControl = ::onOpenAccessibilitySettings,
        onEnableBluetooth = ::onEnableBluetooth,
        isAggressiveOem = isAggressiveOemRom(),
        isIgnoringBatteryOptimizations = ::isIgnoringBatteryOptimizations,
    )

    /**
     * Launch-time mode selection: trust exists → start VortexService (it owns
     * the stack; the activity stays minimal so we don't double-occupy the BLE
     * adapter) and request READ_PHONE_STATE for real-call hand-off. No trust →
     * wait for the user to tap "Start pairing". `auto_advertise` (DEBUG) forces
     * pairable mode for scripted first-pairing tests.
     */
    private fun selectLaunchMode() {
        val autoAdv = BuildConfig.DEBUG && intent.getBooleanExtra("auto_advertise", false)
        val trustExists = peerStore.list().isNotEmpty()
        if (trustExists && !autoAdv) {
            VortexService.start(applicationContext)
            state.value = AdvertiseState.TrustedPresence
            // READ_PHONE_STATE isn't requested on the trusted path (it only
            // starts the service); ask now so real-call hand-off works. The
            // callback refreshes the service's telephony listener on grant.
            if (ContextCompat.checkSelfPermission(
                    this,
                    Manifest.permission.READ_PHONE_STATE,
                ) != PackageManager.PERMISSION_GRANTED
            ) {
                callPermissionLauncher.launch(Manifest.permission.READ_PHONE_STATE)
            }
        } else if (autoAdv) {
            onStartClicked()
        }
    }

    override fun onResume() {
        super.onResume()
        // Bluetooth-off banner: re-read on entry and stay live while in front.
        // Without BT there's no BLE pairing/reconnect — surface it rather than
        // sit silently dead. RECEIVER_NOT_EXPORTED: this is a system broadcast
        // we only listen to, never a cross-app surface.
        bluetoothOff.value = isBluetoothOff()
        if (!btReceiverRegistered) {
            ContextCompat.registerReceiver(
                this,
                btStateReceiver,
                IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED),
                ContextCompat.RECEIVER_NOT_EXPORTED,
            )
            btReceiverRegistered = true
        }
        // KDE-Connect-style: when the app is in the foreground and no
        // trust exists yet, auto-start the pairable advertisement so a
        // nearby Linux can find this phone without the user tapping
        // anything. Trust-exists case is handled at onCreate (service).
        if (peerStore.list().isEmpty() && !advertiser.isAdvertising()) {
            onStartClicked()
        }
        // Setup prompts (only once a peer is paired). Show at most ONE at a
        // time and NEVER clobber one already on screen — recomputing blindly
        // every resume used to replace the one-shot Autostart dialog with the
        // notif one (a resume cycle while it was up lost it before the user
        // could act), and re-raise a just-dismissed dialog on every
        // screen-off/on. Order: Autostart first (one-shot, since MIUI hides
        // whether it's done), then notification access (re-asked each entry
        // until granted — it's detectable, so it auto-stops).
        val trust = peerStore.list().isNotEmpty()
        // Access just granted (user returned from settings) → clear any stale prompt.
        if (trust && isNotificationAccessGranted()) showNotifAccessDialog.value = false
        val dialogAlreadyUp = showAutostartDialog.value || showNotifAccessDialog.value
        if (trust && !dialogAlreadyUp) {
            val autostartPending = isAggressiveOemRom() &&
                !settingsPrefs.getBoolean("autostart_prompt_shown", false)
            when {
                autostartPending -> {
                    settingsPrefs.edit().putBoolean("autostart_prompt_shown", true).apply()
                    showAutostartDialog.value = true
                }
                !isNotificationAccessGranted() -> showNotifAccessDialog.value = true
            }
        }
    }

    override fun onPause() {
        super.onPause()
        if (btReceiverRegistered) {
            try { unregisterReceiver(btStateReceiver) } catch (_: Exception) { /* already gone */ }
            btReceiverRegistered = false
        }
        // Privacy: if no trust exists, stop advertising when the user
        // leaves the app. We never want to broadcast to strangers while
        // the user isn't actively pairing. Trust-exists case is handled
        // by VortexService which keeps trusted-presence advertising
        // alive in the background.
        if (peerStore.list().isEmpty()) {
            advertiser.stopAll()
            gattServer.stop()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        // Cancel the earbuds poll. lifecycleScope cancellation would
        // eventually clean it up, but on a config-change recreate the
        // OLD coroutine outlives the new Activity briefly and double-
        // polls BlueZ. Explicit cancel avoids the duplicate.
        earbudsPollJob?.cancel()
        earbudsPollJob = null
        // If the Activity is being torn down with an unresolved SAS
        // dialog, send an explicit REJECT so the orchestrator doesn't
        // sit in AwaitingApprovals forever (and the peer doesn't hang
        // on its half of the dual-approval gate).
        val pending = pendingApproval.value
        val orch = pairingOrchestrator
        if (pending != null && orch != null) {
            val frame = orch.buildLocalApprovalFrame(pending.device, approve = false)
            if (frame != null) {
                gattServer.sendPairingControl(pending.device, frame)
                orch.commitLocalDecision(pending.device, approve = false)
            }
        }
        // Only stop Activity-local components if no trust has been
        // established. Once trust exists, VortexService owns these and
        // must keep running after Activity is destroyed.
        if (peerStore.list().isEmpty()) {
            advertiser.stopAll()
            gattServer.stop()
            lanServer?.stop()
        }
    }

























}

// PickerState + AdvertiseState moved to ui/UiModels.kt.

// Vortex design tokens (color palettes) live in ui/Theme.kt.

/**
 * Laptop card goes grey if we haven't heard from the peer for this long.
 * Linux beats every ~12-15 s; 30 s covers ~2 missed beats — short enough
 * to detect a closed laptop or killed daemon, long enough to ride out a
 * dropped frame without flickering. Public so the extracted
 * [com.vortex.a3.ui.screens.HomeScreen] can use the same threshold.
 */
const val LAPTOP_STALE_MS: Long = 30_000

// Constants and shared composables moved to ui.components.Common.
// Keep a few in scope for now — the inline alias points at the same
// instances so the existing call sites compile unchanged.
private val CardCorner = com.vortex.a3.ui.components.CardCorner
private val CardHeight = com.vortex.a3.ui.components.CardHeight

// PeerDeviceCard, EarbudsCard, EarbudsAddPlaceholder moved to ui.components.

// Common composables and ByteArray.toHex live in
// ui.components.Common — imported at the top of this file.
// SettingsScreen + helpers moved to ui.screens.SettingsScreen.

@Suppress("unused")
private val _ignored: BluetoothDevice? = null
