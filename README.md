# PhoneCast

Hear your Android phone's audio and your PC's audio at the same time on one
pair of Bluetooth headphones — the headphones stay connected to the PC, the
phone streams its playback audio over Wi-Fi, and the OS mixer does the rest.
No virtual audio drivers, no A2DP link between phone and PC.

```
┌──────────┐  AudioPlaybackCapture   ┌─────────────┐  UDP (PCM/Opus)  ┌──────────────┐
│ Android  │ ───────────────────────▶│ Wi-Fi LAN   │─────────────────▶│ PC receiver  │──▶ speakers
│ app      │   TCP control + lease   └─────────────┘                  │ (Rust/cpal)  │   (mixed by OS)
└──────────┘                                                          └──────────────┘
```

## Components

| Path | What |
|---|---|
| `receiver/` | Rust PC receiver: plays streamed audio to an output device (WASAPI shared on Windows, ALSA/Pulse on Linux) |
| `android/` | Kotlin app: captures playback audio (MediaProjection consent per session) and streams it |
| `wire/` | Shared Rust lib for the v1 wire format (mirrored in `android/.../Wire.kt`) |
| `sender/` | `wav-sender` test tool: streams a WAV file like the phone does; `--generate` writes a test file |
| `docs/protocol.md` | The wire protocol — source of truth |
| `docs/setup.md` | Login autostart (Windows Run key / systemd user service), firewall |
| `docs/android-test.md` | On-device test procedure |
| `docs/linux-verify.md` | Steps to confirm the Linux build on a Linux box |

## Quickstart (Windows)

**No build needed:** grab the three binaries from [Releases](../../releases) —
`phonecast-receiver-windows-x64.exe` (double-click; if SmartScreen objects:
*More info → Run anyway*), `phonecast-app-release.apk` (sideload), done.
The build-from-source route follows.

Prereqs: Rust toolchain; CMake on PATH (the `opus` crate compiles libopus
through it).

```powershell
# 1. Terminal A: start the receiver (find the PC's LAN IP with ipconfig)
cargo run -p phonecast-receiver

# 2. Terminal B: smoke-test without a phone
cargo run -p wav-sender -- --generate test.wav
cargo run -p wav-sender -- 127.0.0.1 test.wav          # loopback
cargo run -p wav-sender -- 192.168.1.20 test.wav       # across the LAN
cargo run -p wav-sender -- 192.168.1.20 test.wav --codec opus

# 3. Phone: install android/app/build/outputs/apk/debug/app-debug.apk
#    Open PhoneCast → enter the PC address → Start → grant the
#    screen-capture dialog → play audio anywhere on the phone.
#    If the PC's address is boring to type, the receiver advertises via
#    mDNS and the app lists discovered receivers automatically.
```

Windows Firewall will block UDP 53001/TCP 53000 the first time audio comes
from another machine; the receiver prints the two `netsh` commands
(Private networks only) at startup — run them once from admin PowerShell.

## Configuration

`phonecast-receiver --write-example-config` then edit
`%APPDATA%\PhoneCast\config.toml` (Windows) or
`~/.config/phonecast/config.toml` (Linux). Precedence: CLI flag > config
file > default. Useful keys: `token`, `device`, `jitter_target_ms` (10–200,
default 30), `extra_delay_ms` (lip-sync hold), `log_file`, `mdns`.

Autostart: `phonecast-receiver --install-autostart` / `--remove-autostart`
— details in [docs/setup.md](docs/setup.md).

## How the audio pipeline works (receiver)

Four threads, one contract: the audio callback allocates nothing, locks
nothing, logs nothing.

```
UDP recv ──▶ jitter buffer ──▶ playout thread ──▶ render ring ──▶ cpal callback ──▶ DAC
  (parse,     (adaptive target,  (device-paced,    (lock-free      (silence on
   decode      gap→silence,       drift ratio      SPSC ring       underrun,
   Opus)       evict oldest)      ±0.05 %)         f32 queue)      never repeats)
```

The playout thread is paced by the **device's** consumption, not wall-clock
time — the one thing that keeps latency from silently growing. Telemetry
back to the sender (lost/late counts, jitter target, buffer level, a
lower-bound latency estimate) is sent as 1 Hz status packets and shown in
both `wav-sender` and the phone UI.

## Current status

- Milestones 1–6: done (see `git log`), including adaptive buffering and
  Opus end-to-end **between Rust components**.
- **Verified on a real device** (iQOO, Android 16): phone → PC streaming over
  Wi-Fi with adaptive buffering works; this test found & fixed the Android
  15+ `RECORD_AUDIO` requirement for playback capture. Procedure stays in
  `docs/android-test.md` for future changes.
- **GitHub Releases:** push a `v*` tag → CI builds the Windows receiver, the
  Linux receiver (first compile confirmed in real CI), and a release-signed
  Android APK — see `.github/workflows/release.yml`.
- **Open items:** absolute end-to-end latency numbers (click-track test; the
  receiver reports a buffered-pipeline lower bound today); phone-side Opus
  encode would need NDK libopus (Android has no Opus encoder in MediaCodec).
- Out of scope by design: phone mic input, PC→phone audio, video sync,
  multi-phone, anything internet-facing, virtual audio drivers.

## Development rules

This project follows `CLAUDE.md` — notably: protocol changes land in the
same commit as `docs/protocol.md` updates; `cargo fmt` +
`cargo clippy -- -D warnings` must pass; no latency numbers without a
measurement.
