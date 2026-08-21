package com.vortex.a3.core.lan

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.util.Log
import com.southernstorm.noise.protocol.CipherState
import com.southernstorm.noise.protocol.CipherStatePair
import com.southernstorm.noise.protocol.HandshakeState
import com.southernstorm.noise.protocol.Noise
import com.vortex.a3.core.ble.FRAME_HEADER_LEN
import com.vortex.a3.core.ble.Frame
import com.vortex.a3.core.ble.FrameSub
import com.vortex.a3.core.ble.FrameType
import com.vortex.a3.core.ble.MAX_FRAME_PAYLOAD
import com.vortex.a3.core.crypto.NoiseRunner
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.storage.PeerStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.ServerSocket
import java.net.Socket

/**
 * LAN runtime per spec §8 and §9.2.
 *
 * Publishes Vortex mDNS via Android NSD and accepts inbound TCP
 * connections, running Noise IK against each connecting peer (validated
 * against [PeerStore]).
 *
 * Two mDNS lifecycles, picked by [LanServerMode] at [start]:
 *
 *   - [LanServerMode.PairingWindow] — publishes `_vortex-pair._tcp.local.`
 *     while the user is actively pairing. Instance matches the BLE
 *     `payload_8` so a discoverer can correlate the BLE and mDNS
 *     advertisements as the same device.
 *
 *   - [LanServerMode.TrustedRuntime] — publishes `_vortex._tcp.local.`
 *     once trust exists. Instance is the current presence-token bucket
 *     so passive observers cannot link sightings across rotations.
 */
sealed class LanServerMode {
    /** Active pairing window. `instanceId` MUST be the same 8 bytes
     *  used as `AdvPayload.payload_8` for the BLE advertisement so a
     *  peer scanning both transports can correlate them. */
    data class PairingWindow(val instanceId: ByteArray) : LanServerMode() {
        init { require(instanceId.size == 8) { "instanceId must be 8 bytes" } }
        override fun equals(other: Any?): Boolean =
            other is PairingWindow && instanceId.contentEquals(other.instanceId)
        override fun hashCode(): Int = instanceId.contentHashCode()
    }

    /** Trusted-runtime — published once at least one trusted peer
     *  exists. Instance name is the current presence-token bucket. */
    object TrustedRuntime : LanServerMode()
}

class LanServer(
    private val context: Context,
    private val identity: IdentityRecord,
    private val peerStore: PeerStore,
) {

    /**
     * Provider for the local AppState snapshot sent to peers after the
     * handshake. Wired by VortexService so the orchestrator can pull a
     * fresh battery reading on every session.
     */
    var localAppStateProvider: () -> com.vortex.a3.core.appstate.AppState = {
        com.vortex.a3.core.appstate.AppState(
            deviceClass = com.vortex.a3.core.appstate.DeviceClass.PHONE,
        )
    }

    /**
     * Callback fired whenever a peer sends us its AppState. The UI
     * subscribes here to render battery / locale / earbuds info.
     */
    var onPeerAppState: (ByteArray, com.vortex.a3.core.appstate.AppState) -> Unit = { _, _ -> }

    /**
     * LAN bulk-sync provider (BULK_SYNC 0x3D). Called with a dataset key
     * ("contacts"; later "call_log"/"sms") and the peer's cached-content
     * sha256-hex. Returns the full JSON + its hash when the peer is stale,
     * or null when the hashes match (nothing to send). Wired by VortexStack.
     */
    var bulkProvider: (key: String, peerHash: String) -> Pair<ByteArray, String>? = { _, _ -> null }

    /** Fired AFTER a dataset reached the peer over this socket ("sent": all
     *  chunk writes completed; "match": the peer already had this hash) so
     *  the stack can gate the redundant BLE burst. */
    var onBulkDelivered: (key: String, hash: String) -> Unit = { _, _ -> }

    /** Fired after an instant-share FILE blob has been written to the peer,
     *  with the content token it pulled by. Closes the loop the outgoing-offer
     *  watchdog waits on: an offer is only really done once the laptop has the
     *  bytes. */
    var onFileServed: (token: String) -> Unit = { }

    /**
     * Watermark-dataset provider for bulk-sync (currently `sms_history`):
     * called with the peer's "I have everything up to [sinceMs]" watermark,
     * returns the JSON of everything NEWER (oldest-first, provider-capped)
     * or null when the peer is already caught up. Wired by VortexStack.
     */
    var historyProvider: (key: String, sinceMs: Long) -> ByteArray? = { _, _ -> null }

    /**
     * Screen-mirror handoff: when a connection's FIRST post-IK frame is a
     * SCREEN_MIRROR START, `handleClient` calls this with the live socket +
     * cipher pair + IK handshake hash + that first frame. VortexStack wires it
     * to build a [MirrorSession] (capture/encode → UDP video). Null = mirroring
     * unavailable (the connection is then dropped).
     */
    @Volatile
    var mirrorHandler: ((
        Socket, DataInputStream, DataOutputStream, CipherStatePair, ByteArray, Frame,
    ) -> Unit)? = null

    /**
     * Force the NSD service to re-announce itself with a fresh instance
     * tag. Used as an event-based wake signal: whenever the user
     * changes locale / theme / earbuds, calling this nudges the
     * laptop's long-lived mDNS browser into firing a ServiceResolved
     * event, which fires `mDNS wake-up` on the other side and
     * triggers an immediate reconnect — so a locale change propagates
     * in roughly a round-trip instead of the 12s heartbeat cycle.
     */
    fun nudge() {
        val listener = registrationListener ?: return
        val nsd = context.getSystemService(NsdManager::class.java) ?: return
        try {
            nsd.unregisterService(listener)
        } catch (_: Exception) { /* listener already torn down — fine */ }
        registrationListener = null
        // Re-announce after a tiny delay so the unregister completes;
        // we use a worker thread to avoid blocking the caller.
        scope.launch {
            kotlinx.coroutines.delay(150)
            reannounce()
        }
    }

    private suspend fun reannounce() {
        val socket = serverSocket ?: return
        val nsd = context.getSystemService(NsdManager::class.java) ?: return
        val info = NsdServiceInfo().apply {
            serviceName = derivePrivateInstanceName()
            serviceType = currentServiceType()
            this.port = socket.localPort
        }
        val l = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(i: NsdServiceInfo) {
                Log.i(TAG, "NSD re-registered: ${i.serviceName}")
            }
            override fun onRegistrationFailed(i: NsdServiceInfo, code: Int) {
                Log.e(TAG, "NSD re-register failed: $code")
            }
            override fun onServiceUnregistered(i: NsdServiceInfo) {}
            override fun onUnregistrationFailed(i: NsdServiceInfo, code: Int) {}
        }
        try {
            nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, l)
            registrationListener = l
        } catch (e: Exception) {
            Log.w(TAG, "NSD re-register threw: ${e.message}")
        }
    }
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var serverSocket: ServerSocket? = null
    private var registrationListener: NsdManager.RegistrationListener? = null
    private var acceptJob: Job? = null
    private var multicastLock: WifiManager.MulticastLock? = null

    /** High-throughput Wi-Fi lock held ONLY while serving a file/image blob.
     *  Wi-Fi power-save parks the radio between packets → huge RTT → throughput
     *  collapses to ~one chunk per round-trip. LOW_LATENCY keeps the radio hot
     *  for the (brief) transfer, then releases to spare the battery. */
    private var perfLock: WifiManager.WifiLock? = null

    private fun acquirePerfLock() {
        if (perfLock?.isHeld == true) return
        try {
            val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            val mode = if (android.os.Build.VERSION.SDK_INT >= 29) {
                WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            } else {
                @Suppress("DEPRECATION") WifiManager.WIFI_MODE_FULL_HIGH_PERF
            }
            perfLock = wifi.createWifiLock(mode, "vortex:file-xfer").apply { acquire() }
        } catch (_: Exception) {}
    }

    private fun releasePerfLock() {
        // A batch window ([keepLanHot]) owns the lock for its whole duration —
        // the per-blob `finally` must not drop it between the laptop's rounds,
        // which is exactly what let the radio park mid-batch.
        if (lanHotJob?.isActive == true) return
        try { if (perfLock?.isHeld == true) perfLock?.release() } catch (_: Exception) {}
        perfLock = null
    }

    /** Runs for the duration of the current "a file pull is imminent" window. */
    @Volatile
    private var lanHotJob: Job? = null

    /** Bumped per window so a superseded expiry (the scope is multi-threaded:
     *  an old timer can resume just as a new window opens) can't release the
     *  locks the new window is holding. */
    @Volatile
    private var lanHotGen: Int = 0

    /** Whether the persistent BLE link is up, per [setBleLinked] — decides
     *  whether a closing hot window hands the multicast lock back or keeps it. */
    @Volatile
    private var bleLinked: Boolean = false

    /**
     * A phone→laptop file pull is imminent: file offers just went out over BLE
     * and the laptop will dial us, ONE file per heartbeat round. Keep the LAN
     * path hot for [ms] so those rounds land:
     *
     *  - hold the throughput Wi-Fi lock, so the radio doesn't park between
     *    rounds and answer the laptop's cold TCP probe too late,
     *  - hold the multicast lock and re-announce, so the laptop can find our
     *    CURRENT address. While BLE is up we release that lock and answer no
     *    mDNS, which leaves the laptop's cached IP as its only guess — and a
     *    DHCP renew since the last successful handshake makes it a dead one.
     *
     * Repeat calls extend the window; only the first one re-announces. Returns
     * true when this call OPENED the window, so a caller can pair it with a
     * one-per-batch action (the BLE AppState push) instead of a per-file one.
     */
    fun keepLanHot(ms: Long = HOT_WINDOW_MS): Boolean {
        val first = lanHotJob?.isActive != true
        acquirePerfLock()
        acquireMulticast()
        if (first) {
            nudge()
            Log.i(TAG, "LAN hot: radio + mDNS held for an incoming file pull")
        }
        val gen = ++lanHotGen
        lanHotJob?.cancel()
        lanHotJob = scope.launch {
            kotlinx.coroutines.delay(ms)
            if (gen != lanHotGen) return@launch
            // Clear before releasing: [releasePerfLock] refuses to drop the
            // lock while a window is live, and this one is over.
            lanHotJob = null
            releasePerfLock()
            if (bleLinked) releaseMulticast()
            Log.i(TAG, "LAN hot window over (no file pull for ${ms}ms)")
        }
        return first
    }

    /** Bound concurrent client handlers so a slow-loris attacker cannot
     *  pin every coroutine + socket FD on the device. */
    private val clientSlots = Semaphore(MAX_CONCURRENT_CLIENTS)

    /** Current mDNS publication mode. Set by [start] and read by
     *  [reannounce] / [derivePrivateInstanceName] to keep the service
     *  type stable across re-announces. */
    @Volatile
    private var mode: LanServerMode = LanServerMode.TrustedRuntime

    fun start(mode: LanServerMode = LanServerMode.TrustedRuntime): Boolean {
        if (acceptJob != null) {
            Log.w(TAG, "already started")
            return true
        }
        this.mode = mode
        acquireMulticast()
        // Bind + register on a worker thread — Android forbids socket
        // creation on the main thread (EPERM on some OEMs).
        acceptJob = scope.launch { startInternal() }
        return true
    }

    private suspend fun startInternal() {
        val socket = try {
            ServerSocket(DEFAULT_PORT)
        } catch (e: Exception) {
            try {
                ServerSocket(0)
            } catch (e2: Exception) {
                Log.e(TAG, "failed to bind ServerSocket", e2)
                return
            }
        }
        val port = socket.localPort
        serverSocket = socket
        Log.i(TAG, "TCP listener on port $port")

        // Register NSD service from the worker thread too.
        //
        // Privacy: the instance name is derived from a trusted-peer
        // PRS + the current rotation bucket so two sightings of the
        // same device across networks do not present the same stable
        // "vortex-android" name. If no trust exists yet, we publish a
        // neutral random instance — no OS hint either way.
        val nsd = context.getSystemService(NsdManager::class.java)
        val instanceName = derivePrivateInstanceName()
        val serviceInfo = NsdServiceInfo().apply {
            serviceName = instanceName
            serviceType = currentServiceType()
            this.port = port
        }
        val listener = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) {
                Log.i(TAG, "NSD registered: ${info.serviceName} (${info.serviceType}) on port ${info.port}")
            }
            override fun onRegistrationFailed(info: NsdServiceInfo, code: Int) {
                Log.e(TAG, "NSD register failed: $code")
            }
            override fun onServiceUnregistered(info: NsdServiceInfo) {
                Log.i(TAG, "NSD unregistered")
            }
            override fun onUnregistrationFailed(info: NsdServiceInfo, code: Int) {
                Log.w(TAG, "NSD unregister failed: $code")
            }
        }
        try {
            nsd.registerService(serviceInfo, NsdManager.PROTOCOL_DNS_SD, listener)
            registrationListener = listener
        } catch (e: Exception) {
            Log.e(TAG, "NSD registerService threw", e)
        }

        acceptLoop(socket)
    }

    // MulticastLock — without this, Android optimises away multicast
    // (including mDNS) when the Wi-Fi radio sleeps or the app goes into
    // background. Symptom: NSD service announcement is silently dropped,
    // peers can't find us via mDNS. Required for the background-stress
    // 100% success rate.
    private fun acquireMulticast() {
        if (multicastLock?.isHeld == true) return
        try {
            val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            multicastLock = wifi.createMulticastLock(TAG).apply {
                setReferenceCounted(false)
                acquire()
            }
            Log.i(TAG, "multicast lock acquired")
        } catch (e: Exception) {
            Log.w(TAG, "could not acquire multicast lock: ${e.message}")
        }
    }

    private fun releaseMulticast() {
        val lock = multicastLock ?: return
        try { if (lock.isHeld) lock.release() } catch (_: Exception) {}
        multicastLock = null
        Log.i(TAG, "multicast lock released")
    }

    /**
     * Battery: the multicast lock keeps the Wi-Fi radio awake so we can
     * ANSWER mDNS queries — i.e. it only matters for discovery. While the
     * BLE session is alive the laptop already reaches us directly (BLE
     * fast-path + cached IP heartbeat), so holding the lock buys nothing
     * and is the single biggest Wi-Fi battery cost. Release it while BLE
     * is up; re-acquire the moment BLE drops and mDNS matters again.
     */
    fun setBleLinked(linked: Boolean) {
        bleLinked = linked
        if (linked) {
            // Exception: a file pull in flight ([keepLanHot]) needs mDNS to
            // answer, because the laptop's cached IP may be a dead lease and
            // an unanswered browse leaves it nothing else to dial.
            if (lanHotJob?.isActive != true) releaseMulticast()
        } else if (acceptJob != null) {
            acquireMulticast()
        }
    }

    fun stop() {
        acceptJob?.cancel()
        acceptJob = null
        registrationListener?.let {
            try {
                context.getSystemService(NsdManager::class.java).unregisterService(it)
            } catch (e: Exception) {
                Log.w(TAG, "NSD unregister threw: ${e.message}")
            }
        }
        registrationListener = null
        try { serverSocket?.close() } catch (_: Exception) {}
        serverSocket = null
        multicastLock?.let {
            try { if (it.isHeld) it.release() } catch (_: Exception) {}
        }
        multicastLock = null
        Log.i(TAG, "LAN server stopped")
    }

    private suspend fun acceptLoop(socket: ServerSocket) {
        while (scope.isActive) {
            val client = try {
                withContext(Dispatchers.IO) { socket.accept() }
            } catch (e: Exception) {
                if (scope.isActive) Log.w(TAG, "accept threw: ${e.message}")
                return
            }
            // Disable Nagle: file chunks are large frames sent back-to-back; with
            // Nagle on, each small write waited on the peer's delayed ACK and
            // throughput collapsed (~640 KB/s). NODELAY ⇒ full LAN speed.
            try { client.tcpNoDelay = true } catch (_: Exception) {}
            // Big send buffer so many chunks stay in flight — on a high-RTT
            // Wi-Fi link (power-save), a small window caps throughput at
            // ~one chunk per round-trip. 4 MB fills the bandwidth-delay product.
            try { client.sendBufferSize = 4 * 1024 * 1024 } catch (_: Exception) {}
            Log.i(TAG, "TCP accept from ${client.inetAddress.hostAddress}:${client.port}")
            // Reject early if we already have MAX_CONCURRENT_CLIENTS
            // pending handshakes. Better to drop one new connection than
            // run the FD table dry under a slow-loris flood.
            if (!clientSlots.tryAcquire()) {
                Log.w(TAG, "client limit reached; dropping ${client.inetAddress.hostAddress}")
                try { client.close() } catch (_: Exception) {}
                continue
            }
            scope.launch {
                try {
                    handleClient(client)
                } finally {
                    clientSlots.release()
                }
            }
        }
    }

    private suspend fun handleClient(client: Socket) {
        try {
            // Bound the handshake phase: a peer that never sends bytes
            // will close after the soTimeout fires (slow-loris defense).
            try { client.soTimeout = HANDSHAKE_TIMEOUT_MS } catch (_: Exception) {}
            client.use { sock ->
                val input = DataInputStream(sock.getInputStream())
                val output = DataOutputStream(sock.getOutputStream())

                // Read frame 1 — must be IK msg1.
                val msg1 = readFrame(input) ?: return
                if (msg1.type != FrameType.RECONNECT_HANDSHAKE
                    || msg1.sub != HANDSHAKE_MSG1) {
                    Log.w(TAG, "first frame not IK msg1: type=0x${"%02x".format(msg1.type)}")
                    return
                }

                // PRS-bound reconnect (H5): try each trusted peer's
                // PRS as the IK prologue until msg1 decrypts. See
                // ReconnectOrchestrator.handleIkMsg1 for rationale.
                val trustedList = try { peerStore.list() } catch (_: Exception) { emptyList() }
                if (trustedList.isEmpty()) {
                    Log.w(TAG, "no trusted peers — rejecting IK")
                    return
                }
                var handshake: HandshakeState? = null
                var peerPub: ByteArray? = null
                var trusted: com.vortex.a3.core.storage.TrustedPeer? = null
                var peerCounter: Long = 0L
                for (peer in trustedList) {
                    val candidate = HandshakeState(NoiseRunner.NOISE_IK, HandshakeState.RESPONDER)
                    val prologue = ByteArray(NoiseRunner.PROLOGUE_IK.size + 32)
                    System.arraycopy(NoiseRunner.PROLOGUE_IK, 0, prologue, 0,
                        NoiseRunner.PROLOGUE_IK.size)
                    System.arraycopy(peer.prs, 0, prologue, NoiseRunner.PROLOGUE_IK.size, 32)
                    candidate.setPrologue(prologue, 0, prologue.size)
                    candidate.localKeyPair.setPrivateKey(identity.staticPriv, 0)
                    candidate.start()
                    val readBuf = ByteArray(Noise.MAX_PACKET_LEN)
                    val ptLen = try {
                        candidate.readMessage(msg1.payload, 0, msg1.payload.size, readBuf, 0)
                    } catch (_: Exception) {
                        candidate.destroy()
                        -1
                    }
                    if (ptLen >= 0) {
                        val pub = ByteArray(32).also { candidate.remotePublicKey.getPublicKey(it, 0) }
                        if (pub.contentEquals(peer.peerStaticPub)) {
                            handshake = candidate
                            peerPub = pub
                            trusted = peer
                            if (ptLen >= 8) {
                                peerCounter = java.nio.ByteBuffer.wrap(readBuf, 0, 8)
                                    .order(java.nio.ByteOrder.BIG_ENDIAN)
                                    .long
                            }
                            break
                        } else {
                            candidate.destroy()
                        }
                    }
                }
                if (handshake == null || peerPub == null || trusted == null) {
                    Log.w(TAG, "no trusted PRS accepted msg1; closing")
                    return
                }
                val localCounter = peerStore.loadCounter(peerPub)
                if (peerCounter < localCounter) {
                    Log.w(
                        TAG,
                        "possible trust rollback over LAN: peer=$peerCounter local=$localCounter",
                    )
                }
                val nextCounter = peerStore.bumpCounter(peerPub, peerCounter)

                val writeBuf = ByteArray(Noise.MAX_PACKET_LEN)
                val counterPayload = java.nio.ByteBuffer.allocate(8)
                    .order(java.nio.ByteOrder.BIG_ENDIAN)
                    .putLong(nextCounter)
                    .array()
                val n = handshake.writeMessage(
                    writeBuf, 0, counterPayload, 0, counterPayload.size,
                )
                writeFrame(output, Frame(FrameType.RECONNECT_HANDSHAKE, HANDSHAKE_MSG2, writeBuf.copyOf(n)))
                // Redaction (spec §3.5): log only the transcript prefix.
                val transcriptHash = handshake.handshakeHash.copyOf()
                Log.i(TAG, "✅ IK over TCP complete; transcript=${transcriptHash.toHexPrefix()}")

                // Split into transport ciphers so all post-handshake
                // frames below are AEAD-authenticated.
                //
                // **Nonce-ordering invariant:** the `pair` cipher set
                // has a monotonically-increasing nonce per direction.
                // Every aeadSeal / aeadOpen consumes one nonce.
                // The on-the-wire frame sequence is fixed:
                //   1. ping (in) → pong (out)  (responder side)
                //   2. app-state (in) → app-state (out)
                // The Linux initiator in `l3/.../tcp_client.rs` MUST
                // step through this exact same order or the nonces
                // desynchronize and the next AEAD verify fails. New
                // frame types must be appended (not interleaved).
                val pair: CipherStatePair = handshake.split()
                // split() copied what the transport ciphers need; zero the
                // handshake's static-priv copy + ephemeral now (GC never wipes).
                handshake.destroy()

                // After IK completes we're talking to a known peer.
                // Switch to a longer idle timeout so legitimate clients
                // can hold the connection open between ping bursts but
                // an abandoned socket still gets reaped.
                try { sock.soTimeout = IDLE_TIMEOUT_MS } catch (_: Exception) {}

                // Capture peerPub into a final non-null local so the
                // session-writer registration (and its tear-down in
                // `finally`) can reference it without re-checking the
                // smart-cast nullable across try boundaries.
                val peerPubFinal: ByteArray = peerPub

                // Single per-connection writer lock.
                //
                // Two correctness invariants ride on this lock:
                //
                //  1. `pair.sender.encryptWithAd` is NOT thread-safe.
                //     The Noise cipher advances its internal nonce on
                //     each seal, and two parallel callers would burn
                //     the same nonce — a catastrophic AEAD failure
                //     (key recovery on some constructions). The audio
                //     writer used to be locked here; the read-loop's
                //     APP_DATA response branch did its own
                //     unsynchronised `aeadSeal(pair.sender, …)` →
                //     racing with the audio writer if they fired
                //     close together. ChatGPT review #7.
                //
                //  2. `DataOutputStream.write*` is not atomic per
                //     frame. Two concurrent `writeFrame` calls would
                //     interleave bytes on the TCP socket and corrupt
                //     the frame header on the peer's side.
                //
                // Every outbound frame on this socket must go through
                // `lockedWrite` (plain frames) or `lockedSealAndWrite`
                // (AEAD-sealed frames). The audio writer below uses
                // these too, so audio + app-state + keepalive all
                // serialise through one lock.
                val outLock = java.util.concurrent.locks.ReentrantLock()
                fun lockedWrite(frame: Frame) {
                    outLock.lock()
                    try { writeFrame(output, frame) } finally { outLock.unlock() }
                }
                fun lockedSealAndWrite(type: Byte, sub: Byte, plaintext: ByteArray) {
                    outLock.lock()
                    try {
                        val ct = aeadSeal(pair.sender, plaintext)
                        writeFrame(output, Frame(type, sub, ct))
                    } finally { outLock.unlock() }
                }

                val writer: suspend (com.vortex.a3.core.earbuds.AudioOpFrame) -> Result<Unit> =
                    { outFrame ->
                        try {
                            lockedSealAndWrite(FrameType.AUDIO_OP, 0x01, outFrame.toJsonBytes())
                            Result.success(Unit)
                        } catch (e: Exception) {
                            Log.w(TAG, "audio-op session write failed: ${e.message}")
                            Result.failure(e)
                        }
                    }

                // Screen-mirror: a SCREEN_MIRROR START as the FIRST post-IK
                // frame means a dedicated video session — hand the socket +
                // cipher pair off to the mirror handler (wired by VortexStack)
                // and skip the normal control loop. Otherwise the frame is the
                // opening PING; feed it into the loop below via `pending`.
                val firstFrame = readFrame(input)
                val mh = mirrorHandler
                if (firstFrame != null && mh != null &&
                    com.vortex.a3.core.lan.MirrorSession.isMirrorStart(firstFrame)
                ) {
                    mh(sock, input, output, pair, transcriptHash, firstFrame)
                    return
                }

                try {
                // Post-handshake loop:
                //   - PING → PONG (liveness; pairing protocol level)
                //   - TRANSPORT_APP_DATA (app-state sync; orthogonal)
                // V1 does not exchange a channel join proof — each
                // transport runs its own IK (spec §8.5).
                var pending: Frame? = firstFrame
                while (true) {
                    val frame = pending ?: readFrame(input) ?: break
                    pending = null
                    when {
                        frame.type == FrameType.TRANSPORT_KEEPALIVE
                            && frame.sub == FrameSub.PING -> {
                            Log.i(TAG, "ping (${frame.payload.size} bytes); responding")
                            lockedWrite(
                                Frame(FrameType.TRANSPORT_KEEPALIVE, FrameSub.PONG, frame.payload.copyOf()),
                            )
                        }
                        frame.type == FrameType.TRANSPORT_APP_DATA -> {
                            // Peer wants to share their app state (battery,
                            // locale, theme, earbuds, …). AEAD-decrypt and
                            // emit to the UI; then echo our own snapshot
                            // back so the peer can render us.
                            val plain = runCatching {
                                aeadOpen(pair.receiver, frame.payload)
                            }.getOrNull()
                            if (plain == null) {
                                Log.w(TAG, "app-state AEAD decrypt failed")
                                continue
                            }
                            val peerState = com.vortex.a3.core.appstate.AppState
                                .fromJsonBytes(plain)
                            if (peerState != null) {
                                val budsLog = peerState.earbuds
                                    ?.let { "earbuds=${it.name}(battery=${it.battery}, connected=${it.connected})" }
                                    ?: "earbuds=none"
                                Log.i(
                                    TAG,
                                    "← app-state from peer (battery=${peerState.battery} " +
                                        "class=${peerState.deviceClass} $budsLog)",
                                )
                                try {
                                    onPeerAppState(peerPub, peerState)
                                } catch (e: Exception) {
                                    Log.w(TAG, "onPeerAppState listener threw: ${e.message}")
                                }
                            } else {
                                Log.w(TAG, "app-state JSON parse failed")
                            }
                            // Reply with our own snapshot. Routed
                            // through `lockedSealAndWrite` so the
                            // `pair.sender.encryptWithAd` call
                            // serialises with the audio writer that
                            // may fire at the same moment (ChatGPT
                            // review #7).
                            val local = localAppStateProvider()
                            lockedSealAndWrite(
                                FrameType.TRANSPORT_APP_DATA,
                                0x01,
                                local.toJsonBytes(),
                            )
                            Log.i(TAG, "→ app-state sent")
                        }
                        frame.type == FrameType.BULK_SYNC -> {
                            // Laptop's bulk-sync request: per-dataset hashes of
                            // its on-disk caches. Ship full JSON only for stale
                            // ones (chunked over THIS reliable TCP socket — no
                            // BLE notify-loss risk), then a done frame with the
                            // per-dataset outcome. Zero bytes when nothing
                            // changed — the common case.
                            val plain = runCatching {
                                aeadOpen(pair.receiver, frame.payload)
                            }.getOrNull()
                            if (plain == null) {
                                Log.w(TAG, "bulk-sync AEAD decrypt failed")
                                continue
                            }
                            val req = runCatching {
                                org.json.JSONObject(String(plain, Charsets.UTF_8))
                            }.getOrNull()
                            if (req == null) {
                                Log.w(TAG, "bulk-sync request JSON parse failed")
                                continue
                            }
                            val status = org.json.JSONObject()
                            // Ship one dataset's JSON as chunked frames of [type].
                            fun sendChunked(frameType: Byte, json: ByteArray) {
                                val total = ((json.size + BULK_CHUNK - 1) / BULK_CHUNK).coerceAtLeast(1)
                                for (idx in 0 until total) {
                                    val s = idx * BULK_CHUNK
                                    val e = minOf(s + BULK_CHUNK, json.size)
                                    val payload = java.io.ByteArrayOutputStream().apply {
                                        write((total ushr 8) and 0xFF); write(total and 0xFF)
                                        write((idx ushr 8) and 0xFF); write(idx and 0xFF)
                                        write(json, s, e - s)
                                    }.toByteArray()
                                    lockedSealAndWrite(frameType, 0x00, payload)
                                }
                            }
                            for (key in req.keys()) {
                                // Clipboard image pull: the value is a token —
                                // serve the stashed PNG reliably over TCP.
                                if (key == "clipboard_image") {
                                    val token = req.optString(key, "")
                                    val png = com.vortex.a3.core.clipboard.ClipboardImageStore
                                        .getByToken(token)
                                    if (png == null) {
                                        Log.i(TAG, "bulk-sync: clipboard_image token=$token not found")
                                        status.put(key, "nomatch")
                                    } else {
                                        acquirePerfLock()
                                        try { sendChunked(FrameType.CLIPBOARD_IMAGE, png) }
                                        finally { releasePerfLock() }
                                        Log.i(TAG, "bulk-sync: clipboard_image sent (${png.size} bytes)")
                                        status.put(key, "sent")
                                    }
                                    continue
                                }
                                // Instant-share file pull: serve the stashed blob
                                // reliably over TCP as CLIPBOARD_FILE chunks.
                                if (key == "clipboard_file") {
                                    val token = req.optString(key, "")
                                    val blob = com.vortex.a3.core.clipboard.ClipboardBlobStore
                                        .getByToken(token)
                                    if (blob == null) {
                                        Log.i(TAG, "bulk-sync: clipboard_file token=$token not found")
                                        status.put(key, "nomatch")
                                    } else {
                                        // Extends the hot window: the laptop
                                        // comes back for the NEXT queued file
                                        // in a fresh round moments from now.
                                        keepLanHot()
                                        sendChunked(FrameType.CLIPBOARD_FILE, blob)
                                        Log.i(TAG, "bulk-sync: clipboard_file sent (${blob.size} bytes)")
                                        status.put(key, "sent")
                                        try { onFileServed(token) } catch (e: Exception) {
                                            Log.w(TAG, "onFileServed listener threw: ${e.message}")
                                        }
                                    }
                                    continue
                                }
                                // Watermark datasets: the value is "everything
                                // up to <ms>" rather than a content hash.
                                val historyFrameType = when (key) {
                                    "sms_history" -> FrameType.SMS_THREAD
                                    "call_log_history" -> FrameType.CALL_LOG_HISTORY
                                    else -> null
                                }
                                if (historyFrameType != null) {
                                    val since = req.optString(key, "").toLongOrNull() ?: 0L
                                    // A denied READ_SMS / READ_CALL_LOG throws here
                                    // (ContentResolver read). Catch it so ONE missing
                                    // permission can't kill the whole bulk-sync
                                    // connection (which left the laptop on stale data
                                    // with a repeating "early eof"): mark this dataset
                                    // errored and move on, still reaching the done frame.
                                    val json = try {
                                        historyProvider(key, since)
                                    } catch (e: Exception) {
                                        Log.w(TAG, "bulk-sync: $key history provider threw (permission denied?): ${e.message}")
                                        status.put(key, "error")
                                        continue
                                    }
                                    if (json == null) {
                                        Log.i(TAG, "bulk-sync: $key caught up (since=$since)")
                                        status.put(key, "match")
                                    } else {
                                        sendChunked(historyFrameType, json)
                                        Log.i(TAG, "bulk-sync: $key sent (${json.size} bytes since=$since)")
                                        status.put(key, "sent")
                                    }
                                    continue
                                }
                                val peerHash = req.optString(key, "")
                                val frameType = when (key) {
                                    "contacts" -> FrameType.CONTACTS
                                    "call_log" -> FrameType.CALL_LOG
                                    "sms" -> FrameType.SMS
                                    "sms_ids" -> FrameType.SMS_IDS
                                    else -> {
                                        Log.w(TAG, "bulk-sync: unknown dataset '$key'")
                                        status.put(key, "unknown")
                                        continue
                                    }
                                }
                                // Same guard as the history datasets: a provider that
                                // touches a denied ContentResolver (e.g. sms_ids →
                                // readAllIds needs READ_SMS) must not tear down the
                                // whole connection. Errored dataset is skipped; the
                                // rest still sync and the done frame still ships.
                                val data = try {
                                    bulkProvider(key, peerHash)
                                } catch (e: Exception) {
                                    Log.w(TAG, "bulk-sync: $key provider threw (permission denied?): ${e.message}")
                                    status.put(key, "error")
                                    continue
                                }
                                if (data == null) {
                                    Log.i(TAG, "bulk-sync: $key matches peer cache; nothing to send")
                                    status.put(key, "match")
                                    onBulkDelivered(key, peerHash)
                                } else {
                                    val (json, hash) = data
                                    sendChunked(frameType, json)
                                    Log.i(TAG, "bulk-sync: $key sent (${json.size} bytes)")
                                    status.put(key, "sent")
                                    onBulkDelivered(key, hash)
                                }
                            }
                            lockedSealAndWrite(
                                FrameType.BULK_SYNC, 0x02,
                                status.toString().toByteArray(Charsets.UTF_8),
                            )
                        }
                        frame.type == FrameType.AUDIO_OP -> {
                            // Earbuds-switch frame (Phase 1). AEAD-decrypt
                            // the payload, decode the AudioOpFrame JSON,
                            // and dispatch to the orchestrator. Responses
                            // (Approve / Released) flow back on this same
                            // socket via the session writer registered
                            // just below — heartbeat-only connections
                            // don't reach this branch, so the slot stays
                            // claimed by whichever connection actually
                            // carries the switch flow.
                            val plain = runCatching {
                                aeadOpen(pair.receiver, frame.payload)
                            }.getOrNull()
                            if (plain == null) {
                                Log.w(TAG, "audio-op AEAD decrypt failed")
                                continue
                            }
                            val audioFrame = com.vortex.a3.core.earbuds.AudioOpFrame
                                .fromJsonBytes(plain)
                            if (audioFrame == null) {
                                Log.w(TAG, "audio-op JSON parse failed")
                                continue
                            }
                            Log.i(TAG, "← audio-op ${audioFrame.op} nonce=${audioFrame.nonce}")
                            // Claim the slot RIGHT before dispatch so the
                            // responder's outbound (Approve / Released)
                            // resolves. Idempotent: subsequent frames on
                            // the same connection re-set the same writer.
                            com.vortex.a3.core.earbuds.EarbudsSwitchHolder
                                .setSessionWriter(peerPubFinal, writer)
                            try {
                                com.vortex.a3.core.earbuds.EarbudsSwitchHolder
                                    .onIncoming(peerPub, audioFrame)
                            } catch (e: Exception) {
                                Log.w(TAG, "audio-op dispatch threw: ${e.message}")
                            }
                        }
                        frame.type == FrameType.FILE_PUSH_OFFER -> {
                            // Laptop → phone file PUSH batch (reverse-direction share). The
                            // offer lists every file {name,bytes}; we ask the user
                            // (ONE consent prompt), reply FILE_PUSH_DECISION (1 byte
                            // accept/decline), and on accept read each file's
                            // FILE_PUSH chunk stream in order (each self-delimited by
                            // its own [total][idx] header) and save to Downloads.
                            // Lock-step: offer(1) + decision(1) [+ chunks] keeps the
                            // cipher nonces in sync with the laptop.
                            val plain = runCatching {
                                aeadOpen(pair.receiver, frame.payload)
                            }.getOrNull()
                            if (plain == null) {
                                Log.w(TAG, "file-push offer AEAD decrypt failed")
                                continue
                            }
                            val names = ArrayList<String>()
                            // Parallel to names: true ⇒ the laptop zipped a folder for
                            // us, so auto-extract it back to a folder and drop the .zip.
                            val extracts = ArrayList<Boolean>()
                            var total = 0L
                            runCatching {
                                val obj = org.json.JSONObject(String(plain, Charsets.UTF_8))
                                total = obj.optLong("total", 0L)
                                val arr = obj.optJSONArray("files")
                                if (arr != null) {
                                    for (i in 0 until arr.length()) {
                                        val fo = arr.getJSONObject(i)
                                        names.add(fo.optString("name", "vortex-file"))
                                        extracts.add(fo.optBoolean("extract", false))
                                    }
                                } else {
                                    // Back-compat: single-file offer {name,bytes}.
                                    names.add(obj.optString("name", "vortex-file"))
                                    extracts.add(false)
                                    total = obj.optLong("bytes", 0L)
                                }
                            }
                            if (names.isEmpty()) {
                                Log.w(TAG, "file-push offer: empty/invalid")
                                continue
                            }
                            val label = if (names.size == 1) names[0] else "${names.size} files"
                            Log.i(TAG, "← file-push offer: $label ($total bytes); asking user")
                            val accepted = FileConsent.request(context, label, names.size, total)
                            // Reply with the decision (1 = accept, 0 = decline).
                            lockedSealAndWrite(
                                FrameType.FILE_PUSH_DECISION, 0x00,
                                byteArrayOf(if (accepted) 1 else 0),
                            )
                            if (!accepted) {
                                Log.i(TAG, "file-push declined")
                                continue
                            }
                            var saved = 0
                            var aborted = false
                            for ((fi, name) in names.withIndex()) {
                                if (aborted) break
                                val asm = com.vortex.a3.core.clipboard.ClipboardImageAssembler()
                                var fileBytes: ByteArray? = null
                                while (fileBytes == null) {
                                    val chunkFrame = readFrame(input)
                                    if (chunkFrame == null) {
                                        aborted = true
                                        break
                                    }
                                    if (chunkFrame.type != FrameType.FILE_PUSH) {
                                        Log.w(TAG, "file-push: unexpected frame 0x${"%02x".format(chunkFrame.type)}; aborting")
                                        pending = chunkFrame // re-dispatch in main loop
                                        aborted = true
                                        break
                                    }
                                    val cplain = runCatching {
                                        aeadOpen(pair.receiver, chunkFrame.payload)
                                    }.getOrNull()
                                    if (cplain == null) {
                                        aborted = true
                                        break
                                    }
                                    fileBytes = asm.add(cplain)
                                }
                                val received = fileBytes
                                val ok = when {
                                    received == null -> false
                                    // Folder we zipped → unpack back to a folder, drop the zip.
                                    extracts.getOrElse(fi) { false } ->
                                        IncomingFile.saveZipExtracted(context, name, received)
                                    else -> IncomingFile.save(context, name, received)
                                }
                                if (ok) {
                                    saved++
                                } else {
                                    Log.w(TAG, "file-push '$name' incomplete; discarded")
                                }
                            }
                            if (saved > 0) {
                                IncomingFile.notifyReceived(context, label, saved)
                            }
                        }
                        else -> Log.i(TAG, "post-IK frame type=0x${"%02x".format(frame.type)} ignored")
                    }
                }
                } finally {
                    // Clear the session writer on connection teardown,
                    // but only if THIS connection still owns the slot
                    // (CAS-style remove(key, value)). Otherwise we'd
                    // race a concurrent audio-flow connection out of
                    // its writer.
                    com.vortex.a3.core.earbuds.EarbudsSwitchHolder
                        .clearSessionWriter(peerPubFinal, writer)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "client handler error: ${e.message}")
        }
    }

    private fun readFrame(input: DataInputStream): Frame? {
        return try {
            val header = ByteArray(FRAME_HEADER_LEN)
            input.readFully(header)
            val length = ((header[2].toInt() and 0xFF) shl 8) or (header[3].toInt() and 0xFF)
            if (length > MAX_FRAME_PAYLOAD) return null
            val payload = ByteArray(length)
            if (length > 0) input.readFully(payload)
            val full = header + payload
            Frame.decode(full).getOrNull()
        } catch (e: Exception) {
            null
        }
    }

    private fun writeFrame(output: DataOutputStream, frame: Frame) {
        output.write(frame.encode())
        output.flush()
    }

    /** Pick the mDNS service type matching the current mode. */
    private fun currentServiceType(): String = when (mode) {
        is LanServerMode.PairingWindow -> NSD_SERVICE_TYPE_PAIRING
        LanServerMode.TrustedRuntime -> NSD_SERVICE_TYPE_TRUSTED
    }

    /**
     * Compute the per-window mDNS instance name.
     *
     * [LanServerMode.PairingWindow] — instance is the BLE `payload_8`
     * hex so a peer running a single discoverer can correlate the BLE
     * advertisement with this mDNS record as the same device.
     *
     * [LanServerMode.TrustedRuntime] — instance is the current presence-
     * token bucket derived from PRS. The peer can recognise the device
     * because they share PRS; passive observers cannot link sightings
     * across rotation windows.
     *
     * No "android" suffix in either path: OS hint would let an
     * observer profile the device fleet on a shared network.
     */
    private fun derivePrivateInstanceName(): String {
        return when (val m = mode) {
            is LanServerMode.PairingWindow -> "vortex-${m.instanceId.toHexShort()}"
            LanServerMode.TrustedRuntime -> {
                val peers = try { peerStore.list() } catch (_: Exception) { emptyList() }
                val prs = peers.firstOrNull()?.prs
                if (prs != null) {
                    val bucket = System.currentTimeMillis() / 1000L / NSD_ROTATION_SEC
                    val token = com.vortex.a3.core.crypto.Presence.deriveToken(prs, bucket)
                    "vortex-${token.toHexShort()}"
                } else {
                    // Trusted mode requested but no trust on file — fall
                    // back to a random nonce so we don't expose a stable
                    // identifier. Caller should have picked PairingWindow.
                    val nonce = ByteArray(8).also { java.security.SecureRandom().nextBytes(it) }
                    Log.w(TAG, "trusted-runtime mDNS without PRS; using random nonce")
                    "vortex-${nonce.toHexShort()}"
                }
            }
        }
    }

    private fun ByteArray.toHexShort(): String =
        joinToString("") { "%02x".format(it) }

    // ---- AEAD helpers for post-IK transport-mode frames ----

    private fun aeadSeal(cipher: CipherState, plaintext: ByteArray): ByteArray {
        val out = ByteArray(plaintext.size + cipher.macLength)
        val n = cipher.encryptWithAd(null, plaintext, 0, out, 0, plaintext.size)
        return out.copyOf(n)
    }

    private fun aeadOpen(cipher: CipherState, ciphertext: ByteArray): ByteArray {
        if (ciphertext.size < cipher.macLength) {
            throw IllegalArgumentException("ciphertext shorter than MAC")
        }
        val out = ByteArray(ciphertext.size)
        val n = cipher.decryptWithAd(null, ciphertext, 0, out, 0, ciphertext.size)
        return out.copyOf(n)
    }

    private fun ByteArray.toHex(): String =
        joinToString("") { "%02x".format(it) }

    /** Short prefix only, suitable for diagnostic logs (spec §3.5). */
    private fun ByteArray.toHexPrefix(): String =
        take(4).joinToString("") { "%02x".format(it) } + "…"

    companion object {
        private const val TAG = "VortexLan"
        const val DEFAULT_PORT: Int = 51820
        // Trusted-runtime mDNS — published only after trust exists.
        // NsdManager appends ".local." to both.
        const val NSD_SERVICE_TYPE_TRUSTED = "_vortex._tcp."
        // Pairing-window mDNS — published during a user-opened pairing
        // window so a passive observer can distinguish "I'm pairable
        // right now" from "I'm a trusted peer" without sniffing TXT.
        // Spec spec §5.4: instance matches BLE `payload_8`.
        const val NSD_SERVICE_TYPE_PAIRING = "_vortex-pair._tcp."
        const val NSD_INSTANCE_NAME = "vortex-android"
        const val HANDSHAKE_MSG1: Byte = 0x01
        const val HANDSHAKE_MSG2: Byte = 0x02
        /** Slow-loris bound during the handshake phase. */
        const val HANDSHAKE_TIMEOUT_MS: Int = 15_000
        /** Idle timeout for established, post-handshake sessions. */
        const val IDLE_TIMEOUT_MS: Int = 90_000
        /** Cap on concurrent client coroutines (handshake + idle). */
        const val MAX_CONCURRENT_CLIENTS: Int = 16

        /** How long [keepLanHot] keeps the radio + mDNS up after the last file
         *  offer or served blob. Generous on purpose: the laptop needs one
         *  heartbeat round PER queued file, and a round that misses its window
         *  costs far more battery in retries than the lock costs held. */
        const val HOT_WINDOW_MS: Long = 60_000

        /** Bulk-sync chunk payload size over TCP. Far larger than the 450B
         *  BLE chunks (TCP is reliable; only the 8KB frame cap binds) —
         *  a 160KB contact list is ~40 frames instead of ~360 notifies. */
        // 60 KiB — near the ceiling: sealed = 60K + 4 (hdr) + 16 (AEAD) = 61460,
        // under MAX_FRAME_PAYLOAD (63 KiB) and the Noise 65535 message limit.
        // Fewer seal/write/read/decrypt iterations per file than 48 KiB → faster.
        const val BULK_CHUNK: Int = 60 * 1024
        /** mDNS instance-name rotation cadence (matches BLE presence). */
        const val NSD_ROTATION_SEC: Long = 60L
    }
}
