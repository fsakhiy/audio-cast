# PhoneCast wire protocol — v1 (stub)

Status: placeholder. The protocol is specified in full at **milestone 2**,
when the receiver starts accepting raw PCM over UDP.

Plan of record (from CLAUDE.md):

- Control channel: TCP handshake carrying version, sample rate, channels,
  codec, frame size, and a shared pairing token.
- Audio channel: UDP datagrams — magic (2 B), version (1 B), codec (1 B),
  sequence (u32), capture timestamp µs (u64), payload length (u16), payload.
  Little-endian throughout, each datagram under 1200 bytes.

Until this file says otherwise, there is no protocol; the receiver only plays
a built-in test tone.
