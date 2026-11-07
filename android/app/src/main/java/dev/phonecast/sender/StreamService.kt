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
import android.os.IBinder
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
)

/** Shared between StreamService (producer) and MainActivity (viewer). */
object StreamState {
    val ui = MutableStateFlow(SenderUiState())
}

/**
 * Foreground service that captures this device's playback audio and streams
 * raw PCM to a PhoneCast receiver.
 *
 * The Android 14 ordering rule from docs/CLAUDE.md is respected: the service
 * is in the foreground with type mediaProjection BEFORE getMediaProjection()
 * is called, and the projection token comes from a consent dialog freshly
 * granted for this session — never reused.
 */
class StreamService : Service() {

    companion object {
        private const val TAG = "PhoneCastSender"
        private const val CHANNEL_ID = "phonecast_stream"
        private const val NOTIFICATION_ID = 1

        const val ACTION_START = "dev.phonecast.sender.START"
        const val ACTION_STOP = "dev.phonecast.sender.STOP"
        const val EXTRA_HOST = "host"
        const val EXTRA_TCP_PORT = "tcp_port"
        const val EXTRA_UDP_PORT = "udp_port"
        const val EXTRA_TOKEN = "token"
        const val EXTRA_RESULT_CODE = "projection_result_code"
        const val EXTRA_RESULT_DATA = "projection_result_data"

        // Wire parameters (docs/protocol.md). The capture frame is 10 ms
        // (480 samples/channel); the WIRE frame is half of that to stay
        // under the 1200-byte datagram cap.
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 2
        const val WIRE_FRAME_SAMPLES = 240
        const val CAPTURE_FRAME_SAMPLES = 480
    }

    private val running = AtomicBoolean(false)
    private var worker: Thread? = null
    private var record: AudioRecord? = null
    private var projection: MediaProjection? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> start(intent)
            ACTION_STOP -> stopStreaming("stopped by user")
        }
        return START_NOT_STICKY // a dead stream must not silently restart with a stale token
    }

    private fun start(intent: Intent) {
        if (running.getAndSet(true)) return

        // Android 14+: foreground-service first, projection after.
        startForeground(
            NOTIFICATION_ID,
            buildNotification("Connecting…"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
        )

        // IntentCompat keeps this working on API 29–32 too (the typed
        // getParcelableExtra overload is API 33+).
        val resultData = IntentCompat.getParcelableExtra(intent, EXTRA_RESULT_DATA, Intent::class.java)
        if (resultData == null) {
            fail("no projection consent data — grant the screen-capture dialog first")
            return
        }

        worker = thread(name = "phonecast-capture") {
            runCatching { streamLoop(intent, resultData) }
                .onFailure { err ->
                    Log.w(TAG, "stream ended with error", err)
                    fail(err.message ?: err.javaClass.simpleName)
                }
        }
    }

    private fun streamLoop(intent: Intent, resultData: Intent) {
        val host = intent.getStringExtra(EXTRA_HOST) ?: error("missing host")
        val tcpPort = intent.getIntExtra(EXTRA_TCP_PORT, 53000)
        val udpPort = intent.getIntExtra(EXTRA_UDP_PORT, 53001)
        val token = intent.getStringExtra(EXTRA_TOKEN) ?: ""

        val manager = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val mp = manager.getMediaProjection(
            intent.getIntExtra(EXTRA_RESULT_CODE, android.app.Activity.RESULT_OK),
            resultData,
        )

        // If the system revokes the projection (e.g. screen-share stopped),
        // tear down cleanly instead of reading from a dead AudioRecord.
        // Explicit main-looper Handler: the capture thread has no Looper,
        // and a null handler would throw there.
        mp.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() = stopStreaming("screen capture revoked")
        }, android.os.Handler(android.os.Looper.getMainLooper()))
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
        val minBuffer = AudioRecord.getMinBufferSize(SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
        // Builder path (API 29+): setAudioPlaybackCaptureConfig replaces
        // setAudioSource — calling both throws IllegalStateException.
        val ar = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(minBuffer * 4, blockBytes * 10))
            .setAudioPlaybackCaptureConfig(captureConfig)
            .build()
        record = ar
        if (ar.state != AudioRecord.STATE_INITIALIZED) {
            error("AudioRecord failed to initialize (is another capture in progress?)")
        }

        // Control channel first: handshake + lease. UDP only starts after OK.
        Socket().use { tcp ->
            tcp.connect(InetSocketAddress(host, tcpPort), 5_000)
            val params = Wire.SessionParams(SAMPLE_RATE, CHANNELS, WIRE_FRAME_SAMPLES, token)
            tcp.getOutputStream().write(Wire.encodeHandshake(params))
            tcp.getOutputStream().flush()
            val reply = ByteArray(Wire.REPLY_BYTES)
            val read = tcp.getInputStream().read(reply)
            if (read != Wire.REPLY_BYTES) error("receiver closed before replying")
            val status = Wire.parseReply(reply)
            if (status != Wire.STATUS_OK) {
                error("receiver rejected handshake: ${Wire.statusName(status)}")
            }

            DatagramSocket().use { udp ->
                udp.connect(InetSocketAddress(host, udpPort))
                ar.startRecording()
                StreamState.ui.update { it.copy(running = true, statusLine = "Streaming to $host") }

                val block = ByteArray(blockBytes)
                val baseNanos = System.nanoTime()
                var sequence = 0L
                var packets = 0L
                var bytes = 0L
                var lastUiUpdate = 0L
                // 0 = audio present; otherwise nanos when the current
                // sustained-silence run began.
                var quietSince = 0L
                var quiet = false

                while (running.get()) {
                    val n = ar.read(block, 0, blockBytes)
                    if (n < 0) error("AudioRecord read failed (error $n)")
                    if (n == 0) continue
                    if (n != blockBytes) {
                        // Should not happen with a blocking read of a full
                        // block; skip the partial fragment rather than send
                        // a mis-framed payload.
                        continue
                    }

                    // Two wire frames per 10 ms capture block.
                    val tsUs = (System.nanoTime() - baseNanos) / 1_000
                    for (half in 0..1) {
                        val offset = half * (WIRE_FRAME_SAMPLES * CHANNELS * 2)
                        val payloadLen = WIRE_FRAME_SAMPLES * CHANNELS * 2
                        val datagram = Wire.encodeDatagram(sequence, tsUs, block, offset, payloadLen)
                        udp.send(DatagramPacket(datagram, datagram.size)) // connected socket: destination is fixed
                        sequence++
                        packets++
                        bytes += datagram.size
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

                StreamState.ui.update { it.copy(packetsSent = packets, bytesSent = bytes) }
            }
        }
        // Normal exit path (user stop / revoked projection / socket closed).
        if (running.get()) stopStreaming("session closed") else finishStop()
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
        if (!running.getAndSet(false)) return
        Log.i(TAG, "stopping: $reason")
        StreamState.ui.update { it.copy(statusLine = "Stopped: $reason") }
        // Unblocks/limits the capture thread's blocking read.
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

    /** Release audio/projection resources and demote the notification. Safe
     *  to call from either thread, twice is harmless. */
    private fun finishStop() {
        runCatching { record?.release() }
        record = null
        runCatching { projection?.stop() } // also fires our own callback; idempotent
        projection = null
        StreamState.ui.update { it.copy(running = false) }
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    private fun fail(reason: String) {
        Log.w(TAG, reason)
        running.set(false)
        StreamState.ui.update { it.copy(statusLine = "Failed: $reason") }
        finishStop() // releases record/projection and demotes the foreground state
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
