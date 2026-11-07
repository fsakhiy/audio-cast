//! PhoneCast test sender — stands in for the milestone-3 Android app.
//!
//! Streams a WAV file (16-bit PCM) to the receiver exactly as the phone
//! will: TCP handshake first, then paced UDP datagrams, holding the TCP
//! connection open as the session lease.
//!
//! `--generate FILE.wav` writes a 6-second stereo test WAV: a short click
//! burst at the top of every second followed by two alternating tones, so
//! you can hear (and later measure) latency and stereo behavior.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use phonecast_wire::{
    AUDIO_HEADER_SIZE, AudioHeader, Codec, HANDSHAKE_REPLY_SIZE, Handshake, Status, decode_reply,
};

#[derive(Parser)]
#[command(
    name = "wav-sender",
    version,
    about = "Stream a WAV file to a PhoneCast receiver over UDP (or generate a test WAV)"
)]
struct Cli {
    /// Write a generated test WAV to this path and exit (no streaming).
    #[arg(long, value_name = "FILE.wav")]
    generate: Option<PathBuf>,

    /// Receiver host (IP or hostname).
    #[arg(required_unless_present = "generate")]
    host: Option<String>,

    /// WAV file to stream: 16-bit signed PCM, 1 or 2 channels.
    #[arg(required_unless_present = "generate", value_name = "FILE.wav")]
    file: Option<PathBuf>,

    /// TCP control port.
    #[arg(long, default_value_t = 53000)]
    tcp_port: u16,

    /// UDP audio port.
    #[arg(long, default_value_t = 53001)]
    udp_port: u16,

    /// Pairing token, must match the receiver's --token.
    #[arg(long, default_value = "")]
    token: String,

    /// Keep streaming the file from the start until Ctrl-C.
    #[arg(long)]
    repeat: bool,

    /// Frames (samples per channel) per UDP datagram; lowered if needed to
    /// respect the 1200-byte datagram cap. Ignored for Opus (fixed 960 = 20 ms).
    #[arg(long, default_value_t = 240)]
    frame_size: u16,

    /// Wire codec: pcm (raw, like the phone today) or opus (20 ms frames).
    #[arg(long, value_enum, default_value_t = CodecArg::Pcm)]
    codec: CodecArg,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CodecArg {
    Pcm,
    Opus,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if let Some(path) = &cli.generate {
        return generate_test_wav(path);
    }

    let host = cli
        .host
        .as_deref()
        .context("missing <HOST> (the receiver address)")?;
    let file = cli.file.as_ref().context("missing <FILE.wav>")?;
    stream_wav(&cli, host, file)
}

fn stream_wav(cli: &Cli, host: &str, file: &std::path::Path) -> Result<()> {
    let first_open = hound::WavReader::open(file)
        .with_context(|| format!("failed to open {}", file.display()))?;
    let spec = first_open.spec();
    // hound's len() counts samples, not frames: stereo files have 2 per frame.
    let total_seconds = first_open.len() as f64 / (spec.sample_rate as f64 * spec.channels as f64);
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!(
            "only 16-bit integer PCM WAV files are supported (got {:?} {}-bit)",
            spec.sample_format,
            spec.bits_per_sample
        );
    }
    if !(1..=2).contains(&spec.channels) {
        bail!("WAV must have 1 or 2 channels (got {})", spec.channels);
    }

    let use_opus = cli.codec == CodecArg::Opus;
    let frame_size = if use_opus {
        if spec.sample_rate != 48_000 {
            bail!(
                "opus requires a 48 kHz WAV (got {} Hz); resample the file first",
                spec.sample_rate
            );
        }
        // docs/protocol.md: one Opus frame per datagram; 20 ms recommended.
        960u16
    } else {
        let mut frame_size = cli.frame_size;
        while frame_size > 1
            && frame_size as usize * spec.channels as usize * 2 + AUDIO_HEADER_SIZE > 1200
        {
            frame_size /= 2;
        }
        frame_size
    };
    let wire_codec = if use_opus {
        Codec::Opus
    } else {
        Codec::PcmS16Le
    };
    let handshake = Handshake {
        codec: wire_codec,
        channels: spec.channels as u8,
        sample_rate: spec.sample_rate,
        frame_size,
        token: cli.token.clone(),
    };
    if !handshake.fits_mtu() {
        bail!("--frame-size {frame_size} cannot fit the 1200-byte datagram cap");
    }
    let mut encoder = if use_opus {
        let ch = if spec.channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        };
        Some(opus::Encoder::new(48_000, ch, opus::Application::Audio)?)
    } else {
        None
    };

    // Control connection: also the session lease. Closing it ends playback
    // on the receiver, so we hold it until the very end.
    let mut control = TcpStream::connect((host, cli.tcp_port))
        .with_context(|| format!("failed to connect to {host}:{} over TCP", cli.tcp_port))?;
    control
        .write_all(&handshake.encode()?)
        .context("failed to send handshake")?;
    let mut reply = [0u8; HANDSHAKE_REPLY_SIZE];
    control
        .read_exact(&mut reply)
        .context("receiver closed the control connection before replying")?;
    let status = decode_reply(&reply).context("receiver sent an unparseable reply")?;
    if status != Status::Ok {
        bail!(
            "receiver rejected the handshake: {} (see docs/protocol.md status codes)",
            status_name(status)
        );
    }

    let audio = UdpSocket::bind("0.0.0.0:0")?;
    audio.connect((host, cli.udp_port))?;
    println!(
        "streaming {} ({} Hz, {} ch, {total_seconds:.1} s) to {host} tcp {} / udp {} — Ctrl-C stops",
        file.display(),
        spec.sample_rate,
        spec.channels,
        cli.tcp_port,
        cli.udp_port,
    );

    let samples_per_packet = frame_size as usize * spec.channels as usize;
    // Payload room: raw PCM size, or enough for a worst-case Opus frame
    // while keeping the datagram under the 1200-byte cap.
    let payload_capacity = if use_opus {
        1200 - AUDIO_HEADER_SIZE
    } else {
        samples_per_packet * 2
    };
    let mut packet_buf = vec![0u8; AUDIO_HEADER_SIZE + payload_capacity];
    let frame_duration = Duration::from_secs_f64(frame_size as f64 / spec.sample_rate as f64);
    // Give the receiver a head start: 200 ms of audio goes out as a burst
    // before real-time pacing begins. (Kept modest: an excess burst only
    // buys latency, and the receiver discards old frames to catch up.)
    let start = Instant::now() - Duration::from_millis(200);

    let mut sample_slot = vec![0i16; samples_per_packet];
    let mut sequence: u32 = 0;
    let mut next_target = start;
    let mut sent_packets: u64 = 0;

    // Outer loop exists only for --repeat: each lap reopens the file.
    'file: loop {
        let mut reader = hound::WavReader::open(file)?;
        let mut samples = reader.samples::<i16>();
        loop {
            let mut read = 0usize;
            while read < samples_per_packet {
                match samples.next() {
                    Some(Ok(sample)) => {
                        sample_slot[read] = sample;
                        read += 1;
                    }
                    Some(Err(err)) => return Err(err).context("failed reading WAV samples"),
                    None => break,
                }
            }
            if read == 0 {
                // end of file
                if cli.repeat {
                    continue 'file;
                }
                break 'file;
            }
            let used_bytes = if use_opus {
                // Opus frames are fixed-size; pad the final partial frame
                // with silence (audible only at stream end anyway).
                for slot in &mut sample_slot[read..] {
                    *slot = 0;
                }
                let encoder = encoder.as_mut().expect("opus encoder created above");
                let payload = &mut packet_buf[AUDIO_HEADER_SIZE..];
                let n = encoder.encode(&sample_slot, payload)?;
                if n + AUDIO_HEADER_SIZE > 1200 {
                    bail!(
                        "opus produced a {n}-byte frame; datagram would exceed 1200 bytes — lower the bitrate"
                    );
                }
                n
            } else {
                // PCM: write exactly the samples read; the final packet may
                // be shorter (payload_len declares the size).
                let bytes = read * 2;
                let payload = &mut packet_buf[AUDIO_HEADER_SIZE..AUDIO_HEADER_SIZE + bytes];
                for (out, sample) in payload
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(&sample_slot[..read])
                {
                    out.copy_from_slice(&sample.to_le_bytes());
                }
                bytes
            };

            // Pace: never run ahead of the session clock; if we fall behind
            // (coarse Windows timer, slow disk), catch up by sending sooner.
            let now = Instant::now();
            if now < next_target {
                std::thread::sleep(next_target - now);
            }
            let capture_ts_us = (next_target - start).as_micros() as u64;
            let header = AudioHeader {
                codec: wire_codec,
                sequence,
                capture_ts_us,
            };
            packet_buf[..AUDIO_HEADER_SIZE].copy_from_slice(&header.encode(used_bytes as u16));
            audio.send(&packet_buf[..AUDIO_HEADER_SIZE + used_bytes])?;

            sequence = sequence.wrapping_add(1);
            sent_packets += 1;
            next_target += frame_duration;
        }
    }

    // Let the receiver drain the buffered tail before the lease closes.
    std::thread::sleep(Duration::from_millis(700));
    println!("sent {sent_packets} datagrams; closing the session lease.");
    // control drops here, closing TCP and ending the receiver session.
    Ok(())
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok?!",
        Status::BadVersion => "bad magic or version",
        Status::TokenRejected => "token rejected",
        Status::ParamsUnsupported => {
            "parameters unsupported (sample rate/channels/frame size vs the output device)"
        }
        Status::CodecUnsupported => "codec unsupported",
        Status::Busy => "session slot busy",
    }
}

/// Write the test WAV used for the milestone-2 demo.
fn generate_test_wav(path: &PathBuf) -> Result<()> {
    const RATE: u32 = 48_000;
    const SECONDS: usize = 6;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("failed to create {}", path.display()))?;

    // Per second: 3 ms full-scale click burst, then a 440 Hz tone for the
    // first half and 660 Hz for the second half. The click marks t=0 of
    // each second, useful for eyeballing latency later.
    let amplitude = 0.4 * (i16::MAX as f64);
    for _ in 0..SECONDS {
        for n in 0..RATE {
            let t = n as f64 / RATE as f64;
            let value = if n < RATE / 333 {
                // click: alternating full-scale square burst (~3 ms)
                if (n / 120) % 2 == 0 {
                    i16::MAX as f64
                } else {
                    i16::MIN as f64
                }
            } else {
                let freq = if t < 0.5 { 440.0 } else { 660.0 };
                (freq * std::f64::consts::TAU * t).sin() * amplitude
            };
            let sample = value as i16;
            writer.write_sample(sample)?; // left
            writer.write_sample(sample)?; // right
        }
    }
    writer.finalize()?;
    println!("wrote {path:?} ({SECONDS}s, {RATE} Hz, stereo, 16-bit)");
    Ok(())
}
