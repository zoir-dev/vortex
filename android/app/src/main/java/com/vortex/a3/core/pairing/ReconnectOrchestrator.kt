package com.vortex.a3.core.pairing

import android.bluetooth.BluetoothDevice
import android.util.Log
import com.southernstorm.noise.protocol.CipherState
import com.southernstorm.noise.protocol.CipherStatePair
import com.southernstorm.noise.protocol.HandshakeState
import com.southernstorm.noise.protocol.Noise
import com.vortex.a3.core.ble.Frame
import com.vortex.a3.core.ble.FrameSub
import com.vortex.a3.core.ble.FrameType
import com.vortex.a3.core.crypto.NoiseRunner
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.status.DeviceStatusReader
import com.vortex.a3.core.status.LocalStatus
import com.vortex.a3.core.status.PeerStatus
import com.vortex.a3.core.storage.PeerStore
import java.util.concurrent.ConcurrentHashMap

/**
 * Per-device Noise IK **responder** state machine for trusted reconnect
 * (spec §7).
 *
 * The responder accepts an incoming IK msg1, looks up the peer's static
 * public key in the [PeerStore], rejects if untrusted, and otherwise
 * produces msg2. After IK completes the orchestrator handles ping/pong
 * liveness probes (frame `0x30/0x01` → `0x30/0x02`) for that device.
 */
class ReconnectOrchestrator(
    private val identity: IdentityRecord,
    private val peerStore: PeerStore,
) {

    data class ReconnectOutcome(
        val device: BluetoothDevice,
        val peerStaticPub: ByteArray,
        val transcriptHash: ByteArray,
        /**
         * Noise transport-mode cipher pair derived from the IK
         * handshake. The responder's `sender` cipher is what we use to
         * AEAD-seal outgoing AUDIO_SIGNAL notifications; the `receiver`
         * cipher would decrypt anything the initiator sends back over
         * the same channel.
         *
         * This is intentionally created from `handshake.split()` only —
         * the protocol bytes on the wire are unchanged. We just stop
         * discarding the cipher state at the very end of the handshake.
         */
        val ciphers: CipherStatePair,
    )

    /** Per-peer status snapshot emitted whenever a ping arrives. */
    data class PeerStatusEvent(
        val peerStaticPub: ByteArray,
        val status: PeerStatus,
    )

    /**
     * Provider for our own status. Set by VortexService so the
     * orchestrator can emit a fresh battery reading in every pong.
     */
    var localStatusProvider: () -> LocalStatus = {
        LocalStatus(
            battery = com.vortex.a3.core.status.BatteryPct(null),
            deviceClass = com.vortex.a3.core.status.DeviceClass.PHONE,
        )
    }

    // CopyOnWriteArrayList: lock-free iteration during emit, safe to
    // add() concurrently. Same rationale as `listeners` below.
    private val statusListeners: MutableList<(PeerStatusEvent) -> Unit> =
        java.util.concurrent.CopyOnWriteArrayList()
    fun addStatusListener(listener: (PeerStatusEvent) -> Unit) {
        statusListeners.add(listener)
    }
    private fun emitStatus(event: PeerStatusEvent) {
        for (l in statusListeners) l(event)
    }

    private sealed class IkState {
        object Idle : IkState()
        class Established(
            val peerStaticPub: ByteArray,
            val transcriptHash: ByteArray,
        ) : IkState()
    }

    private val states: MutableMap<String, IkState> = ConcurrentHashMap()
    // CopyOnWriteArrayList instead of an ad-hoc synchronized list so
    // iteration during dispatch never blocks add/remove, and adding a
    // listener mid-dispatch is safe (the in-flight iteration uses its
    // own snapshot). This pairs with the ConcurrentHashMap on `states`
    // — both are lock-free for the read path, so a forgetDevice() call
    // racing with onReconnectFrame() can't leave the listener loop
    // walking a recycled IkState object.
    private val listeners: MutableList<(ReconnectOutcome) -> Unit> =
        java.util.concurrent.CopyOnWriteArrayList()

    fun addListener(listener: (ReconnectOutcome) -> Unit) {
        listeners.add(listener)
    }

    fun forgetDevice(device: BluetoothDevice) {
        states.remove(device.address)
    }

    /** Process an incoming Reconnect Control frame; return the response to notify, or null. */
    fun onReconnectFrame(device: BluetoothDevice, frame: Frame): Frame? {
        return when (frame.type) {
            FrameType.RECONNECT_HANDSHAKE -> {
                if (frame.sub == HANDSHAKE_MSG1) handleIkMsg1(device, frame.payload) else null
            }
            FrameType.TRANSPORT_KEEPALIVE -> {
                if (frame.sub == FrameSub.PING) handlePing(device, frame.payload) else null
            }
            else -> {
                Log.w(TAG, "unexpected frame type=0x${"%02x".format(frame.type)} on Reconnect Control")
                null
            }
        }
    }

    private fun handleIkMsg1(device: BluetoothDevice, payload: ByteArray): Frame? {
        Log.i(TAG, "IK msg1 from ${device.address}: ${payload.size} bytes")

        // PRS-bound reconnect (H5): the initiator mixes the peer's PRS
        // into the IK prologue, so we must do the same here. We don't
        // yet know which trusted peer is reaching out — try each one's
        // PRS as the prologue until readMessage succeeds. For V1 the
        // typical trust count is 1-2 so the linear scan is cheap;
        // every wrong PRS fails fast (AEAD verify ≈ microseconds).
        val trustedList = try { peerStore.list() } catch (_: Exception) { emptyList() }
        if (trustedList.isEmpty()) {
            Log.w(TAG, "no trusted peers — rejecting IK")
            return null
        }

        var matched: Triple<HandshakeState, ByteArray, Long>? = null
        for (peer in trustedList) {
            val candidate = try {
                buildResponder(peer.prs)
            } catch (e: Exception) {
                Log.e(TAG, "IK responder build failed for peer ${peer.peerStaticPub.toHex()}", e)
                continue
            }
            val readBuf = ByteArray(Noise.MAX_PACKET_LEN)
            val ptLen = try {
                candidate.readMessage(payload, 0, payload.size, readBuf, 0)
            } catch (_: Exception) {
                // Wrong PRS — AEAD verify of `s` decryption fails. Try
                // the next trusted peer.
                candidate.destroy()
                -1
            }
            if (ptLen >= 0) {
                val pub = ByteArray(32).also { candidate.remotePublicKey.getPublicKey(it, 0) }
                // Final consistency check: the initiator's static_pub
                // (recovered from the decrypted `s` token) must match
                // the peer whose PRS we tried. If anything is off we
                // refuse — should never happen under honest peers.
                if (!pub.contentEquals(peer.peerStaticPub)) {
                    Log.w(TAG, "IK static/PRS mismatch (race?); rejecting")
                    candidate.destroy()
                    continue
                }
                // M6: peer counter from msg1 payload.
                val peerCounter: Long = if (ptLen >= 8) {
                    java.nio.ByteBuffer.wrap(readBuf, 0, 8)
                        .order(java.nio.ByteOrder.BIG_ENDIAN)
                        .long
                } else 0L
                matched = Triple(candidate, pub, peerCounter)
                break
            }
        }

        if (matched == null) {
            Log.w(TAG, "no trusted PRS accepted msg1; rejecting reconnect")
            return null
        }
        val (handshake, peerStaticPub, peerCounter) = matched
        val localCounter = peerStore.loadCounter(peerStaticPub)
        if (peerCounter < localCounter) {
            Log.w(
                TAG,
                "possible trust rollback for ${peerStaticPub.toHexPrefix()}: " +
                    "peer=$peerCounter local=$localCounter",
            )
        }
        val nextCounter = peerStore.bumpCounter(peerStaticPub, peerCounter)

        // Produce msg2. Payload is our updated counter so the peer
        // can compare and detect their side's potential rollback.
        val counterPayload = java.nio.ByteBuffer.allocate(8)
            .order(java.nio.ByteOrder.BIG_ENDIAN)
            .putLong(nextCounter)
            .array()
        val writeBuf = ByteArray(Noise.MAX_PACKET_LEN)
        val n = try {
            handshake.writeMessage(writeBuf, 0, counterPayload, 0, counterPayload.size)
        } catch (e: Exception) {
            Log.e(TAG, "IK msg2 write failed", e)
            handshake.destroy() // zero key material on the error path
            return null
        }

        val transcript = handshake.handshakeHash.copyOf()
        // Promote handshake → transport. The wire bytes for this peer
        // are unchanged (msg1/msg2 already exchanged); we just retain
        // the cipher pair that `split()` derives at the end. Initiator
        // and responder agree on the directionality, so our `sender`
        // matches the initiator's `receiver` (and vice versa).
        val ciphers: CipherStatePair = try {
            handshake.split()
        } catch (e: Exception) {
            Log.e(TAG, "split() failed", e)
            handshake.destroy() // zero key material on the error path
            return null
        }
        // split() copied what the transport ciphers need; the handshake's own
        // static-priv copy + ephemeral are dead now — zero them rather than
        // leaving them for GC (which never wipes).
        handshake.destroy()
        Log.i(TAG, "✅ IK complete for ${device.address}")
        // Redaction (spec §3.5 T-LOC-3): log only short prefixes of public
        // protocol values so field logs cannot be used to correlate stable
        // identities across sessions.
        Log.i(TAG, "   peer_static_pub = ${peerStaticPub.toHexPrefix()}")
        Log.i(TAG, "   transcript_hash = ${transcript.toHexPrefix()}")

        // IK proved this address really is this peer, so it is safe to record
        // (before IK we would only be trusting a presence-token match). This
        // also backfills pairings made before the address was persisted at
        // pair time, so Forget can clean their bonds without re-pairing.
        runCatching { peerStore.savePeerBtAddr(peerStaticPub, device.address) }
            .onSuccess {
                // Prefix only, per the redaction rule above: enough to confirm
                // which laptop was recorded, not enough to correlate.
                Log.i(TAG, "   peer_bt_addr    = ${device.address.take(8)}…")
            }
            .onFailure { Log.w(TAG, "could not persist peer BT addr: ${it.message}") }

        states[device.address] = IkState.Established(peerStaticPub, transcript)
        // CopyOnWriteArrayList — iteration is lock-free over a snapshot,
        // safe to interleave with add()/forgetDevice() on other threads.
        for (l in listeners) l(ReconnectOutcome(device, peerStaticPub, transcript, ciphers))
        return Frame(FrameType.RECONNECT_HANDSHAKE, HANDSHAKE_MSG2, writeBuf.copyOf(n))
    }

    private fun handlePing(device: BluetoothDevice, nonce: ByteArray): Frame? {
        val state = states[device.address]
        if (state !is IkState.Established) {
            Log.w(TAG, "ping in unexpected state: $state")
            return null
        }
        Log.i(TAG, "ping from ${device.address} (${nonce.size} bytes); responding")
        return Frame(FrameType.TRANSPORT_KEEPALIVE, FrameSub.PONG, nonce.copyOf())
    }

    /**
     * Build a fresh IK responder whose prologue mixes in [prs]. Caller
     * iterates over candidate trusted peers and tries each PRS in turn.
     */
    private fun buildResponder(prs: ByteArray): HandshakeState {
        require(prs.size == 32) { "PRS must be 32 bytes" }
        val prologue = ByteArray(NoiseRunner.PROLOGUE_IK.size + 32)
        System.arraycopy(NoiseRunner.PROLOGUE_IK, 0, prologue, 0, NoiseRunner.PROLOGUE_IK.size)
        System.arraycopy(prs, 0, prologue, NoiseRunner.PROLOGUE_IK.size, 32)
        val state = HandshakeState(NoiseRunner.NOISE_IK, HandshakeState.RESPONDER)
        state.setPrologue(prologue, 0, prologue.size)
        state.localKeyPair.setPrivateKey(identity.staticPriv, 0)
        state.start()
        return state
    }

    companion object {
        private const val TAG = "VortexReconnect"
        const val HANDSHAKE_MSG1: Byte = 0x01
        const val HANDSHAKE_MSG2: Byte = 0x02

        private fun ByteArray.toHex(): String =
            joinToString("") { "%02x".format(it) }

        /** Short prefix only, suitable for diagnostic logs (spec §3.5). */
        private fun ByteArray.toHexPrefix(): String =
            take(4).joinToString("") { "%02x".format(it) } + "…"
    }
}
