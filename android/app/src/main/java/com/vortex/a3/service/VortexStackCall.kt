package com.vortex.a3.service

import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.coroutines.launch

/**
 * Phone→laptop call mirroring + the Phase 2 call-handoff orchestrator — split
 * out of [VortexStack]. Drives the laptop's call banner / in-call pill (CALL
 * frame + AppState fallback), routes laptop→phone call-control (Accept/Decline/
 * Mute/End), and on a real call asks the peer to release the earbuds then grabs
 * them here. Extension functions on [VortexStack].
 */

/** True when the call-flow GRABBED the buds from the laptop for the current
 *  call → onCallEnd hands them back. False when the buds were already on the
 *  phone (no grab, no hand-back — handing back would yank them off the phone). */
/** Persisted, because MIUI restarts this service mid-call and a
 *  process-lifetime flag would come back false — so the call would end with
 *  nothing handing the earbuds back. */
@Volatile private var callGrabbedBuds = false

/** Copy a CallEvent with a fresh send timestamp (clock-skew-proof laptop
 *  timer) + the live in-call audio state (mute / speaker / earbuds) so the
 *  laptop pill's card renders the right toggles. */
internal fun VortexStack.enrichCallEvent(ev: com.vortex.a3.core.call.CallEvent): com.vortex.a3.core.call.CallEvent {
    val am = ctx.getSystemService(Context.AUDIO_SERVICE) as? android.media.AudioManager
    val muted = am?.isMicrophoneMute ?: false
    @Suppress("DEPRECATION")
    val speaker = am?.isSpeakerphoneOn ?: false
    val hasEarbuds = am?.let { hasHeadsetOutput(it) } ?: false
    return ev.copy(
        sentAt = System.currentTimeMillis(),
        muted = muted,
        speaker = speaker,
        hasEarbuds = hasEarbuds,
    )
}

/** True if a wireless/wired headset output is connected (so the laptop pill
 *  hides the Speaker button — no point routing to the loudspeaker). */
internal fun VortexStack.hasHeadsetOutput(am: android.media.AudioManager): Boolean = try {
    am.getDevices(android.media.AudioManager.GET_DEVICES_OUTPUTS).any {
        it.type == android.media.AudioDeviceInfo.TYPE_BLUETOOTH_A2DP ||
            it.type == android.media.AudioDeviceInfo.TYPE_BLUETOOTH_SCO ||
            it.type == android.media.AudioDeviceInfo.TYPE_WIRED_HEADSET ||
            it.type == android.media.AudioDeviceInfo.TYPE_WIRED_HEADPHONES ||
            it.type == android.media.AudioDeviceInfo.TYPE_USB_HEADSET
    }
} catch (_: Exception) {
    false
}

/** Re-push the current call to the laptop NOW (after a control action
 *  changed the audio state) so the pill's toggles update promptly instead
 *  of waiting for the next heartbeat. */
internal fun VortexStack.republishCurrentCall() {
    VortexService.currentCall?.let {
        VortexService.callEventBus.tryEmit(it)
        lanServer?.nudge()
    }
}

/** Dispatch a laptop→phone call-control command, deduped by [seq] so the
 *  BLE-frame path and the AppState-fallback path don't double-fire. */
internal fun VortexStack.handleCallControl(ctrl: com.vortex.a3.core.call.CallControl) {
    if (ctrl.seq > 0L) {
        synchronized(this) {
            if (ctrl.seq <= lastHandledCallControlSeq) return
            lastHandledCallControlSeq = ctrl.seq
        }
    }
    // LOAD_THREAD is a data request (laptop's SMS infinite scroll), not a
    // call action: read the requested conversation page and push it back over
    // BLE (SMS_THREAD frames). Handled here — it needs the SMS provider + GATT
    // server, which CallController has no access to.
    if (ctrl.action == com.vortex.a3.core.call.CallControl.Action.LOAD_THREAD) {
        handleLoadThread(ctrl.arg)
        return
    }
    // Actions that act on A CALL must act on the call the laptop was LOOKING AT.
    //
    // The laptop has always stamped `ctrl.id` with the call its banner was
    // showing; this side simply never read it, so every accept/decline/end was
    // applied to "whatever is ringing right now". That is not the same call
    // whenever the command is delayed — and it routinely is: BLE writes are
    // dropped while a call is up, so the command waits for the next LAN
    // heartbeat. Caller A gives up, B rings three seconds later, and B is the
    // one that gets declined. With LAN down it could sit for minutes and fire
    // on an entirely unrelated call.
    val callTargeted = ctrl.action in setOf(
        com.vortex.a3.core.call.CallControl.Action.ACCEPT,
        com.vortex.a3.core.call.CallControl.Action.DECLINE,
        com.vortex.a3.core.call.CallControl.Action.END,
        com.vortex.a3.core.call.CallControl.Action.MUTE,
        com.vortex.a3.core.call.CallControl.Action.UNMUTE,
        com.vortex.a3.core.call.CallControl.Action.SILENCE,
        com.vortex.a3.core.call.CallControl.Action.SPEAKER_ON,
        com.vortex.a3.core.call.CallControl.Action.SPEAKER_OFF,
    )
    if (callTargeted && ctrl.id.isNotEmpty()) {
        val live = VortexService.currentCall
        if (live == null || live.id != ctrl.id) {
            Log.w(
                VortexStack.TAG,
                "dropping stale call-control action=${ctrl.action} for call ${ctrl.id} " +
                    "(current call is ${live?.id ?: "none"})",
            )
            return
        }
    }
    callController.handle(ctrl)
    // Mute/Speaker changed the audio state → re-push the call so the pill's
    // toggles (Mute↔Unmute, Speaker on/off) update on the laptop at once.
    when (ctrl.action) {
        com.vortex.a3.core.call.CallControl.Action.MUTE,
        com.vortex.a3.core.call.CallControl.Action.UNMUTE,
        com.vortex.a3.core.call.CallControl.Action.SPEAKER_ON,
        com.vortex.a3.core.call.CallControl.Action.SPEAKER_OFF,
        -> republishCurrentCall()
    }
}

/**
 * Phase 2 — call-handoff orchestrator. On RINGING/OFFHOOK we ask the
 * peer to release the buds (via the next AppState heartbeat) and grab
 * them here; on IDLE we hand them back and let Linux resume its media.
 * Returns the orchestrator so the fake-call receiver + switch watcher
 * can reference it.
 */
internal fun VortexStack.startCallFlow(): com.vortex.a3.core.call.CallFlowOrchestrator {
    val callFlow = com.vortex.a3.core.call.CallFlowOrchestrator(
        context = ctx,
        onCallStart = {
            // Guard: if THIS phone's Bluetooth is off, it physically
            // cannot take the buds (A2DP needs BT), so the whole
            // hand-off is pointless — the call just rides the phone
            // speaker. Skip it so the laptop does NOT pause its media or
            // release the buds for nothing.
            if (!isBluetoothOn()) {
                Log.i(VortexStack.TAG, "call started but phone Bluetooth is OFF — skipping hand-off")
                callGrabbedBuds = false
            } else if (phoneOwnsBuds()) {
                // The buds are ALREADY on this phone (the user was listening
                // here). Running the grab would force a needless A2DP reconnect
                // — the click the user hears — and handing them to the laptop
                // when the call ends would yank them off the phone. Leave them.
                Log.i(VortexStack.TAG, "call started but buds already on phone — no hand-off")
                callGrabbedBuds = false
            } else {
                Log.i(VortexStack.TAG, "call started; asking peer to release buds")
                // Mark call_phase in the next outgoing AppState so Linux
                // pauses media + disconnects. Nudge so the heartbeat fires
                // within ~1 s instead of waiting the 12 s tick.
                // Grab the buds FIRST, and only tell the laptop a call is
                // taking them if that actually started.
                //
                // The order used to be the other way round. `pendingCallPhase`
                // went out regardless, so a grab that never started — the
                // orchestrator busy, its Failed window, the switch holder not
                // initialised — still made the laptop pause its media and let
                // the earbuds go, for a hand-off that was not happening. At
                // call end `callGrabbedBuds` was false, so nothing handed them
                // back either: media paused, earbuds on nobody.
                callGrabbedBuds = grabBudsToPhone()
                if (callGrabbedBuds) {
                    // Mark call_phase in the next outgoing AppState so Linux
                    // pauses media + disconnects. Nudge so the heartbeat fires
                    // within ~1 s instead of waiting the 12 s tick.
                    VortexService.pendingCallPhase = "ringing"
                    lanServer?.nudge()
                } else {
                    Log.w(VortexStack.TAG, "call grab did not start — not asking the laptop to release")
                }
            }
            // True → a grab is in flight → the orchestrator arms its 2 s
            // speakerphone fallback for the route gap. False → the current
            // route (buds on phone / earpiece) is fine, don't touch it.
            callGrabbedBuds
        },
        onCallEnd = {
            VortexService.pendingCallPhase = null
            // Only return the buds to the laptop if WE grabbed them for this
            // call. If they were already on the phone, leave them — the user
            // wasn't handed off, so there's nothing to hand back.
            if (callGrabbedBuds) {
                Log.i(VortexStack.TAG, "call ended; handing buds back to laptop")
                callGrabbedBuds = false
                // The SAME fast return path as a smart-switch media-stop.
                handBudsToLaptop()
            } else {
                Log.i(VortexStack.TAG, "call ended; buds were already on phone — leaving them")
            }
        },
        onCallEvent = { ev ->
            // Mirror the call to the laptop (banner + in-call pill). Wholly
            // separate from the audio-handoff above — just a UI mirror.
            // Stash for the AppState builder (carries it over BLE+LAN as a
            // resilient second path) and nudge so it ships within ~1s.
            if (ev.phase == com.vortex.a3.core.call.CallEvent.PHASE_ENDED) {
                // Keep the EXPLICIT ended in AppState briefly (not null) so the
                // laptop clears the pill AT ONCE over the resilient LAN/BLE-STATE
                // path — instead of inferring the end from call=None, which the
                // laptop debounces ~5s (so on a bad-signal BLE drop the pill's
                // timer kept ticking). Clear after a few seconds so AppState
                // stops carrying a stale call.
                VortexService.currentCall = ev
                scope.launch {
                    kotlinx.coroutines.delay(6000)
                    if (VortexService.currentCall?.phase ==
                        com.vortex.a3.core.call.CallEvent.PHASE_ENDED
                    ) {
                        VortexService.currentCall = null
                    }
                }
            } else {
                VortexService.currentCall = ev
            }
            VortexService.callEventBus.tryEmit(ev)
            lanServer?.nudge()
        },
    )
    if (!callFlow.start()) {
        Log.w(VortexStack.TAG, "call-flow orchestrator did not start (READ_PHONE_STATE missing?)")
    }
    callFlowOrchestrator = callFlow
    // Phase 2 owner-vote: while a call is ringing/active the phone MUST
    // keep the buds — an incoming peer Request (laptop tap / its media
    // start) is rejected with InCall instead of yanking the call audio.
    // currentCall is nulled on PHASE_ENDED, so the gate lifts itself.
    com.vortex.a3.core.earbuds.EarbudsSwitchHolder.setAcceptanceProvider {
        if (VortexService.callGateActive()) {
            com.vortex.a3.core.earbuds.SwitchOrchestrator.Acceptance.Reject(
                com.vortex.a3.core.earbuds.RejectReason.InCall,
            )
        } else {
            com.vortex.a3.core.earbuds.SwitchOrchestrator.Acceptance.Allow
        }
    }
    return callFlow
}

/**
 * Debug-only fake-call receiver. Lets the live-test scripts exercise the
 * Phase 2 handoff via `adb shell am broadcast -a com.vortex.a3.FAKE_CALL
 * --es state ringing|idle`. Registered only on debug builds.
 */
internal fun VortexStack.registerFakeCallReceiver(callFlow: com.vortex.a3.core.call.CallFlowOrchestrator) {
    if (!com.vortex.a3.BuildConfig.DEBUG) return
    val receiver = object : android.content.BroadcastReceiver() {
        override fun onReceive(c: android.content.Context?, intent: android.content.Intent?) {
            val raw = intent?.getStringExtra("state")?.lowercase() ?: return
            val mapped = when (raw) {
                "ringing" -> android.telephony.TelephonyManager.CALL_STATE_RINGING
                "offhook", "active" -> android.telephony.TelephonyManager.CALL_STATE_OFFHOOK
                "idle", "end" -> android.telephony.TelephonyManager.CALL_STATE_IDLE
                else -> {
                    Log.w(VortexStack.TAG, "FAKE_CALL: unknown state \"$raw\"")
                    return
                }
            }
            // Optional caller number for testing the call mirror's caller
            // label (`--es number +998901234567`); resolved to a contact
            // name on the phone like a real call.
            val number = intent.getStringExtra("number")
            Log.i(VortexStack.TAG, "FAKE_CALL broadcast → $raw")
            callFlow.simulateCallStateForDebug(mapped, number)
        }
    }
    val filter = android.content.IntentFilter("com.vortex.a3.FAKE_CALL")
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        service.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
    } else {
        @Suppress("UnspecifiedRegisterReceiverFlag")
        service.registerReceiver(receiver, filter)
    }
    fakeCallReceiver = receiver
}

/** Forward phone-call events (ringing → active → ended) to the laptop over
 *  BLE (CALL frame) to drive the call banner + in-call pill. Shares the
 *  notification-mirror toggle (it's the same "show my phone on the laptop"
 *  consent). The bus has replay=1, so a peer that subscribes mid-call still
 *  gets the current phase. */
internal fun VortexStack.forwardCallEvents() {
    scope.launch {
        VortexService.callEventBus.collect { ev ->
            if (!com.vortex.a3.core.notif.NotificationMirrorSetting.isEnabled()) return@collect
            val server = gattServer ?: return@collect
            // Stamp the send time (clock-skew-proof timer) + the live audio
            // state (mute/speaker/earbuds) so the laptop pill's card shows
            // the right toggles.
            val json = enrichCallEvent(ev).toJsonBytes()
            for (peer in peerStore.list()) {
                server.sendCallEncrypted(peer.peerStaticPub, json)
            }
            // Push the phone's real Phone-app logo once (same ICON path the
            // notification mirror uses) so the laptop banner/pill shows it.
            val pkg = ev.appId
            if (pkg.isNotEmpty() && sentIconPkgs.add(pkg)) {
                scope.launch { sendAppIcon(pkg) }
            }
        }
    }
}
