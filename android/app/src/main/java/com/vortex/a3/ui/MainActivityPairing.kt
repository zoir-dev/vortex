package com.vortex.a3.ui
import android.content.pm.PackageManager
import android.os.Build
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.outlined.Settings
import com.vortex.a3.BuildConfig
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import android.provider.Settings
import kotlinx.coroutines.launch
import kotlinx.coroutines.delay
import com.vortex.a3.core.ble.Advertiser
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.lan.LanServer
import com.vortex.a3.core.lan.LanServerMode
import com.vortex.a3.core.pairing.PairingOrchestrator
import com.vortex.a3.core.pairing.ReconnectOrchestrator
import com.vortex.a3.core.storage.TrustedPeer
import com.vortex.a3.service.VortexService
import com.vortex.a3.core.appstate.AppState

// MainActivity — Pairing feature methods, split out of MainActivity.kt.
// Extension functions on the Activity so its (now `internal`) state stays in one
// instance; only the handler methods move here.

/**
 * Wire the pairing orchestrator (Phase 5c/5d + manual approval). On
 * XxComplete it opens the SAS dialog (with an explicit approval-window
 * timer); on BothApproved it persists trust and hands the stack off to
 * VortexService; on PeerRejected it clears the dialog. Auto-approve is a
 * DEBUG-only intent opt-in for the live-test scripts.
 */
internal fun MainActivity.wirePairingOrchestrator(identity: IdentityRecord) {
    val orchestrator = PairingOrchestrator(identity)
    orchestrator.autoApprove =
        BuildConfig.DEBUG && intent.getBooleanExtra("auto_approve", false)
    orchestrator.addListener { outcome ->
        handshakeState.value = outcome
        when (outcome.state) {
            PairingOrchestrator.PhaseState.XxComplete -> {
                // Open the SAS dialog.
                pendingApproval.value = outcome
                // Explicit approval-window timer (ChatGPT review #5). The
                // orchestrator sweeps stale awaits only when a fresh
                // inbound frame arrives; an idle peer would otherwise
                // leave this dialog up forever. Auto-dismiss + reject at
                // the same window the responder's sweep uses.
                val timedOutcome = outcome
                lifecycleScope.launch {
                    kotlinx.coroutines.delay(120_000)
                    // Only act if the dialog still shows the SAME outcome.
                    if (pendingApproval.value === timedOutcome) {
                        android.util.Log.w(
                            "PairingTimeout",
                            "SAS approval window expired; auto-rejecting",
                        )
                        onRejectClicked(timedOutcome)
                    }
                }
            }
            PairingOrchestrator.PhaseState.BothApproved -> {
                pendingApproval.value = null
                val prs = orchestrator.peerPrs(outcome.device.address)
                if (prs != null) {
                    peerStore.save(
                        TrustedPeer(
                            peerStaticPub = outcome.peerStaticPub,
                            prs = prs,
                            pairedAt = System.currentTimeMillis() / 1000,
                            peerName = outcome.peerName,
                        )
                    )
                    try {
                        if (outcome.device.bondState == android.bluetooth.BluetoothDevice.BOND_NONE) {
                            outcome.device.createBond()
                        }
                    } catch (e: Exception) {
                        android.util.Log.w("Pairing", "createBond: ${e.message}")
                    }
                    // Remember the laptop's BD_ADDR so Forget can clear any BT
                    // bond for it later (see PeerStore.loadPeerBtAddr). The
                    // central's address here is the laptop's public static
                    // address, so it stays valid for the life of the pairing.
                    peerStore.savePeerBtAddr(outcome.peerStaticPub, outcome.device.address)
                    refreshPeerList()
                    state.value = AdvertiseState.TrustedPresence
                    // Hand off to the background service: stop our
                    // Activity-local advertiser / GATT / LAN, then start
                    // VortexService. Defer the teardown ~600ms so the
                    // in-flight APPROVE notification flies out at L2CAP
                    // before BluetoothGattServer.close() aborts it.
                    lifecycleScope.launch {
                        kotlinx.coroutines.delay(600)
                        advertiser.stopAll()
                        gattServer.stop()
                        lanServer?.stop()
                        VortexService.start(applicationContext)
                    }
                }
            }
            PairingOrchestrator.PhaseState.PeerRejected -> {
                pendingApproval.value = null
            }
        }
    }
    pairingOrchestrator = orchestrator
    gattServer.pairingOrchestrator = orchestrator
}

/** Wire the reconnect orchestrator (Phase 6). */
internal fun MainActivity.wireReconnectOrchestrator(identity: IdentityRecord) {
    val reconnect = ReconnectOrchestrator(identity, peerStore)
    reconnect.addListener { outcome -> reconnectState.value = outcome }
    gattServer.reconnectOrchestrator = reconnect
}

/**
 * Run an Activity-local LAN listener ONLY during a pairing window (no
 * trust yet). Once trust exists VortexService owns the LAN listener so
 * it survives Activity destruction; without this gate both would race
 * for port 51820. The pairing-window mDNS instance matches the BLE
 * advertise `payload_8` (spec §5.4) so a discoverer can correlate them.
 */
internal fun MainActivity.startPairingWindowLanIfUntrusted(identity: IdentityRecord) {
    if (peerStore.list().isNotEmpty()) return
    val instanceId = ByteArray(8).also {
        java.security.SecureRandom().nextBytes(it)
    }
    pairingInstanceId = instanceId
    lanServer = LanServer(applicationContext, identity, peerStore).also {
        it.start(LanServerMode.PairingWindow(instanceId))
    }
}

/**
 * How long a user-opened "pair another laptop" window stays open before
 * presence advertising is restored.
 *
 * Bounded on purpose: the window *preempts* the trusted-presence beacon, so
 * an indefinitely-open one would leave this phone invisible to the laptop it
 * is already paired with (and, worse, would read as "away" to that laptop's
 * proximity auto-lock).
 */
internal const val PAIRING_WINDOW_MS = 120_000L

/**
 * Open a pairing window even though trust already exists — the phone-side
 * counterpart of the laptop's "Add phone".
 *
 * Until now pairable mode was reachable only with an EMPTY peer list
 * (`onResume`'s auto-start and `selectLaunchMode`), so a phone that had ever
 * paired could not be offered to a second laptop without forgetting the
 * first. That is the single-peer trap this unblocks.
 *
 * Why it has to preempt rather than run alongside presence: `Advertiser`
 * holds one advertising set and `startWith` refuses while another is active,
 * and once trust exists VortexService owns the radio, the GATT server and the
 * LAN listener (an Activity-local LanServer would race it for port 51820).
 * So the window stops the service, advertises pairable from the Activity, and
 * hands the radio back when it closes.
 */
internal fun MainActivity.onAddPairClicked() {
    val needed = requiredPermissions().filter {
        ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }
    if (needed.isEmpty()) {
        startPairingWindow()
    } else {
        // Route the post-grant callback to the window instead of the default
        // advertising mode, which would pick trusted-presence and silently do
        // the opposite of what the user asked for.
        pendingPairingWindow = true
        permissionLauncher.launch(needed.toTypedArray())
    }
}

internal fun MainActivity.startPairingWindow() {
    val identity = identityState.value ?: run {
        state.value = AdvertiseState.Error("identity not ready")
        return
    }
    pairingWindowJob?.cancel()
    state.value = AdvertiseState.Starting
    // Release the radio, GATT server and LAN listener from the service first.
    VortexService.stop(applicationContext)
    pairingWindowJob = lifecycleScope.launch {
        // The service tears down asynchronously; advertising before its
        // BluetoothGattServer.close() lands makes our own start() fail with a
        // busy adapter. Same reasoning as the 600 ms hand-off delay used when
        // pairing completes, in the other direction.
        delay(600)
        val instanceId = ByteArray(8).also { java.security.SecureRandom().nextBytes(it) }
        pairingInstanceId = instanceId
        // mDNS instance must match the BLE payload_8 (spec §5.4) so a
        // discoverer correlates the two transports.
        lanServer = LanServer(applicationContext, identity, peerStore).also {
            it.start(LanServerMode.PairingWindow(instanceId))
        }
        if (!gattServer.start()) {
            state.value = AdvertiseState.Error("failed to start GATT server")
            endPairingWindow()
            return@launch
        }
        advertiser.startPairableAdvertiseWith(instanceId) { result ->
            state.value = when (result) {
                is Advertiser.StartResult.Started -> AdvertiseState.Active(result.payload)
                is Advertiser.StartResult.Failed -> AdvertiseState.Error(result.reason)
            }
        }
        delay(PAIRING_WINDOW_MS)
        // Still pairable → nobody paired; close up and go back to presence.
        // A successful pair already restarted the service via BothApproved,
        // which leaves state at TrustedPresence, so this is a no-op then.
        if (state.value is AdvertiseState.Active || state.value is AdvertiseState.Starting) {
            endPairingWindow()
        }
    }
}

/** Close the window and give the radio back to the service. */
internal fun MainActivity.endPairingWindow() {
    pairingWindowJob?.cancel()
    pairingWindowJob = null
    advertiser.stopAll()
    gattServer.stop()
    lanServer?.stop()
    lanServer = null
    if (peerStore.list().isNotEmpty()) {
        VortexService.start(applicationContext)
        state.value = AdvertiseState.TrustedPresence
    } else {
        state.value = AdvertiseState.Idle
    }
}

/**
 * "Switch laptop": look for another remembered laptop while staying connected
 * to the current one.
 *
 * Seek before release (design doc §D3) — nothing is dropped here. The service
 * holds the current link for the whole window and only the arrival of a
 * different laptop ends it, so a cancelled or fruitless seek leaves the phone
 * exactly where it was. Pressing again while a window is open closes it, which
 * doubles as Cancel without spending card space on a second control.
 */
internal fun MainActivity.onSwitchLaptopClicked() {
    if (VortexService.isSeeking()) {
        VortexService.stopSeeking()
        seekingLaptop.value = false
        return
    }
    val started = VortexService.startSeeking()
    seekingLaptop.value = started
    if (!started) {
        // Refused: no stack running, or fewer than two remembered laptops. The
        // card only offers the action with 2+, so this is the service-down case.
        android.util.Log.i("VortexSwitch", "seek not started (service down?)")
    }
}

/**
 * Switch to one specific remembered laptop.
 *
 * Unlike [onSwitchLaptopClicked] this names the destination, so the seek
 * advertises only that peer's token rather than cycling every remembered one —
 * found as fast as the single-peer case, and less time on air.
 */
internal fun MainActivity.onSwitchToPeerClicked(peer: TrustedPeer) {
    if (VortexService.isSeeking()) {
        // Already looking; a second tap would only change the target
        // mid-flight. Treat it as cancel, matching the header action.
        VortexService.stopSeeking()
        seekingLaptop.value = false
        return
    }
    val started = VortexService.startSeeking(peer.peerStaticPub)
    seekingLaptop.value = started
    android.util.Log.i(
        "VortexSwitch",
        "targeted seek for '${peer.peerName ?: "laptop"}' started=$started",
    )
}

internal fun MainActivity.onApproveClicked(outcome: PairingOrchestrator.HandshakeOutcome) {
    val orch = pairingOrchestrator ?: return
    val frame = orch.buildLocalApprovalFrame(
        outcome.device,
        approve = true,
        localName = friendlyLocalName(),
    ) ?: run {
        pendingApproval.value = null
        return
    }
    gattServer.sendPairingControl(outcome.device, frame)
    orch.commitLocalDecision(outcome.device, approve = true)
    pendingApproval.value = null
}

internal fun MainActivity.onRejectClicked(outcome: PairingOrchestrator.HandshakeOutcome) {
    val orch = pairingOrchestrator ?: return
    val frame = orch.buildLocalApprovalFrame(outcome.device, approve = false) ?: run {
        pendingApproval.value = null
        return
    }
    gattServer.sendPairingControl(outcome.device, frame)
    orch.commitLocalDecision(outcome.device, approve = false)
    pendingApproval.value = null
}

/**
 * Forget a single trusted peer (triggered by long-press on the
 * card). Bidirectional: we mark the peer as pending revocation so
 * our next outgoing AppState carries `revoked=true`, give the
 * laptop a short window to pick that up, then delete locally and
 * stop the background service.
 */
internal fun MainActivity.onForgetPeerClicked(peer: TrustedPeer) {
    val hex = peer.peerStaticPub.joinToString("") { "%02x".format(it) }
    VortexService.pendingRevokes.add(hex)
    // Event-based revoke push: nudge re-announces mDNS, the laptop
    // sees it within ~100ms and reconnects, the next heartbeat
    // carries revoked=true. So the laptop forgets us inside a
    // round-trip when online. If offline, the revoke is best-
    // effort — the user will clear it from the laptop side
    // manually if needed.
    VortexService.requestLanNudge()
    // Optimistically clear from the UI right away.
    peerListState.value = peerListState.value.filter {
        !it.peerStaticPub.contentEquals(peer.peerStaticPub)
    }
    peerCountState.value = peerListState.value.size
    lifecycleScope.launch {
        // Short grace window for the nudged heartbeat to complete
        // (mDNS wake ~150ms + IK reconnect ~200ms + AppState push
        // ~50ms ≈ 400ms typical, padded to 1500ms for slow networks).
        // After that we commit the local forget unconditionally:
        // if peer didn't pick up the revoke, the user is OK with
        // forgetting from the other side manually — no point
        // stalling the local cleanup waiting for a peer that may
        // be offline forever.
        kotlinx.coroutines.delay(1_500)
        VortexService.pendingRevokes.remove(hex)
        // Drop any BT bond for this laptop BEFORE forgetting, while its
        // address is still on file. Vortex never creates these bonds (Linux
        // deliberately skips Device::pair()), but one added via the desktop's
        // Bluetooth panel or left by an older build survives the peer-store
        // wipe — and a bond the laptop no longer holds makes the next pairing
        // fail during encryption with `timeout: service discovery`, which is
        // the "retry several times until it works" symptom. Android also
        // hides profile-less LE bonds from Settings, so this is the only way
        // for the user to clear one.
        peerStore.loadPeerBtAddr(peer.peerStaticPub)?.let { mac ->
            val btAdapter =
                getSystemService(android.bluetooth.BluetoothManager::class.java)?.adapter
            btAdapter?.let { com.vortex.a3.core.ble.BondCleaner.removeBond(it, mac) }
        }
        peerStore.forget(peer.peerStaticPub)
        refreshPeerList()
        if (peerStore.list().isEmpty()) {
            VortexService.stop(applicationContext)
            state.value = AdvertiseState.Idle
        }
    }
}

internal fun MainActivity.onForgetAllClicked() {
    // Legacy entrypoint — kept around for any future Settings page
    // 'Danger zone' but no longer wired into the home screen.
    VortexService.stop(applicationContext)
    val btAdapter = getSystemService(android.bluetooth.BluetoothManager::class.java)?.adapter
    for (peer in peerStore.list()) {
        // Same bond cleanup as the single-peer path — see onForgetPeerClicked.
        peerStore.loadPeerBtAddr(peer.peerStaticPub)?.let { mac ->
            btAdapter?.let { com.vortex.a3.core.ble.BondCleaner.removeBond(it, mac) }
        }
        peerStore.forget(peer.peerStaticPub)
    }
    refreshPeerList()
    reconnectState.value = null
    state.value = AdvertiseState.Idle
}

internal fun MainActivity.refreshPeerList() {
    peerListState.value = peerStore.list()
    peerCountState.value = peerListState.value.size
}

/**
 * Friendly device name we expose to the peer during pairing.
 * Tries Settings.Global "device_name" (user-set on Android) first,
 * falls back to "<Manufacturer> <Model>".
 */
internal fun MainActivity.friendlyLocalName(): String {
    return try {
        android.provider.Settings.Global.getString(contentResolver, "device_name")
            ?.takeIf { it.isNotBlank() }
    } catch (_: Exception) { null }
        ?: "${Build.MANUFACTURER} ${Build.MODEL}".trim()
}

internal fun MainActivity.startAdvertising() {
    state.value = AdvertiseState.Starting
    // Per spec §5.5: GATT server MUST be open before advertising so
    // peers can immediately connect.
    if (!gattServer.start()) {
        state.value = AdvertiseState.Error("failed to start GATT server")
        return
    }
    // Mode selection (spec §5.1, §7.3):
    //   - trust exists → trusted-presence with rotating PRS-derived token
    //     (broadcast continuously so paired peers can detect us nearby);
    //   - no trust → pairable mode (user-opened pairing window).
    val firstPeer = peerStore.list().firstOrNull()
    if (firstPeer != null) {
        advertiser.startTrustedPresence(
            prs = firstPeer.prs,
            scope = lifecycleScope,
            rotationWindowSec = 60L,
            onError = { reason ->
                state.value = AdvertiseState.Error(reason)
            },
        )
        state.value = AdvertiseState.TrustedPresence
    } else {
        // Reuse the same 8-byte ID the LanServer is publishing in
        // the `_vortex-pair._tcp.` record so a discoverer that
        // resolves BLE + mDNS sees the same instance on both
        // transports. If pairingInstanceId is null (defensive: it
        // should have been set in onCreate alongside lanServer),
        // fall back to the random-per-advertise path.
        val id = pairingInstanceId
        val cb: (Advertiser.StartResult) -> Unit = { result ->
            state.value = when (result) {
                is Advertiser.StartResult.Started ->
                    AdvertiseState.Active(result.payload)
                is Advertiser.StartResult.Failed -> {
                    gattServer.stop()
                    AdvertiseState.Error(result.reason)
                }
            }
        }
        if (id != null) {
            advertiser.startPairableAdvertiseWith(id, cb)
        } else {
            advertiser.startPairableAdvertise(cb)
        }
    }
}

/** On first launch, request the connectivity/notification essentials so the
 *  user grants "Nearby devices" once up front and pairing isn't blocked by an
 *  unasked permission. No-op when everything's already granted (and after a
 *  permanent denial Android silently returns without a dialog). */
internal fun MainActivity.maybeRequestEssentialPermissions() {
    val needed = essentialPermissions().filter {
        ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }
    if (needed.isNotEmpty()) {
        essentialPermissionLauncher.launch(needed.toTypedArray())
    }
}

internal fun MainActivity.onStartClicked() {
    val needed = requiredPermissions().filter {
        ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }
    if (needed.isEmpty()) {
        startAdvertising()
    } else {
        permissionLauncher.launch(needed.toTypedArray())
    }
}

internal fun MainActivity.onStopClicked() {
    advertiser.stopAll()
    gattServer.stop()
    state.value = AdvertiseState.Idle
}
