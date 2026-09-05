package com.vortex.a3.core.earbuds

import android.Manifest
import android.bluetooth.BluetoothA2dp
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothHeadset
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.content.pm.PackageManager
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import java.lang.reflect.Method
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

/**
 * Test-friendly handle for [AudioDeviceController]. Lets the
 * orchestrator unit tests pass a fake without instantiating Android's
 * Bluetooth stack.
 */
interface AudioDeviceHandle {
    fun prewarm()
    suspend fun connect(mac: String): Result<Unit>
    suspend fun disconnect(mac: String): Result<Unit>
    fun isConnected(mac: String): Boolean
    fun invalidate(mac: String)
    fun close()
    /** True if the local Bluetooth adapter is ON. The buds connect over
     *  Bluetooth, so the grab is pointless without it. Default true so
     *  test fakes that don't model the radio keep compiling. */
    fun isBluetoothEnabled(): Boolean = true
}

/**
 * Classic-BT (A2DP + HFP) connect / disconnect wrapper for earbuds
 * switching. Designed for the Phase 1 manual-switch flow per
 * the earbuds-switch design notes §7.1.
 *
 * **S1 — pre-warmed profile proxy.** Opening
 * `BluetoothA2dp` / `BluetoothHeadset` via `BluetoothAdapter.getProfileProxy`
 * triggers an `OnServiceConnected` IPC round-trip that takes ~200-500 ms
 * on first use. We pre-open both proxies at construction (typically
 * `VortexService.onCreate`) and hold them for the process lifetime. The
 * first user-driven switch then sees the connect / disconnect call
 * complete with no startup latency.
 *
 * **Reflection.** `BluetoothA2dp.connect(BluetoothDevice)` and
 * `disconnect(BluetoothDevice)` are hidden APIs (always have been, on
 * every Android version where Vortex runs). We invoke them via
 * reflection. Failure to find the method is a hard error — the device
 * is genuinely incapable of programmatic A2DP control.
 *
 * **Cached `isConnected`.** Each lookup reads from the proxy's
 * `getConnectionState(BluetoothDevice)` which is a synchronous binder
 * call (~1-3 ms but on the caller thread). The 300 ms TTL cache stops
 * a UI poll loop from hammering binder.
 */
class AudioDeviceController(private val appContext: Context) : AudioDeviceHandle {

    @Volatile private var a2dp: BluetoothA2dp? = null
    @Volatile private var headset: BluetoothHeadset? = null
    private val adapter: BluetoothAdapter? =
        appContext.getSystemService(BluetoothManager::class.java)?.adapter

    private data class CachedState(val connected: Boolean, val capturedAtMs: Long)
    /** isConnected TTL cache. `ConcurrentHashMap` so the hot polling
     *  path (UI / orchestrator) can race writers without corrupting
     *  the internal table.
     *
     *  The previous shape — `AtomicReference<MutableMap<…>>` wrapping
     *  a plain `HashMap` — only made the *reference* atomic; the
     *  HashMap itself was still mutated from multiple coroutines and
     *  could enter a bad internal state (lost writes at best, infinite
     *  loops on rehash at worst). ChatGPT review #5. */
    private val cache = ConcurrentHashMap<String, CachedState>()

    /** Last successful prewarm timestamp. Diagnostics only. */
    private val warmedAtMs = AtomicLong(0L)

    /** Reflection `Method` cache. The hidden `connect` / `disconnect` /
     *  `getConnectionState` methods on `BluetoothA2dp` / `BluetoothHeadset`
     *  were resolved via `getMethod(...)` on *every* call — and
     *  `awaitState` polls `getConnectionState` ~13×/s while a switch
     *  settles. `getMethod` walks the class hierarchy each time; the
     *  proxy classes are fixed system types, so we resolve once per
     *  (proxy class, method name) and reuse. */
    private val methodCache = ConcurrentHashMap<String, Method>()

    init {
        prewarm()
    }

    /** Open the A2DP + HFP profile proxies. Idempotent. Safe to call
     *  off the main thread. Failures are logged; the controller
     *  degrades to "operations error out" rather than crashing. */
    override fun prewarm() {
        if (!hasConnectPermission()) {
            Log.w(TAG, "prewarm skipped: missing BLUETOOTH_CONNECT")
            return
        }
        val ad = adapter ?: run {
            Log.w(TAG, "prewarm skipped: no Bluetooth adapter")
            return
        }
        if (a2dp == null) openProfile(ad, BluetoothProfile.A2DP) { a2dp = it as BluetoothA2dp }
        if (headset == null) openProfile(ad, BluetoothProfile.HEADSET) { headset = it as BluetoothHeadset }
    }

    /** Connect the earbuds. Prefers A2DP (media path); falls back to
     *  HFP if A2DP fails (some buds expose only voice). ONE shot per
     *  call — the caller (SwitchOrchestrator.attemptConnect) is the
     *  one that knows about retry semantics. A nested retry loop here
     *  blew the outer 3-retry budget into a 30-second wait, which is
     *  exactly why "Start call" used to look stuck — the inner 3× and
     *  outer 3× compounded to 9 internal connect tries before the
     *  orchestrator could ever surface a Failed state. */
    override suspend fun connect(mac: String): Result<Unit> = withContext(Dispatchers.IO) {
        val device = remoteDevice(mac) ?: return@withContext fail("no remote device for $mac")
        if (isConnectedNow(device)) {
            invalidate(mac)
            return@withContext Result.success(Unit)
        }
        // Try A2DP first; if it returned true and the state flips to
        // CONNECTED within the settle window, we're done.
        if (callProfileMethod(a2dp, "connect", device)) {
            if (awaitConnected(device, CONNECT_SETTLE_MS)) {
                invalidate(mac)
                return@withContext Result.success(Unit)
            }
        }
        // Fallback to HFP for buds that only expose the voice profile.
        if (callProfileMethod(headset, "connect", device)) {
            if (awaitConnected(device, CONNECT_SETTLE_MS)) {
                invalidate(mac)
                return@withContext Result.success(Unit)
            }
        }
        Result.failure(IllegalStateException("connect failed: did not reach CONNECTED within ${CONNECT_SETTLE_MS}ms"))
    }

    /** Disconnect both A2DP and HFP. Even if one fails, attempts the
     *  other — buds with both profiles connected need both released. */
    override suspend fun disconnect(mac: String): Result<Unit> = withContext(Dispatchers.IO) {
        val device = remoteDevice(mac) ?: return@withContext fail("no remote device for $mac")
        var anyOk = false
        if (callProfileMethod(a2dp, "disconnect", device)) anyOk = true
        if (callProfileMethod(headset, "disconnect", device)) anyOk = true
        if (!anyOk && !isConnectedNow(device)) {
            // Already disconnected — call this a success.
            invalidate(mac)
            return@withContext Result.success(Unit)
        }
        // Wait for the state to flip to DISCONNECTED.
        val ok = awaitDisconnected(device, DISCONNECT_SETTLE_MS)
        invalidate(mac)
        if (ok) Result.success(Unit)
        else Result.failure(IllegalStateException("disconnect did not settle within ${DISCONNECT_SETTLE_MS}ms"))
    }

    /** Cached: re-reads the proxy state if the entry is older than
     *  [CACHE_TTL_MS]. Returns false on permission denial / no adapter. */
    override fun isConnected(mac: String): Boolean {
        val now = System.currentTimeMillis()
        val hit = cache[mac]
        if (hit != null && now - hit.capturedAtMs < CACHE_TTL_MS) {
            return hit.connected
        }
        val device = remoteDevice(mac) ?: return false
        val fresh = isConnectedNow(device)
        cache[mac] = CachedState(fresh, now)
        return fresh
    }

    /** Force re-read on the next [isConnected] call. Use after a
     *  connect / disconnect so callers see fresh state immediately. */
    override fun invalidate(mac: String) {
        cache.remove(mac)
    }

    override fun isBluetoothEnabled(): Boolean = adapter?.isEnabled == true

    /** Close the profile proxies. Call from `VortexService.onDestroy`. */
    override fun close() {
        val ad = adapter ?: return
        try { a2dp?.let { ad.closeProfileProxy(BluetoothProfile.A2DP, it) } } catch (_: Throwable) {}
        try { headset?.let { ad.closeProfileProxy(BluetoothProfile.HEADSET, it) } } catch (_: Throwable) {}
        a2dp = null
        headset = null
        cache.clear()
    }

    // ---- internals ----

    private fun hasConnectPermission(): Boolean {
        if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.S) return true
        return ContextCompat.checkSelfPermission(
            appContext, Manifest.permission.BLUETOOTH_CONNECT,
        ) == PackageManager.PERMISSION_GRANTED
    }

    private fun remoteDevice(mac: String): BluetoothDevice? {
        val ad = adapter ?: return null
        return try { ad.getRemoteDevice(mac) } catch (e: IllegalArgumentException) {
            Log.w(TAG, "bad MAC '$mac': ${e.message}"); null
        }
    }

    /** Best-effort connection-state read via the public profile API.
     *  Public surface: BluetoothA2dp/Headset.getConnectionState(device)
     *  has been stable since API 11 / 19 respectively. */
    private fun isConnectedNow(device: BluetoothDevice): Boolean {
        val a = a2dp
        val h = headset
        if (a != null && safeState(a, "getConnectionState", device) == BluetoothProfile.STATE_CONNECTED) {
            return true
        }
        if (h != null && safeState(h, "getConnectionState", device) == BluetoothProfile.STATE_CONNECTED) {
            return true
        }
        return false
    }

    /** Resolve (and cache) a hidden `BluetoothDevice`-taking method on a
     *  profile proxy. Keyed by proxy class + method name — both stable —
     *  so a reopened proxy of the same class reuses the entry. Returns
     *  null if the method genuinely doesn't exist on this proxy. */
    private fun resolveMethod(proxy: Any, name: String): Method? {
        val key = proxy.javaClass.name + "#" + name
        methodCache[key]?.let { return it }
        return try {
            val m = proxy.javaClass.getMethod(name, BluetoothDevice::class.java)
            m.isAccessible = true
            methodCache[key] = m
            m
        } catch (e: Throwable) {
            Log.w(TAG, "$name via reflection failed to resolve: ${e.message}")
            null
        }
    }

    /** Read the profile connection state, or `null` if the read itself
     *  failed (reflection threw / returned a non-Int). `null` means
     *  *unknown*, NOT *disconnected* — callers must not treat a failed
     *  read as a confirmed state. Conflating the two let a transient
     *  binder/reflection hiccup report a still-connected device as gone,
     *  which during `awaitDisconnected` ended the wait early (the switch
     *  proceeds before the buds physically drop) and during `isConnected`
     *  could trip a needless reconnect. */
    private fun safeState(proxy: Any, method: String, device: BluetoothDevice): Int? {
        val m = resolveMethod(proxy, method) ?: return null
        return try {
            m.invoke(proxy, device) as? Int
        } catch (e: Throwable) {
            Log.w(TAG, "$method via reflection failed: ${e.message}")
            null
        }
    }

    private fun callProfileMethod(proxy: Any?, name: String, device: BluetoothDevice): Boolean {
        val p = proxy ?: return false
        val m = resolveMethod(p, name) ?: return false
        return try {
            (m.invoke(p, device) as? Boolean) ?: false
        } catch (e: Throwable) {
            Log.w(TAG, "$name via reflection failed: ${e.message}")
            false
        }
    }

    private suspend fun awaitConnected(device: BluetoothDevice, maxMs: Long): Boolean =
        awaitState(device, BluetoothProfile.STATE_CONNECTED, maxMs)

    private suspend fun awaitDisconnected(device: BluetoothDevice, maxMs: Long): Boolean =
        awaitState(device, BluetoothProfile.STATE_DISCONNECTED, maxMs)

    /** Lightweight poll — Android's intent broadcast for state change
     *  is not always delivered to a background service; polling the
     *  profile proxy is the reliable path. Poll interval 75 ms balances
     *  responsiveness (~13 reads/s) and binder pressure. */
    private suspend fun awaitState(device: BluetoothDevice, target: Int, maxMs: Long): Boolean {
        val deadline = System.currentTimeMillis() + maxMs
        while (System.currentTimeMillis() < deadline) {
            val a = a2dp
            val h = headset
            val s1 = a?.let { safeState(it, "getConnectionState", device) }
            val s2 = h?.let { safeState(it, "getConnectionState", device) }
            if (target == BluetoothProfile.STATE_CONNECTED) {
                // A confirmed CONNECTED on either profile is success. A
                // failed read (null) is just not a confirmation — keep
                // polling.
                if (s1 == target || s2 == target) return true
            } else {
                // DISCONNECTED: conclude only when every PRESENT proxy
                // CONFIRMS it. An absent proxy counts as down; a failed
                // read (proxy present but state null/unknown) does NOT —
                // treating "unknown" as "disconnected" let a transient
                // reflection/binder hiccup end the wait before the buds
                // physically dropped.
                val aDown = a == null || s1 == target
                val hDown = h == null || s2 == target
                if (aDown && hDown) return true
            }
            delay(75)
        }
        return false
    }

    private fun openProfile(ad: BluetoothAdapter, kind: Int, onReady: (BluetoothProfile) -> Unit) {
        val listener = object : BluetoothProfile.ServiceListener {
            override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
                onReady(proxy)
                warmedAtMs.set(System.currentTimeMillis())
                Log.i(TAG, "profile proxy opened: $kind")
            }
            override fun onServiceDisconnected(profile: Int) {
                Log.i(TAG, "profile proxy disconnected: $kind — will reopen on next use")
                when (kind) {
                    BluetoothProfile.A2DP -> a2dp = null
                    BluetoothProfile.HEADSET -> headset = null
                }
            }
        }
        try {
            ad.getProfileProxy(appContext, listener, kind)
        } catch (e: SecurityException) {
            Log.w(TAG, "getProfileProxy denied: ${e.message}")
        }
    }

    private fun fail(msg: String): Result<Unit> = Result.failure(IllegalStateException(msg))

    companion object {
        private const val TAG = "VortexAudioCtrl"

        // ---- Timing constants — see the earbuds-switch design notes §6 ----
        //
        // NOTE: retry count / pause live in SwitchOrchestrator, which
        // owns retry semantics (connect() here is deliberately one-shot
        // — see the connect() KDoc). Don't re-add retry constants here:
        // a duplicate pair drifted (350 vs 280) and was never used.

        /** Window for the connection state to transition to CONNECTED
         *  after a successful reflection-based connect call.
         *
         *  It was 1500 ms, described as "a generous ceiling" on the belief that
         *  buds flip within 400-800 ms. Measured against the real hardware here,
         *  the laptop's own connects to these same earbuds took 3.2-4.8 s in 14
         *  of 18 cases. So the ceiling sat well BELOW the normal case, and the
         *  phone declared failure while Android was still connecting — the OS
         *  then finished the job, leaving the buds on the phone while the app
         *  believed it had none. Seen 12 times in a week, and it feeds straight
         *  into the failed post-call hand-back.
         *
         *  4.5 s covers the measured range. Nothing waits on this synchronously
         *  from a user's point of view: the switch is already asynchronous, so
         *  the cost of being generous is a slower FAILURE report, while the cost
         *  of being tight is a wrong one. */
        const val CONNECT_SETTLE_MS: Long = 4_500

        /** Same idea for disconnect — usually faster than connect. */
        const val DISCONNECT_SETTLE_MS: Long = 1_000

        /** isConnected cache TTL. Long enough that a UI polling at 4 Hz
         *  hits cache 80 %+ of the time, short enough that a real
         *  state change shows up within one user-perceptible tick. */
        const val CACHE_TTL_MS: Long = 300
    }
}
