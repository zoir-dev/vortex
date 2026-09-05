package com.vortex.a3.service

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import android.util.Range
import android.view.Surface
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.io.BufferedOutputStream
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket

/**
 * Continuity-Camera SENDER (phone → laptop): open the phone camera with
 * Camera2, feed its frames straight into a MediaCodec **H.264** encoder's input
 * Surface (no copy), seal each access unit ([com.vortex.a3.core.mirror.MirrorTcpSealer],
 * the same wire as the screen mirror) and serve it over TCP on [CAMERA_PORT].
 * The laptop dials in, decodes, and writes the frames into a v4l2 webcam.
 *
 * H.264 (not the screen mirror's HEVC) so the laptop's plain `avdec_h264` path
 * works and webcam apps stay happy. The media key arrives via [EXTRA_KEY] (the
 * caller generated it and shipped it to the laptop in the AppState camera offer).
 */
class CameraStreamService : Service() {

    private val tag = "VortexCamera"
    private var codec: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var cameraThread: HandlerThread? = null
    private var cameraHandler: Handler? = null

    @Volatile private var running = false
    private var wantFront = false
    private var server: ServerSocket? = null
    private var client: Socket? = null
    private var out: BufferedOutputStream? = null
    private var sealer: com.vortex.a3.core.mirror.MirrorTcpSealer? = null
    private val writeLock = Any()
    private var configAnnexB: ByteArray? = null

    private val worker = HandlerThread("camera-stream").also { it.start() }
    private val workerHandler = Handler(worker.looper)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Promote FIRST, before any decision that might bail out. The laptop
        // starts us with startForegroundService(), and Android then requires a
        // startForeground() within a few seconds no matter what we decide —
        // every early return below used to skip it, so a missing CAMERA
        // permission did not merely refuse the stream, it killed the whole app
        // with RemoteServiceException. Posting the notice costs nothing on the
        // paths that go on to stream, and `stopSelf()` takes it down again on
        // the ones that do not.
        startForegroundNotice()
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        // Lens flip: swap the camera in place — keep the encoder, TCP server and
        // the connected laptop, so the flip is seamless (no reconnect).
        if (intent?.action == ACTION_FLIP && running) {
            wantFront = intent.getStringExtra(EXTRA_FACING) == "front"
            Log.i(tag, "flip → ${if (wantFront) "front" else "back"}")
            workerHandler.post {
                try { session?.close() } catch (_: Throwable) {}
                try { camera?.close() } catch (_: Throwable) {}
                session = null; camera = null
                openCamera()
            }
            return START_NOT_STICKY
        }
        // Normal path: the 32-byte key arrives as a byte array. Debug builds
        // ALSO accept a hex string via `--es key_hex <64 hex>` so the stream can
        // be exercised from `adb` without the laptop UI — compiled out of release
        // (a media key must never be accepted in plaintext from an intent).
        val key = intent?.getByteArrayExtra(EXTRA_KEY)
            ?: (if (com.vortex.a3.BuildConfig.DEBUG) {
                intent?.getStringExtra(EXTRA_KEY_HEX)?.let { hex ->
                    runCatching {
                        ByteArray(hex.length / 2) { hex.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
                    }.getOrNull()
                }
            } else null)
        if (key == null || key.size != 32) {
            Log.w(tag, "no/invalid media key — refusing")
            stopSelf()
            return START_NOT_STICKY
        }
        if (ContextCompat.checkSelfPermission(this, android.Manifest.permission.CAMERA)
            != PackageManager.PERMISSION_GRANTED
        ) {
            Log.w(tag, "CAMERA permission not granted — can't stream")
            stopSelf()
            return START_NOT_STICKY
        }
        wantFront = intent?.getStringExtra(EXTRA_FACING) == "front"
        running = true
        sealer = com.vortex.a3.core.mirror.MirrorTcpSealer(key)
        workerHandler.post { startCamera() }
        return START_NOT_STICKY
    }

    private fun startCamera() {
        try {
            configureEncoder(WIDTH, HEIGHT, FPS)
            server = ServerSocket().apply {
                reuseAddress = true
                soTimeout = 1000
                bind(InetSocketAddress(CAMERA_PORT))
            }
            // The accept loop and the encoder loop BOTH block, so each needs its
            // OWN thread — sharing one would starve whichever runs second (the
            // encoder's while-loop would never yield to accept the laptop).
            Thread({ acceptLoop() }, "camera-accept").start()
            openCamera()
        } catch (t: Throwable) {
            Log.e(tag, "camera start failed: ${t.message}", t)
            stopSelf()
        }
    }

    private fun configureEncoder(width: Int, height: Int, fps: Int) {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            // ~1s keyframes so the laptop locks on quickly when it dials in.
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
                setInteger(MediaFormat.KEY_LEVEL, MediaCodecInfo.CodecProfileLevel.AVCLevel31)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                setFloat(MediaFormat.KEY_OPERATING_RATE, fps.toFloat())
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                setInteger(MediaFormat.KEY_LATENCY, 0)
                setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            }
        }
        codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC).apply {
            configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = createInputSurface()
            start()
        }
        Thread({ encoderLoop() }, "camera-encoder").start()
    }

    @Suppress("MissingPermission") // checked in onStartCommand
    private fun openCamera() {
        val mgr = getSystemService(Context.CAMERA_SERVICE) as CameraManager
        // Pick the requested lens (front=selfie / back), falling back to the
        // other facing, then to whatever's available.
        val want = if (wantFront) CameraMetadata.LENS_FACING_FRONT else CameraMetadata.LENS_FACING_BACK
        fun byFacing(f: Int) = mgr.cameraIdList.firstOrNull { id ->
            mgr.getCameraCharacteristics(id)
                .get(android.hardware.camera2.CameraCharacteristics.LENS_FACING) == f
        }
        val camId = byFacing(want)
            ?: byFacing(if (wantFront) CameraMetadata.LENS_FACING_BACK else CameraMetadata.LENS_FACING_FRONT)
            ?: mgr.cameraIdList.firstOrNull() ?: run {
                Log.w(tag, "no camera"); stopSelf(); return
            }
        val camThread = HandlerThread("camera2").also { it.start() }
        cameraThread = camThread
        val camHandler = Handler(camThread.looper)
        cameraHandler = camHandler
        mgr.openCamera(camId, object : CameraDevice.StateCallback() {
            override fun onOpened(device: CameraDevice) {
                camera = device
                val surface = inputSurface ?: return
                @Suppress("DEPRECATION")
                device.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(s: CameraCaptureSession) {
                        session = s
                        try {
                            val req = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                addTarget(surface)
                                // FPS range: pick a camera-supported range containing
                                // our target (a fixed [30,30] some sensors reject and
                                // then deliver NO frames). Fall back to leaving it unset.
                                pickFpsRange(mgr, camId)?.let {
                                    set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, it)
                                }
                            }
                            s.setRepeatingRequest(req.build(), null, camHandler)
                            Log.i(tag, "camera streaming ${WIDTH}x$HEIGHT → :$CAMERA_PORT")
                        } catch (t: Throwable) {
                            Log.e(tag, "setRepeatingRequest failed: ${t.message}", t)
                            stopSelf()
                        }
                    }
                    override fun onConfigureFailed(s: CameraCaptureSession) {
                        Log.w(tag, "capture session configure failed"); stopSelf()
                    }
                }, camHandler)
            }
            override fun onDisconnected(device: CameraDevice) { device.close(); stopSelf() }
            override fun onError(device: CameraDevice, error: Int) {
                Log.w(tag, "camera error $error"); device.close(); stopSelf()
            }
        }, camHandler)
    }

    /** A camera-supported AE FPS range containing our target (or the one with the
     *  largest min ≤ target); null if none, so the request leaves it unset. */
    private fun pickFpsRange(mgr: CameraManager, camId: String): Range<Int>? = try {
        val ranges = mgr.getCameraCharacteristics(camId)
            .get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
        ranges?.filter { it.upper >= FPS }?.minByOrNull { it.upper - it.lower }
            ?: ranges?.maxByOrNull { it.upper }
    } catch (_: Throwable) {
        null
    }

    private fun encoderLoop() {
        val c = codec ?: return
        val info = MediaCodec.BufferInfo()
        var frames = 0L
        while (running) {
            val idx = try {
                c.dequeueOutputBuffer(info, 10_000)
            } catch (t: Throwable) {
                Log.e(tag, "encoder dequeue failed", t); break
            }
            when {
                idx >= 0 -> {
                    val buf = c.getOutputBuffer(idx)
                    if (buf != null && info.size > 0) {
                        buf.position(info.offset)
                        buf.limit(info.offset + info.size)
                        val au = ByteArray(info.size)
                        buf.get(au)
                        if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                            configAnnexB = au // SPS/PPS — resend on each new client
                        } else {
                            frames++
                            if (frames == 1L || frames % 90L == 0L) {
                                Log.i(tag, "encoded $frames frames (${au.size}B)")
                            }
                        }
                        sendAu(au)
                    }
                    c.releaseOutputBuffer(idx, false)
                }
                idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val fmt = c.outputFormat
                    val csd0 = fmt.getByteBuffer("csd-0")
                    val csd1 = fmt.getByteBuffer("csd-1")
                    if (csd0 != null && csd1 != null) {
                        val a = ByteArray(csd0.remaining()); csd0.get(a)
                        val b = ByteArray(csd1.remaining()); csd1.get(b)
                        configAnnexB = a + b
                    }
                }
            }
        }
        running = false
    }

    private fun acceptLoop() {
        val srv = server ?: return
        // Keep accepting, not just once.
        //
        // The laptop tears its pipe down and dials again whenever the lens is
        // flipped — front and back sensors have different orientations, so the
        // pipeline has to be rebuilt. With `client == null` in the loop
        // condition this thread had already exited by then, so the second
        // connect sat unaccepted in the kernel backlog while the write to the
        // closed first socket set `running = false` and ended the encoder. One
        // tap on "flip camera" killed the webcam until it was toggled off and
        // on again.
        while (running) {
            try {
                val s = srv.accept()
                s.tcpNoDelay = true
                // A new connection replaces the old one: the laptop only ever
                // has one pipe open, so an arriving socket means the previous
                // one is finished with.
                runCatching { client?.close() }
                client = s
                out = BufferedOutputStream(s.getOutputStream())
                Log.i(tag, "laptop connected ${s.inetAddress?.hostAddress}")
                configAnnexB?.let { runCatching { write(it) } } // SPS/PPS first
            } catch (_: java.net.SocketTimeoutException) {
                // keep waiting for the laptop to dial in
            } catch (e: Exception) {
                if (running) Log.w(tag, "accept failed: ${e.message}"); break
            }
        }
    }

    private fun sendAu(au: ByteArray) {
        if (client == null) return
        write(au)
    }

    private fun write(au: ByteArray) = synchronized(writeLock) {
        val s = sealer ?: return
        val o = out ?: return
        try {
            o.write(s.sealAccessUnit(au))
            o.flush()
        } catch (e: Exception) {
            // Drop THIS connection and wait for the next one, rather than
            // ending the service. A failed write usually means the laptop
            // closed its side — which is exactly what a lens flip does.
            Log.w(tag, "video write failed: ${e.message}")
            runCatching { client?.close() }
            client = null
            out = null
        }
    }

    private fun startForegroundNotice() {
        val mgr = getSystemService(android.app.NotificationManager::class.java)
        val ch = android.app.NotificationChannel(
            CHANNEL, "Camera streaming", android.app.NotificationManager.IMPORTANCE_MIN,
        )
        mgr.createNotificationChannel(ch)
        val notif = NotificationCompat.Builder(this, CHANNEL)
            .setContentTitle("Camera in use")
            .setContentText("Streaming to laptop as a webcam")
            .setSmallIcon(android.R.drawable.presence_video_online)
            .setOngoing(true)
            .build()
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        } else {
            0
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, notif, type)
        } else {
            startForeground(NOTIF_ID, notif)
        }
    }

    override fun onDestroy() {
        running = false
        try { session?.close() } catch (_: Throwable) {}
        try { camera?.close() } catch (_: Throwable) {}
        try { codec?.stop(); codec?.release() } catch (_: Throwable) {}
        try { inputSurface?.release() } catch (_: Throwable) {}
        try { client?.close() } catch (_: Throwable) {}
        try { server?.close() } catch (_: Throwable) {}
        cameraThread?.quitSafely()
        worker.quitSafely()
        super.onDestroy()
    }

    companion object {
        const val ACTION_START = "com.vortex.a3.action.CAMERA_START"
        const val ACTION_STOP = "com.vortex.a3.action.CAMERA_STOP"
        const val ACTION_FLIP = "com.vortex.a3.action.CAMERA_FLIP"
        const val EXTRA_KEY = "key"
        const val EXTRA_KEY_HEX = "key_hex" // debug: drive from adb
        const val EXTRA_FACING = "facing" // "front" | "back"
        const val CAMERA_PORT = 51824 // mirror_tcp::CAMERA_VIDEO_PORT
        private const val CHANNEL = "vortex_camera"
        private const val NOTIF_ID = 0x5CA
        private const val WIDTH = 1280
        private const val HEIGHT = 720
        private const val FPS = 30
        private const val BITRATE = 4_000_000
    }
}
