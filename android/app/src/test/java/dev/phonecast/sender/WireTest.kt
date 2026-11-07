package dev.phonecast.sender

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The Kotlin side must agree with the Rust `wire` crate — these vectors are
 * taken from docs/protocol.md's byte tables, not from the implementation, so
 * a bug in one language's code shows up here.
 */
class WireTest {

    private val params = Wire.SessionParams(
        sampleRate = 48_000,
        channels = 2,
        frameSize = 240,
        token = "abc",
    )

    @Test
    fun handshakeLayoutMatchesSpec() {
        val bytes = Wire.encodeHandshake(params)
        assertEquals(48, bytes.size)
        assertArrayEquals("PCH1".toByteArray(), bytes.sliceArray(0..3))
        assertEquals(1.toByte(), bytes[4]) // version
        assertEquals(0.toByte(), bytes[5]) // codec pcm_s16le
        assertEquals(2.toByte(), bytes[6]) // channels
        assertEquals(0.toByte(), bytes[7]) // flags
        // 48000 = 0x0000BB80 little-endian
        assertArrayEquals(
            byteArrayOf(0x80.toByte(), 0xBB.toByte(), 0x00, 0x00),
            bytes.sliceArray(8..11),
        )
        // frame_size 240 = 0x00F0 little-endian
        assertArrayEquals(byteArrayOf(0xF0.toByte(), 0x00), bytes.sliceArray(12..13))
        assertArrayEquals(byteArrayOf(0, 0), bytes.sliceArray(14..15))
        // token at 16..47: "abc" NUL-padded
        assertEquals('a'.code.toByte(), bytes[16])
        assertEquals('b'.code.toByte(), bytes[17])
        assertEquals('c'.code.toByte(), bytes[18])
        for (i in 19..47) assertEquals("padding at $i", 0.toByte(), bytes[i])
    }

    @Test
    fun replyParsesStatusAndMagic() {
        val ok = byteArrayOf('P'.code.toByte(), 'C'.code.toByte(), 'A'.code.toByte(), '1'.code.toByte(), 0)
        assertEquals(Wire.STATUS_OK, Wire.parseReply(ok))
        val rejected = ok.copyOf(5).also { it[4] = Wire.STATUS_TOKEN_REJECTED }
        assertEquals(Wire.STATUS_TOKEN_REJECTED, Wire.parseReply(rejected))
    }

    @Test(expected = IllegalArgumentException::class)
    fun replyRejectsBadMagic() {
        Wire.parseReply(byteArrayOf('X'.code.toByte(), 'Y'.code.toByte(), 'Z'.code.toByte(), 'Q'.code.toByte(), 0))
    }

    @Test
    fun datagramHeaderMatchesSpec() {
        val payload = byteArrayOf(1, 2, 3, 4)
        val d = Wire.encodeDatagram(sequence = 7, captureTsUs = 512, payload = payload, offset = 0, length = 4)
        assertEquals(22, d.size)
        assertArrayEquals(byteArrayOf('P'.code.toByte(), 'C'.code.toByte()), d.sliceArray(0..1))
        assertEquals(1.toByte(), d[2])
        assertEquals(0.toByte(), d[3])
        // sequence 7 LE u32
        assertArrayEquals(byteArrayOf(7, 0, 0, 0), d.sliceArray(4..7))
        // capture ts 512 LE u64
        assertArrayEquals(byteArrayOf(0, 2, 0, 0, 0, 0, 0, 0), d.sliceArray(8..15))
        // payload len 4 LE u16
        assertArrayEquals(byteArrayOf(4, 0), d.sliceArray(16..17))
        assertArrayEquals(payload, d.sliceArray(18..21))
    }

    @Test
    fun statusParsesSpecLayout() {
        // Hand-built from docs/protocol.md's status byte table — the same
        // vectors the Rust test uses, so the two languages must agree.
        val b = ByteArray(24)
        "PS".toByteArray().copyInto(b, 0)
        b[2] = 1
        b[3] = 0
        byteArrayOf(0, 0, 0, 0, 0, 1, 0, 0).copyInto(b, 4) // lost = 1<<40
        byteArrayOf(7, 0, 0, 0).copyInto(b, 12) // late = 7
        byteArrayOf(43, 0).copyInto(b, 16) // jitter target 43
        byteArrayOf(51, 0).copyInto(b, 18) // buffered 51
        byteArrayOf(0x60, 0).copyInto(b, 20) // latency 96 = 0x0060 LE

        val st = Wire.parseStatus(b, 24)!!
        assert(st.lostFrames == 1L shl 40)
        assert(st.lateFrames == 7L)
        assert(st.jitterTargetMs == 43)
        assert(st.bufferedMs == 51)
        assert(st.latencyEstMs == 96)
        // Foreign traffic is ignored, not crashed on.
        assert(Wire.parseStatus("HELLO!".toByteArray() + ByteArray(18), 24) == null)
        assert(Wire.parseStatus(b, 23) == null)
    }

    @Test
    fun fullWireFrameStaysUnderMtuCap() {
        val perHalf = Wire.HEADER_BYTES + 240 * 2 * 2
        assert(perHalf <= Wire.MAX_DATAGRAM_BYTES)
    }
}
