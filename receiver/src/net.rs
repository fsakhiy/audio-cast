//! Milestone 2: accept raw PCM over the network and play it.
//!
//! Threading model:
//! - main thread: TCP accept loop; each connection is handled inline (single
//!   session at a time), which also holds the output stream open for the
//!   session's lifetime;
//! - one UDP receive thread: parses datagrams, tracks sequence numbers, and
//!   pushes samples into a lock-free ring buffer;
//! - cpal's audio thread: pops samples from the ring buffer's consumer half.
//!   The callback never touches a Mutex — only the ring buffer (lock-free)
//!   and relaxed atomics for counters.
//!
//! Lock discipline: the UDP thread and the session handler only ever hold
//! `state` or `producer` for a few statements and NEVER both at once, so
//! there is no lock-order to deadlock on.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Mutable per-session state shared between the session handler (main
/// thread) and the UDP receive thread.
pub struct Shared {
    /// Swapped wholesale when a session starts/ends. The UDP thread pushes
    /// samples into it; the audio callback owns the matching Consumer half.
    /// `None` means "no session, drop everything".
    pub producer: Mutex<Option<HeapProd<i16>>>,
    pub state: Mutex<SessionState>,
}

#[derive(Default)]
pub struct SessionState {
    /// Expected payload bytes per datagram for the active session; 0 = no
    /// session. Doubles as the "session active" flag.
    pub frame_bytes: usize,
    pub expected_seq: Option<u32>,
    pub lost: u64,
    pub reordered: u64,
    pub overrun: u64,
    pub bad_packets: u64,
    pub no_session: u64,
    last_stats: Option<Instant>,
}

pub struct ListenArgs {
    pub bind: IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub token: String,
}

pub fn run(device: &cpal::Device, args: &ListenArgs) -> Result<()> {
    let tcp_addr = std::net::SocketAddr::new(args.bind, args.tcp_port);
    let udp_addr = std::net::SocketAddr::new(args.bind, args.udp_port);
    let tcp = TcpListener::bind(tcp_addr)
        .with_context(|| format!("failed to bind control port {tcp_addr} (already in use?)"))?;
    let udp = UdpSocket::bind(udp_addr)
        .with_context(|| format!("failed to bind audio port {udp_addr} (already in use?)"))?;

    if args.token.is_empty() {
        tracing::warn!(
            "no pairing token configured; accepting any handshake (token enforcement arrives in milestone 5)"
        );
    }
    tracing::info!(
        %tcp_addr, %udp_addr,
        "listening — LAN use only, do not expose these ports to the internet"
    );

    let shared = Arc::new(Shared {
        producer: Mutex::new(None),
        state: Mutex::new(SessionState::default()),
    });

    let udp_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || udp_loop(udp, shared))
    };

    // One session at a time; the next handshake is accepted only after the
    // current sender's TCP connection closes.
    for incoming in tcp.incoming() {
        match incoming {
            Ok(stream) => {
                if let Err(err) = handle_session(device, stream, &shared, &args.token) {
                    tracing::warn!(%err, "session ended with an error");
                    clear_session(&shared);
                }
            }
            Err(err) => tracing::warn!("tcp accept failed: {err}"),
        }
    }
    // tcp.incoming() never ends; if it somehow does, tidy up the UDP thread.
    let _ = udp_thread.join();
    Ok(())
}

/// Handle one sender connection from handshake to lease close.
fn handle_session(
    device: &cpal::Device,
    mut stream: TcpStream,
    shared: &Shared,
    expected_token: &str,
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
        Err(DecodeError::UnknownCodec(_)) => {
            return reject(&mut stream, Status::CodecUnsupported);
        }
        Err(_) => return reject(&mut stream, Status::BadVersion),
    };

    // Cheap sanity first, so nonsense never reaches cpal.
    if hs.channels < 1
        || hs.channels > 2
        || hs.sample_rate == 0
        || hs.frame_size == 0
        || !hs.fits_mtu()
    {
        return reject(&mut stream, Status::ParamsUnsupported);
    }
    if hs.codec != Codec::PcmS16Le {
        return reject(&mut stream, Status::CodecUnsupported);
    }
    if !expected_token.is_empty() && hs.token != expected_token {
        tracing::warn!(%peer, "handshake rejected: token mismatch");
        return reject(&mut stream, Status::TokenRejected);
    }

    // Open the output stream at the negotiated rate BEFORE telling the
    // sender OK — if the device rejects the rate, the sender gets a clean
    // ParamsUnsupported instead of a silent no-audio session.
    let supported = device
        .default_output_config()
        .context("device has no default output config")?;
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: cpal::SampleRate(hs.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    // 2 seconds' worth of interleaved stream samples, for absorbable bursts.
    let rb = HeapRb::<i16>::new(hs.sample_rate as usize * 2 * hs.channels as usize);
    let (producer, consumer) = rb.split();
    let underruns = Arc::new(AtomicU64::new(0));

    let out_stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            open_pcm_stream::<f32>(device, &config, consumer, &hs, &underruns)
        }
        cpal::SampleFormat::I16 => {
            open_pcm_stream::<i16>(device, &config, consumer, &hs, &underruns)
        }
        cpal::SampleFormat::I32 => {
            open_pcm_stream::<i32>(device, &config, consumer, &hs, &underruns)
        }
        format => {
            // No wire status for "receiver's own device is weird";
            // ParamsUnsupported is the closest fit.
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
    out_stream.play().context("failed to start output stream")?;

    // Install the session. Order of the two swaps matters only in that both
    // happen before the reply: a datagram racing in right after OK finds a
    // complete session.
    {
        let mut guard = lock(&shared.producer);
        *guard = Some(producer);
    }
    {
        let mut guard = lock(&shared.state);
        *guard = SessionState {
            frame_bytes: hs.frame_size as usize * hs.channels as usize * 2,
            last_stats: Some(Instant::now()),
            ..SessionState::default()
        };
    }

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

    // Read stats before clearing, log, then clear. Dropping out_stream
    // stops playback; dropping the producer detaches the ring buffer so
    // late datagrams from this sender are discarded.
    let stats = {
        let guard = lock(&shared.state);
        StatsSnapshot {
            lost: guard.lost,
            reordered: guard.reordered,
            overrun: guard.overrun,
            bad_packets: guard.bad_packets,
            underruns: underruns.load(Ordering::Relaxed),
        }
    };
    drop(out_stream);
    clear_session(shared);
    tracing::info!(
        %peer,
        lost = stats.lost,
        reordered = stats.reordered,
        overrun = stats.overrun,
        bad = stats.bad_packets,
        underruns = stats.underruns,
        "session closed"
    );
    Ok(())
}

struct StatsSnapshot {
    lost: u64,
    reordered: u64,
    overrun: u64,
    bad_packets: u64,
    underruns: u64,
}

fn reject(stream: &mut TcpStream, status: Status) -> Result<()> {
    // Best effort: if the write fails, the peer is gone anyway.
    let _ = stream.write_all(&encode_reply(status));
    Ok(())
}

fn clear_session(shared: &Shared) {
    *lock(&shared.producer) = None;
    *lock(&shared.state) = SessionState::default();
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // If some other thread panicked while holding a lock, carrying on with
    // the intact data is better than dying for an unrelated panic.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The UDP receive thread. Each section takes its Mutex for a few
/// statements and releases it before taking the other — never nested.
fn udp_loop(udp: UdpSocket, shared: Arc<Shared>) {
    // Headroom over the spec limit so oversized datagrams are *counted*
    // rather than silently truncated (truncation would also trip the
    // declared-length check and hide which case happened).
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, _from) = match udp.recv_from(&mut buf) {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!("udp recv failed: {err}");
                continue;
            }
        };

        // Phase 1: validate + sequence bookkeeping against session state.
        let accepted = {
            let mut guard = lock(&shared.state);
            let outcome = validate(&mut guard, n, &buf);
            maybe_log_stats(&mut guard);
            outcome
        };
        let Some(payload) = accepted else {
            continue;
        };
        // Copy the payload out before dropping the state lock: `buf` is
        // reused by the next recv. (A 960-byte memcpy on the network thread
        // is cheap; the audio thread never pays for it.)
        let payload: Vec<u8> = payload.to_vec();

        // Phase 2: push samples into the ring buffer.
        let short = {
            let mut guard = lock(&shared.producer);
            match guard.as_mut() {
                Some(producer) => {
                    let want = payload.len() / 2;
                    let got = producer.push_iter(
                        payload
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| i16::from_le_bytes([b[0], b[1]])),
                    );
                    want > got
                }
                // Session ended between the locks: counted as no-session.
                None => {
                    lock(&shared.state).no_session += 1;
                    false
                }
            }
        };
        if short {
            // Ring was full: the tail of the packet is dropped.
            lock(&shared.state).overrun += 1;
        }
    }
}

/// Check one datagram against the session; on success, update sequence
/// bookkeeping and return the payload slice.
fn validate<'a>(guard: &mut SessionState, n: usize, buf: &'a [u8]) -> Option<&'a [u8]> {
    if guard.frame_bytes == 0 {
        guard.no_session += 1;
        return None;
    }
    if n > MAX_DATAGRAM_BYTES {
        guard.bad_packets += 1;
        return None;
    }
    let Ok((header, payload)) = AudioHeader::parse(&buf[..n]) else {
        guard.bad_packets += 1;
        return None;
    };
    if header.codec != Codec::PcmS16Le || payload.len() != guard.frame_bytes {
        guard.bad_packets += 1;
        return None;
    }
    match guard.expected_seq {
        // First packet of the session defines the baseline.
        None => guard.expected_seq = Some(header.sequence.wrapping_add(1)),
        Some(exp) => {
            // wrapping_sub keeps this correct across a u32 sequence wrap
            // (~6 days at 200 packets/s).
            let delta = header.sequence.wrapping_sub(exp) as i32;
            if delta == 0 {
                guard.expected_seq = Some(exp.wrapping_add(1));
            } else if delta > 0 {
                // A newer packet marks every skipped sequence number lost.
                // No reorder buffer until milestone 4.
                guard.lost += delta as u64;
                guard.expected_seq = Some(header.sequence.wrapping_add(1));
            } else {
                // Late (reordered) packet: drop it.
                guard.reordered += 1;
                return None;
            }
        }
    }
    Some(payload)
}

fn maybe_log_stats(guard: &mut SessionState) {
    let due = match guard.last_stats {
        Some(t) => t.elapsed() >= Duration::from_secs(5),
        None => return,
    };
    if !due {
        return;
    }
    guard.last_stats = Some(Instant::now());
    if guard.lost + guard.reordered + guard.overrun + guard.bad_packets + guard.no_session > 0 {
        tracing::info!(
            lost = guard.lost,
            reordered = guard.reordered,
            overrun = guard.overrun,
            bad = guard.bad_packets,
            no_session = guard.no_session,
            "network stats"
        );
    }
}

/// Open the output stream whose callback drains the ring buffer.
///
/// `S` is the device's own sample format (f32 on Windows WASAPI shared
/// mode). The stream's channel count (from the handshake) may differ from
/// the device's: mono upmixes by duplicating. An exhausted ring buffer
/// yields silence — stale samples are never repeated.
fn open_pcm_stream<S: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: HeapCons<i16>,
    hs: &Handshake,
    underruns: &Arc<AtomicU64>,
) -> Result<cpal::Stream> {
    let stream_channels = hs.channels as usize;
    let device_channels = config.channels as usize;
    let underruns = Arc::clone(underruns);

    device
        .build_output_stream(
            config,
            move |data: &mut [S], _info| {
                // Audio callback: no allocation, no locks, no logging.
                // One stream frame is consumed per device frame; try_pop is
                // a lock-free atomic read.
                let mut frame = [0i16; 2];
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
                        // Silence the rest of this callback in one pass.
                        for sample in &mut data[start..] {
                            *sample = S::from_sample(0.0f32);
                        }
                        return;
                    }
                    let device_frame = &mut data[start..start + device_channels];
                    for (channel, out) in device_frame.iter_mut().enumerate() {
                        // Mono stream on a stereo device: duplicate.
                        let source = frame[channel.min(stream_channels - 1)];
                        *out = S::from_sample(source as f32 / 32768.0);
                    }
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
