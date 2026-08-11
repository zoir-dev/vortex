package com.vortex.a3.core.appstate

import org.json.JSONException
import org.json.JSONObject

/**
 * Post-handshake app-state sync — mirrors `l3/daemon/src/core/appstate.rs`.
 *
 * Pairing security stays in the IK + ping/pong + join-proof path. App-
 * level state (battery, locale, theme, earbuds) flows here, AEAD-wrapped
 * by the Noise transport-mode cipher pair we get after `handshake.split()`.
 * Keeping the layers separate means we can add fields here without
 * touching pairing code.
 */
const val APPSTATE_SCHEMA_V: Int = 1

object DeviceClass {
    const val UNKNOWN = "unknown"
    const val LAPTOP = "laptop"
    const val PHONE = "phone"
    const val TABLET = "tablet"
    const val EARBUDS = "earbuds"
}

data class EarbudsInfo(
    val name: String,
    // The buds' BD_ADDR so the peer can persist them into its own store — the
    // card then auto-appears on a freshly-paired device, not only the side that
    // picked them. Empty when unknown / from an older peer.
    val address: String = "",
    val battery: Int? = null,
    val connected: Boolean = false,
)

data class AppState(
    val v: Int = APPSTATE_SCHEMA_V,
    val battery: Int? = null,
    val deviceClass: String = DeviceClass.UNKNOWN,
    val name: String? = null,
    val locale: String? = null,
    /** Unix-seconds timestamp of the user's last explicit locale change.
     *  `0` means "running system default — no opinion to push". LWW on
     *  receive: peer's strictly-greater value adopted. */
    val localeChangedAt: Long = 0L,
    val theme: String? = null,
    val themeChangedAt: Long = 0L,
    val earbuds: EarbudsInfo? = null,
    /** Revocation signal — see Rust-side doc on `AppState.revoked`. */
    val revoked: Boolean = false,
    /** "You claim the buds" signal. Set by the side that currently
     *  holds the buds when the user taps swap; the receiver runs
     *  its own initiator flow. One-shot — cleared the moment we
     *  encode it (we don't want the peer to re-trigger on every
     *  heartbeat). */
    val audioClaimRequest: Boolean = false,
    /** Phase 2 — phone's current call state. Linux uses this to
     *  pause MPRIS players on `ringing`/`active` and resume on the
     *  transition back to `null`/Idle. Transmitted as a wire string
     *  ("ringing" | "active" | "ending" | null) so older clients
     *  that don't know Phase 2 just see an unknown field and ignore
     *  it. The Linux side persists the previous value internally
     *  and acts only on changes. */
    val callPhase: String? = null,
    /** Call mirror — the full current call (caller/phase/start) additively
     *  carried through AppState so the laptop banner/pill survives a BLE drop
     *  during a call (AppState rides both LAN and the BLE STATE frame). The
     *  dedicated BLE CALL frame stays the fast path; the laptop dedups by
     *  (id, phase). null = no call. */
    val call: com.vortex.a3.core.call.CallEvent? = null,
    /** Laptop→phone call-control command (Accept/Decline/End/Mute) carried
     *  additively over AppState (one-shot) so the laptop's banner/pill buttons
     *  still reach this phone when BLE is down mid-call. The phone acts
     *  idempotently. null = nothing to do. */
    val callControl: com.vortex.a3.core.call.CallControl? = null,
    /** Laptop→phone notification action/reply INVOKE — the LAN backstop for a
     *  clicked notification action when the BLE NOTIFICATION write failed. Deduped
     *  by the mirror's `seq`. null = nothing to do. */
    val notifInvoke: com.vortex.a3.core.notif.NotificationMirror? = null,
    /** Phone→laptop browsing HANDOFF carried over AppState as the LAN backstop —
     *  the page this phone is on, so the laptop's "continue" pill survives a BLE
     *  drop and stays fresh. null / empty url = nothing to hand off. */
    val handoff: com.vortex.a3.core.handoff.HandoffEvent? = null,
    /** Phase 3 — smart audio-follow. `true` while media is actively
     *  playing on THIS device. Carried so the peer can render an owner
     *  indicator and (future) tie-break simultaneous grabs. The actual
     *  auto-switch decision runs on each side's own local detection;
     *  this field is advisory. Older clients ignore the unknown key. */
    val mediaPlaying: Boolean = false,
    /** Phase 3 — last-play-wins arbitration. MILLISECONDS this device has been
     *  in its current playback session at send time (0 = not playing). A
     *  RELATIVE age, not an absolute timestamp, so it's immune to cross-device
     *  clock skew: the receiver re-anchors it to its OWN monotonic clock
     *  (`peer_start = my_now - age`). Frozen across a hand-off pause/resume so
     *  a switch-induced resume doesn't look newer. When both play, the SMALLER
     *  age (most recent play) keeps the buds; the larger yields. Older clients
     *  omit it (0) and fall back to the bool-only behaviour. */
    val mediaPlayAgeMs: Long = 0L,
    /** Laptop→phone now-playing display: track title playing on the laptop
     *  (MPRIS). Empty/null = nothing to show → this phone clears its
     *  laptop-media notification. Phones never send these three. */
    val mediaTitle: String? = null,
    /** Laptop→phone now-playing display: artist. */
    val mediaArtist: String? = null,
    /** Laptop→phone now-playing display: player name (e.g. "Spotify"). */
    val mediaApp: String? = null,
    /** Laptop→phone now-playing display: http(s) cover art for the media card.
     *  THIS phone fetches it (the laptop only names it), so the laptop drops
     *  `file://` art at the source. Null/empty = no art. */
    val mediaArtUrl: String? = null,
    /** Laptop→phone now-playing display: the RAW MPRIS playing state for the
     *  notification's ⏸/▶ — distinct from [mediaPlaying], the smart-switch's
     *  handoff-aware advisory. */
    val mediaNpPlaying: Boolean = false,
    /** Phone→laptop media transport command: "media_play_pause" |
     *  "media_next" | "media_prev" (the laptop-media notification's buttons).
     *  Rides every snapshot until its TTL (the [lockCommand] model); the
     *  laptop dedups by [mediaControlSeq] and executes via MPRIS. */
    val mediaControl: String? = null,
    /** Monotonic sequence for [mediaControl] duplicate suppression (shares
     *  the persisted [LockCommandSeq] counter); 0 = none. */
    val mediaControlSeq: Long = 0L,
    /** Phase 3 — smart audio-follow on/off. A SHARED system setting (not
     *  per-device): toggling on either side applies to both, synced
     *  last-writer-wins via [smartSwitchChangedAt]. Defaults to true so an
     *  older client that omits the key reads as enabled. */
    val smartSwitchEnabled: Boolean = true,
    /** Unix-seconds timestamp of the last explicit smart-switch toggle.
     *  LWW: on receive, the peer's strictly-greater value is adopted. */
    val smartSwitchChangedAt: Long = 0L,
    /** Whether this device is currently plugged in / charging. Carried so
     *  the peer can paint its battery indicator blue (with a charging
     *  glyph) instead of the usual green. Older clients ignore it. */
    val charging: Boolean = false,
    /** Laptop→phone: the laptop's lock-screen state (logind LockedHint),
     *  so this phone's UI can render the right action (Lock vs Unlock).
     *  null = unknown / sender doesn't track it (phones don't send it). */
    val locked: Boolean? = null,
    /** Phone→laptop: is THIS phone currently unlocked (keyguard dismissed)?
     *  Gates the laptop's proximity auto-unlock (owner-present gate). null when
     *  the sender doesn't report it (the laptop). */
    val unlocked: Boolean? = null,
    /** Phone→laptop one-shot remote-lock command: "lock" | "unlock".
     *  Set for a single outgoing snapshot then cleared (the
     *  audioClaimRequest pattern); the laptop executes it and dedups by
     *  [lockCommandSeq]. */
    val lockCommand: String? = null,
    /** Monotonic sequence for [lockCommand] duplicate suppression (BLE +
     *  LAN can deliver the same snapshot). Persisted across restarts via
     *  [LockCommandSeq]; 0 = "no command" and is never executed. */
    val lockCommandSeq: Long = 0L,
    /** Phone→laptop: true while the user wants to VIEW the laptop's screen
     *  (laptop→phone mirror). A level — the laptop starts casting on the
     *  false→true edge (pops its screen-share consent) and stops on true→false. */
    val laptopMirrorReq: Boolean = false,
    /** Phone→laptop: which kind of screen the request is for — true a second
     *  monitor to drag windows onto, false a view of the screen the laptop
     *  already has. Only sent alongside a live request; a laptop that never sees
     *  it falls back to its own saved preference. */
    val laptopMirrorExtend: Boolean = false,
    /** Laptop→phone: set while the laptop IS casting — the params this phone's
     *  viewer dials. null = not casting. Carried under the Noise-sealed
     *  transport; the key is never logged. */
    val laptopCast: LaptopCast? = null,
    /** Laptop→phone: why the laptop's last cast attempt failed; null when it
     *  didn't. Lets us stop asking and tell the user, instead of re-sending a
     *  request the laptop cannot satisfy on every heartbeat forever — which
     *  wedged the UI, since [LaptopMirror.requestView] ignores taps while a
     *  request is already active. Absent from older laptops, hence nullable. */
    val laptopCastError: String? = null,
    /** Laptop→phone: true while the laptop wants the phone's camera as a webcam
     *  (Continuity Camera). The phone starts its camera on the false→true edge. */
    val cameraReq: Boolean = false,
    /** Laptop→phone: which lens — "front" (selfie) or "back". We re-open the
     *  camera when this changes mid-stream. Empty = our default. */
    val cameraFacing: String = "",
    /** Phone→laptop: set while the phone IS streaming its camera — the params
     *  the laptop dials to pull it. null = off. Key never logged. */
    val cameraOffer: CameraOffer? = null,
    /** Laptop→phone "ring my phone" (Find-My): unix-millis of the laptop's last
     *  Ring tap. We ring loudly (overriding silent/DND) on the rising edge and
     *  dedup repeats by comparing against the last value we acted on. 0 = none. */
    val ringSeq: Long = 0L,
    /** This device's OWN current Wi-Fi IPv4 (dotted quad), read live at build
     *  time and carried on every push over BOTH transports. The laptop adopts
     *  it into its cached-peer-IP fast path so mirror/cast/camera never dial a
     *  stale DHCP lease — crucially it also rides BLE, when the phone answers
     *  no mDNS at all (multicast lock released). null = Wi-Fi down. */
    val wifiIp: String? = null,
    /** This device's display refresh rate in Hz, rounded. The laptop asks for a
     *  mirror frame rate capped by this: a capture can never carry more frames
     *  per second than the panel produces, so a fixed request either wastes a
     *  120 Hz panel or asks a 60 Hz one for frames it will never make.
     *  null = unknown (older build, or no display). */
    val displayHz: Int? = null,
    val ts: Long = System.currentTimeMillis() / 1000L,
) {
    fun toJsonBytes(): ByteArray {
        val obj = JSONObject()
        obj.put("v", v)
        battery?.let { obj.put("battery", it) }
        obj.put("class", deviceClass)
        name?.let { obj.put("name", it) }
        locale?.let { obj.put("locale", it) }
        obj.put("locale_changed_at", localeChangedAt)
        theme?.let { obj.put("theme", it) }
        obj.put("theme_changed_at", themeChangedAt)
        earbuds?.let {
            val e = JSONObject()
            e.put("name", it.name)
            if (it.address.isNotEmpty()) e.put("address", it.address)
            it.battery?.let { b -> e.put("battery", b) }
            e.put("connected", it.connected)
            obj.put("earbuds", e)
        }
        if (revoked) obj.put("revoked", true)
        if (audioClaimRequest) obj.put("audio_claim_request", true)
        callPhase?.let { obj.put("call_phase", it) }
        call?.let { obj.put("call", JSONObject(String(it.toJsonBytes(), Charsets.UTF_8))) }
        handoff?.let { obj.put("handoff", JSONObject(String(it.toJsonBytes(), Charsets.UTF_8))) }
        if (mediaPlaying) obj.put("media_playing", true)
        if (mediaPlayAgeMs > 0L) obj.put("media_play_age_ms", mediaPlayAgeMs)
        mediaTitle?.takeIf { it.isNotEmpty() }?.let { obj.put("media_title", it) }
        mediaArtist?.takeIf { it.isNotEmpty() }?.let { obj.put("media_artist", it) }
        mediaApp?.takeIf { it.isNotEmpty() }?.let { obj.put("media_app", it) }
        mediaArtUrl?.takeIf { it.isNotEmpty() }?.let { obj.put("media_art_url", it) }
        if (mediaNpPlaying) obj.put("media_np_playing", true)
        mediaControl?.takeIf { it.isNotEmpty() }?.let {
            obj.put("media_control", it)
            obj.put("media_control_seq", mediaControlSeq)
        }
        // Always emit (incl. the default true) so LWW has a value + ts to
        // compare; the timestamp is what the peer adopts on.
        obj.put("smart_switch_enabled", smartSwitchEnabled)
        obj.put("smart_switch_changed_at", smartSwitchChangedAt)
        if (charging) obj.put("charging", true)
        locked?.let { obj.put("locked", it) }
        unlocked?.let { obj.put("unlocked", it) }
        lockCommand?.let {
            obj.put("lock_command", it)
            obj.put("lock_command_seq", lockCommandSeq)
        }
        if (laptopMirrorReq) {
            obj.put("laptop_mirror_req", true)
            // Only with a request: on its own it would tell the laptop nothing,
            // and `false` there must keep meaning "the phone did not say".
            obj.put("laptop_mirror_extend", laptopMirrorExtend)
        }
        laptopCast?.let {
            val c = JSONObject()
            c.put("ip", it.ip)
            c.put("port", it.port)
            c.put("key", it.key)
            obj.put("laptop_cast", c)
        }
        if (cameraReq) obj.put("camera_req", true)
        if (cameraFacing.isNotEmpty()) obj.put("camera_facing", cameraFacing)
        cameraOffer?.let {
            val c = JSONObject()
            c.put("port", it.port)
            c.put("key", it.key)
            c.put("rot", it.rot)
            obj.put("camera_offer", c)
        }
        wifiIp?.takeIf { it.isNotBlank() }?.let { obj.put("wifi_ip", it) }
        displayHz?.takeIf { it > 0 }?.let { obj.put("display_hz", it) }
        obj.put("ts", ts)
        return obj.toString().toByteArray(Charsets.UTF_8)
    }

    companion object {
        // Battery is a 0..100 percentage. A misbehaving peer (buggy
        // firmware, hostile client) can put any Int into the JSON;
        // clamp here so the UI never tries to render 999% or -5%
        // and so saved-state surfaces stay in the expected range.
        private fun sanitizeBattery(raw: Int?): Int? =
            raw?.takeIf { it in 0..100 }

        /** Mirror the sanitizer used in pairing — see PairingOrchestrator
         *  `sanitizePeerName` for the rationale. A peer name field here
         *  is post-authenticated by Noise, but we still cap length and
         *  strip control / RTL-override characters so a buggy or
         *  misbehaving peer cannot push a 1 MB string or a Unicode
         *  spoofing payload into our UI / persistent stores. */
        private fun sanitizeName(raw: String?): String? {
            if (raw.isNullOrBlank()) return null
            return com.vortex.a3.core.pairing.PairingOrchestrator
                .sanitizePeerName(raw)
                .takeIf { it.isNotEmpty() }
        }

        fun fromJsonBytes(bytes: ByteArray): AppState? = try {
            val obj = JSONObject(String(bytes, Charsets.UTF_8))
            val earbuds: EarbudsInfo? = obj.optJSONObject("earbuds")?.let { e ->
                EarbudsInfo(
                    name = sanitizeName(e.optString("name", "Earbuds")) ?: "Earbuds",
                    address = e.optString("address", ""),
                    battery = sanitizeBattery(
                        if (e.has("battery") && !e.isNull("battery")) e.getInt("battery") else null,
                    ),
                    connected = e.optBoolean("connected", false),
                )
            }
            AppState(
                v = obj.optInt("v", APPSTATE_SCHEMA_V),
                battery = sanitizeBattery(
                    if (obj.has("battery") && !obj.isNull("battery")) obj.getInt("battery") else null,
                ),
                deviceClass = obj.optString("class", DeviceClass.UNKNOWN),
                name = sanitizeName(obj.optString("name", "")),
                locale = obj.optString("locale", "").takeIf { it.isNotBlank() },
                localeChangedAt = obj.optLong("locale_changed_at", 0L),
                theme = obj.optString("theme", "").takeIf { it.isNotBlank() },
                themeChangedAt = obj.optLong("theme_changed_at", 0L),
                earbuds = earbuds,
                revoked = obj.optBoolean("revoked", false),
                audioClaimRequest = obj.optBoolean("audio_claim_request", false),
                callPhase = obj.optString("call_phase", "").takeIf { it.isNotBlank() },
                call = obj.optJSONObject("call")?.let {
                    com.vortex.a3.core.call.CallEvent.fromJsonBytes(it.toString().toByteArray(Charsets.UTF_8))
                },
                callControl = obj.optJSONObject("call_control")?.let {
                    com.vortex.a3.core.call.CallControl.fromJsonBytes(it.toString().toByteArray(Charsets.UTF_8))
                },
                notifInvoke = obj.optJSONObject("notif_invoke")?.let {
                    com.vortex.a3.core.notif.NotificationMirror.fromJsonBytes(it.toString().toByteArray(Charsets.UTF_8))
                },
                mediaPlaying = obj.optBoolean("media_playing", false),
                mediaPlayAgeMs = obj.optLong("media_play_age_ms", 0L),
                mediaTitle = obj.optString("media_title", "").takeIf { it.isNotBlank() },
                mediaArtist = obj.optString("media_artist", "").takeIf { it.isNotBlank() },
                mediaApp = obj.optString("media_app", "").takeIf { it.isNotBlank() },
                mediaArtUrl = obj.optString("media_art_url", "").takeIf { it.isNotBlank() },
                mediaNpPlaying = obj.optBoolean("media_np_playing", false),
                mediaControl = obj.optString("media_control", "").takeIf { it.isNotBlank() },
                mediaControlSeq = obj.optLong("media_control_seq", 0L),
                smartSwitchEnabled = obj.optBoolean("smart_switch_enabled", true),
                smartSwitchChangedAt = obj.optLong("smart_switch_changed_at", 0L),
                charging = obj.optBoolean("charging", false),
                locked = if (obj.has("locked") && !obj.isNull("locked")) {
                    obj.optBoolean("locked")
                } else {
                    null
                },
                unlocked = if (obj.has("unlocked") && !obj.isNull("unlocked")) {
                    obj.optBoolean("unlocked")
                } else {
                    null
                },
                lockCommand = obj.optString("lock_command", "").takeIf { it.isNotBlank() },
                lockCommandSeq = obj.optLong("lock_command_seq", 0L),
                laptopMirrorReq = obj.optBoolean("laptop_mirror_req", false),
                laptopMirrorExtend = obj.optBoolean("laptop_mirror_extend", false),
                laptopCastError = obj.optString("laptop_cast_error", "")
                    .takeIf { it.isNotBlank() },
                laptopCast = obj.optJSONObject("laptop_cast")?.let { c ->
                    // ip is unused (the laptop dials us — we're the server); only
                    // port + key matter.
                    val port = c.optInt("port", 0)
                    val key = c.optString("key", "")
                    if (port != 0 && key.isNotEmpty()) {
                        LaptopCast(ip = c.optString("ip", ""), port = port, key = key)
                    } else {
                        null
                    }
                },
                cameraReq = obj.optBoolean("camera_req", false),
                ringSeq = obj.optLong("ring_seq", 0L),
                wifiIp = obj.optString("wifi_ip", "").takeIf { it.isNotBlank() },
                displayHz = obj.optInt("display_hz", 0).takeIf { it > 0 },
                cameraFacing = obj.optString("camera_facing", ""),
                cameraOffer = obj.optJSONObject("camera_offer")?.let { c ->
                    val port = c.optInt("port", 0)
                    val key = c.optString("key", "")
                    if (port != 0 && key.isNotEmpty()) {
                        CameraOffer(port = port, key = key, rot = c.optInt("rot", 0))
                    } else {
                        null
                    }
                },
                ts = obj.optLong("ts", 0L),
            )
        } catch (_: JSONException) {
            null
        }
    }
}

/** Laptop→phone screen-cast offer (rides [AppState.laptopCast]): where this
 *  phone's viewer connects + the base64 32-byte media key to open the sealed
 *  HEVC stream. Mirrors the Rust `appstate::LaptopCast`. */
data class LaptopCast(
    val ip: String,
    val port: Int,
    val key: String,
)

/** Phone→laptop Continuity-Camera offer (rides [AppState.cameraOffer]): the
 *  port + base64/hex media key the laptop dials to pull the phone camera.
 *  Mirrors the Rust `appstate::CameraOffer`. */
data class CameraOffer(
    val port: Int,
    val key: String,
    /** Clockwise degrees the laptop rotates frames to show upright (sensor
     *  orientation; changes on a lens flip). */
    val rot: Int = 0,
)
