# PhoneCast wire protocol — v1

Source of truth for both the Android sender and the Rust receiver. Any
protocol change must land in the same commit as the code change.

Encoding everywhere: **little-endian**, unsigned unless noted.

## Transport overview

| Channel | Transport | Default port | Purpose |
|---|---|---|---|
| Discovery | mDNS/DNS-SD | (multicast 5353) | Advertise receivers on the LAN |
| Control | TCP | 53000 | Handshake + session lease |
| Audio | UDP | 53001 | PCM (later Opus) datagrams |

## Discovery (mDNS, milestone 5)

Receivers advertise, one instance per LAN IPv4 interface:

- Service type: `_phonecast._tcp.local.`
- Instance name: `PhoneCast @ <hostname-label>`
- SRV target host: `<hostname-label>.local.`; **SRV port = the TCP control
  port** (the UDP audio port is only carried in TXT, below).
- TXT keys:

| key | meaning |
|---|---|
| `ver` | protocol version this receiver speaks (`"1"`) |
| `udp` | UDP audio port (decimal) |
| `auth` | `"1"` if the receiver requires a pairing token, `"0"` = open |

Senders may ignore discovery and connect by typed IP:port — discovery is
convenience, not part of session establishment. `auth=0` does not mean
"no handshake": the TCP handshake is always required, the token field is
simply empty.

The sender opens TCP first, performs the handshake, and keeps the connection
open for as long as the session should live. Closing it ends the session on
the receiver. Audio datagrams are ignored until a session is active.

Datagrams are kept **under 1200 bytes** to avoid IP fragmentation on common
MTUs. Note the Android capture frame is 10 ms (480 samples/channel at
48 kHz stereo = 1920 bytes of PCM), which is too large for one datagram, so
the *wire frame* is half a capture frame: 240 samples/channel = 960 bytes +
18-byte header = 978 bytes per datagram. Two datagrams per capture frame.

## Control: handshake

Sender → receiver, exactly **48 bytes**:

```
offset  size  field
0       4     magic: "PCH1" (0x50 0x43 0x48 0x31)
4       1     version: 1
5       1     codec: 0 = pcm_s16le, 1 = opus (reserved, not yet usable)
6       1     channels: 1 or 2
7       1     flags: 0 (reserved, sender sets zero, receiver must ignore)
8       4     sample_rate (u32), e.g. 48000
12      2     frame_size (u16): samples per channel per UDP payload, e.g. 240
14      2     reserved: 0
16      32    token: ASCII, NUL-padded on the right; all-zero = no token
```

Receiver → sender, exactly **5 bytes**:

```
offset  size  field
0       4     magic: "PCA1"
4       1     status:
              0 = OK
              1 = bad magic or version unsupported
              2 = token rejected
              3 = parameters not supported (rate/channels/frame_size combo)
              4 = codec unsupported
              5 = session slot busy
```

On any non-zero status the receiver closes the connection. The sender must
not start UDP before receiving status 0.

Validation the receiver performs: `frame_size × channels × 2 + 18 ≤ 1200`,
channels ≤ 2, codec it can decode, sample rate the output device accepts.

Token note: pairing enforcement (config file, first-run pairing) arrives in
milestone 5. Until then the receiver runs open unless started with
`--token`, in which case the handshake token must match exactly.

## Audio: UDP datagrams

```
offset  size  field
0       2     magic: "PC" (0x50 0x43)
2       1     version: 1
3       1     codec: same enum as handshake; must match the active session
4       4     sequence number (u32), starts at 0, +1 per datagram
8       8     capture timestamp (u64), microseconds, sender-monotonic clock
16      2     payload length (u16)
18      n     payload
```

For `codec = pcm_s16le` the payload is `frame_size × channels` interleaved
signed 16-bit little-endian samples (frame-major: L0 R0 L1 R1 …).

For `codec = opus` (live as of milestone 4):
- `sample_rate` in the handshake MUST be 48000 (Opus's native rate; the
  decoder resamples internally if a different playback rate is ever needed).
- Each datagram carries exactly one Opus frame. `frame_size` in the
  handshake declares the nominal samples per channel per frame; the
  receiver decodes and sizes output from the packet itself. Recommended
  value: **960 samples (20 ms)** — best quality/bandwidth for music.
- The 1200-byte datagram cap still applies: the sender must keep encoded
  payload + 18-byte header under the cap (lower the bitrate if it doesn't).
- The receiver validates the declared header codec against the session
  codec and drops mismatches.

The capture timestamp is the sender's monotonic clock at the moment the
samples left the capture callback. The receiver does not interpret its unit
beyond monotonicity; latency measurement uses it from milestone 4 on.

### Receiver behavior on audio datagrams (milestone 2)

- Packets that arrive without an active session, with bad magic/version, a
  codec that doesn't match the session, or a truncated/oversized declared
  length are dropped silently (counted, logged periodically).
- Sequence numbers detect loss and reordering. Milestone 2 has no jitter
  buffer: a gap counts as lost packets and playback continues with the
  payload received; late (reordered) packets are dropped.
- Payload samples are pushed into a lock-free ring buffer; the audio callback
  pops from it. Underrun outputs silence — stale samples are never repeated.
  Ring-full drops the new packet (counted as overrun).
- Clock-drift correction and adaptive buffering: milestone 4.

## Session teardown

When the TCP connection closes, the receiver ends the session: the ring
buffer and sequence counters are discarded and further UDP datagrams are
ignored until a new handshake. The output stream is torn down; the next
handshake may negotiate different parameters.

## Receiver buffering behavior (milestone 4)

The wire format does not change for buffering; the receiver's policy:

- Frames queue in a jitter buffer with an adaptive target:
  `base (default 30 ms, --jitter-target-ms 10–200) + 4 × inter-arrival
  EMA + bumps on loss events`, clamped to 10–200 ms.
- Sequence gaps are gap-filled with silence so playout never skips past
  the lost audio (keeps lip-sync honest). A reordered frame that arrives
  before its (silence-filled) slot plays out replaces the silence.
- Late frames that fall behind the play watermark are dropped and counted.
- Buffer overflow evicts the OLDEST frames.
- Clock drift is corrected with a playout ratio capped at ±0.05 %, so long
  sessions neither drift nor audible-pitch-shift. Levels far above target
  (> target + 200 ms, e.g. a bursty sender's prefill) are recovered by
  discarding whole frames rather than waiting on the capped ratio.
- `--extra-delay-ms` adds manual playout hold for lip-sync.

## Remaining work touching this document

- Pairing UX beyond the shared secret (first-run QR / PIN): not scheduled;
  v1 keeps the manually-configured token model.
- Phone-side Opus encode (NDK libopus; Android's MediaCodec has no Opus
  encoder). PCM remains the phone default until then.
