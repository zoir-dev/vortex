package com.vortex.a3.service

import android.util.Log
import kotlinx.coroutines.launch

/**
 * Outgoing/inbound AppState heartbeat handling — split out of [VortexStack].
 * Builds this phone's snapshot (battery, earbuds, call phase, smart-switch,
 * lock command, …) for the BLE+LAN heartbeat, and applies the laptop's inbound
 * snapshot (UI publish, bidirectional forget, claim-request initiator, buds
 * release when the laptop starts playing). Extension functions on [VortexStack].
 */

/** Build the outgoing AppState heartbeat. Snapshots each field once so a
 *  concurrent MainActivity edit can't half-apply between captures. */
internal fun VortexStack.buildLocalAppState(): com.vortex.a3.core.appstate.AppState {
    val battery = com.vortex.a3.core.status.DeviceStatusReader.readBattery(ctx).value
    val charging = com.vortex.a3.core.status.DeviceStatusReader.readCharging(ctx)
    val earbuds = try {
        com.vortex.a3.core.earbuds.EarbudsDetector.readConnectedEarbuds(ctx)
    } catch (t: Throwable) {
        Log.w(VortexStack.TAG, "earbuds detection threw: ${t.message}")
        null
    }
    // V1 has at most one trusted peer, so the per-peer revoke set
    // collapses to a global flag (any element ⇒ revoke). `.toSet()`
    // snapshots the ConcurrentHashMap key set at this instant.
    val revokeNow = VortexService.pendingRevokes.toSet().isNotEmpty()
    // One-shot "you-claim-now" signal. getAndSet so a single heartbeat
    // carries the flag; if the peer misses it the user can tap again.
    val claimNow = VortexService.pendingAudioClaim.getAndSet(false)
    // Remote lock/unlock for the laptop. Deliberately NOT consumed
    // here: the command rides every snapshot until its TTL so a lost
    // BLE write can't eat the tap (the laptop dedups by seq).
    val lockPending = VortexService.pendingLock.get()?.takeIf {
        android.os.SystemClock.elapsedRealtime() < it.expiresAtMs
    }
    // Media transport tap for the laptop (same ride-until-TTL model).
    val mediaPending = VortexService.pendingMediaControl.get()?.takeIf {
        android.os.SystemClock.elapsedRealtime() < it.expiresAtMs
    }
    // Phase 2 — current call phase. Persistent across heartbeats so a
    // missed beat doesn't lose the pause/resume signal.
    val phaseNow = VortexService.pendingCallPhase
    // Keyguard state for the laptop's proximity auto-unlock (owner-present gate):
    // the laptop only auto-unlocks when this phone is itself unlocked. null if
    // we somehow can't read it → the laptop fails closed (no auto-unlock).
    val km = ctx.getSystemService(android.app.KeyguardManager::class.java)
    val phoneUnlocked: Boolean? = km?.let { !it.isKeyguardLocked }
    // Locale + theme intentionally omitted from the wire — per-device
    // prefs, no cross-device sync.
    return com.vortex.a3.core.appstate.AppState(
        battery = battery,
        deviceClass = com.vortex.a3.core.appstate.DeviceClass.PHONE,
        name = friendlyDeviceName(),
        earbuds = earbuds,
        revoked = revokeNow,
        audioClaimRequest = claimNow,
        callPhase = phaseNow,
        // Call mirror additively over AppState (BLE+LAN) so the laptop
        // banner/pill survives a BLE drop mid-call. Persistent across
        // heartbeats; null when no call. Enrich with a fresh sentAt (timer
        // anchor) + live audio state each beat.
        call = VortexService.currentCall?.let { enrichCallEvent(it) },
        // Browsing-handoff LAN backstop: the page this phone is on (null when
        // not browsing). The laptop dedups by URL vs the BLE HANDOFF frame.
        handoff = VortexService.currentHandoff,
        mediaPlaying = localMediaPlaying,
        // Last-play-wins: send our play AGE (frozen across hand-off pauses),
        // not an absolute time, so the laptop can re-anchor it skew-free.
        mediaPlayAgeMs = mediaHandoff?.let { mh ->
            val e = mh.localPlayEpochMono
            if (e > 0L) android.os.SystemClock.elapsedRealtime() - e else 0L
        } ?: 0L,
        // Shared smart-switch setting + its LWW timestamp (cross-device).
        smartSwitchEnabled = com.vortex.a3.core.media.SmartSwitchSetting.isEnabled(),
        smartSwitchChangedAt = com.vortex.a3.core.media.SmartSwitchSetting.changedAt(),
        charging = charging,
        unlocked = phoneUnlocked,
        lockCommand = lockPending?.op,
        lockCommandSeq = lockPending?.seq ?: 0L,
        mediaControl = mediaPending?.op,
        mediaControlSeq = mediaPending?.seq ?: 0L,
        // Laptop→phone screen mirror: tell the laptop we want to see its screen
        // (it starts casting on the false→true edge). Level, not one-shot.
        laptopMirrorReq = com.vortex.a3.core.mirror.LaptopMirror.requestActive,
        laptopMirrorExtend = com.vortex.a3.core.mirror.LaptopMirror.extendWanted,
        // Continuity Camera: where to dial + the key, while we're streaming.
        cameraOffer = VortexService.cameraOffer,
        // Our LIVE Wi-Fi IP (re-read on every build, never cached) so the
        // laptop's peer-IP cache tracks DHCP renews even over BLE-only.
        wifiIp = currentWifiIp(),
        // The panel's refresh rate — the ceiling for any mirror frame rate,
        // since a capture cannot produce frames the display never draws.
        displayHz = currentDisplayHz(ctx),
    )
}

/** This phone's display refresh rate in Hz, rounded. Read per build rather than
 *  cached: a phone with an adaptive panel reports whatever mode it is in right
 *  now, and the mirror should track that rather than a number from boot.
 *  null when the display manager has nothing to say. */
private fun currentDisplayHz(ctx: android.content.Context): Int? = try {
    val dm = ctx.getSystemService(android.content.Context.DISPLAY_SERVICE)
        as? android.hardware.display.DisplayManager
    dm?.getDisplay(android.view.Display.DEFAULT_DISPLAY)
        ?.refreshRate
        ?.takeIf { it > 1f }
        ?.let { Math.round(it) }
} catch (_: Throwable) {
    null
}

/** This phone's current Wi-Fi IPv4, read live from the interface list — or
 *  null when Wi-Fi is down. Covers both Wi-Fi-client (`wlan*`) and hotspot
 *  (`ap*` / Samsung `swlan*`) interfaces; skips Wi-Fi Direct (`p2p*`) and
 *  cellular, whose addresses the laptop can't dial. Permission-free (no
 *  WifiManager) and cheap enough to run per heartbeat. */
private fun currentWifiIp(): String? = try {
    java.net.NetworkInterface.getNetworkInterfaces()?.asSequence()
        ?.filter { it.isUp && !it.isLoopback }
        ?.filter { ni ->
            val n = ni.name
            n.startsWith("wlan") || n.startsWith("ap") || n.startsWith("swlan")
        }
        ?.flatMap { it.inetAddresses.asSequence() }
        ?.filterIsInstance<java.net.Inet4Address>()
        ?.firstOrNull { !it.isLoopbackAddress && !it.isLinkLocalAddress }
        ?.hostAddress
} catch (_: Exception) {
    null
}

/** Handle an inbound peer (laptop) AppState: publish it to the UI bus +
 *  notification, honour a bidirectional forget, run the initiator on a
 *  claim request, and release the buds when the laptop starts playing. */
internal fun VortexStack.handlePeerAppState(peerPub: ByteArray, state: com.vortex.a3.core.appstate.AppState) {
    VortexService.peerStateBus.tryEmit(peerPub.toHex() to state)
    latestPeerState = state
    latestPeerStateAtMs = android.os.SystemClock.elapsedRealtime()
    onStateChanged()
    // Auto-pin the peer's (laptop's) earbuds into our store so the card shows on
    // this phone too — only when connected with a real address and we have none
    // saved (never overwrites the user's own pick). No feedback loop: once
    // saved, our readConnectedEarbuds reports the same (addr, name) and the
    // peer's own no-saved guard makes that a no-op.
    state.earbuds?.let { buds ->
        if (buds.connected && buds.address.isNotBlank() &&
            com.vortex.a3.core.earbuds.EarbudsStore.load(ctx) == null
        ) {
            com.vortex.a3.core.earbuds.EarbudsStore.save(
                ctx,
                com.vortex.a3.core.earbuds.SavedEarbuds(address = buds.address, name = buds.name),
            )
            Log.i(VortexStack.TAG, "auto-saved peer earbuds (card pinned locally)")
        }
    }
    // Laptop→phone call-control fallback over AppState (when BLE was down for
    // the dedicated CALL_CONTROL frame). Deduped by seq vs the BLE path.
    state.callControl?.let { handleCallControl(it) }
    // Laptop→phone notification action/reply INVOKE fallback over AppState (when
    // the BLE NOTIFICATION write failed). Deduped by seq vs the BLE path.
    state.notifInvoke?.let { handleNotifInvoke(it) }
    // Laptop→phone screen mirror: while the laptop is casting it carries a
    // `laptopCast` offer → launch the viewer (idempotent; the coordinator
    // ignores repeats). Key is lowercase hex (64 chars); never logged. When the
    // offer DROPS (laptop hit "Stop sharing" / capture errored) close the viewer
    // so both screens stay in sync.
    val cast = state.laptopCast
    val castError = state.laptopCastError
    if (cast != null) {
        val key = hexToBytes(cast.key)
        if (key != null && key.size == 32) {
            com.vortex.a3.core.mirror.LaptopMirror.onLaptopOffer(ctx, cast.port, key)
        }
    } else if (castError != null) {
        // The laptop tried and cannot: stop asking and say why. Checked BEFORE
        // the silent path — an explicit reason beats waiting out a timeout.
        com.vortex.a3.core.mirror.LaptopMirror.onLaptopCastFailed(castError)
    } else {
        // No offer and no reason: either a viewer that just ended, or a request
        // the laptop never answered. Both are handled, and each ignores the case
        // that belongs to the other.
        com.vortex.a3.core.mirror.LaptopMirror.onLaptopCastEnded()
        com.vortex.a3.core.mirror.LaptopMirror.onLaptopCastSilent()
    }
    // Continuity Camera: the laptop wants this phone's camera as a webcam.
    handleCameraRequest(state.cameraReq, state.cameraFacing)
    // Find-My: the laptop tapped "Ring my phone" — ring loudly on the edge.
    com.vortex.a3.core.ring.RingController.onRingSeq(ctx, state.ringSeq)
    // Shared smart-switch setting, LWW: adopt the laptop's value when its
    // timestamp is newer. The coordinator's flag updates via the
    // SmartSwitchSetting.enabled collector wired in startMediaFollow.
    if (com.vortex.a3.core.media.SmartSwitchSetting
            .applyFromPeer(state.smartSwitchEnabled, state.smartSwitchChangedAt)
    ) {
        Log.i(VortexStack.TAG, "smart-switch: adopted peer setting (LWW) = ${state.smartSwitchEnabled}")
    }
    // Bidirectional forget — peer asked us to drop their trust.
    if (state.revoked) {
        Log.i(VortexStack.TAG, "peer revoked us; forgetting ${peerPub.toHexPrefix()}")
        peerStore.forget(peerPub)
        VortexService.revokedByPeerBus.tryEmit(peerPub.toHex())
    }
    // Peer holds the buds and is asking us to claim them. We become the
    // initiator. The orchestrator drops the request if a flow is already
    // in progress, so this is idempotent across repeated heartbeats —
    // but once the grab COMPLETES (state back to Idle) the peer's flag
    // can persist another beat or two, which used to fire a pointless
    // second full switch flow. "Already connected" is the natural
    // claim-satisfied check, robust against lost beats too.
    if (state.audioClaimRequest) {
        val saved = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx)
        if (saved != null) {
            if (audioCtl?.isConnected(saved.address) == true) {
                Log.i(VortexStack.TAG, "audio_claim_request still set but buds already here; ignoring")
            } else {
                Log.i(VortexStack.TAG, "peer set audio_claim_request; running initiator")
                com.vortex.a3.core.earbuds.EarbudsSwitchHolder.request(peerPub, saved.address)
            }
        }
    }
    // Phase 3 — peer (laptop) started playing media: release the buds so
    // it can grab them. Level-triggered (NOT a one-shot edge): while the
    // laptop is the media device AND we still hold the buds, keep
    // releasing each heartbeat until they leave (a one-shot edge would be
    // lost if the laptop was mid-switch when it first fired). The
    // disconnect is idempotent; once gone (isConnected=false) it stops.
    val peerNow = state.mediaPlaying
    lastPeerMediaPlaying = peerNow
    // Last-play-wins: re-anchor the laptop's play AGE to OUR monotonic clock
    // (`now - age`) so the compare is clock-skew immune, feed it into the
    // coordinator's grab gate, and use it to gate the RELEASE below — only
    // hand the buds to the laptop if it played MORE recently than us (a
    // greater re-anchored epoch) or we're idle. Otherwise WE played last →
    // keep the buds here.
    mediaHandoff?.peerPlaying = peerNow
    val peerEpochMono = if (state.mediaPlayAgeMs > 0L && peerNow) {
        android.os.SystemClock.elapsedRealtime() - state.mediaPlayAgeMs
    } else 0L
    mediaHandoff?.peerPlayEpochMono = peerEpochMono
    val ourEpoch = mediaHandoff?.localPlayEpochMono ?: 0L
    val peerPlayedLast = ourEpoch == 0L || (peerEpochMono != 0L && peerEpochMono > ourEpoch)
    val mac = com.vortex.a3.core.earbuds.EarbudsStore.load(ctx)?.address
    val inCall = VortexService.callGateActive()
    val enabled = mediaHandoff?.smartSwitchEnabled != false
    val ctl = audioCtl
    if (peerNow && peerPlayedLast && enabled && !inCall &&
        mac != null && ctl != null && ctl.isConnected(mac)
    ) {
        Log.i(VortexStack.TAG, "laptop is the media device; releasing buds so it can grab")
        scope.launch { ctl.disconnect(mac) }
    }
    // Laptop's now-playing → the phone's MediaStyle notification (with
    // transport buttons). Empty title clears it.
    com.vortex.a3.core.media.LaptopMediaNotification.update(
        ctx,
        title = state.mediaTitle.orEmpty(),
        artist = state.mediaArtist.orEmpty(),
        app = state.mediaApp.orEmpty(),
        artUrl = state.mediaArtUrl.orEmpty(),
        playing = state.mediaNpPlaying,
    )
}

/** Decode lowercase/upper hex to bytes; null on odd length or a bad digit. */
private fun hexToBytes(s: String): ByteArray? {
    if (s.length % 2 != 0) return null
    return try {
        ByteArray(s.length / 2) { i ->
            s.substring(i * 2, i * 2 + 2).toInt(16).toByte()
        }
    } catch (_: NumberFormatException) {
        null
    }
}

/** Best human-readable name for this phone (Settings "device_name", else
 *  manufacturer + model). */
internal fun VortexStack.friendlyDeviceName(): String {
    return try {
        android.provider.Settings.Global.getString(service.contentResolver, "device_name")
            ?.takeIf { it.isNotBlank() }
    } catch (_: Exception) { null }
        ?: "${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}".trim()
}
