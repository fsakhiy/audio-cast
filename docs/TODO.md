# PhoneCast — deferred ideas & development notes

Not urgent, not forgotten. Everything here is *optional next work* on top of
the working system; the project is deliberately parked at "current state
works" until something below earns priority.

Last updated: 2026-11-07, after the first successful real phone→PC session
(iQOO I2218, Android 16) and the "can we look like a media output?" design
discussion.

---

## 1. Make Android treat the PC as a real media output (the big idea)

**Goal:** instead of capturing playback with a consent dialog every session,
make the phone *route* audio to us like it routes to a speaker — zero app
software on the phone, no dialog, works with DRM/opt-out apps.

**Hard constraint discovered:** third-party apps can never register an
Android audio output device; routing devices come from the system audio
HAL. So every option is a system-level trick. Options, ranked:

1. **PC as Bluetooth A2DP sink** (the dream)
   - Windows: DEAD END. Microsoft removed A2DP *sink* from the BT stack
     (old Win10 had "listen to audio"; Win11 does not). No public API.
   - Linux: VIABLE. BlueZ A2DP sink / `bluepulse` makes a Linux box
     discoverable as a BT speaker; feed those samples into our existing
     jitter/playout engine. Would be a "native mode" of the Linux receiver.

2. **USB audio gadget relay** (favorite — best fit with PhoneCast) ⭐
   - A ~$15 Pi Zero 2 W (or ESP32-S3 with USB device stack) plugs into the
     phone's USB-C and enumerates as a USB DAC. Android shows "wired
     headphones/DAC" and routes *everything* natively — no consent dialog,
     no capture opt-out, DRM works.
   - The gadget captures its own playback stream and relays PCM frames to
     the PC receiver over Wi-Fi. PhoneCast's receiver protocol needs no
     change ("some sender streams PCM"); the Android app becomes optional.
   - Main unknown to spike: some Funtouch/Origin builds may require the
     gadget to declare headphone-class descriptors to auto-switch. One
     evening with a Pi answers it. Measure latency while there.

3. **Analog cheat: USB-C→3.5mm adapter + PC line-in** (~$8)
   - Phone thinks headphones attached; PC records the jack and mixes to
     output. Works today, no software on phone.
   - Big caveat for THIS project: recording line-in means opening an input
     device on Windows, which CLAUDE.md bans because it can force BT
     headphones into the hands-free (low-quality) profile. Only acceptable
     if the PC's output is wired/USB. Ugly; probably skip.

4. **MediaRouteProvider ("Bluetooth + Cast" entry)**
   - The only supported way a 3rd-party app appears in Android's cast menu.
   - Per-app cooperation only (YouTube/Spotify/Chrome yes; games/system
     sounds no) → does not replace capture, could only polish it.

5. **LE Audio / Auracast broadcast**
   - Phone broadcasts LC3; PC adapter receives at OS level. Windows 11
     24H2+ supports Auracast *receive* on supported chipsets; Android has
     broadcast *source* but not exposed to third-party apps yet.
   - Watch this space; revisit ~yearly.

Rule check: none of 1–3 violates the original "no A2DP between phone and
PC" constraint — the phone connects to dumb hardware (gadget/adapter), our
transport stays the LAN, and the user's headphones stay attached to the PC.

---

## 2. Smaller deferred items

- **Phone-side Opus encode (NDK libopus).** Android MediaCodec has no Opus
  *encoder*, so this needs the NDK + CMake cross builds. Value: ~5× fewer
  packets → the real fix for weak/bursty Wi-Fi (first real session showed
  bursty-lossless stutters; underrun-adaptive target now compensates, Opus
  would reduce the load itself). Phone keeps streaming PCM meanwhile —
  fully interoperable.
- **System tray icon** for the receiver (start/stop/status menu). Skipped in
  M6 on purpose: needs a GUI event-loop dependency (`tray-icon` crate is the
  lightweight route). Interim answer: autostart + `log_file` + console run.
- **Click-track absolute latency measurement.** The generated test WAV has
  per-second click bursts designed for this; receiver-side numbers are
  buffered-pipeline estimates only. Needs phone-in-hand session (~2 min with
  a stopwatch or a second phone recording).
- ~~**Linux build confirmation**~~ — ✅ DONE via CI: the v0.1.0 release run
  compiles `phonecast-receiver` on ubuntu-latest (alsa-sys + libopus/CMake
  both fine). `docs/linux-verify.md` remains for audible on-desktop checks.
- ~~**Release keystore**~~ — ✅ DONE: CI signs `app-release.apk` from GitHub
  secrets (keystore lives in `~/tools/phonecast-release`, OUTSIDE git —
  back it up; losing it means users must uninstall+reinstall to upgrade).
- **`--install-autostart` was never executed** (registry/systemd writes are
  user decisions). Command builders are unit-tested; first real use will
  confirm vivo-era Windows behavior. Also consider deleting the extra
  all-profile firewall rule Windows created: `netsh advfirewall firewall
  delete rule name=PhoneCast-UDP profile=public` + same for Domain.
- **Vivo guide deep links unverified:** the in-app guide tries known
  vivo/iQOO component names and reports which opened. Nobody has walked
  through it on the real device yet (Android 16 UI may differ from the
  dontkillmyapp.com docs it was written from).
- **Reconnect under screen-off / OEM killer:** implemented (FGS-before-
  consent, backoff, wake+wifi locks) and survived ad-hoc use, but no
  formal "3–5 min screen off with music" run recorded yet.

## 3. Known rough edges (fine to live with, listed for honesty)

- Receiver level equilibrates at jitter-target + ~48 ms ring seed + the
  phone's startup backlog; on bursty Wi-Fi the adaptive target climbs and
  slowly decays after calm. Feels like "150–250 ms delay". Tunable via
  `--jitter-target-ms`; only Opus (above) reduces it structurally.
- `produced_real` vs `consumed` accounting shows a small constant offset
  (starvation padding); cosmetic in logs only.
- UDP session source address = last packet's; a sender that changes ports
  mid-session (some OEM Wi-Fi power saving) would retarget telemetry but
  audio keeps flowing. Fine.
- Release exe on PATH (`C:\Users\fairuz\tools\bin`) is a manual copy —
  rebuilds need `copy target\release\phonecast-receiver.exe C:\Users\fairuz\tools\bin\`.
  A cargo alias or update script would fix this.

## 4. Where we stopped

Milestones 1–6 complete (see `git log`; 22 commits). Working as of
2026-11-07: phone → PC over Wi-Fi with adaptive buffering, live stats
overlay, reconnect logic, autostart tooling, mDNS discovery, README.
User chose to settle here. Nothing below section 1 starts without a word.
