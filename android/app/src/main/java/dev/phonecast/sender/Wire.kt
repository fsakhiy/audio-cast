package dev.phonecast.sender

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * PhoneCast v1 wire format — the Kotlin mirror of docs/protocol.md and the
 * Rust `wire` crate. Little-endian everywhere. If you change one side,
 * change the spec and both implementations in the same commit.
 */
object Wire {
    const val PROTOCOL_VERSION: Byte = 1
    const val HANDSHAKE_BYTES = 48
    const val REPLY_BYTES = 5
    const val HEADER_BYTES = 18
    const val MAX_DATAGRAM_BYTES = 1200
    const val STATUS_SIZE = 24

    const val CODEC_PCM_S16LE: Byte = 0

    // Receiver handshake status codes (docs/protocol.md).
    const val STATUS_OK: Byte = 0
    const val STATUS_BAD_VERSION: Byte = 1
    const val STATUS_TOKEN_REJECTED: Byte = 2
    const val STATUS_PARAMS_UNSUPPORTED: Byte = 3
    const val STATUS_CODEC_UNSUPPORTED: Byte = 4
    const val STATUS_BUSY: Byte = 5

    fun statusName(status: Byte): String = when (status) {
        STATUS_OK -> "ok"
        STATUS_BAD_VERSION -> "bad magic or version"
        STATUS_TOKEN_REJECTED -> "token rejected"
        STATUS_PARAMS_UNSUPPORTED -> "parameters unsupported by the output device"
        STATUS_CODEC_UNSUPPORTED -> "codec unsupported"
        STATUS_BUSY -> "session slot busy"
        else -> "unknown status $status"
    }

    data class SessionParams(
        val sampleRate: Int,
        val channels: Int,
        /** Samples per channel per UDP payload (the *wire* frame). */
        val frameSize: Int,
        val token: String,
    )

    /** The 48-byte control handshake the sender writes first on TCP. */
    fun encodeHandshake(p: SessionParams): ByteArray {
        require(p.token.toByteArray(Charsets.US_ASCII).size <= 32) { "token too long" }
        val buf = ByteBuffer.allocate(HANDSHAKE_BYTES).order(ByteOrder.LITTLE_ENDIAN)
        buf.put("PCH1".toByteArray(Charsets.US_ASCII))
        buf.put(PROTOCOL_VERSION)
        buf.put(CODEC_PCM_S16LE)
        buf.put(p.channels.toByte())
        buf.put(0) // flags, reserved
        buf.putInt(p.sampleRate)
        buf.putShort(p.frameSize.toShort())
        buf.putShort(0) // reserved
        val token = ByteArray(32)
        p.token.toByteArray(Charsets.US_ASCII).copyInto(token)
        buf.put(token)
        return buf.array()
    }

    /** Parse the 5-byte reply; returns the status code. */
    fun parseReply(bytes: ByteArray): Byte {
        require(bytes.size == REPLY_BYTES) { "reply must be $REPLY_BYTES bytes" }
        val magic = String(bytes, 0, 4, Charsets.US_ASCII)
        require(magic == "PCA1") { "bad reply magic: $magic" }
        return bytes[4]
    }

    data class ReceiverStats(
        val lostFrames: Long,
        val lateFrames: Long,
        val jitterTargetMs: Int,
        val bufferedMs: Int,
        val latencyEstMs: Int,
    )

    /** Parse a receiver status packet (docs/protocol.md "Status"); null on
     *  anything that isn't one — senders MUST ignore unknown UDP traffic. */
    fun parseStatus(bytes: ByteArray, len: Int): ReceiverStats? {
        if (len < STATUS_SIZE) return null
        if (bytes[0] != 'P'.code.toByte() || bytes[1] != 'S'.code.toByte()) return null
        if (bytes[2] != PROTOCOL_VERSION) return null
        val buf = ByteBuffer.wrap(bytes, 0, STATUS_SIZE).order(ByteOrder.LITTLE_ENDIAN)
        buf.position(4)
        val lost = buf.getLong()
        val late = buf.getInt().toLong() and 0xFFFF_FFFFL
        val target = buf.getShort().toInt() and 0xFFFF
        val buffered = buf.getShort().toInt() and 0xFFFF
        val latency = buf.getShort().toInt() and 0xFFFF
        return ReceiverStats(lost, late, target, buffered, latency)
    }

    /**
     * Build one audio datagram: 18-byte header + PCM payload. The payload is
     * raw little-endian s16 samples exactly as AudioRecord produced them.
     */
    fun encodeDatagram(sequence: Long, captureTsUs: Long, payload: ByteArray, offset: Int, length: Int): ByteArray {
        val out = ByteBuffer.allocate(HEADER_BYTES + length).order(ByteOrder.LITTLE_ENDIAN)
        out.put("PC".toByteArray(Charsets.US_ASCII))
        out.put(PROTOCOL_VERSION)
        out.put(CODEC_PCM_S16LE)
        out.putInt(sequence.toInt()) // u32 sequence; wrap handled by toInt()
        out.putLong(captureTsUs)
        out.putShort(length.toShort())
        out.put(payload, offset, length)
        return out.array()
    }
}
