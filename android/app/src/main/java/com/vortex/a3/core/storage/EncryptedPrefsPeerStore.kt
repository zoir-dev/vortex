package com.vortex.a3.core.storage

import android.content.Context
import android.util.Base64
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import com.vortex.a3.core.pairing.PairingOrchestrator
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Trusted Peer record per spec §3.3 (V1 subset).
 *
 * Encoded as 72 bytes:
 *   |  0 | 32 | peer_static_pub |
 *   | 32 | 32 | prs (secret)    |
 *   | 64 |  8 | paired_at u64 BE |
 */
data class TrustedPeer(
    val peerStaticPub: ByteArray,   // 32 bytes
    val prs: ByteArray,             // 32 bytes
    val pairedAt: Long,             // Unix seconds
    val peerName: String? = null,   // friendly name from peer's APPROVE payload
) {
    init {
        require(peerStaticPub.size == 32) { "peer_static_pub must be 32 bytes" }
        require(prs.size == 32) { "prs must be 32 bytes" }
    }

    fun encode(): ByteArray {
        // Defense-in-depth: even though peer_name was sanitized when it
        // arrived from the wire, re-sanitize at encode time so we never
        // persist control chars / RTL overrides / oversized strings.
        val cleanName = peerName?.let { PairingOrchestrator.sanitizePeerName(it) }
        val nameBytes = if (cleanName.isNullOrEmpty()) {
            ByteArray(0)
        } else {
            cleanName.toByteArray(Charsets.UTF_8)
        }
        val buf = ByteBuffer.allocate(72 + nameBytes.size).order(ByteOrder.BIG_ENDIAN)
        buf.put(peerStaticPub)
        buf.put(prs)
        buf.putLong(pairedAt)
        if (nameBytes.isNotEmpty()) buf.put(nameBytes)
        return buf.array()
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is TrustedPeer) return false
        return peerStaticPub.contentEquals(other.peerStaticPub) &&
            prs.contentEquals(other.prs) &&
            pairedAt == other.pairedAt &&
            peerName == other.peerName
    }

    override fun hashCode(): Int {
        var r = peerStaticPub.contentHashCode()
        r = 31 * r + prs.contentHashCode()
        r = 31 * r + pairedAt.hashCode()
        r = 31 * r + (peerName?.hashCode() ?: 0)
        return r
    }

    companion object {
        /** Hard cap on the encoded name region. Conservative — utf-8 max
         *  4 bytes per scalar × PEER_NAME_MAX_CHARS (64) = 256, plus
         *  small slack. Anything larger is treated as corruption and
         *  yields a null name rather than allocating an unbounded
         *  string from arbitrary backing-store bytes. */
        const val PEER_NAME_MAX_BYTES: Int = 384

        fun decode(bytes: ByteArray): TrustedPeer? {
            if (bytes.size < 72) return null
            val peerStaticPub = bytes.copyOfRange(0, 32)
            val prs = bytes.copyOfRange(32, 64)
            val pairedAt = ByteBuffer.wrap(bytes, 64, 8).order(ByteOrder.BIG_ENDIAN).long
            val tailLen = bytes.size - 72
            val peerName = if (tailLen in 1..PEER_NAME_MAX_BYTES) {
                runCatching {
                    val raw = String(bytes, 72, tailLen, Charsets.UTF_8)
                    PairingOrchestrator.sanitizePeerName(raw)
                }.getOrNull()?.takeIf { it.isNotEmpty() }
            } else null
            return TrustedPeer(peerStaticPub, prs, pairedAt, peerName)
        }
    }
}

interface PeerStore {
    fun save(peer: TrustedPeer)
    fun load(peerStaticPub: ByteArray): TrustedPeer?
    fun list(): List<TrustedPeer>
    fun forget(peerStaticPub: ByteArray)

    /**
     * Monotonic reconnect counter per peer (M6 audit fix).
     *
     * Each successful reconnect bumps the counter on both sides. The
     * counter is exchanged in IK msg1 / msg2 payloads so a backup-
     * restore attack on either side surfaces as a counter mismatch
     * (peer_counter < local_counter) — a forensic signal logged at
     * warn level. We do not hard-reject because legitimate partial
     * reconnects (network failures between msg2 and persist) can
     * also desync; enforcement would require a 3-way commit handshake.
     */
    fun loadCounter(peerStaticPub: ByteArray): Long = 0L
    fun bumpCounter(peerStaticPub: ByteArray, peerSeen: Long): Long = 0L

    /**
     * Atomically read-and-increment the outbound audio-op nonce. Returns
     * the new value to embed in the next [AudioOpFrame]. Persisted so a
     * process restart cannot rewind the counter and let a network
     * attacker replay an old captured frame.
     */
    fun nextAudioOutNonce(peerStaticPub: ByteArray): Long = 0L

    /** Highest accepted inbound nonce. Frames with `nonce <= this` are
     *  dropped by the orchestrator as replays. */
    fun loadAudioInNonce(peerStaticPub: ByteArray): Long = 0L

    /** Persist a new accepted inbound nonce. Implementations MUST
     *  `max()` against the current value so a duplicate frame delivered
     *  via BLE + LAN can't write a smaller nonce. */
    fun commitAudioInNonce(peerStaticPub: ByteArray, nonce: Long) {}

    /** Atomically accept `nonce` if and only if it's strictly greater
     *  than the previously-seen value. Returns true when the caller
     *  may dispatch the frame; false when it's a replay and must be
     *  dropped.
     *
     *  Defends against the BLE+LAN duplicate-delivery race: both
     *  transports can hand the orchestrator the same frame in the
     *  same millisecond, and the prior `load → if greater → commit`
     *  shape let both readers see the pre-commit value. Real stores
     *  override this with a single synchronized window. Default impl
     *  matches the no-op stores. */
    fun tryAcceptAudioInNonce(peerStaticPub: ByteArray, nonce: Long): Boolean {
        val seen = loadAudioInNonce(peerStaticPub)
        if (nonce <= seen) return false
        commitAudioInNonce(peerStaticPub, nonce)
        return true
    }

    /**
     * The laptop's Bluetooth address (BD_ADDR string), learned from the
     * connected central during pairing and refreshed on every reconnect.
     *
     * Exists so [forget] can hand it to
     * [com.vortex.a3.core.ble.BondCleaner.removeBond]. Vortex itself never
     * bonds — Linux deliberately skips `Device::pair()` — but a bond can
     * still appear via the desktop's Bluetooth panel or an older build, and
     * Android hides such profile-less LE bonds from Settings, so the user
     * cannot clear them by hand. Without this the phone keeps its half of a
     * bond the laptop has dropped, and the next pairing dies during
     * encryption with `timeout: service discovery`.
     *
     * Laptops use a public static address, so one value per peer is stable —
     * unlike the phone's own rotating RPA.
     */
    fun loadPeerBtAddr(peerStaticPub: ByteArray): String? = null
    fun savePeerBtAddr(peerStaticPub: ByteArray, addr: String) {}
}

/** EncryptedSharedPreferences-backed Trusted Peer store. */
class EncryptedPrefsPeerStore(context: Context) : PeerStore {

    private val prefs = run {
        val masterKey = MasterKey.Builder(context, MASTER_KEY_ALIAS)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .setRequestStrongBoxBacked(false)
            .build()
        EncryptedSharedPreferences.create(
            context.applicationContext,
            PREFS_FILENAME,
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    override fun save(peer: TrustedPeer) {
        val key = "peer-${peer.peerStaticPub.toHex()}"
        val b64 = Base64.encodeToString(peer.encode(), Base64.NO_WRAP)
        prefs.edit().putString(key, b64).apply()
    }

    override fun load(peerStaticPub: ByteArray): TrustedPeer? {
        val key = "peer-${peerStaticPub.toHex()}"
        val b64 = prefs.getString(key, null) ?: return null
        return TrustedPeer.decode(Base64.decode(b64, Base64.NO_WRAP))
    }

    override fun list(): List<TrustedPeer> {
        val all = prefs.all
        return all.entries
            .asSequence()
            .filter { it.key.startsWith("peer-") }
            .mapNotNull { entry ->
                val s = entry.value as? String ?: return@mapNotNull null
                runCatching { TrustedPeer.decode(Base64.decode(s, Base64.NO_WRAP)) }.getOrNull()
            }
            .filterNotNull()
            // Deterministic order. `prefs.all` is a HashMap, so without this the
            // sequence follows hash order — and every `list().firstOrNull()`
            // caller silently means "an arbitrary peer". That showed up as the
            // phone's home card displaying whichever laptop hashed first
            // instead of the one it was actually connected to. Most recently
            // paired first, so "the one peer" degrades to something sensible.
            .sortedByDescending { it.pairedAt }
            .toList()
    }

    override fun forget(peerStaticPub: ByteArray) {
        val hex = peerStaticPub.toHex()
        prefs.edit()
            .remove("peer-$hex")
            .remove("counter-$hex")
            .remove("audio_out_nonce-$hex")
            .remove("audio_in_nonce-$hex")
            .remove("btaddr-$hex")
            .apply()
    }

    override fun loadPeerBtAddr(peerStaticPub: ByteArray): String? =
        prefs.getString("btaddr-${peerStaticPub.toHex()}", null)

    override fun savePeerBtAddr(peerStaticPub: ByteArray, addr: String) {
        prefs.edit().putString("btaddr-${peerStaticPub.toHex()}", addr).apply()
    }

    override fun loadCounter(peerStaticPub: ByteArray): Long {
        val key = "counter-${peerStaticPub.toHex()}"
        // The read alone needs no lock — SharedPreferences is internally
        // synchronized — but readers concurrent with `bumpCounter` could
        // see a value between read-and-write of a competing bump. We
        // synchronize all access on `counterLock` so reads observe
        // committed bumps and bumps are atomic. The lock is per-store,
        // not per-peer; the operation is a single getLong/putLong so
        // contention is negligible.
        synchronized(counterLock) {
            return prefs.getLong(key, 0L)
        }
    }

    override fun bumpCounter(peerStaticPub: ByteArray, peerSeen: Long): Long {
        val key = "counter-${peerStaticPub.toHex()}"
        // The 3-step read → max → write must be atomic. Without this
        // lock, concurrent BLE + LAN reconnects can both read the same
        // local counter, both compute the same next value, and one
        // commit overwrites the other — monotonicity gone, which
        // defeats the backup-restore rollback detection that the
        // counter is the only line of defence for (spec §3.3).
        synchronized(counterLock) {
            val local = prefs.getLong(key, 0L)
            val next = maxOf(local, peerSeen) + 1
            // commit() (synchronous) not apply(): this is the replay-rollback
            // counter (spec §3.3). A crash after an async apply() but before
            // the disk flush would rewind it, reopening a replay window. The
            // write rate is one-per-reconnect, so the sync cost is negligible.
            prefs.edit().putLong(key, next).commit()
            return next
        }
    }

    private val counterLock = Any()
    private val nonceLock = Any()

    override fun nextAudioOutNonce(peerStaticPub: ByteArray): Long {
        // Same RMW discipline as bumpCounter — concurrent BLE+LAN
        // sends could otherwise re-use the same nonce, making the
        // receiver reject the second one as a replay.
        val key = "audio_out_nonce-${peerStaticPub.toHex()}"
        synchronized(nonceLock) {
            val current = prefs.getLong(key, 0L)
            val next = current + 1
            // commit(): a rewound out-nonce after a crash would make the peer
            // reject our next frame as a replay. Sync write, low rate.
            prefs.edit().putLong(key, next).commit()
            return next
        }
    }

    override fun loadAudioInNonce(peerStaticPub: ByteArray): Long {
        val key = "audio_in_nonce-${peerStaticPub.toHex()}"
        synchronized(nonceLock) {
            return prefs.getLong(key, 0L)
        }
    }

    override fun commitAudioInNonce(peerStaticPub: ByteArray, nonce: Long) {
        val key = "audio_in_nonce-${peerStaticPub.toHex()}"
        synchronized(nonceLock) {
            val current = prefs.getLong(key, 0L)
            // max() — a duplicate frame delivered via BLE+LAN must
            // not write a smaller value.
            if (nonce > current) {
                prefs.edit().putLong(key, nonce).commit()
            }
        }
    }

    /** Atomic accept-if-fresh: holds `nonceLock` across the read,
     *  compare, and (conditional) commit. Two concurrent callers
     *  therefore linearise — the first decides, the second sees the
     *  freshly-committed nonce and returns false. Fixes the BLE+LAN
     *  duplicate-delivery race that the prior load-then-commit
     *  shape allowed (ChatGPT review #2). */
    override fun tryAcceptAudioInNonce(peerStaticPub: ByteArray, nonce: Long): Boolean {
        val key = "audio_in_nonce-${peerStaticPub.toHex()}"
        synchronized(nonceLock) {
            val current = prefs.getLong(key, 0L)
            if (nonce <= current) return false
            // commit(): the inbound replay high-water mark must survive a crash
            // (an apply() that didn't flush would let a captured frame replay).
            prefs.edit().putLong(key, nonce).commit()
            return true
        }
    }

    companion object {
        const val MASTER_KEY_ALIAS = "com.vortex.peers.v1.master"
        const val PREFS_FILENAME = "vortex_peers_v1"
    }
}

private fun ByteArray.toHex(): String =
    joinToString("") { "%02x".format(it) }
