package com.vortex.a3.core.ble

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.util.Log
import com.southernstorm.noise.protocol.CipherState
import com.vortex.a3.core.pairing.PairingOrchestrator
import com.vortex.a3.core.pairing.ReconnectOrchestrator
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.Collections
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * BLE GATT server hosting the Vortex Service per spec §9.1 and
 * §10.1.
 *
 * V1 service hierarchy:
 *
 *   Vortex Service (primary)
 *     ├── Pairing Control characteristic   (write + notify) — Phase 5b
 *     ├── Reconnect Control characteristic (write + notify) — Phase 6
 *     └── Capability characteristic        (read)
 *
 * Phase 5a wires only the Capability characteristic; the others are
 * placeholders that succeed on read but reject writes until subsequent
 * phases implement the protocol.
 */
class GattServer(
    private val context: Context,
    /**
     * Optional pairing orchestrator. When set, Pairing Control frames are
     * dispatched here (Phase 5c+). When null, only the echo path is wired.
     */
    var pairingOrchestrator: PairingOrchestrator? = null,
    /**
     * Optional reconnect orchestrator. When set, Reconnect Control frames
     * are dispatched here (Phase 6).
     */
    var reconnectOrchestrator: ReconnectOrchestrator? = null,
) {

    private var server: BluetoothGattServer? = null
    private var pairingControlChar: BluetoothGattCharacteristic? = null
    private var reconnectControlChar: BluetoothGattCharacteristic? = null
    private var audioSignalChar: BluetoothGattCharacteristic? = null

    // Rate-limit for malformed-frame warnings. An attacker who can
    // write garbage to our GATT characteristics should not be able to
    // fill logcat (or any future remote log sink) by spraying bad
    // frames; cap the warn rate at 1/sec.
    @Volatile private var lastDecodeWarnMs: Long = 0L
    private val decodeWarnIntervalMs: Long = 1000L

    /** Devices that have enabled Notifications on Pairing Control. */
    private val pairingSubscribers: MutableSet<BluetoothDevice> =
        Collections.synchronizedSet(HashSet())
    /** Devices that have enabled Notifications on Reconnect Control. */
    private val reconnectSubscribers: MutableSet<BluetoothDevice> =
        Collections.synchronizedSet(HashSet())
    /** Devices that have enabled Notifications on Audio Signal. Linux
     *  subscribes after a trusted Noise transport session is up so the
     *  phone can push call-state + manual-claim opcodes without paying
     *  the LAN heartbeat round-trip. */
    private val audioSignalSubscribers: MutableSet<BluetoothDevice> =
        Collections.synchronizedSet(HashSet())

    /** Per-peer Noise transport SEND cipher captured from the BLE-IK
     *  reconnect handshake's `.split()`. Used by [sendAudioOpEncrypted]
     *  to AEAD-seal outbound AUDIO_OP frames so the wire bytes pushed
     *  over BLE NOTIFY are indistinguishable from those that ride the
     *  LAN audio-op socket — same crypto, same nonce discipline. Keyed
     *  by the BluetoothDevice's address so writes from the GATT
     *  callbacks can find the cipher without a peer-pub lookup. */
    private val audioSendCiphers: ConcurrentHashMap<String, CipherState> = ConcurrentHashMap()

    /** Per-peer Noise transport RECEIVE cipher (the IK pair's other
     *  half). Used to AEAD-open AUDIO_SIGNAL frames the laptop writes
     *  to us — Linux's reverse channel for Approve / Released / Done.
     *  Without this the laptop's BLE writes land in `else` of the
     *  WRITE handler and get dropped. Keyed by device address (same
     *  shape as [audioSendCiphers]). ChatGPT review #4. */
    private val audioRecvCiphers: ConcurrentHashMap<String, CipherState> = ConcurrentHashMap()

    /** Consecutive AEAD-open failures per device. A BLE frame lost on a flaky
     *  link (no disconnect) leaves our recv nonce behind the laptop's sender →
     *  every later frame fails. After a few in a row the cipher is desynced for
     *  good, so we drop the link to force a fresh IK handshake (resets both
     *  ciphers). Reset to 0 on any successful open. */
    private val recvAeadFails: ConcurrentHashMap<String, Int> = ConcurrentHashMap()

    /** Expected receive nonce per device for the AUDIO_SIGNAL stream. noise-java
     *  exposes `setNonce` but no getter, so we track it ourselves: a dropped
     *  laptop→phone write leaves us behind the laptop's send nonce, and on a
     *  failed open we skip this forward up to [NONCE_RESYNC_WINDOW] and retry,
     *  resyncing past the gap WITHOUT a re-handshake (mirrors the laptop's
     *  run_listener). Reset on (re)handshake; this is what makes call control
     *  reliable BLE-only. */
    private val audioRecvNonce: ConcurrentHashMap<String, Long> = ConcurrentHashMap()

    /** Peer-static-pub bytes (Vortex identity) per BluetoothDevice address.
     *  Lets the WRITE handler dispatch a decrypted incoming AUDIO_OP frame
     *  through [onAudioOpReceived] without forcing the caller to keep a
     *  parallel address↔peer-pub lookup table. */
    private val deviceToPeerPub: ConcurrentHashMap<String, ByteArray> = ConcurrentHashMap()

    /** Long-write (prepared write) reassembly buffer, per device address.
     *  BlueZ on the laptop fragments ANY laptop→phone frame larger than the
     *  negotiated ATT_MTU-3 (e.g. the ~500 B STATE / notes frames) into a run
     *  of PREPARE_WRITE slices followed by one EXECUTE_WRITE. We accumulate the
     *  slices here and run the normal dispatch once on execute. Without this,
     *  each slice was decoded as a whole frame (garbage → dropped) and the
     *  execute failed (ATT 0x0E), tearing the link down — so STATE never
     *  reached a BLE-only phone and it showed the laptop disconnected. */
    private val prepWriteBuf: ConcurrentHashMap<String, java.io.ByteArrayOutputStream> = ConcurrentHashMap()
    private val prepWriteChar: ConcurrentHashMap<String, java.util.UUID> = ConcurrentHashMap()

    /** Dispatch hook for incoming AUDIO_OP frames received over GATT
     *  WRITE on the AUDIO_SIGNAL characteristic (laptop → phone path,
     *  symmetric to [sendAudioOpEncrypted]). VortexService wires this
     *  to `EarbudsSwitchHolder.onIncoming`. Default no-op so tests
     *  that don't exercise this path don't need to plug it. */
    @Volatile var onAudioOpReceived: (peerStaticPub: ByteArray, jsonBytes: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when the laptop writes a NOTIFICATION frame (a mirrored
     *  desktop notification) — the JSON bytes are a [com.vortex.a3.core.notif.NotificationMirror].
     *  Wired by VortexStack to post an Android notification. */
    @Volatile var onNotificationReceived: (peerStaticPub: ByteArray, jsonBytes: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when the laptop writes a CLIPBOARD frame (text it copied) —
     *  the JSON bytes are a `{text, ts}` clipboard payload. Wired by
     *  VortexStack to setPrimaryClip on this phone. */
    @Volatile var onClipboardReceived: (peerStaticPub: ByteArray, jsonBytes: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked per CLIPBOARD_IMAGE chunk (laptop copied an image) — the
     *  payload is `[total][idx][data]`. VortexStack reassembles + sets the
     *  phone clipboard image. */
    @Volatile var onClipboardImageChunk: (peerStaticPub: ByteArray, chunk: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked per CLIPBOARD_TEXT chunk (laptop copied LONG text) — the payload
     *  is `[total][idx][utf8]`. VortexStack reassembles + setPrimaryClip. */
    @Volatile var onClipboardTextChunk: (peerStaticPub: ByteArray, chunk: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when the laptop writes a STATE frame (its battery/charging/
     *  earbuds) — the JSON bytes are an AppState. Wired by VortexStack to the
     *  same peer-state handler as the LAN heartbeat, so the phone shows the
     *  laptop CONNECTED over BLE alone (Wi-Fi with AP isolation). Default no-op. */
    @Volatile var onStateReceived: (peerStaticPub: ByteArray, jsonBytes: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when the laptop writes a CALL_CONTROL frame (Accept/Decline/End/
     *  Mute from its call banner) — the JSON bytes are a
     *  [com.vortex.a3.core.call.CallControl]. Wired by VortexStack to a
     *  CallController that acts via TelecomManager/AudioManager. Default no-op. */
    @Volatile var onCallControlReceived: (peerStaticPub: ByteArray, jsonBytes: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when the laptop WRITES a NOTES_SYNC chunk (`[total][idx][data]`):
     *  reassembled + LWW-merged into the local notes store. */
    @Volatile var onNotesSyncReceived: (peerStaticPub: ByteArray, chunk: ByteArray) -> Unit =
        { _, _ -> }

    /** Invoked when a device (the laptop) ENABLES notifications on the
     *  AUDIO_SIGNAL characteristic — i.e. the BLE notify path just became
     *  deliverable. VortexStack uses this to flush any notifications that
     *  were buffered while the link was down. Default no-op. */
    @Volatile var onAudioSignalSubscribed: (device: BluetoothDevice) -> Unit = { }

    /** Invoked when a GATT central disconnects — i.e. the BLE session is
     *  gone until the next reconnect. VortexStack uses this to re-acquire
     *  the LAN multicast lock (mDNS discovery matters again once the BLE
     *  fast-path is down). Default no-op. */
    @Volatile var onPeerDisconnected: (device: BluetoothDevice) -> Unit = { }

    /** Peer-static-pub (hex) → the BluetoothDevice we'll push notifications
     *  to. Filled in alongside [audioSendCiphers] so callers that route by
     *  trust identity (CallFlowOrchestrator / SwitchOrchestrator) can find
     *  the device without holding a BluetoothDevice reference themselves. */
    private val peerToDevice: ConcurrentHashMap<String, BluetoothDevice> = ConcurrentHashMap()

    /** V1 capability response per spec §9.1.5. */
    private val capabilityResponse: ByteArray = ByteBuffer.allocate(3)
        .order(ByteOrder.BIG_ENDIAN)
        .put(Ble.V1_VERSION)
        .putShort(0)        // capability_bits = 0 in V1
        .array()

    /** Standard Client Characteristic Configuration descriptor UUID. */
    private val cccUuid: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

    fun start(): Boolean {
        if (server != null) {
            Log.w(TAG, "server already running")
            return true
        }
        val bm = context.getSystemService(BluetoothManager::class.java)
        val s = try {
            bm.openGattServer(context, callback)
        } catch (e: SecurityException) {
            Log.e(TAG, "missing BLUETOOTH_CONNECT", e)
            return false
        } ?: run {
            Log.e(TAG, "openGattServer returned null")
            return false
        }

        val service = BluetoothGattService(
            Ble.VORTEX_SERVICE_UUID,
            BluetoothGattService.SERVICE_TYPE_PRIMARY,
        )
        service.addCharacteristic(
            BluetoothGattCharacteristic(
                Ble.CAPABILITY_UUID,
                BluetoothGattCharacteristic.PROPERTY_READ,
                BluetoothGattCharacteristic.PERMISSION_READ,
            )
        )
        // Placeholders for Phase 5b/6. They register UUIDs so peers can
        // discover the full service shape now; writes are rejected until
        // the orchestrator wires them up.
        // Pairing Control: write + notify with CCC descriptor. Phase 5b
        // wires only the echo path; Phase 5c will dispatch to the Noise
        // orchestrator.
        val pairingControl = BluetoothGattCharacteristic(
            Ble.PAIRING_CONTROL_UUID,
            BluetoothGattCharacteristic.PROPERTY_WRITE
                or BluetoothGattCharacteristic.PROPERTY_NOTIFY,
            BluetoothGattCharacteristic.PERMISSION_WRITE,
        )
        pairingControl.addDescriptor(
            BluetoothGattDescriptor(
                cccUuid,
                BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
            )
        )
        service.addCharacteristic(pairingControl)
        pairingControlChar = pairingControl

        // Reconnect Control: write + notify with CCC descriptor (Phase 6).
        val reconnectControl = BluetoothGattCharacteristic(
            Ble.RECONNECT_CONTROL_UUID,
            BluetoothGattCharacteristic.PROPERTY_WRITE
                or BluetoothGattCharacteristic.PROPERTY_NOTIFY,
            BluetoothGattCharacteristic.PERMISSION_WRITE,
        )
        reconnectControl.addDescriptor(
            BluetoothGattDescriptor(
                cccUuid,
                BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
            )
        )
        service.addCharacteristic(reconnectControl)
        reconnectControlChar = reconnectControl

        // Audio Signal: write + notify (Phase 2). Linux subscribes
        // post-pairing; this is the fast-path that carries 1-byte
        // AES-GCM-encrypted opcodes (call ringing/active/ended, manual
        // swap claim, etc.) without paying the LAN heartbeat latency.
        val audioSignal = BluetoothGattCharacteristic(
            Ble.AUDIO_SIGNAL_UUID,
            // WRITE_NO_RESPONSE is essential: bluer's default char.write() on the
            // laptop sends a fire-and-forget Write Command, which BlueZ REJECTS
            // ("Failed to initiate write") if the characteristic only advertises
            // PROPERTY_WRITE (with-response). Without it, every laptop→phone write
            // failed over BLE — so the phone showed the laptop DISCONNECTED unless
            // LAN happened to carry the state. Command can drop frames under
            // contention, but the recv-side nonce-resync recovers from that.
            BluetoothGattCharacteristic.PROPERTY_WRITE
                or BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE
                or BluetoothGattCharacteristic.PROPERTY_NOTIFY,
            BluetoothGattCharacteristic.PERMISSION_WRITE,
        )
        audioSignal.addDescriptor(
            BluetoothGattDescriptor(
                cccUuid,
                BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
            )
        )
        service.addCharacteristic(audioSignal)
        audioSignalChar = audioSignal

        try {
            s.addService(service)
        } catch (e: SecurityException) {
            Log.e(TAG, "addService missing permission", e)
            s.close()
            return false
        }
        server = s
        Log.i(TAG, "GATT server started, service ${Ble.VORTEX_SERVICE_UUID}")
        return true
    }

    fun stop() {
        val s = server ?: return
        try {
            s.close()
        } catch (e: SecurityException) {
            Log.w(TAG, "close threw: ${e.message}")
        }
        server = null
        pairingControlChar = null
        reconnectControlChar = null
        audioSignalChar = null
        pairingSubscribers.clear()
        reconnectSubscribers.clear()
        audioSignalSubscribers.clear()
        Log.i(TAG, "GATT server stopped")
    }

    /**
     * Push a Pairing Control frame to a SPECIFIC peer device. Used by
     * the UI to deliver an asynchronously-decided APPROVE / REJECT
     * frame after the user taps the SAS dialog, and by the GATT write
     * handler to reply to the originating peer.
     *
     * Critical: pairing msg2, IK msg2, approval frames, and pong are
     * session-private. Broadcasting them to every subscriber would
     * let an attacker who subscribed to the same characteristic
     * observe (and in some cases hijack) another peer's handshake.
     * Always target the device that initiated the exchange.
     */
    fun sendPairingControl(device: BluetoothDevice, frame: Frame) =
        notifyTo(device, frame, pairingControlChar, pairingSubscribers)

    private fun notifyReconnectTo(device: BluetoothDevice, frame: Frame) =
        notifyTo(device, frame, reconnectControlChar, reconnectSubscribers)

    /** Negotiated ATT MTU per device (from [callback]'s onMtuChanged). The
     *  spec default 23 applies until the central negotiates up — in practice
     *  BlueZ requests 512+ before subscribing, so the conservative default
     *  only ever covers the first instants of a connection. */
    private val deviceMtu = java.util.concurrent.ConcurrentHashMap<String, Int>()

    /** Push an AUDIO_SIGNAL frame to a peer that has subscribed to the
     *  characteristic. The frame is an opaque AEAD-protected payload
     *  (already encrypted by the Noise transport session) — this method
     *  is just the wire path; crypto stays with [SwitchOrchestrator] /
     *  the audio-op layer so we don't fork the security model.
     *
     *  A single notify longer than ATT_MTU−3 is silently TRUNCATED by the
     *  Android stack (observed live: 529–696-byte NOTIFICATION frames capped
     *  at 514 on a 517 MTU) — the peer receives an undecodable frame AND our
     *  send nonce is burned, so the payload is unrecoverable. Frames that
     *  don't fit are therefore split into FRAG envelopes
     *  (`[total u16 BE][idx u16 BE][slice]` of the fully-encoded sealed
     *  frame); the laptop reassembles and processes them as one arrival.
     *  Called under `synchronized(cipher)` from [sealAndNotify], so a
     *  fragment burst can't interleave with another sealed frame. */
    fun sendAudioSignal(device: BluetoothDevice, frame: Frame): Boolean {
        val budget = ((deviceMtu[device.address] ?: 23) - 3).coerceAtLeast(1)
        val encoded = frame.encode()
        if (encoded.size <= budget) {
            return notifyTo(device, frame, audioSignalChar, audioSignalSubscribers)
        }
        // 4-byte FRAG frame header + 4-byte chunk header ride inside the budget.
        val cap = budget - FRAME_HEADER_LEN - 4
        if (cap <= 0) {
            Log.w(TAG, "sendAudioSignal: budget $budget too small to fragment; dropping")
            return false
        }
        val total = (encoded.size + cap - 1) / cap
        if (total > 0xFFFF) return false
        for (idx in 0 until total) {
            val start = idx * cap
            val end = minOf(start + cap, encoded.size)
            val payload = ByteArray(4 + (end - start))
            payload[0] = ((total ushr 8) and 0xFF).toByte()
            payload[1] = (total and 0xFF).toByte()
            payload[2] = ((idx ushr 8) and 0xFF).toByte()
            payload[3] = (idx and 0xFF).toByte()
            encoded.copyInto(payload, 4, start, end)
            if (!notifyTo(device, Frame(FrameType.FRAG, 0x00, payload),
                    audioSignalChar, audioSignalSubscribers)
            ) {
                return false
            }
            // No pacing sleep here any more: notifyTo now returns only once
            // the stack has acked the previous fragment, which is the real
            // condition the old 10 ms guess was standing in for.
        }
        Log.i(TAG, "sendAudioSignal: fragmented ${encoded.size}B frame into $total FRAGs (budget $budget)")
        return true
    }

    /** Register the Noise transport cipher pair (from the IK reconnect
     *  handshake) for a trusted peer.
     *
     *  - [sendCipher] AEAD-seals outbound NOTIFY frames (Linux receives
     *    them via subscribe on `AUDIO_SIGNAL`).
     *  - [recvCipher] AEAD-opens inbound WRITE frames (Linux writes
     *    them on `AUDIO_SIGNAL`, see [onCharacteristicWriteRequest]
     *    branch for `AUDIO_SIGNAL_UUID`). This is the reverse channel
     *    Linux uses when the LAN session isn't open.
     *
     *  Replaces any previously registered ciphers for the same peer
     *  (a fresh IK supersedes the old transport state). */
    fun registerAudioSession(
        peerStaticPub: ByteArray,
        device: BluetoothDevice,
        sendCipher: CipherState,
        recvCipher: CipherState,
    ) {
        val peerHex = peerStaticPub.toHex()
        peerToDevice[peerHex] = device
        audioSendCiphers[device.address] = sendCipher
        audioRecvCiphers[device.address] = recvCipher
        audioRecvNonce[device.address] = 0L // fresh handshake → nonce starts at 0
        deviceToPeerPub[device.address] = peerStaticPub.copyOf()
        Log.i(TAG, "registered audio session for peer=${peerHex.take(8)}… device=${device.address}")
    }

    /** Drop the audio session for a peer (call on un-trust). Safe to
     *  call repeatedly — the maps tolerate missing keys. */
    fun forgetAudioSession(peerStaticPub: ByteArray) {
        val peerHex = peerStaticPub.toHex()
        val device = peerToDevice.remove(peerHex) ?: return
        audioSendCiphers.remove(device.address)
        audioRecvCiphers.remove(device.address)
        audioRecvNonce.remove(device.address)
        deviceToPeerPub.remove(device.address)
        Log.i(TAG, "forgot audio session for peer=${peerHex.take(8)}…")
    }

    /** AEAD-seal [plain] with the SEND cipher registered for [peerStaticPub]
     *  (via [registerAudioSession]) and push it as a Frame([frameType]) on
     *  the AUDIO_SIGNAL characteristic. The single send path behind every
     *  `send*Encrypted` method — the frames differ only in type byte.
     *
     *  Returns true ONLY when the BLE notify was accepted by the OS for
     *  delivery. Any earlier-path failure (no device, no cipher, peer hasn't
     *  subscribed, cipher superseded, AEAD seal error, notify rejected by
     *  stack) returns false so the caller's LAN fallback / retry fires.
     *  Previously we returned true unconditionally past the encrypt step,
     *  which silently dropped the fast path (ChatGPT #3).
     *
     *  Cipher mutation + notify run inside a `synchronized(cipher)` block:
     *  AEAD `encryptWithAd` advances the cipher's internal nonce counter,
     *  and a concurrent second call would burn the same nonce — a
     *  catastrophic AEAD failure (key recovery on some constructions).
     *  Additionally, holding the lock until after the notify keeps the
     *  on-wire frame order matching the encryption order, so the peer's
     *  replay-protection nonce check doesn't reject the older frame when
     *  the newer one overtakes it.
     *
     *  Inside the lock we re-check the registered cipher is still THIS one:
     *  a fresh IK reconnect may have superseded the session while we waited
     *  (registerAudioSession replaces unconditionally), and sealing with the
     *  old cipher would emit a frame the peer can no longer open.
     *
     *  [logTag] names the caller in logs; [verbose] adds the miss-path
     *  warnings the audio-op fast path needs for LAN-fallback debugging;
     *  [logSuccess] logs delivered frames (off for bulk chunk streams).
     *  Payload content is never logged. */
    private fun sealAndNotify(
        peerStaticPub: ByteArray,
        frameType: Byte,
        plain: ByteArray,
        logTag: String,
        logSuccess: Boolean = false,
        verbose: Boolean = false,
    ): Boolean {
        val peerHex = peerStaticPub.toHex()
        val device = peerToDevice[peerHex] ?: run {
            if (verbose) Log.w(TAG, "$logTag: no device for peer=${peerHex.take(8)}…")
            return false
        }
        val cipher = audioSendCiphers[device.address] ?: run {
            if (verbose) Log.w(TAG, "$logTag: no cipher for device=${device.address}")
            return false
        }
        // No subscriber yet → bail out before paying the encrypt cost.
        val subscribed = synchronized(audioSignalSubscribers) { device in audioSignalSubscribers }
        if (!subscribed) {
            if (verbose) Log.w(TAG, "$logTag: ${device.address} hasn't subscribed to AUDIO_SIGNAL yet")
            return false
        }
        val notifyOk = synchronized(cipher) {
            if (audioSendCiphers[device.address] !== cipher) {
                Log.w(TAG, "$logTag: send cipher superseded by a fresh IK; dropping frame")
                return false
            }
            val ct = ByteArray(plain.size + cipher.macLength)
            val n = try {
                cipher.encryptWithAd(null, plain, 0, ct, 0, plain.size)
            } catch (e: Exception) {
                Log.e(TAG, "$logTag: AEAD seal failed", e)
                return false
            }
            sendAudioSignal(device, Frame(frameType, 0x00, ct.copyOf(n)))
        }
        if (notifyOk) {
            if (logSuccess) Log.i(TAG, "$logTag: notified ${device.address}")
        } else if (verbose) {
            Log.w(TAG, "$logTag: notifyCharacteristicChanged refused for ${device.address}; caller should LAN-fallback")
        }
        return notifyOk
    }

    /** Earbuds-switch op (AUDIO_OP 0x32) — the hand-off fast path; verbose
     *  logging so a LAN fallback is diagnosable. */
    fun sendAudioOpEncrypted(peerStaticPub: ByteArray, audioOpJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.AUDIO_OP, audioOpJson, "sendAudioOpEncrypted", logSuccess = true, verbose = true)

    /** App-state push (STATE 0x33): battery/charging → peer's state handler. */
    fun sendStateEncrypted(peerStaticPub: ByteArray, stateJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.STATE, stateJson, "sendStateEncrypted", logSuccess = true)

    /** Mirrored notification (NOTIFICATION 0x34) → peer's desktop display. */
    fun sendNotificationEncrypted(peerStaticPub: ByteArray, notifJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.NOTIFICATION, notifJson, "sendNotificationEncrypted", logSuccess = true)

    /** One notes/todos sync chunk (NOTES_SYNC 0x4D): `[total][idx][data]` of the
     *  full item set. Bidirectional LWW sync; see NoteSync. */
    fun sendNotesSyncEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.NOTES_SYNC, chunkPayload, "sendNotesSync")

    /** Clipboard sync (CLIPBOARD 0x40) → peer's system clipboard. */
    fun sendClipboardEncrypted(peerStaticPub: ByteArray, clipJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CLIPBOARD, clipJson, "sendClipboardEncrypted", logSuccess = true)

    /** One clipboard-image PNG chunk (CLIPBOARD_IMAGE 0x41): `[total][idx][data]`. */
    fun sendClipboardImageChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CLIPBOARD_IMAGE, chunkPayload, "sendClipboardImageChunk")

    /** One LONG clipboard-text chunk (CLIPBOARD_TEXT 0x43): `[total][idx][utf8]`. */
    fun sendClipboardTextChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CLIPBOARD_TEXT, chunkPayload, "sendClipboardTextChunk")

    /** "Wi-Fi Direct ready" signal (WIFI_DIRECT_OFFER 0x46): `{ssid, pass}` JSON. */
    fun sendWifiDirectOfferEncrypted(peerStaticPub: ByteArray, offerJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.WIFI_DIRECT_OFFER, offerJson, "sendWifiDirectOffer", logSuccess = true)

    /** Small "image available, pull over LAN" signal (CLIPBOARD_IMAGE_OFFER 0x42). */
    fun sendClipboardImageOfferEncrypted(peerStaticPub: ByteArray, offerJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CLIPBOARD_IMAGE_OFFER, offerJson, "sendClipboardImageOffer", logSuccess = true)

    /** Live activity (LIVE_ACTIVITY 0x35) → peer's top-bar pill. No fallback:
     *  these update frequently, the next tick re-syncs. */
    fun sendLiveActivityEncrypted(peerStaticPub: ByteArray, liveJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.LIVE_ACTIVITY, liveJson, "sendLiveActivityEncrypted", logSuccess = true)

    /** Phone-call event (CALL 0x37) → peer's call banner + in-call pill. */
    fun sendCallEncrypted(peerStaticPub: ByteArray, callJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CALL, callJson, "sendCallEncrypted", logSuccess = true)

    /** Browsing handoff (HANDOFF 0x4C) → laptop opens the page / shows a pill. */
    fun sendHandoffEncrypted(peerStaticPub: ByteArray, handoffJson: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.HANDOFF, handoffJson, "sendHandoffEncrypted", logSuccess = true)

    /** One app-icon PNG chunk (ICON 0x36): `[idLen][id][total][idx][data]`;
     *  peer reassembles + caches so mirrored notifications show real logos. */
    fun sendIconChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.ICON, chunkPayload, "sendIconChunkEncrypted")

    /** One contacts-list chunk (CONTACTS 0x39): `[total][idx][json-chunk]`. */
    fun sendContactsChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CONTACTS, chunkPayload, "sendContactsChunkEncrypted")

    /** One recent-calls chunk (CALL_LOG 0x3A): `[total][idx][json-chunk]`. */
    fun sendCallLogChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.CALL_LOG, chunkPayload, "sendCallLogChunkEncrypted")

    /** One recent-SMS chunk (SMS 0x3B): `[total][idx][json-chunk]`. */
    fun sendSmsChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.SMS, chunkPayload, "sendSmsChunkEncrypted")

    /** One conversation-page chunk (SMS_THREAD 0x3C, on-demand infinite
     *  scroll) — same wire shape as SMS but the laptop MERGES it into the
     *  open thread instead of replacing the recent list. */
    fun sendSmsThreadChunkEncrypted(peerStaticPub: ByteArray, chunkPayload: ByteArray): Boolean =
        sealAndNotify(peerStaticPub, FrameType.SMS_THREAD, chunkPayload, "sendSmsThreadChunkEncrypted")

    private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }

    /** Snapshot of devices currently subscribed to Audio Signal. Caller
     *  iterates a copy; the underlying set is concurrently mutated as
     *  peers (un)subscribe so a direct iteration would risk
     *  ConcurrentModificationException. */
    fun audioSignalSubscriberSnapshot(): List<BluetoothDevice> =
        synchronized(audioSignalSubscribers) { audioSignalSubscribers.toList() }

    /** One per connected device: serialises notifies and holds the result of
     *  the one currently in flight. */
    private class SendGate {
        val lock = Object()
        /** A notify is queued with the stack and `onNotificationSent` has
         *  not landed yet. */
        var pending = false
        /** `BluetoothGatt.GATT_*` from the last `onNotificationSent`. */
        var status = BluetoothGatt.GATT_FAILURE
    }

    private val sendGates =
        java.util.concurrent.ConcurrentHashMap<String, SendGate>()

    /** How long to wait for `onNotificationSent` before calling a notify lost.
     *  A healthy link acks within a connection interval or two (single-digit
     *  ms up to ~50 ms); this only has to be far enough above that to not trip
     *  on congestion, and low enough that a wedged link fails over to LAN
     *  while the data is still worth sending. */
    private val notifyAckTimeoutMs = 1500L

    /** Release a device's gate — a dropped link never delivers the ack the
     *  sender is blocked on. */
    private fun releaseSendGate(addr: String) {
        val gate = sendGates.remove(addr) ?: return
        synchronized(gate.lock) {
            gate.pending = false
            gate.status = BluetoothGatt.GATT_FAILURE
            gate.lock.notifyAll()
        }
    }

    /** Send one notification and WAIT for the stack to confirm it went out.
     *
     *  Android's contract is one notification in flight per device: the next
     *  `notifyCharacteristicChanged` must not be issued until
     *  `onNotificationSent` has fired. We previously never implemented that
     *  callback and paced bursts with `Thread.sleep(10)` instead — a guess
     *  that holds on an idle link and fails exactly when it matters (contacts,
     *  SMS, icon and notes bursts), where the stack silently drops notifies.
     *
     *  Silently is the damaging part: [sealAndNotify] has already advanced the
     *  Noise send nonce by then, so the laptop's receive nonce falls behind and
     *  a large enough gap drops the session and forces a fresh handshake.
     *
     *  Blocking here is also what makes the return value honest. On API 29-32
     *  `notifyCharacteristicChanged` returns true as soon as the binder call
     *  succeeds — it says nothing about delivery — so callers that fall back to
     *  the outbox or LAN were reading a value that could not tell them what
     *  they were asking. Now `true` means the stack reported the notify sent.
     *
     *  Callers are always on background threads (the send paths already slept
     *  here), and the wait is bounded by [notifyAckTimeoutMs]. */
    private fun notifyTo(
        device: BluetoothDevice,
        frame: Frame,
        char: BluetoothGattCharacteristic?,
        subscribers: MutableSet<BluetoothDevice>,
    ): Boolean {
        val s = server ?: return false
        val c = char ?: return false
        // Drop if the target hasn't enabled notifications. Android's
        // stack would no-op anyway, but the explicit check catches
        // logic bugs (replying to a write from a peer that never
        // subscribed to the characteristic).
        val subscribed = synchronized(subscribers) { device in subscribers }
        if (!subscribed) {
            Log.w(TAG, "skipped notify to ${device.address}: not subscribed to ${c.uuid}")
            return false
        }
        c.value = frame.encode()

        val gate = sendGates.getOrPut(device.address) { SendGate() }
        // Held across the notify AND the wait, so concurrent senders to the
        // same device queue up here instead of racing into the stack. The
        // callback thread only needs the monitor briefly, and `wait()` below
        // has already released it by then.
        synchronized(gate.lock) {
            gate.pending = true
            gate.status = BluetoothGatt.GATT_FAILURE

            val queued = try {
                // false = the OS refused outright (busy stack, no GATT link).
                // No callback follows, so don't wait for one.
                @Suppress("DEPRECATION")
                s.notifyCharacteristicChanged(device, c, /*confirm=*/false)
            } catch (e: SecurityException) {
                Log.w(TAG, "notify threw for ${device.address}: ${e.message}")
                false
            }
            if (!queued) {
                gate.pending = false
                return false
            }

            val deadline = android.os.SystemClock.elapsedRealtime() + notifyAckTimeoutMs
            while (gate.pending) {
                val left = deadline - android.os.SystemClock.elapsedRealtime()
                if (left <= 0L) break
                try {
                    gate.lock.wait(left)
                } catch (_: InterruptedException) {
                    Thread.currentThread().interrupt()
                    break
                }
            }

            if (gate.pending) {
                // No ack in time. Report failure so the caller queues to the
                // outbox or falls back to LAN rather than assuming delivery.
                gate.pending = false
                Log.w(TAG, "notify to ${device.address} not acked in ${notifyAckTimeoutMs}ms")
                return false
            }
            if (gate.status != BluetoothGatt.GATT_SUCCESS) {
                Log.w(TAG, "notify to ${device.address} failed: status=${gate.status}")
                return false
            }
            return true
        }
    }

    fun isRunning(): Boolean = server != null

    /** Live GATT connections (by address) + when the last one dropped.
     *  Drives the advertiser's reconnect-seeking (LOW_LATENCY) window.
     *  Seeded "just disconnected" at construction so a fresh process
     *  (app restart while the laptop is trying to connect) boosts too. */
    private val connectedAddrs =
        java.util.concurrent.ConcurrentHashMap.newKeySet<String>()

    @Volatile
    var lastDisconnectAtMs: Long = android.os.SystemClock.elapsedRealtime()
        private set

    fun hasActiveConnection(): Boolean = connectedAddrs.isNotEmpty()

    private val callback = object : BluetoothGattServerCallback() {
        override fun onMtuChanged(device: BluetoothDevice?, mtu: Int) {
            // Track the negotiated ATT MTU per device: the notify payload
            // budget is mtu−3, and the stack silently TRUNCATES anything
            // longer — sendAudioSignal fragments oversized frames against
            // this. BlueZ centrals request 512+ right after connecting.
            val addr = device?.address ?: return
            deviceMtu[addr] = mtu
            Log.i(TAG, "ATT MTU for $addr → $mtu (notify budget ${mtu - 3})")
        }

        /** The ack [notifyTo] blocks on. Without this override the stack has
         *  no way to tell us a notify completed, which is why bursts used to
         *  be paced by a sleep and dropped under load. */
        override fun onNotificationSent(device: BluetoothDevice?, status: Int) {
            val addr = device?.address ?: return
            val gate = sendGates[addr] ?: return
            synchronized(gate.lock) {
                gate.status = status
                gate.pending = false
                gate.lock.notifyAll()
            }
        }

        override fun onConnectionStateChange(device: BluetoothDevice?, status: Int, newState: Int) {
            val state = when (newState) {
                BluetoothProfile.STATE_CONNECTED -> "CONNECTED"
                BluetoothProfile.STATE_DISCONNECTED -> "DISCONNECTED"
                else -> "STATE_$newState"
            }
            Log.i(TAG, "GATT $state device=${device?.address ?: "?"} status=$status")
            if (newState == BluetoothProfile.STATE_CONNECTED && device != null) {
                connectedAddrs.add(device.address)
            }
            if (newState == BluetoothProfile.STATE_DISCONNECTED && device != null) {
                connectedAddrs.remove(device.address)
                if (connectedAddrs.isEmpty()) {
                    lastDisconnectAtMs = android.os.SystemClock.elapsedRealtime()
                }
                // Wake anyone blocked waiting for an ack that can no longer
                // arrive, and drop the gate so the next link starts clean.
                releaseSendGate(device.address)
                // Drop any half-buffered long-write so a reconnect starts clean.
                prepWriteBuf.remove(device.address)
                prepWriteChar.remove(device.address)
                // MTU is per-connection — a reconnect renegotiates it. Keeping
                // the old value could over-estimate the notify budget and
                // reintroduce silent truncation on the fresh link.
                deviceMtu.remove(device.address)
                // A dropped link can never finish its in-flight handshake —
                // clear the per-address state so a retry on the same address
                // starts clean instead of being rejected by the in-flight
                // guard for up to the 30s timeout sweep. (Completed pairing
                // state is preserved; reconnect state is dead either way.)
                pairingOrchestrator?.forgetDeviceOnDisconnect(device)
                reconnectOrchestrator?.forgetDevice(device)
                try { onPeerDisconnected(device) } catch (e: Exception) {
                    Log.w(TAG, "onPeerDisconnected hook threw: ${e.message}")
                }
            }
        }

        override fun onCharacteristicReadRequest(
            device: BluetoothDevice?,
            requestId: Int,
            offset: Int,
            characteristic: BluetoothGattCharacteristic?,
        ) {
            val s = server ?: return
            val uuid = characteristic?.uuid
            val payload: ByteArray? = when (uuid) {
                Ble.CAPABILITY_UUID -> capabilityResponse
                else -> {
                    Log.w(TAG, "unexpected READ on $uuid")
                    null
                }
            }
            try {
                if (payload != null) {
                    val slice = if (offset >= payload.size) ByteArray(0)
                    else payload.copyOfRange(offset, payload.size)
                    s.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, offset, slice)
                } else {
                    s.sendResponse(
                        device, requestId, BluetoothGatt.GATT_REQUEST_NOT_SUPPORTED, 0, null,
                    )
                }
            } catch (e: SecurityException) {
                Log.w(TAG, "sendResponse threw: ${e.message}")
            }
        }

        override fun onCharacteristicWriteRequest(
            device: BluetoothDevice?,
            requestId: Int,
            characteristic: BluetoothGattCharacteristic?,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val s = server ?: return
            val uuid = characteristic?.uuid
            val payload = value ?: ByteArray(0)

            // Long-write (prepared) slice: buffer it and wait for the EXECUTE.
            // BlueZ fragments any laptop→phone frame > ATT_MTU-3 this way; the
            // ATT spec requires the prepare response to echo back value+offset.
            if (preparedWrite) {
                val addr = device?.address
                if (addr != null && uuid != null) {
                    prepWriteBuf.getOrPut(addr) { java.io.ByteArrayOutputStream() }.write(payload)
                    prepWriteChar[addr] = uuid
                }
                if (responseNeeded) {
                    try {
                        s.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, offset, payload)
                    } catch (e: SecurityException) {
                        Log.w(TAG, "sendResponse(prepare) threw: ${e.message}")
                    }
                }
                return
            }

            // ACK the (single-PDU) write before processing.
            if (responseNeeded) {
                try {
                    s.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, offset, payload)
                } catch (e: SecurityException) {
                    Log.w(TAG, "sendResponse threw: ${e.message}")
                }
            }
            dispatchWrite(device, uuid, payload)
        }

        // EXECUTE_WRITE: the laptop finished a long-write. Assemble the buffered
        // PREPARE slices and run the normal dispatch once. Always ACK the execute
        // (the ATT Execute-Write expects a response before the link is reused —
        // a missing/failed one is what stalled then dropped the BLE session).
        override fun onExecuteWrite(device: BluetoothDevice?, requestId: Int, execute: Boolean) {
            val s = server ?: return
            val addr = device?.address
            val assembled = addr?.let { prepWriteBuf.remove(it) }?.toByteArray()
            val uuid = addr?.let { prepWriteChar.remove(it) }
            try {
                s.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, null)
            } catch (e: SecurityException) {
                Log.w(TAG, "sendResponse(execute) threw: ${e.message}")
            }
            if (execute && assembled != null && assembled.isNotEmpty()) {
                dispatchWrite(device, uuid, assembled)
            }
        }

        /** Decode + route one complete laptop→phone write payload — either a
         *  single-PDU write or the reassembled bytes of a long-write. */
        private fun dispatchWrite(device: BluetoothDevice?, uuid: java.util.UUID?, payload: ByteArray) {
            val frame = Frame.decode(payload).getOrNull()
            if (frame == null) {
                val now = System.currentTimeMillis()
                if (now - lastDecodeWarnMs >= decodeWarnIntervalMs) {
                    lastDecodeWarnMs = now
                    Log.w(TAG, "frame decode failed for write of ${payload.size} bytes on $uuid")
                }
                return
            }

            when (uuid) {
                Ble.PAIRING_CONTROL_UUID -> {
                    Log.i(TAG, "PairingControl WRITE ${payload.size} bytes")
                    val orchestrator = pairingOrchestrator
                    if (orchestrator == null || device == null) {
                        // No pairing window open — drop the frame.
                        // Previously this branch fell through to a
                        // Phase 5b "echo back" smoke path, but that
                        // gave any nearby attacker a free probe to
                        // detect we are a Vortex device when the
                        // legitimate pairing window is closed.
                        Log.w(TAG, "PairingControl write rejected: no orchestrator wired")
                        return
                    }
                    val out = orchestrator.onPairingControlFrame(device, frame)
                    if (out != null) sendPairingControl(device, out)
                }

                Ble.RECONNECT_CONTROL_UUID -> {
                    Log.i(TAG, "ReconnectControl WRITE ${payload.size} bytes")
                    val orchestrator = reconnectOrchestrator
                    if (orchestrator == null || device == null) {
                        Log.w(TAG, "no reconnect orchestrator wired; dropping frame")
                        return
                    }
                    val out = orchestrator.onReconnectFrame(device, frame)
                    if (out != null) notifyReconnectTo(device, out)
                }

                Ble.AUDIO_SIGNAL_UUID -> {
                    // Laptop's BLE-WRITE reverse channel (ChatGPT
                    // review #4): peer encrypts an AudioOpFrame with
                    // its SEND cipher and writes it here; we AEAD-open
                    // with the matching RECV cipher and dispatch
                    // through the orchestrator's onIncoming.
                    val addr = device?.address ?: return
                    val peerPub = deviceToPeerPub[addr] ?: run {
                        Log.w(TAG, "AudioSignal WRITE: no peer-pub for $addr; drop")
                        return
                    }
                    val cipher = audioRecvCiphers[addr] ?: run {
                        Log.w(TAG, "AudioSignal WRITE: no recv cipher for $addr; drop")
                        return
                    }
                    if (frame.type != FrameType.AUDIO_OP &&
                        frame.type != FrameType.NOTIFICATION &&
                        frame.type != FrameType.STATE &&
                        frame.type != FrameType.CALL_CONTROL &&
                        frame.type != FrameType.CLIPBOARD &&
                        frame.type != FrameType.CLIPBOARD_IMAGE &&
                        frame.type != FrameType.CLIPBOARD_TEXT &&
                        frame.type != FrameType.NOTES_SYNC
                    ) {
                        Log.w(TAG, "AudioSignal WRITE: unexpected frame type ${frame.type}")
                        return
                    }
                    // AEAD-open with a nonce-RESYNC window. A laptop→phone BLE
                    // write can be DROPPED before it hits the air under a call's
                    // 2.4 GHz contention, leaving our recv nonce behind the
                    // laptop's send nonce. We TRACK the expected nonce (noise-java
                    // has setNonce but no getter), and on a failed open skip
                    // ahead up to NONCE_RESYNC_WINDOW, retrying — resyncing past
                    // the dropped frame(s) WITHOUT a re-handshake. setNonce each
                    // attempt makes us independent of the lib's failure-advance
                    // behaviour. This is what makes call control reliable BLE-only.
                    val plain = ByteArray(frame.payload.size)
                    val baseNonce = audioRecvNonce[addr] ?: 0L
                    var n = -1
                    var usedNonce = baseNonce
                    for (skip in 0..NONCE_RESYNC_WINDOW) {
                        cipher.setNonce(baseNonce + skip)
                        try {
                            n = cipher.decryptWithAd(null, frame.payload, 0, plain, 0, frame.payload.size)
                            usedNonce = baseNonce + skip
                            break
                        } catch (_: Exception) {
                            n = -1
                        }
                    }
                    if (n < 0) {
                        // No nonce in the window opened it — restore + count
                        // toward a desync disconnect (genuine corruption / >window loss).
                        cipher.setNonce(baseNonce)
                        val fails = (recvAeadFails[addr] ?: 0) + 1
                        recvAeadFails[addr] = fails
                        Log.w(TAG, "AudioSignal WRITE: AEAD open failed (#$fails); drop")
                        if (fails >= 3) {
                            Log.w(TAG, "AudioSignal WRITE: recv cipher desynced — disconnecting to force re-handshake")
                            recvAeadFails.remove(addr)
                            audioRecvNonce.remove(addr)
                            // Retire the CIPHER too, not just the nonce.
                            //
                            // Dropping the nonce alone makes the next write
                            // start again from 0 while the same cipher stays
                            // registered — so nonces 0..128 of this session
                            // become acceptable a second time. If cancelConnection
                            // does not take, or a write lands before the re-IK,
                            // that is a replay window: a captured frame — a
                            // CALL_CONTROL "accept", say — would open cleanly.
                            // The next IK installs a fresh pair anyway, so
                            // there is nothing to lose by removing it now.
                            audioRecvCiphers.remove(addr)
                            try { device?.let { server?.cancelConnection(it) } } catch (_: Exception) {}
                        }
                        return
                    }
                    if (usedNonce > baseNonce) {
                        Log.w(TAG, "AudioSignal WRITE: resynced past ${usedNonce - baseNonce} dropped frame(s) (no re-handshake)")
                    }
                    audioRecvNonce[addr] = usedNonce + 1 // advance past the frame we opened
                    recvAeadFails[addr] = 0 // success → reset the desync counter
                    val jsonBytes = plain.copyOf(n)
                    when (frame.type) {
                        FrameType.AUDIO_OP -> {
                            Log.i(TAG, "AudioSignal WRITE: dispatching $n bytes from peer=${peerPub.toHex().take(8)}…")
                            try {
                                onAudioOpReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onAudioOpReceived threw: ${e.message}")
                            }
                        }
                        FrameType.NOTIFICATION -> {
                            // Laptop→phone mirrored desktop notification.
                            // Content not logged (privacy).
                            try {
                                onNotificationReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onNotificationReceived threw: ${e.message}")
                            }
                        }
                        FrameType.CLIPBOARD -> {
                            // Laptop→phone clipboard sync. Content not logged.
                            try {
                                onClipboardReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onClipboardReceived threw: ${e.message}")
                            }
                        }
                        FrameType.CLIPBOARD_IMAGE -> {
                            // Laptop→phone clipboard image chunk.
                            try {
                                onClipboardImageChunk(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onClipboardImageChunk threw: ${e.message}")
                            }
                        }
                        FrameType.CLIPBOARD_TEXT -> {
                            // Laptop→phone LONG clipboard text chunk. Not logged.
                            try {
                                onClipboardTextChunk(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onClipboardTextChunk threw: ${e.message}")
                            }
                        }
                        FrameType.STATE -> {
                            // Laptop→phone app-state push (battery/charging) so
                            // the phone shows the laptop connected over BLE.
                            try {
                                onStateReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onStateReceived threw: ${e.message}")
                            }
                        }
                        FrameType.CALL_CONTROL -> {
                            // Laptop→phone call control (Accept/Decline/End/Mute
                            // from the call banner). Action logged, number not.
                            try {
                                onCallControlReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onCallControlReceived threw: ${e.message}")
                            }
                        }
                        FrameType.NOTES_SYNC -> {
                            // Laptop→phone notes/todos sync chunk → reassemble +
                            // LWW-merge into the local store (see NoteSync).
                            try {
                                onNotesSyncReceived(peerPub, jsonBytes)
                            } catch (e: Exception) {
                                Log.w(TAG, "onNotesSyncReceived threw: ${e.message}")
                            }
                        }
                    }
                }

                else -> {
                    Log.w(TAG, "WRITE on unexpected $uuid — ignored")
                }
            }
        }

        override fun onDescriptorWriteRequest(
            device: BluetoothDevice?,
            requestId: Int,
            descriptor: BluetoothGattDescriptor?,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val s = server ?: return
            if (descriptor?.uuid == cccUuid && device != null) {
                val enabled = value?.contentEquals(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE) == true
                val charUuid = descriptor.characteristic.uuid
                val targetSet = when (charUuid) {
                    Ble.PAIRING_CONTROL_UUID -> pairingSubscribers
                    Ble.RECONNECT_CONTROL_UUID -> reconnectSubscribers
                    Ble.AUDIO_SIGNAL_UUID -> audioSignalSubscribers
                    else -> null
                }
                if (targetSet != null) {
                    if (enabled) targetSet.add(device) else targetSet.remove(device)
                    Log.i(TAG, "${device.address} ${if (enabled) "subscribed to" else "unsubscribed from"} $charUuid")
                    // BLE notify path just became deliverable → let the stack
                    // flush anything buffered while it was down.
                    if (enabled && charUuid == Ble.AUDIO_SIGNAL_UUID) {
                        try { onAudioSignalSubscribed(device) } catch (e: Exception) {
                            Log.w(TAG, "onAudioSignalSubscribed hook threw: ${e.message}")
                        }
                    }
                }
            }
            if (responseNeeded) {
                try {
                    s.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, offset, null)
                } catch (e: SecurityException) {
                    Log.w(TAG, "sendResponse threw: ${e.message}")
                }
            }
        }
    }

    companion object {
        private const val TAG = "VortexGattSrv"
        /** How far to skip the recv nonce forward when an AUDIO_SIGNAL open
         *  fails, to resync past dropped BLE writes without a re-handshake
         *  (mirrors the laptop daemon's NONCE_RESYNC_WINDOW). */
        private const val NONCE_RESYNC_WINDOW = 128
    }
}
