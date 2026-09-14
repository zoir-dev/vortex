package com.vortex.a3.core.ble

import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Vortex wire frame per spec §6.3. */
object FrameType {
    const val PAIRING_HANDSHAKE: Byte = 0x10
    const val PAIRING_APPROVAL: Byte = 0x11
    const val PAIRING_TRUSTED_INFO: Byte = 0x12
    const val RECONNECT_HANDSHAKE: Byte = 0x20
    /** Reserved: V2 channel-promotion / join-proof (spec §8.5). V1
     *  does NOT exchange this — each transport runs its own IK. The
     *  byte is reserved so the V2 design space stays compatible. */
    const val LAN_JOIN_PROOF_RESERVED_V2: Byte = 0x21
    const val TRANSPORT_KEEPALIVE: Byte = 0x30
    const val TRANSPORT_APP_DATA: Byte = 0x31
    /** Post-handshake AEAD-wrapped earbuds-switch op frame. Carries an
     *  `AudioOpFrame` JSON inside (see `core/earbuds/AudioOp.kt`).
     *  Lives next to `TRANSPORT_APP_DATA` so switches don't have to
     *  piggyback on the 12 s heartbeat — they get their own narrow
     *  frame and the mDNS nudge wakes the peer immediately. */
    const val AUDIO_OP: Byte = 0x32
    /** Post-handshake AEAD-wrapped app-state push. Carries an `AppState`
     *  JSON (same shape as `TRANSPORT_APP_DATA`) so a battery/charging
     *  change reaches the peer over the already-open BLE link in ~200 ms.
     *  Routed entirely separately from `AUDIO_OP` — never touches the
     *  audio-handoff state machine. Mirrors Rust `ble::frame::ty::STATE`. */
    const val STATE: Byte = 0x33
    /** Post-handshake AEAD-wrapped notification-mirror push: a phone
     *  notification (app + title + text + ts) forwarded to the laptop for
     *  desktop display. Routed separately from AUDIO_OP/STATE; never
     *  touches the audio handoff. Mirrors Rust `ble::frame::ty::NOTIFICATION`. */
    const val NOTIFICATION: Byte = 0x34
    /** A "live activity" — an ongoing, progress-bearing notification (ride
     *  ETA, navigation, delivery, timer) that updates in place and drives a
     *  persistent pill on the laptop's top bar. Routed separately from
     *  AUDIO_OP/STATE/NOTIFICATION; never touches the audio handoff. Mirrors
     *  Rust `ble::frame::ty::LIVE_ACTIVITY`. */
    const val LIVE_ACTIVITY: Byte = 0x35
    /** A chunk of an app-icon PNG (sent once per app, reassembled + cached on
     *  the laptop so mirrored notifications show the real app logo). Payload:
     *  [appIdLen u8][appId][total u16 BE][idx u16 BE][png-chunk bytes]. Routed
     *  separately; never touches the audio handoff. Mirrors Rust ty::ICON. */
    const val ICON: Byte = 0x36
    /** An incoming/ongoing phone-call event mirrored to the laptop (caller
     *  name/number, phase: ringing/active/ended, call start time). Drives a
     *  continuity-style call banner (ringing → Accept/Decline) then an
     *  in-call pill (caller + live duration → Mute/End). Routed separately from
     *  AUDIO_OP/STATE/NOTIFICATION/LIVE_ACTIVITY; never touches the audio
     *  handoff FSM. Mirrors Rust `ble::frame::ty::CALL`. */
    const val CALL: Byte = 0x37
    /** A laptop→phone call-control command (accept/decline/end/mute/unmute/
     *  speaker/sms-reject) acting on the current call via TelecomManager /
     *  AudioManager. The control-side counterpart of CALL; never touches the
     *  audio handoff. Mirrors Rust `ble::frame::ty::CALL_CONTROL`. */
    const val CALL_CONTROL: Byte = 0x38
    /** One chunk of the phone's contacts list (name + numbers), sent to the
     *  laptop companion's Contacts page. Chunked like ICON because the JSON can
     *  exceed one BLE notify. Payload: `[total u16 BE][idx u16 BE][json-chunk]`.
     *  Routed separately; never touches the audio handoff. Mirrors Rust
     *  `ble::frame::ty::CONTACTS`. */
    const val CONTACTS: Byte = 0x39
    /** One chunk of the phone's recent call log (number, name, type, time,
     *  duration) for the laptop companion's Recents page. Chunked like CONTACTS:
     *  `[total u16 BE][idx u16 BE][json-chunk]`. Routed separately; never touches
     *  the audio handoff. Mirrors Rust `ble::frame::ty::CALL_LOG`. */
    const val CALL_LOG: Byte = 0x3A
    /** One chunk of the phone's recent SMS messages (address, body, type, time,
     *  thread) for the laptop companion's Messages page. Chunked like CALL_LOG:
     *  `[total u16 BE][idx u16 BE][json-chunk]`. Routed separately; never touches
     *  the audio handoff. Mirrors Rust `ble::frame::ty::SMS`. */
    const val SMS: Byte = 0x3B
    /** One chunk of a SINGLE conversation's message page, sent on demand when
     *  the laptop opens a thread and scrolls up (infinite scroll). The laptop
     *  requests a page via a CALL_CONTROL `load_thread` command; the phone reads
     *  that thread (offset/limit) and replies with these frames. Same chunk
     *  format as SMS but MERGED into the thread (not a full-list replace), so the
     *  full history loads page-by-page without one giant burst (which would
     *  desync the cipher). Mirrors Rust `ble::frame::ty::SMS_THREAD`. */
    const val SMS_THREAD: Byte = 0x3C
    /** LAN-only bulk-sync negotiation (never rides BLE). sub=0x01: the laptop
     *  sends `{"<dataset>":"<sha256-hex of its cached JSON>"}`; the phone
     *  replies with the dataset's chunked frames (e.g. CONTACTS) only for
     *  hashes that DIFFER, then sub=0x02 (done) with
     *  `{"<dataset>":"sent"|"match"}`. Unchanged data costs zero bytes; big
     *  lists ride reliable TCP instead of a BLE notify burst. Mirrors Rust
     *  `ble::frame::ty::BULK_SYNC`. */
    const val BULK_SYNC: Byte = 0x3D
    /** One chunk of a call-log HISTORY batch (bulk-sync watermark dataset,
     *  LAN-only): same wire shape as CALL_LOG but MERGED into the laptop's
     *  persistent history store instead of replacing the recent list.
     *  Mirrors Rust `ble::frame::ty::CALL_LOG_HISTORY`. */
    const val CALL_LOG_HISTORY: Byte = 0x3E
    /** One chunk of the phone's FULL SMS id list (bulk-sync hash dataset,
     *  LAN-only): lets the laptop prune history entries the phone deleted.
     *  Mirrors Rust `ble::frame::ty::SMS_IDS`. */
    const val SMS_IDS: Byte = 0x3F
    /** Clipboard-sync push (text copied on one device, mirrored to the other,
     *  universal-clipboard style). Bidirectional over AUDIO_SIGNAL.
     *  Mirrors Rust `ble::frame::ty::CLIPBOARD`. */
    const val CLIPBOARD: Byte = 0x40
    /** One chunk of a clipboard IMAGE (PNG), `[total][idx][data]`.
     *  Mirrors Rust `ble::frame::ty::CLIPBOARD_IMAGE`. */
    const val CLIPBOARD_IMAGE: Byte = 0x41
    /** "Image available, pull over LAN" signal `{token, bytes}`.
     *  Mirrors Rust `ble::frame::ty::CLIPBOARD_IMAGE_OFFER`. */
    const val CLIPBOARD_IMAGE_OFFER: Byte = 0x42
    /** One chunk of a LONG clipboard text (`[total][idx][utf8]`) — used when a
     *  copied text is too big for one CLIPBOARD frame. Never split mid-char.
     *  Mirrors Rust `ble::frame::ty::CLIPBOARD_TEXT`. */
    const val CLIPBOARD_TEXT: Byte = 0x43
    /** One chunk of an instant-share-style FILE (`[total][idx][data]`), pulled
     *  over LAN. Reassembled and saved to the laptop's Downloads (not clipboard).
     *  Mirrors Rust `ble::frame::ty::CLIPBOARD_FILE`. */
    const val CLIPBOARD_FILE: Byte = 0x45
    /** "Wi-Fi Direct ready" signal `{ssid, pass}` — phone made a P2P group for a
     *  fast pull. Mirrors Rust `ble::frame::ty::WIFI_DIRECT_OFFER`. */
    const val WIFI_DIRECT_OFFER: Byte = 0x46
    /** Laptop → phone file PUSH (reverse-direction share): `FILE_PUSH_OFFER` {name,mime,
     *  bytes}, then `FILE_PUSH` chunks. Phone saves to Downloads. Mirrors Rust
     *  `ble::frame::ty::FILE_PUSH*`. (0x47/0x48 are the screen-mirror module's.) */
    const val FILE_PUSH_OFFER: Byte = 0x49
    const val FILE_PUSH: Byte = 0x4A
    /** Phone → laptop reply to a FILE_PUSH_OFFER: AEAD payload is one byte
     *  (1 = accept, 0 = decline) the user chose on this phone. The laptop sends
     *  FILE_PUSH chunks only on accept (receive consent). Mirrors Rust
     *  `ble::frame::ty::FILE_PUSH_DECISION`. */
    const val FILE_PUSH_DECISION: Byte = 0x4B
    /** Phone → laptop browsing HANDOFF (seamless-continuity): `{url,title,app_id,
     *  open_now}` JSON. open_now=true (Share) opens it on the laptop now;
     *  false (accessibility live-read) shows a "continue from phone" pill.
     *  Mirrors Rust `ble::frame::ty::HANDOFF`. */
    const val HANDOFF: Byte = 0x4C
    /** Notes/Todos bidirectional sync: the full item set (incl. tombstones) as
     *  `[total][idx][data]` JSON chunks. Both sides push on connect + after a
     *  local edit and LWW-merge (`updated_at`). Mirrors Rust `ty::NOTES_SYNC`. */
    const val NOTES_SYNC: Byte = 0x4D
    /** Transport-level fragment of ONE oversized sealed frame on the
     *  AUDIO_SIGNAL notify channel. Android silently TRUNCATES any notify
     *  longer than ATT_MTU−3 (observed: 529–696-byte NOTIFICATION frames
     *  capped at 514 → undecodable + a burned nonce — long email
     *  notifications never arrived). [GattServer.sendAudioSignal] splits the
     *  fully-ENCODED sealed frame into FRAG envelopes, payload
     *  `[total u16 BE][idx u16 BE][slice]`; the laptop reassembles the inner
     *  bytes and processes them as one arrival. FRAG itself is NOT sealed —
     *  the inner frame already is. Mirrors Rust `ty::FRAG`. */
    /** A listing of one folder on the phone: `[total][idx][json]` chunks,
     *  answering a `browse` key in the bulk-sync request. Browsing, not
     *  syncing — nothing is copied until the laptop asks for a file, and then
     *  it comes back over CLIPBOARD_FILE. Mirrors Rust `ty::PHONE_FILES`. */
    const val PHONE_FILES: Byte = 0x4F

    const val FRAG: Byte = 0x4E
    /** Session-ownership handoff (design doc §D4). A device may TRUST many
     *  peers but is ACTIVE with exactly one; this frame is how the two sides
     *  agree which. `sub` carries the kind ([FrameSub.HANDOFF_RELEASE] etc.),
     *  the AEAD payload an optional UTF-8 successor name for the UI.
     *  Additive: both sides log-and-ignore unknown frame types, so a peer
     *  without this build is unaffected. Mirrors Rust `ty::PEER_HANDOFF`. */
    // 0x54, not 0x4F — see the note on the Rust side: upstream took 0x4F for
    // PHONE_FILES, and PEER_HANDOFF is the one that has never shipped.
    const val PEER_HANDOFF: Byte = 0x54
    /** Ranged-filesystem request. `sub` carries the op
     *  ([com.vortex.a3.core.fs.FsOp]), the payload a JSON request — plus a
     *  binary byte tail for WRITE.
     *
     *  BIDIRECTIONAL and symmetric: this phone both serves these (so the
     *  laptop can browse its storage) and sends them (so it can browse the
     *  laptop's). Neither the frame nor its handler names a side. See
     *  `docs/design/file-browsing.md`. Mirrors Rust `ty::FS_REQ`. */
    const val FS_REQ: Byte = 0x50
    /** Successful non-data reply — directory page, stat, open result, write
     *  ack. Carries `FsReply` JSON. Mirrors Rust `ty::FS_META`. */
    const val FS_META: Byte = 0x51
    /** Read result: `[id u32 BE][offset u64 BE][flags u8][bytes]`. Binary, not
     *  JSON: base64 would cost 33% on the protocol's hottest path. Mirrors
     *  Rust `ty::FS_DATA`. */
    const val FS_DATA: Byte = 0x52
    /** A definite failure for one request id (`FsErr` JSON). Every failing op
     *  answers with one — a file manager blocked on a read that will never be
     *  answered is this feature's worst outcome, so silence is never valid.
     *  Mirrors Rust `ty::FS_ERR`. */
    const val FS_ERR: Byte = 0x53
    const val ERROR: Byte = 0x7F
}

object FrameSub {
    const val PING: Byte = 0x01
    const val PONG: Byte = 0x02
    const val ECHO_REQUEST: Byte = 0x01
    const val ECHO_RESPONSE: Byte = 0x02
    /** [FrameType.PEER_HANDOFF] kinds. Mirror Rust `ty::sub::HANDOFF_*`. */
    /** "You are no longer my active peer" — sent by the side handing ownership
     *  over, so the receiver stops presenting itself as connected instead of
     *  finding out on next contact. */
    const val HANDOFF_RELEASE: Byte = 0x01
    /** Refused: another peer is already active. Explicit because silence is
     *  indistinguishable from packet loss and invites a retry loop. */
    const val HANDOFF_BUSY: Byte = 0x02
    /** Request to become the active peer. */
    const val HANDOFF_CLAIM: Byte = 0x03
}

/** Header size in bytes. */
const val FRAME_HEADER_LEN: Int = 4

/** Per spec §11. */
const val MAX_FRAME_PAYLOAD: Int = 63 * 1024

data class Frame(
    val type: Byte,
    val sub: Byte,
    val payload: ByteArray,
) {
    init {
        require(payload.size <= MAX_FRAME_PAYLOAD) { "payload too large" }
    }

    fun encode(): ByteArray {
        val buf = ByteBuffer.allocate(FRAME_HEADER_LEN + payload.size)
            .order(ByteOrder.BIG_ENDIAN)
        buf.put(type)
        buf.put(sub)
        buf.putShort(payload.size.toShort())
        buf.put(payload)
        return buf.array()
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Frame) return false
        return type == other.type && sub == other.sub && payload.contentEquals(other.payload)
    }

    override fun hashCode(): Int {
        var r = type.toInt()
        r = 31 * r + sub.toInt()
        r = 31 * r + payload.contentHashCode()
        return r
    }

    companion object {
        fun echoResponse(payload: ByteArray): Frame =
            Frame(FrameType.TRANSPORT_KEEPALIVE, FrameSub.ECHO_RESPONSE, payload.copyOf())

        fun decode(bytes: ByteArray): Result<Frame> {
            if (bytes.size < FRAME_HEADER_LEN) {
                return Result.failure(IllegalArgumentException("frame too short: ${bytes.size}"))
            }
            val length = (bytes[2].toInt() and 0xFF shl 8) or (bytes[3].toInt() and 0xFF)
            if (length > MAX_FRAME_PAYLOAD) {
                return Result.failure(IllegalArgumentException("declared length $length exceeds max"))
            }
            val total = FRAME_HEADER_LEN + length
            if (bytes.size != total) {
                return Result.failure(
                    IllegalArgumentException(
                        "length mismatch: declared $total, actual ${bytes.size}",
                    )
                )
            }
            return Result.success(
                Frame(
                    type = bytes[0],
                    sub = bytes[1],
                    payload = bytes.copyOfRange(FRAME_HEADER_LEN, bytes.size),
                )
            )
        }
    }
}
