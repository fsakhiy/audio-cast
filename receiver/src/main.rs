//! PhoneCast PC receiver — milestone 1.
//!
//! Current behavior: play a generated sine test tone to a chosen output
//! device via cpal. Later milestones replace the tone with audio received
//! over the network.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

mod tone;
use tone::Tone;

#[derive(Parser)]
#[command(
    name = "phonecast-receiver",
    version,
    about = "PhoneCast PC receiver (milestone 1: sine test tone on an output device)"
)]
struct Cli {
    /// Print all output devices and exit.
    #[arg(short, long)]
    list: bool,

    /// Output device to use, matched case-insensitively against device names.
    /// Defaults to the system default output device.
    #[arg(short, long, value_name = "NAME")]
    device: Option<String>,

    /// Test tone frequency in Hz. Must stay below half the sample rate.
    #[arg(long, default_value_t = 440.0)]
    freq: f64,

    /// Peak volume from 0.0 to 1.0. Kept well below full scale by default to
    /// protect your ears and headphones.
    #[arg(long, default_value_t = 0.3)]
    volume: f64,

    /// Seconds to play; 0 plays until you press Ctrl-C.
    #[arg(long, default_value_t = 5.0)]
    duration: f64,

    /// Requested output sample rate in Hz. Default: the device's native rate.
    #[arg(long)]
    rate: Option<u32>,
}

fn main() {
    if let Err(err) = run() {
        // {:#} prints the anyhow error together with its context chain.
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    init_logging();
    let cli = Cli::parse();

    if !(0.0 < cli.volume && cli.volume <= 1.0) {
        bail!("--volume must be greater than 0 and at most 1");
    }

    let host = cpal::default_host();

    if cli.list {
        return list_devices(&host);
    }

    let device = pick_device(&host, cli.device.as_deref())?;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "<unknown device>".to_owned());

    let supported = device
        .default_output_config()
        .with_context(|| format!("'{device_name}' has no default output config"))?;

    // Start from the device's default output config and only override the
    // sample rate when --rate asks for it.
    let config = {
        let mut config = supported.config();
        if let Some(rate) = cli.rate {
            config.sample_rate = cpal::SampleRate(rate);
        }
        config
    };
    let sample_rate = config.sample_rate;
    if !(0.0 < cli.freq && cli.freq < sample_rate.0 as f64 / 2.0) {
        bail!(
            "--freq {freq} is outside 0..{} Hz for a {} Hz output; \
             a sine needs at least two samples per cycle",
            sample_rate.0 / 2,
            sample_rate.0,
            freq = cli.freq
        );
    }

    // No explicit support check: build_output_stream below returns a clear
    // error if the device rejects this config.

    tracing::info!(
        device = %device_name,
        rate = sample_rate.0,
        channels = config.channels,
        format = ?supported.sample_format(),
        "opening output stream"
    );

    let tone = Tone::new(cli.freq, sample_rate.0, cli.volume);
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => open_tone_stream::<f32>(&device, &config, tone),
        cpal::SampleFormat::I16 => open_tone_stream::<i16>(&device, &config, tone),
        cpal::SampleFormat::I32 => open_tone_stream::<i32>(&device, &config, tone),
        format => bail!("unsupported output sample format: {format:?}"),
    }?;

    stream.play().context("failed to start the output stream")?;

    if cli.duration <= 0.0 {
        tracing::info!("playing until Ctrl-C");
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    tracing::info!(
        freq_hz = cli.freq,
        seconds = cli.duration,
        "playing test tone"
    );
    std::thread::sleep(Duration::from_secs_f64(cli.duration));
    // `stream` is dropped here, which stops playback cleanly.
    Ok(())
}

/// Build an output stream whose callback generates the test tone.
///
/// The tone is moved into the callback closure so the audio thread owns it
/// outright — no locks, no allocation, nothing that can block the callback.
/// `S` is the sample format the device actually requested (f32 on Windows
/// WASAPI shared mode, varies on Linux).
fn open_tone_stream<S: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut tone: Tone,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    device
        .build_output_stream(
            config,
            // Data callback: runs on cpal's realtime audio thread. Only
            // arithmetic and writing into the buffer cpal gave us.
            move |data: &mut [S], _info| tone.write(data, channels),
            // Error callback: also runs on the audio thread, but only when
            // something already went wrong, so a plain stderr write is fine.
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .context("failed to build the output stream")
}

fn list_devices(host: &cpal::Host) -> Result<()> {
    let default = host.default_output_device();
    let mut count = 0usize;
    for device in host
        .output_devices()
        .context("no audio backend available")?
    {
        let name = device
            .name()
            .unwrap_or_else(|_| "<unknown device>".to_owned());
        let is_default = default
            .as_ref()
            .and_then(|d| d.name().ok())
            .is_some_and(|d_name| d_name == name);
        let marker = if is_default { "default" } else { "" };
        println!("{name}  {marker}");
        count += 1;
    }
    if count == 0 {
        println!("(no output devices found)");
    }
    Ok(())
}

fn pick_device(host: &cpal::Host, needle: Option<&str>) -> Result<cpal::Device> {
    let Some(needle) = needle else {
        return host
            .default_output_device()
            .context("no default output device — pass --device or set one in the OS");
    };

    let needle_l = needle.to_lowercase();
    let matches: Vec<(String, cpal::Device)> = host
        .output_devices()
        .context("no audio backend available")?
        .filter_map(|device| {
            let name = device.name().ok()?;
            name.to_lowercase()
                .contains(&needle_l)
                .then_some((name, device))
        })
        .collect();

    match matches.as_slice() {
        [] => bail!(
            "no output device name contains {needle:?}; \
             run with --list to see available devices"
        ),
        [(name, device)] => {
            tracing::info!(device = %name, "selected device");
            Ok(device.clone())
        }
        many => {
            let names: Vec<&str> = many.iter().map(|(n, _)| n.as_str()).collect();
            bail!(
                "{needle:?} matches several devices ({}); be more specific",
                names.join(", ")
            );
        }
    }
}

fn init_logging() {
    // RUST_LOG wins if set; otherwise log at info and above.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
