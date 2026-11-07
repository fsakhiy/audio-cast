package dev.phonecast.sender

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.IntentCompat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

data class SenderUiState(
    val running: Boolean = false,
    val statusLine: String = "Idle",
    val packetsSent: Long = 0,
    val bytesSent: Long = 0,
    /** True when the capture has produced only silence for several seconds —
     *  usually an app that opted out of playback capture. */
    val captureQuiet: Boolean = false,
    /** Telemetry echoed back by the receiver (status packets). */
    val receiverStats: Wire.ReceiverStats? = null,
    /** Datagrams not sent during a network interruption (drop-during-outage
     *  counter; the receiver's own prefill gate re-cushions on resume). */
    val droppedDuringOutage: Long = 0,
    val reconnects: Long = 0,
)

/** Shared between StreamService (producer) and MainActivity (viewer). */
object StreamState {
    val ui = MutableStateFlow(SenderUiState())
}

/**
 * Foreground service that captures this device's playback audio and streams
 * it to a PhoneCast receiver, with reconnect-on-drop.
 *
 * Android 14 ordering is respected: the service is in the foreground with
 * type mediaProjection BEFORE getMediaProjection() is called, and the
 * projection token always comes from a consent dialog freshly granted for
 * this session — never reused.
 *
 * Architecture: MediaProjection + AudioRecord are acquired ONCE per Start
 * (consent is per capture session, not per network session); sockets are
 * per network session and re-established with backoff while the capture
 * keeps running.
 */
class StreamService : Service() {

    companion object {
        private const val TAG = "PhoneCastSender"
        private const val CHANNEL_ID = "phonecast_stream"
        private const val NOTIFICATION_ID = 1

        const val ACTION_START = "dev.phonecast.sender.START"
        const val ACTION_PREPARE = "dev.phonecast.sender.PREPARE"
        const val ACTION_STOP = "dev.phonecast.sender.STOP"
        const val EXTRA_HOST = "host"
        const val EXTRA_TCP_PORT = "tcp_port"
        const val EXTRA_UDP_PORT = "udp_port"
        const val EXTRA_TOKEN = "token"
        const val EXTRA_RESULT_CODE = "projection_result_code"
        const val EXTRA_RESULT_DATA = "projection_result_data"

        // Wire parameters (docs/protocol.md): 10 ms capture frames split
        // into two 240-sample wire frames to stay under the datagram cap.
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 2
        const val WIRE_FRAME_SAMPLES = 240
        const val CAPTURE_FRAME_SAMPLES = 480
    }

    private val running = AtomicBoolean(false)
    private var worker: Thread? = null
    private var record: AudioRecord? = null
    private var projection: MediaProjection? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_PREPARE -> enterHoldingForeground()
            ACTION_START -> start(intent)
            ACTION_STOP -> stopStreaming("stopped by user")
        }
        return START_NOT_STICKY // a dead stream must not silently restart with a stale token
    }

    /**
     * Entered foreground (type mediaProjection) WITHOUT creating a
     * projection yet. Started before the consent dialog: OEM aggressive
     * process management can freeze/kill a plain UI process while it waits
     * on that dialog, which silently kills the projection result (the
     * consent never lands, and the later AudioRecord then fails with
     * INVALID_OPERATION). A foreground service survives that.
     */
    private fun enterHoldingForeground() {
        if (running.get()) return // a session is already in progress
        startForeground(
            NOTIFICATION_ID,
            buildNotification("Waiting for screen-capture consent…"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
        )
    }

    private fun start(intent: Intent) {
        if (running.getAndSet(true)) return

        // Android 14+: foreground-service first, projection after.
        startForeground(
            NOTIFICATION_ID,
            buildNotification("Connecting…"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
        )

        val resultData = IntentCompat.getParcelableExtra(intent, EXTRA_RESULT_DATA, Intent::class.java)
        if (resultData == null) {
            fail("no projection consent data — grant the screen-capture dialog first")
            return
        }

        // Keep CPU and Wi-Fi awake only while streaming; released in finishStop.
        val pm = getSystemService(POWER_SERVICE) as PowerManager
        @Suppress("DEPRECATION")
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "phonecast:stream").apply {
            setReferenceCounted(false)
            // 4 h cap: a leaked lock would be worse than a stalled stream.
            acquire(4 * 60 * 60 * 1000L)
        }
        val wifi = getSystemService(WIFI_SERVICE) as WifiManager
        @Suppress("DEPRECATION")
        wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "phonecast:stream").apply {
            setReferenceCounted(false)
            acquire()
        }

        worker = thread(name = "phonecast-capture") {
            runCatching { captureLoop(intent, resultData) }
                .onFailure { err ->
                    Log.w(TAG, "capture ended with error", err)
                    fail(err.message ?: err.javaClass.simpleName)
                }
        }
    }

    /** Whole-session lifecycle: projection + AudioRecord once, network
     *  sessions repeatedly until stopped. */
    private fun captureLoop(intent: Intent, resultData: Intent) {
        val host = intent.getStringExtra(EXTRA_HOST) ?: error("missing host")
        val tcpPort = intent.getIntExtra(EXTRA_TCP_PORT, 53000)
        val udpPort = intent.getIntExtra(EXTRA_UDP_PORT, 53001)
        val token = intent.getStringExtra(EXTRA_TOKEN) ?: ""

        val manager = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val mp = manager.getMediaProjection(
            intent.getIntExtra(EXTRA_RESULT_CODE, android.app.Activity.RESULT_OK),
            resultData,
        )
        // Explicit main-looper Handler: the capture thread has no Looper,
        // and a null handler would throw there.
        mp.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() = stopStreaming("screen capture revoked")
        }, Handler(Looper.getMainLooper()))
        projection = mp

        val captureConfig = AudioPlaybackCaptureConfiguration.Builder(mp)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .build()
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val blockBytes = CAPTURE_FRAME_SAMPLES * CHANNELS * 2 // 1920 B = 10 ms
        val minBuffer = AudioRecord.getMinBufferSize(
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT,
        )
        val ar = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(minBuffer * 4, blockBytes * 10))
            .setAudioPlaybackCaptureConfig(captureConfig)
            .build()
        record = ar
        if (ar.state != AudioRecord.STATE_INITIALIZED) {
            error("AudioRecord failed to initialize (is another capture in progress?)")
        }
        ar.startRecording()
        if (ar.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
            error("AudioRecord did not enter RECORDING state (${ar.recordingState})")
        }
        // Single timebase for capture timestamps, fixed for the whole
        // capture session and continuous across network reconnects.
        sessionBaseNanos = System.nanoTime()
        nextSequence = 0L
        quietSince = 0L

        val block = ByteArray(blockBytes)
        var reconnects = 0L

        while (running.get()) {
            // true = the receiver closed the lease (gentle retry);
            // IOException = network failure (exponential backoff).
            val closedCleanly = try {
                if (!runNetworkSession(host, tcpPort, udpPort, token, ar, block)) break
                true
            } catch (e: java.io.IOException) {
                if (!running.get()) break
                Log.w(TAG, "network session failed", e)
                false
            }
            reconnects++
            val waitMs =
                if (closedCleanly) 1_000L else minOf(30_000L, 1_000L shl minOf(reconnects.toInt(), 5))
            val why =
                if (closedCleanly) "receiver closed the session" else "network error, backing off"
            StreamState.ui.update {
                it.copy(
                    statusLine = "Reconnecting ($why) — attempt #${reconnects + 1} in ${waitMs}ms",
                    reconnects = reconnects,
                )
            }
            try {
                Thread.sleep(waitMs)
            } catch (_: InterruptedException) {
                break
            }
        }

        if (running.get()) stopStreaming("session closed") else finishStop()
    }

    /**
     * One network session: TCP handshake/lease + UDP audio.
     * Returns true when the receiver closed the lease (caller reconnects),
     * false when the user stopped streaming (caller breaks out). Throws
     * IOException on network failures. Audio captured while sockets are
     * down is intentionally dropped — bounded latency beats an unbounded
     * backlog for a live stream, and the receiver's prefill gate re-cushions.
     */
    private fun runNetworkSession(
        host: String,
        tcpPort: Int,
        udpPort: Int,
        token: String,
        ar: AudioRecord,
        block: ByteArray,
    ): Boolean {
        val baseNanos = sessionBaseNanos
        var sequence = nextSequence
        var packets = 0L
        var bytes = 0L
        var lastUiUpdate = 0L
        var quietSince = this.quietSince
        var quiet = false
        var leaseEnded = false

        Socket().use { tcp ->
            tcp.connect(InetSocketAddress(host, tcpPort), 5_000)
            tcp.soTimeout = 0 // lease polled non-blockingly from the send loop
            val params = Wire.SessionParams(SAMPLE_RATE, CHANNELS, WIRE_FRAME_SAMPLES, token)
            tcp.getOutputStream().write(Wire.encodeHandshake(params))
            tcp.getOutputStream().flush()
            val reply = ByteArray(Wire.REPLY_BYTES)
            val read = tcp.getInputStream().read(reply)
            if (read != Wire.REPLY_BYTES) throw java.io.IOException("receiver closed before replying")
            val status = Wire.parseReply(reply)
            if (status != Wire.STATUS_OK) {
                throw java.io.IOException("receiver rejected handshake: ${Wire.statusName(status)}")
            }

            DatagramSocket().use { udp ->
                udp.connect(InetSocketAddress(host, udpPort))
                udp.soTimeout = 2 // status poll between sends; cheap on a 10 ms loop
                StreamState.ui.update { it.copy(running = true, statusLine = "Streaming to $host") }
                // If the process dies from here on (OEM task killer), this
                // flag outlives it and the next launch warns the user.
                SavedSettings(applicationContext).streamWasActive = true

                val statusBuf = ByteArray(64)
                while (running.get()) {
                    val n = ar.read(block, 0, block.size)
                    // A broken AudioRecord is NOT a network problem: throwing
                    // IllegalStateException here ends the whole capture
                    // (fail path) instead of the reconnect loop pretending
                    // a TCP retry will fix it.
                    if (n < 0) error("AudioRecord read failed (error $n)")
                    if (n != block.size) continue // partial or stopped: skip, never mis-frame
                    if (leaseClosed(tcp)) {
                        // Receiver ended the session; let the caller reconnect.
                        leaseEnded = true
                        break
                    }
                    val tsUs = (System.nanoTime() - baseNanos) / 1_000
                    for (half in 0..1) {
                        val offset = half * (WIRE_FRAME_SAMPLES * CHANNELS * 2)
                        val payloadLen = WIRE_FRAME_SAMPLES * CHANNELS * 2
                        val datagram = Wire.encodeDatagram(sequence, tsUs, block, offset, payloadLen)
                        udp.send(DatagramPacket(datagram, datagram.size))
                        sequence++
                        packets++
                        bytes += datagram.size
                    }
                    // Drain any status telemetry the receiver sent back.
                    try {
                        val pkt = DatagramPacket(statusBuf, statusBuf.size)
                        udp.receive(pkt)
                        Wire.parseStatus(statusBuf, pkt.length)?.let { st ->
                            StreamState.ui.update { it.copy(receiverStats = st) }
                        }
                    } catch (_: java.net.SocketTimeoutException) {
                        // Nothing waiting — the normal case.
                    }

                    // Sustained-silence detection: apps may opt out of
                    // playback capture, which yields digital zero.
                    val peak = peakAbsolute(block)
                    val now = System.nanoTime()
                    if (peak < 64) {
                        if (quietSince == 0L) quietSince = now
                        if (now - quietSince > 3_000_000_000L) quiet = true
                    } else {
                        quietSince = 0L
                        quiet = false
                    }

                    if (now - lastUiUpdate > 500_000_000L) {
                        lastUiUpdate = now
                        StreamState.ui.update {
                            it.copy(statusLine = "Streaming to $host", packetsSent = packets, bytesSent = bytes, captureQuiet = quiet)
                        }
                    }
                }
            }
        }
        // Persist bookkeeping so the next session continues seamlessly.
        nextSequence = sequence
        this.quietSince = quietSince
        return leaseEnded || running.get() // false only when user stopped mid-loop
    }

    // Session-spanning capture bookkeeping (monotonic across reconnects).
    private var sessionBaseNanos = 0L
    private var nextSequence = 0L
    private var quietSince = 0L

    /** Non-blocking peek at the lease socket: readable + EOF/-1 means the
     *  receiver closed it; readable + data is an unexpected byte — treat
     *  both as session end. */
    private fun leaseClosed(tcp: Socket): Boolean {
        return try {
            val ins = tcp.inputStream
            if (ins.available() > 0) {
                ins.read() == -1 || true
            } else {
                false
            }
        } catch (_: java.io.IOException) {
            true
        }
    }

    private fun peakAbsolute(block: ByteArray): Int {
        var peak = 0
        var i = 0
        while (i + 1 < block.size) {
            // Little-endian s16 on Android; sign handled by toShort.
            val v = ((block[i + 1].toInt() shl 8) or (block[i].toInt() and 0xFF)).toShort()
            val a = if (v < 0) -v else v.toInt()
            if (a > peak) peak = a
            i += 2
        }
        return peak
    }

    private fun stopStreaming(reason: String) {
        val wasStreaming = running.getAndSet(false)
        if (!wasStreaming) {
            // Holding foreground (waiting for consent) or already stopped:
            // just leave the foreground state.
            StreamState.ui.update { it.copy(running = false, statusLine = "Idle") }
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return
        }
        Log.i(TAG, "stopping: $reason")
        runCatching { record?.stop() }
        val w = worker
        worker = null
        if (w != null && Thread.currentThread() != w) {
            // Don't join from the capture thread itself (deadlock).
            runCatching { w.join(2_000) }
        }
        finishStop()
        StreamState.ui.update { it.copy(running = false, captureQuiet = false, statusLine = "Stopped: $reason") }
        stopSelf()
    }

    /** Release audio/projection/locks. Safe from either thread, idempotent. */
    private fun finishStop() {
        SavedSettings(applicationContext).streamWasActive = false
        runCatching { record?.release() }
        record = null
        runCatching { projection?.stop() } // also fires our own callback; idempotent
        projection = null
        runCatching { if (wakeLock?.isHeld == true) wakeLock?.release() }
        wakeLock = null
        runCatching { if (wifiLock?.isHeld == true) wifiLock?.release() }
        wifiLock = null
        StreamState.ui.update { it.copy(running = false) }
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    private fun fail(reason: String) {
        Log.w(TAG, reason)
        running.set(false)
        StreamState.ui.update { it.copy(statusLine = "Failed: $reason") }
        finishStop()
        StreamState.ui.update { it.copy(running = false, captureQuiet = false) }
        notifyManager()?.notify(NOTIFICATION_ID, buildNotification("PhoneCast stopped: $reason"))
        stopSelf()
    }

    private fun buildNotification(text: String): Notification {
        val nm = notifyManager()
        nm?.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "PhoneCast streaming", NotificationManager.IMPORTANCE_LOW)
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setContentTitle("PhoneCast")
            .setContentText(text)
            .setOngoing(true)
            .build()
    }

    private fun notifyManager(): NotificationManager? =
        getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
}
