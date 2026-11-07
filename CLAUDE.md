# Project: PhoneCast — stream Android audio to a PC and mix it into the PC's output

## Goal
Let me hear my phone's audio and my PC's audio at the same time on one pair of
Bluetooth headphones that stays connected to the PC. The phone captures its own
playback audio and streams it over the local network to a receiver on the PC.
The receiver plays it to the PC's default output device; the OS mixer blends it
with PC audio. No virtual audio driver, and no Bluetooth A2DP link between phone
and PC.

## Components
1. `android/` — Kotlin Android app
2. `receiver/` — Rust PC receiver (cargo workspace member)
3. `docs/protocol.md` — wire protocol spec (source of truth; update it in the
   same commit as any protocol change)

## Targets
- PRIMARY PC: Windows 10/11, output via WASAPI shared mode (cpal's WASAPI backend).
- SECONDARY PC: Linux (PipeWire/PulseAudio/ALSA via cpal). Must keep working
  from the same codebase. Isolate OS-specific code (autostart, firewall help,
  service setup) behind a small module with `cfg(target_os)` gates.
- Phone: Android 10+ (API 29+). Test device is an iQOO/vivo phone running
  Funtouch OS or OriginOS on a recent Android version (assume 14+; verify and
  tell me if the device needs something different).

## Android (Kotlin)
- `MediaProjection` + `AudioPlaybackCaptureConfiguration` + `AudioRecord`
- Foreground service with type `mediaProjection`. Declare the
  `FOREGROUND_SERVICE_MEDIA_PROJECTION` permission, start the foreground service
  BEFORE obtaining the MediaProjection (required on Android 14+), and handle
  the per-session screen-capture consent dialog every time. Never try to reuse
  a stale projection token.
- Capture 48 kHz, 16-bit, stereo PCM in 10 ms frames (480 samples/channel).
  Opus comes in a later milestone; start with raw PCM.
- Jetpack Compose UI: receiver address, start/stop, connection status, live
  latency/packet-loss readout.
- Apps that opt out of capture produce silence: detect sustained silence and
  show a clear notice, don't crash.
- Do NOT request microphone/RECORD input from the phone beyond what
  AudioPlaybackCapture needs.
- iQOO/vivo background-kill handling (Funtouch OS / OriginOS):
  - On first run, show a guided screen that deep-links where possible and
    otherwise explains: enable Autostart for the app, set battery usage to
    "High background power consumption" / unrestricted, exempt it from battery
    optimization (`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`), and lock the
    app in the recents screen.
  - Hold a partial wake lock and a Wi-Fi lock (high-performance mode) only
    while streaming; release them on stop.
  - Detect service death and surface a notification so I know the stream stopped.
  - Treat dontkillmyapp.com guidance for vivo as a reference, and tell me which
    steps you could not verify without the real device.

## Receiver (Rust)
- `cpal` for output, `ringbuf` (lock-free SPSC) between the network thread and
  the audio callback, `tokio` or plain `std::net` for UDP, `clap` for CLI,
  `tracing` for logs, `serde` + `toml` for config.
- `audiopus` or `opus` for decoding (milestone 4), `rubato` for resampling,
  `mdns-sd` for discovery (milestone 5).
- Audio callback rules: no allocation, no locks, no logging, no blocking I/O.
- Open an OUTPUT stream only. Never open any input/capture device, because on
  Windows that can make Bluetooth headphones drop from A2DP to the low-quality
  hands-free profile.
- Output to the default device with an option to select a device by name.
  Re-open the stream gracefully if the default device changes (e.g., headphones
  reconnect).
- Windows: run as a tray app or a background process launched at login (Startup
  folder or Task Scheduler). No admin rights required. Provide a documented
  `netsh advfirewall` command (or first-run prompt guidance) to allow the UDP
  port on Private networks only.
- Linux: provide an example systemd user service file.

## Wire protocol (v1, define fully in docs/protocol.md)
- Control: TCP handshake carrying version, sample rate, channels, codec, frame
  size, and a shared pairing token.
- Audio: UDP datagrams with header: magic (2B), version (1B), codec (1B),
  sequence number (u32), capture timestamp in microseconds (u64), payload
  length (u16), then payload. Little-endian everywhere.
- Keep every datagram under 1200 bytes to avoid fragmentation.
- Receiver detects loss and reordering from sequence numbers.

## Receiver behavior requirements
- Adaptive jitter buffer, default target 30 ms, configurable 10–200 ms.
- Underrun: output silence (or a short fade), never repeat stale samples or
  panic. Overrun: drop oldest frames.
- Clock drift correction: monitor buffer fill and apply light resampling
  (<0.1% ratio adjustment) so long sessions don't drift.
- `--extra-delay-ms` flag to tune lip-sync manually for video.
- Pairing token required; ignore packets from unknown sources; bind to the LAN
  interface only; never expose to the internet.

## Milestones (work in order; stop and show me a working demo after each)
1. Rust receiver plays a generated sine wave through cpal on Windows; verify
   device selection. Confirm it also builds on Linux.
2. Receiver accepts raw PCM over UDP; a small test sender streams a WAV file.
3. Android app captures playback audio and streams raw PCM to the receiver.
4. Jitter buffer, loss handling, drift correction, then Opus encode/decode.
5. mDNS discovery, pairing token, config file, Windows autostart + tray,
   Linux systemd unit.
6. Polish: stats overlay, reconnect logic, vivo battery/background handling
   flow, README.

## Working rules
- Plan before coding: per milestone, give a short plan and the files you'll
  touch, then implement.
- Small, reviewable commits, one concern each.
- Before adding a dependency, say why and what the alternatives are.
- Prefer simple, correct code. No abstractions until a second use case exists.
- Rust: `cargo fmt` and `cargo clippy -- -D warnings` must pass. Avoid
  `unwrap()` outside tests; `anyhow` in the binary, `thiserror` in libraries.
  Unit-test packet parsing, the jitter buffer, and the drift controller.
- Kotlin: standard Android conventions, nothing blocking on the main thread,
  handle lifecycle and permission denial properly.
- When something can't be tested without hardware (Bluetooth, my phone), say so
  and give exact manual test steps instead of claiming it works.
- Don't claim latency numbers you haven't measured. Add a click-track
  end-to-end latency test and report real figures.
- I'm newer to Rust. For non-obvious ownership, lifetime, or concurrency
  decisions, add a one-line comment explaining why.
- If requirements are ambiguous, ask one concise question. If you must assume,
  state the assumption in the commit message.

## Out of scope (don't build unless I ask)
- Phone microphone input, PC-to-phone streaming, video sync, multi-phone
  support, cloud relay, any internet-facing service, virtual audio drivers
