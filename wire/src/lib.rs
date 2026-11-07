//! PhoneCast v1 wire format, shared by the receiver and the test sender.
//!
//! This mirrors `docs/protocol.md` exactly — if the two disagree, the doc is
//! wrong and both must be fixed in the same commit.

use thiserror::Error;

pub const PROTOCOL_VERSION: u8 = 1;

pub const CONTROL_MAGIC: [u8; 4] = *b"PCH1";
pub const CONTROL_REPLY_MAGIC: [u8; 4] = *b"PCA1";
pub const AUDIO_MAGIC: [u8; 2] = *b"PC";

/// Byte sizes from the spec.
pub const HANDSHAKE_SIZE: usize = 48;
pub const HANDSHAKE_REPLY_SIZE: usize = 5;
pub const AUDIO_HEADER_SIZE: usize = 18;
pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub const TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    PcmS16Le = 0,
    Opus = 1,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::PcmS16Le),
            1 => Some(Self::Opus),
            _ => None,
        }
    }
}

/// Receiver's answer to a handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    BadVersion = 1,
    TokenRejected = 2,
    ParamsUnsupported = 3,
    CodecUnsupported = 4,
    Busy = 5,
}

impl Status {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ok),
            1 => Some(Self::BadVersion),
            2 => Some(Self::TokenRejected),
            3 => Some(Self::ParamsUnsupported),
            4 => Some(Self::CodecUnsupported),
            5 => Some(Self::Busy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub codec: Codec,
    pub channels: u8,
    pub sample_rate: u32,
    /// Samples per channel per UDP payload.
    pub frame_size: u16,
    pub token: String,
}

#[derive(Debug, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },
    #[error("bad magic bytes")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown codec id {0}")]
    UnknownCodec(u8),
    #[error("unknown status code {0}")]
    UnknownStatusCode(u8),
    #[error("token is not valid UTF-8 or not NUL-padded")]
    BadToken,
    #[error("payload length {declared} does not match datagram size {actual}")]
    PayloadLenMismatch { declared: usize, actual: usize },
}

impl Handshake {
    pub fn encode(&self) -> Result<[u8; HANDSHAKE_SIZE], EncodeError> {
        if self.token.len() > TOKEN_LEN || !self.token.is_ascii() {
            return Err(EncodeError::BadToken);
        }
        let mut buf = [0u8; HANDSHAKE_SIZE];
        buf[0..4].copy_from_slice(&CONTROL_MAGIC);
        buf[4] = PROTOCOL_VERSION;
        buf[5] = self.codec as u8;
        buf[6] = self.channels;
        buf[7] = 0; // flags, reserved
        buf[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        buf[12..14].copy_from_slice(&self.frame_size.to_le_bytes());
        buf[14..16].copy_from_slice(&0u16.to_le_bytes()); // reserved
        buf[16..16 + self.token.len()].copy_from_slice(self.token.as_bytes());
        // bytes 16+token.len()..48 are already zero (NUL padding)
        Ok(buf)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HANDSHAKE_SIZE {
            return Err(DecodeError::TooShort {
                expected: HANDSHAKE_SIZE,
                got: bytes.len(),
            });
        }
        if bytes[0..4] != CONTROL_MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if bytes[4] != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion(bytes[4]));
        }
        let codec = Codec::from_u8(bytes[5]).ok_or(DecodeError::UnknownCodec(bytes[5]))?;
        let channels = bytes[6];
        let sample_rate = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
        let frame_size = u16::from_le_bytes(bytes[12..14].try_into().expect("2 bytes"));
        let token = decode_token(&bytes[16..48])?;
        Ok(Self {
            codec,
            channels,
            sample_rate,
            frame_size,
            token,
        })
    }

    /// Spec's datagram-size rule, checked on both sides. PCM payloads are
    /// fixed by frame_size; Opus payloads are whatever the encoder produced
    /// and are validated per-datagram by the receiver instead (the sender
    /// must still respect the cap — see docs/protocol.md).
    pub fn fits_mtu(&self) -> bool {
        match self.codec {
            Codec::PcmS16Le => {
                self.frame_size as usize * self.channels as usize * 2 + AUDIO_HEADER_SIZE
                    <= MAX_DATAGRAM_BYTES
            }
            Codec::Opus => {
                // Opus runs at its native rate only, per docs/protocol.md.
                self.sample_rate == 48_000 && self.frame_size <= 5_760
            }
        }
    }
}

fn decode_token(raw: &[u8]) -> Result<String, DecodeError> {
    // Token is ASCII with NUL padding: everything after the last non-NUL byte
    // must be NUL, otherwise the sender is not following the spec.
    let end = raw
        .iter()
        .rposition(|b| *b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    std::str::from_utf8(&raw[..end])
        .map(|s| s.to_owned())
        .map_err(|_| DecodeError::BadToken)
}

/// Receiver -> sender status packet (docs/protocol.md "Status"): fixed
/// 24 bytes, sent ~1/s to the UDP source address of the last audio
/// datagram. Senders may ignore anything they don't understand.
pub const STATUS_MAGIC: [u8; 2] = *b"PS";
pub const STATUS_SIZE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusPacket {
    pub lost_frames: u64,
    pub late_frames: u32,
    pub jitter_target_ms: u16,
    pub buffered_ms: u16,
    /// Receiver's estimate of sender-capture -> DAC-start delay, ms.
    /// Saturates at u16::MAX.
    pub latency_est_ms: u16,
}

impl StatusPacket {
    pub fn encode(&self) -> [u8; STATUS_SIZE] {
        let mut b = [0u8; STATUS_SIZE];
        b[0..2].copy_from_slice(&STATUS_MAGIC);
        b[2] = PROTOCOL_VERSION;
        b[3] = 0; // kind: receiver status
        b[4..12].copy_from_slice(&self.lost_frames.to_le_bytes());
        b[12..16].copy_from_slice(&self.late_frames.to_le_bytes());
        b[16..18].copy_from_slice(&self.jitter_target_ms.to_le_bytes());
        b[18..20].copy_from_slice(&self.buffered_ms.to_le_bytes());
        b[20..22].copy_from_slice(&self.latency_est_ms.to_le_bytes());
        // 22..24 reserved zeros
        b
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < STATUS_SIZE {
            return Err(DecodeError::TooShort {
                expected: STATUS_SIZE,
                got: bytes.len(),
            });
        }
        if bytes[0..2] != STATUS_MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if bytes[2] != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion(bytes[2]));
        }
        Ok(Self {
            lost_frames: u64::from_le_bytes(bytes[4..12].try_into().expect("8 bytes")),
            late_frames: u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")),
            jitter_target_ms: u16::from_le_bytes(bytes[16..18].try_into().expect("2 bytes")),
            buffered_ms: u16::from_le_bytes(bytes[18..20].try_into().expect("2 bytes")),
            latency_est_ms: u16::from_le_bytes(bytes[20..22].try_into().expect("2 bytes")),
        })
    }
}

/// Build the 5-byte reply the sender reads after its handshake.
pub fn encode_reply(status: Status) -> [u8; HANDSHAKE_REPLY_SIZE] {
    let mut buf = [0u8; HANDSHAKE_REPLY_SIZE];
    buf[0..4].copy_from_slice(&CONTROL_REPLY_MAGIC);
    buf[4] = status as u8;
    buf
}

pub fn decode_reply(bytes: &[u8]) -> Result<Status, DecodeError> {
    if bytes.len() < HANDSHAKE_REPLY_SIZE {
        return Err(DecodeError::TooShort {
            expected: HANDSHAKE_REPLY_SIZE,
            got: bytes.len(),
        });
    }
    if bytes[0..4] != CONTROL_REPLY_MAGIC {
        return Err(DecodeError::BadMagic);
    }
    Status::from_u8(bytes[4]).ok_or(DecodeError::UnknownStatusCode(bytes[4]))
}

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error("token must be ASCII and at most {TOKEN_LEN} bytes")]
    BadToken,
}

#[derive(Debug, Error)]
pub enum PacketError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("codec {0:?} does not match session codec {1:?}")]
    CodecMismatch(Codec, Codec),
}

/// The 18-byte UDP audio header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub codec: Codec,
    pub sequence: u32,
    /// Microseconds on the sender's monotonic clock.
    pub capture_ts_us: u64,
}

impl AudioHeader {
    pub fn encode(&self, payload_len: u16) -> [u8; AUDIO_HEADER_SIZE] {
        let mut buf = [0u8; AUDIO_HEADER_SIZE];
        buf[0..2].copy_from_slice(&AUDIO_MAGIC);
        buf[2] = PROTOCOL_VERSION;
        buf[3] = self.codec as u8;
        buf[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        buf[8..16].copy_from_slice(&self.capture_ts_us.to_le_bytes());
        buf[16..18].copy_from_slice(&payload_len.to_le_bytes());
        buf
    }

    /// Parse a complete datagram, returning the header and the payload slice.
    pub fn parse(datagram: &[u8]) -> Result<(Self, &[u8]), PacketError> {
        if datagram.len() < AUDIO_HEADER_SIZE {
            return Err(DecodeError::TooShort {
                expected: AUDIO_HEADER_SIZE,
                got: datagram.len(),
            }
            .into());
        }
        if datagram[0..2] != AUDIO_MAGIC {
            return Err(DecodeError::BadMagic.into());
        }
        if datagram[2] != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion(datagram[2]).into());
        }
        let codec = Codec::from_u8(datagram[3]).ok_or(DecodeError::UnknownCodec(datagram[3]))?;
        let sequence = u32::from_le_bytes(datagram[4..8].try_into().expect("4 bytes"));
        let capture_ts_us = u64::from_le_bytes(datagram[8..16].try_into().expect("8 bytes"));
        let payload_len =
            u16::from_le_bytes(datagram[16..18].try_into().expect("2 bytes")) as usize;
        let payload = &datagram[AUDIO_HEADER_SIZE..];
        if payload_len != payload.len() {
            return Err(DecodeError::PayloadLenMismatch {
                declared: payload_len,
                actual: payload.len(),
            }
            .into());
        }
        Ok((
            Self {
                codec,
                sequence,
                capture_ts_us,
            },
            payload,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hs() -> Handshake {
        Handshake {
            codec: Codec::PcmS16Le,
            channels: 2,
            sample_rate: 48_000,
            frame_size: 240,
            token: "letmein".into(),
        }
    }

    #[test]
    fn handshake_round_trips() {
        let original = hs();
        let decoded = Handshake::decode(&original.encode().unwrap()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn empty_token_round_trips_as_empty_string() {
        let mut h = hs();
        h.token = String::new();
        let decoded = Handshake::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(decoded.token, "");
    }

    #[test]
    fn handshake_rejects_bad_magic_and_version() {
        let mut bytes = hs().encode().unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            Handshake::decode(&bytes),
            Err(DecodeError::BadMagic)
        ));
        let mut bytes = hs().encode().unwrap();
        bytes[4] = 2;
        assert!(matches!(
            Handshake::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn handshake_rejects_truncated_input() {
        assert!(matches!(
            Handshake::decode(&[0u8; 47]),
            Err(DecodeError::TooShort {
                expected: 48,
                got: 47
            })
        ));
    }

    #[test]
    fn status_packet_round_trip_and_layout() {
        let s = StatusPacket {
            lost_frames: 1 << 40,
            late_frames: 7,
            jitter_target_ms: 43,
            buffered_ms: 51,
            latency_est_ms: 96,
        };
        let b = s.encode();
        assert_eq!(&b[0..2], b"PS");
        assert_eq!(b[2], 1);
        assert_eq!(StatusPacket::parse(&b), Ok(s));
        assert!(matches!(
            StatusPacket::parse(&b[..23]),
            Err(DecodeError::TooShort { .. })
        ));
    }

    #[test]
    fn reply_round_trips_all_statuses() {
        for status in [
            Status::Ok,
            Status::BadVersion,
            Status::TokenRejected,
            Status::ParamsUnsupported,
            Status::CodecUnsupported,
            Status::Busy,
        ] {
            let bytes = encode_reply(status);
            assert_eq!(decode_reply(&bytes).unwrap(), status);
        }
        assert!(matches!(
            decode_reply(&[b'X', b'Y', b'Z', b'Q', 0]),
            Err(DecodeError::BadMagic)
        ));
        let mut bogus = encode_reply(Status::Ok);
        bogus[4] = 9;
        assert!(matches!(
            decode_reply(&bogus),
            Err(DecodeError::UnknownStatusCode(9))
        ));
    }

    #[test]
    fn token_longer_than_field_is_rejected_on_encode() {
        let mut h = hs();
        h.token = "x".repeat(TOKEN_LEN + 1);
        assert!(matches!(h.encode(), Err(EncodeError::BadToken)));
    }

    #[test]
    fn mtu_rule_matches_spec_example() {
        // The spec's own example: 240 samples * 2 ch * 2 B + 18 = 978 <= 1200.
        assert!(hs().fits_mtu());
        // A full 10 ms capture frame (480 samples/ch) would exceed the cap.
        let mut h = hs();
        h.frame_size = 480;
        assert!(!h.fits_mtu());
    }

    #[test]
    fn opus_rules_from_spec() {
        // Opus: native 48 kHz, frame up to 120 ms; payload MTU checked per
        // datagram, not by the PCM size formula.
        let mut h = hs();
        h.codec = Codec::Opus;
        h.frame_size = 960; // the spec's recommended 20 ms
        assert!(h.fits_mtu());
        let mut h = hs();
        h.codec = Codec::Opus;
        h.sample_rate = 44_100;
        assert!(!h.fits_mtu(), "opus requires 48 kHz per spec");
    }

    #[test]
    fn audio_header_round_trips_with_payload() {
        let header = AudioHeader {
            codec: Codec::PcmS16Le,
            sequence: 0xDEAD_BEEF,
            capture_ts_us: 1 << 40, // beyond u32 range to catch byte-order bugs
        };
        let payload: Vec<u8> = (0..40u8).collect();
        let mut datagram = Vec::new();
        datagram.extend_from_slice(&header.encode(payload.len() as u16));
        datagram.extend_from_slice(&payload);

        let (decoded, decoded_payload) = AudioHeader::parse(&datagram).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded_payload, &payload[..]);
    }

    #[test]
    fn audio_rejects_bad_magic_and_length_mismatch() {
        let header = AudioHeader {
            codec: Codec::PcmS16Le,
            sequence: 1,
            capture_ts_us: 0,
        };
        let mut datagram = header.encode(4).to_vec();
        datagram.extend_from_slice(&[0, 1, 2, 3]);

        let mut bad_magic = datagram.clone();
        bad_magic[0] = b'Z';
        assert!(matches!(
            AudioHeader::parse(&bad_magic),
            Err(PacketError::Decode(DecodeError::BadMagic))
        ));

        // Declared length disagrees with the bytes present.
        assert!(matches!(
            AudioHeader::parse(&header.encode(9)),
            Err(PacketError::Decode(DecodeError::PayloadLenMismatch {
                declared: 9,
                actual: 0
            }))
        ));
    }

    #[test]
    fn audio_rejects_unknown_codec() {
        let mut datagram = AudioHeader {
            codec: Codec::PcmS16Le,
            sequence: 0,
            capture_ts_us: 0,
        }
        .encode(0)
        .to_vec();
        datagram[3] = 7;
        assert!(matches!(
            AudioHeader::parse(&datagram),
            Err(PacketError::Decode(DecodeError::UnknownCodec(7)))
        ));
    }
}
