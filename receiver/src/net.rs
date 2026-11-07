//! Milestone 4: receive audio, jitter-buffer it, play it drift-corrected.
//!
//! Threading model:
//! - main thread: TCP accept loop; each connection handled inline (single
//!   session at a time);
//! - one UDP receive thread: parses datagrams and pushes PCM frames into
//!   the active session's jitter buffer (Mutex-guarded; ordinary thread,
//!   may allocate and log);
//! - one playout thread per session: 10 ms ticks, drains the jitter buffer,
//!   applies the drift-corrected rate, and feeds the render ring;
//! - cpal's audio thread: pops f32 samples from the render ring. The
//!   callback never locks: the ring is lock-free, counters are relaxed
//!   atomics, and underrun pads silence (never repeats stale samples).

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use phonecast_wire::{
    AudioHeader, Codec, DecodeError, HANDSHAKE_SIZE, Handshake, MAX_DATAGRAM_BYTES, Status,
    encode_reply,
};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer as _, Producer as _, Split as _},
};

use crate::jitter::{Frame, JitterBuffer};
use crate::playout::{Playout, PlayoutConfig};

/// Opus decoder plus its reusable output scratch (samples per channel,
/// max 120 ms at 48 kHz = 5760). Lives on the UDP thread behind one Mutex
/// so decode + scratch access stay in lockstep.
struct DecodeState {
    decoder: opus::Decoder,
    scratch: Vec<i16>,
}

/// The live session, if any. UDP pushes here; the playout thread pops.
pub struct Session {
    pub channels: usize,
    pub sample_rate: u32,
    pub codec: Codec,
    /// Expected payload bytes per PCM datagram (validated).
    pub frame_payload_bytes: usize,
    pub jitter: Mutex<JitterBuffer>,
    /// Some for Opus sessions, None for PCM. Private: DecodeState wraps the
    /// non-Sync-friendly scratch and must stay inside this module.
    decode: Option<Mutex<DecodeState>>,
    /// Cleared when the lease closes; threads wind down on it.
    pub running: AtomicBool,
    /// Where to send status packets: UDP source of the latest audio.
    last_peer: Mutex<Option<std::net::SocketAddr>>,
    /// One-way-delay estimator inputs: `net_min_us` is the running minimum
    /// of (arrival - capture), which absorbs the constant, unmeasurable
    /// sender/receiver clock offset; `net_last_us` the most recent sample.
    /// Their difference is the *queuing* part of network delay. The
    /// constant part (LAN propagation ~1-3 ms) is in neither, so the
    /// reported latency is a lower-bound estimate — no cross-machine clock
    /// sync is claimed or needed.
    net_min_us: std::sync::atomic::AtomicI64,
    net_last_us: std::sync::atomic::AtomicI64,
}

pub struct Shared {
    pub current: Mutex<Option<Arc<Session>>>,
    pub bad_packets: AtomicU64,
    pub no_session: AtomicU64,
}

pub struct ListenArgs {
    pub bind: IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub token: String,
    pub jitter_target_ms: f64,
    pub extra_delay_ms: f64,
    pub mdns: bool,
}

pub fn run(device: &cpal::Device, args: &ListenArgs) -> Result<()> {
    let tcp_addr = SocketAddr::new(args.bind, args.tcp_port);
    let udp_addr = SocketAddr::new(args.bind, args.udp_port);
    let tcp = TcpListener::bind(tcp_addr)
        .with_context(|| format!("failed to bind control port {tcp_addr} (already in use?)"))?;
    // Shared: the receive thread reads from it, playout threads reply to
    // the active sender with status packets through it (send_to is &self,
    // safe concurrently with recv_from).
    let udp = Arc::new(
        UdpSocket::bind(udp_addr)
            .with_context(|| format!("failed to bind audio port {udp_addr} (already in use?)"))?,
    );

    if args.token.is_empty() {
        tracing::warn!(
            "no pairing token configured; accepting any handshake (token enforcement arrives in milestone 5)"
        );
    }
    tracing::info!(
        %tcp_addr, %udp_addr,
        jitter_target_ms = args.jitter_target_ms,
        extra_delay_ms = args.extra_delay_ms,
        "listening — LAN use only, do not expose these ports to the internet"
    );
    let hint = crate::platform::firewall_hint(args.udp_port, args.tcp_port);
    if !hint.is_empty() {
        tracing::info!("{hint}");
    }

    // mDNS advertising; the daemon must outlive the accept loop below.
    let _mdns = if args.mdns {
        let ips = crate::mdns::advertise_ips(args.bind);
        if ips.is_empty() {
            if args.bind.is_unspecified() {
                tracing::warn!("no non-loopback IPv4 interfaces found; not advertising via mDNS");
            }
            None
        } else {
            match crate::mdns::advertise(&ips, args.tcp_port, args.udp_port, !args.token.is_empty())
            {
                Ok((daemon, names)) => {
                    tracing::info!(services = %names.join(", "), "advertising _phonecast._tcp.local via mDNS");
                    Some(daemon)
                }
                Err(err) => {
                    tracing::warn!("{err:#}; continuing without mDNS");
                    None
                }
            }
        }
    } else {
        None
    };

    let shared = Arc::new(Shared {
        current: Mutex::new(None),
        bad_packets: AtomicU64::new(0),
        no_session: AtomicU64::new(0),
    });

    let udp_thread = {
        let shared = Arc::clone(&shared);
        let udp = Arc::clone(&udp);
        std::thread::spawn(move || udp_loop(udp, shared))
    };

    for incoming in tcp.incoming() {
        match incoming {
            Ok(stream) => {
                if let Err(err) = handle_session(device, stream, &shared, args, &udp) {
                    tracing::warn!(%err, "session ended with an error");
                    *lock(&shared.current) = None;
                }
            }
            Err(err) => tracing::warn!("tcp accept failed: {err}"),
        }
    }
    let _ = udp_thread.join();
    Ok(())
}

/// Handle one sender connection from handshake to lease close.
fn handle_session(
    device: &cpal::Device,
    mut stream: TcpStream,
    shared: &Shared,
    args: &ListenArgs,
    udp: &Arc<UdpSocket>,
) -> Result<()> {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "<unknown>".to_owned());

    let mut buf = [0u8; HANDSHAKE_SIZE];
    if stream.read_exact(&mut buf).is_err() {
        // Client vanished before finishing the handshake; nothing to report.
        return Ok(());
    }

    let hs = match Handshake::decode(&buf) {
        Ok(hs) => hs,
        Err(DecodeError::UnknownCodec(_)) => return reject(&mut stream, Status::CodecUnsupported),
        Err(_) => return reject(&mut stream, Status::BadVersion),
    };

    if hs.channels < 1
        || hs.channels > 2
        || hs.sample_rate == 0
        || hs.frame_size == 0
        || !hs.fits_mtu()
    {
        return reject(&mut stream, Status::ParamsUnsupported);
    }
    if hs.codec != Codec::PcmS16Le && hs.codec != Codec::Opus {
        return reject(&mut stream, Status::CodecUnsupported);
    }
    if !args.token.is_empty() && hs.token != args.token {
        tracing::warn!(%peer, "handshake rejected: token mismatch");
        return reject(&mut stream, Status::TokenRejected);
    }

    // Opus sessions get a decoder (48 kHz is enforced by hs.fits_mtu()).
    let decode = match hs.codec {
        Codec::Opus => {
            let channels = if hs.channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            };
            match opus::Decoder::new(48_000, channels) {
                Ok(decoder) => Some(Mutex::new(DecodeState {
                    decoder,
                    scratch: vec![0i16; 5_760 * hs.channels as usize],
                })),
                Err(err) => {
                    tracing::warn!(%peer, %err, "failed to create opus decoder");
                    return reject(&mut stream, Status::CodecUnsupported);
                }
            }
        }
        Codec::PcmS16Le => None,
    };

    // Open the output stream at the negotiated rate BEFORE telling the
    // sender OK — a device that rejects the rate answers ParamsUnsupported
    // instead of opening a silent session.
    let supported = device
        .default_output_config()
        .context("device has no default output config")?;
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: cpal::SampleRate(hs.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // Render ring: ~1 s of f32 frames, fed by the playout thread and
    // drained by the audio callback. Sized so the playout thread can never
    // realistically overrun it (it paces to the device clock).
    let ring_samples = hs.sample_rate as usize * hs.channels as usize;
    let (render_producer, render_consumer) = HeapRb::<f32>::new(ring_samples).split();

    let session = Arc::new(Session {
        channels: hs.channels as usize,
        sample_rate: hs.sample_rate,
        codec: hs.codec,
        frame_payload_bytes: hs.frame_size as usize * hs.channels as usize * 2,
        jitter: Mutex::new(JitterBuffer::new(
            hs.channels as usize,
            hs.sample_rate,
            args.jitter_target_ms,
        )),
        decode,
        running: AtomicBool::new(true),
        last_peer: Mutex::new(None),
        net_min_us: std::sync::atomic::AtomicI64::new(i64::MAX),
        net_last_us: std::sync::atomic::AtomicI64::new(0),
    });

    let consumed = Arc::new(AtomicU64::new(0));
    let underruns = Arc::new(AtomicU64::new(0));

    let out_stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => open_playback_stream::<f32>(
            device,
            &config,
            render_consumer,
            hs.channels as usize,
            &underruns,
            &consumed,
        ),
        cpal::SampleFormat::I16 => open_playback_stream::<i16>(
            device,
            &config,
            render_consumer,
            hs.channels as usize,
            &underruns,
            &consumed,
        ),
        cpal::SampleFormat::I32 => open_playback_stream::<i32>(
            device,
            &config,
            render_consumer,
            hs.channels as usize,
            &underruns,
            &consumed,
        ),
        format => {
            tracing::error!(format = ?format, "unsupported device sample format");
            return reject(&mut stream, Status::ParamsUnsupported);
        }
    };
    let out_stream = match out_stream {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%peer, %err, "device rejected the negotiated stream config");
            return reject(&mut stream, Status::ParamsUnsupported);
        }
    };

    // Publish the session before the reply so the very first datagrams
    // after OK already have somewhere to land.
    *lock(&shared.current) = Some(Arc::clone(&session));
    out_stream.play().context("failed to start output stream")?;

    let playout_thread = {
        let session = Arc::clone(&session);
        let consumed = Arc::clone(&consumed);
        let udp = Arc::clone(udp);
        // Threads need 'static data, so plain values cross the boundary.
        let extra_delay_ms = args.extra_delay_ms;
        std::thread::spawn(move || {
            playout_loop(session, render_producer, consumed, extra_delay_ms, udp)
        })
    };

    stream
        .write_all(&encode_reply(Status::Ok))
        .context("failed to write handshake reply")?;
    tracing::info!(
        %peer,
        rate = hs.sample_rate,
        channels = hs.channels,
        frame_size = hs.frame_size,
        device = %device.name().unwrap_or_else(|_| "<unknown>".into()),
        "session open — waiting for audio datagrams"
    );

    // The TCP connection is the session lease: block until the sender
    // closes it, then tear everything down.
    let mut one = [0u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) | Err(_) => break,
            Ok(_) => {} // keep-alive bytes, if any; ignored
        }
    }

    session.running.store(false, Ordering::Relaxed);
    // The playout loop polls `running` every tick; join is bounded by one
    // tick plus a small grace.
    let playout_stats = playout_thread.join().ok();
    *lock(&shared.current) = None;
    drop(out_stream); // stops playback; the consumer going away drains nothing further

    let jb_stats = lock(&session.jitter).stats();
    let (playout_underruns, produced) = playout_stats.unwrap_or((0, 0));
    tracing::info!(
        %peer,
        received = jb_stats.received,
        lost = jb_stats.lost,
        late = jb_stats.late_dropped,
        evicted = jb_stats.evicted,
        bad = shared.bad_packets.swap(0, Ordering::Relaxed),
        no_session = shared.no_session.swap(0, Ordering::Relaxed),
        playout_underruns,
        device_underruns = underruns.load(Ordering::Relaxed),
        produced_frames = produced,
        "session closed"
    );
    Ok(())
}

/// The playout thread: owns the render ring producer and clocks frames out
/// at (drift-corrected) real-time pace.
fn playout_loop(
    session: Arc<Session>,
    mut producer: HeapProd<f32>,
    consumed: Arc<AtomicU64>,
    extra_delay_ms: f64,
    udp: Arc<UdpSocket>,
) -> (u64, u64) {
    let mut pl = Playout::new(PlayoutConfig {
        sample_rate: session.sample_rate,
        channels: session.channels,
        extra_delay_ms,
    });
    let tick = Duration::from_millis(10);
    let mut last = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_status = Instant::now();
    while session.running.load(Ordering::Relaxed) {
        let now = Instant::now();
        let dt = now.duration_since(last);
        if dt < tick {
            std::thread::sleep(tick - dt);
            continue;
        }
        last = now;
        let out = {
            // Lock discipline: held only for the tick's frame bookkeeping,
            // never across the sleep, never in the audio callback.
            let mut jb = lock(&session.jitter);
            pl.tick(dt.as_secs_f64(), consumed.load(Ordering::Relaxed), &mut jb)
        };
        // push_slice fills as much as the ring has room for and reports how
        // many landed: pathological excess (after a stall) is dropped as
        // the newest data, never a block.
        let pushed = producer.push_slice(&out);
        if pushed < out.len() {
            tracing::debug!(
                "render ring full, dropped {} newest samples",
                out.len() - pushed
            );
        }

        if last_stats.elapsed() >= Duration::from_secs(5) {
            last_stats = Instant::now();
            let jb = lock(&session.jitter);
            let target = jb.target_ms();
            let buffered = jb.buffered_ms();
            let stats = jb.stats();
            drop(jb);
            let consumed_now = consumed.load(Ordering::Relaxed);
            let (produced_real, _consumed_at_log, ring_real) = pl.ring_diag(consumed_now);
            tracing::info!(
                jitter_target_ms = format!("{target:.0}"),
                buffered_ms = format!("{buffered:.0}"),
                level_ms = format!("{:.0}", pl.level_ms(consumed_now, &lock(&session.jitter))),
                ring_real_ms = format!(
                    "{:.0}",
                    ring_real as f64 * 1000.0 / session.sample_rate as f64
                ),
                produced_real,
                consumed_at_log = _consumed_at_log,
                consumed_now,
                lost = stats.lost,
                late = stats.late_dropped,
                underrun_ticks = pl.underrun_ticks,
                "playout stats"
            );
        }

        // Status packet back to the sender ~1 Hz (docs/protocol.md
        // "Status"). Best effort: a lost reply must never disturb audio.
        if last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let peer = lock(&session.last_peer);
            let Some(peer) = *peer else { continue };
            let jb = lock(&session.jitter);
            let stats = jb.stats();
            let target = jb.target_ms();
            let buffered = jb.buffered_ms();
            drop(jb);
            let level = pl.level_ms(consumed.load(Ordering::Relaxed), &lock(&session.jitter));
            // Queuing part of the network delay; see Session::net_min_us
            // for why this is a lower bound.
            let min_us = session.net_min_us.load(Ordering::Relaxed);
            let last_us = session.net_last_us.load(Ordering::Relaxed);
            let queue_us = if min_us == i64::MAX {
                0
            } else {
                last_us - min_us
            };
            let latency_ms = level + queue_us.max(0) as f64 / 1_000.0;
            let packet = phonecast_wire::StatusPacket {
                lost_frames: stats.lost,
                late_frames: stats.late_dropped.min(u32::MAX as u64) as u32,
                jitter_target_ms: saturate_ms(target),
                buffered_ms: saturate_ms(buffered),
                latency_est_ms: saturate_ms(latency_ms),
            };
            let _ = udp.send_to(&packet.encode(), peer);
        }
    }
    (pl.underrun_ticks, pl.produced_frames())
}

fn saturate_ms(ms: f64) -> u16 {
    if ms.is_nan() || ms <= 0.0 {
        0
    } else if ms > u16::MAX as f64 {
        u16::MAX
    } else {
        ms.round() as u16
    }
}

fn reject(stream: &mut TcpStream, status: Status) -> Result<()> {
    // Best effort: if the write fails, the peer is gone anyway.
    let _ = stream.write_all(&encode_reply(status));
    Ok(())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // If some other thread panicked while holding a lock, carrying on with
    // the intact data is better than dying for an unrelated panic.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The UDP receive thread: parse datagrams and hand frames to the session's
/// jitter buffer. All the realtime-sensitive work lives elsewhere.
fn udp_loop(udp: Arc<UdpSocket>, shared: Arc<Shared>) {
    // Headroom over the spec limit so oversized datagrams are *counted*
    // rather than silently truncated.
    let mut buf = vec![0u8; 4096];
    let t0 = Instant::now();
    loop {
        let (n, from) = match udp.recv_from(&mut buf) {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!("udp recv failed: {err}");
                continue;
            }
        };
        let arrival_ms = t0.elapsed().as_secs_f64() * 1_000.0;

        if n > MAX_DATAGRAM_BYTES {
            shared.bad_packets.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Ok((header, payload)) = AudioHeader::parse(&buf[..n]) else {
            shared.bad_packets.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        // Clone the Arc out under a brief lock; the heavy work (copy +
        // jitter push) happens under the session's own lock only.
        let session = lock(&shared.current).clone();
        let Some(session) = session else {
            shared.no_session.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if !session.running.load(Ordering::Relaxed) {
            shared.no_session.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if header.codec != session.codec {
            shared.bad_packets.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let samples: Vec<i16> = match session.codec {
            Codec::PcmS16Le => {
                if payload.len() != session.frame_payload_bytes {
                    shared.bad_packets.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect()
            }
            Codec::Opus => {
                // Decode on the network thread; the playout thread only
                // ever sees PCM frames regardless of the wire codec.
                let Some(decode) = &session.decode else {
                    shared.bad_packets.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let mut st = lock(decode);
                // Destructure so decoder and scratch are separate mutable
                // borrows (one struct field can't borrow another).
                let DecodeState { decoder, scratch } = &mut *st;
                match decoder.decode(payload, scratch, false) {
                    Ok(samples_per_channel) => {
                        let total = samples_per_channel * session.channels;
                        st.scratch[..total].to_vec()
                    }
                    Err(err) => {
                        // Corrupt/truncated payload: count and move on; the
                        // jitter buffer's gap policy covers the audio hole.
                        tracing::debug!("opus decode failed: {err}");
                        shared.bad_packets.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }
        };
        let frame = Frame::pcm(
            header.sequence,
            header.capture_ts_us,
            samples,
            session.channels,
            session.sample_rate,
        );
        lock(&session.jitter).push(frame, arrival_ms);

        // One-way-delay inputs + where status replies should go.
        let d_us = (arrival_ms * 1_000.0) as i64 - header.capture_ts_us as i64;
        session.net_min_us.fetch_min(d_us, Ordering::Relaxed);
        session.net_last_us.store(d_us, Ordering::Relaxed);
        let mut peer = lock(&session.last_peer);
        if peer.as_ref() != Some(&from) {
            *peer = Some(from);
        }
    }
}

/// Open the output stream whose callback drains the render ring.
///
/// The ring carries interleaved f32 in -1.0..1.0 at the stream's channel
/// count; `S` is the device's own sample format. Mono streams upmix by
/// duplicating. An exhausted ring yields silence, never repeated samples
/// (project rule); `consumed` lets the playout thread see the true pace.
fn open_playback_stream<S: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: HeapCons<f32>,
    stream_channels: usize,
    underruns: &Arc<AtomicU64>,
    consumed: &Arc<AtomicU64>,
) -> Result<cpal::Stream> {
    let device_channels = config.channels as usize;
    let underruns = Arc::clone(underruns);
    let consumed = Arc::clone(consumed);

    device
        .build_output_stream(
            config,
            move |data: &mut [S], _info| {
                // Audio callback: no allocation, no locks, no logging.
                // Atomic fetch_add is lock-free and safe here.
                let mut frame = [0.0f32; 2];
                let frames = data.len() / device_channels;
                for index in 0..frames {
                    let mut starved = false;
                    for slot in frame.iter_mut().take(stream_channels) {
                        match consumer.try_pop() {
                            Some(sample) => *slot = sample,
                            None => {
                                starved = true;
                                break;
                            }
                        }
                    }
                    let start = index * device_channels;
                    if starved {
                        underruns.fetch_add(1, Ordering::Relaxed);
                        for sample in &mut data[start..] {
                            *sample = S::from_sample(0.0f32);
                        }
                        return;
                    }
                    let device_frame = &mut data[start..start + device_channels];
                    for (channel, out) in device_frame.iter_mut().enumerate() {
                        // Mono stream on a stereo device: duplicate.
                        let source = frame[channel.min(stream_channels - 1)];
                        *out = S::from_sample(source);
                    }
                    consumed.fetch_add(1, Ordering::Relaxed);
                }
            },
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .with_context(|| {
            format!(
                "failed to open {} Hz output stream (the device may not support this rate)",
                config.sample_rate.0
            )
        })
}

#[cfg(test)]
mod tests {
    use phonecast_wire::{AudioHeader, Codec};

    /// Encode -> header round-trip -> decode, the exact chain a datagram
    /// takes; guards the codec plumbing against silent format drift.
    #[test]
    fn opus_roundtrip_through_wire_header() {
        // 20 ms stereo 440 Hz tone at half scale.
        let rate = 48_000usize;
        let samples: Vec<i16> = (0..960 * 2)
            .map(|i| {
                let t = (i / 2) as f64 / rate as f64;
                (440.0 * std::f64::consts::TAU * t).sin() * 16_000.0
            })
            .map(|v| v as i16)
            .collect();

        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let packet = enc.encode_vec(&samples, 1_100).unwrap();
        assert!(packet.len() + phonecast_wire::AUDIO_HEADER_SIZE <= 1_200);

        // Build the datagram as the sender would, then parse like the receiver.
        let header = AudioHeader {
            codec: Codec::Opus,
            sequence: 42,
            capture_ts_us: 20_000,
        };
        let mut datagram = header.encode(packet.len() as u16).to_vec();
        datagram.extend_from_slice(&packet);
        let (parsed, payload) = AudioHeader::parse(&datagram).unwrap();
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.codec, Codec::Opus);

        let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut out = vec![0i16; 5_760 * 2];
        let n = dec.decode(payload, &mut out, false).unwrap();
        assert_eq!(n, 960, "20 ms at 48 kHz");
        // Round-tripped audio must carry real energy near the input level.
        let rms = (out[..n * 2]
            .iter()
            .map(|&s| (s as f64).powi(2))
            .sum::<f64>()
            / (n * 2) as f64)
            .sqrt();
        assert!(rms > 8_000.0 && rms < 20_000.0, "rms {rms}");
    }
}
