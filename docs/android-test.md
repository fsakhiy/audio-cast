# Testing the Android sender on a real phone (milestone 3)

Everything in milestone 3 that involves actual audio capture can only be
verified on the phone. This is the exact procedure.

## Prerequisites

1. PC and phone on the **same Wi-Fi network** (a phone hotspot / AP-isolation
   network will break UDP between them).
2. Find the PC's LAN address on Windows: `ipconfig` → look for the Wi-Fi or
   Ethernet adapter IPv4, e.g. `192.168.1.20`.
3. Start the receiver on the PC:
   ```
   cargo run -p phonecast-receiver
   ```
   Note the `listening` log lines. Windows may show a firewall prompt — click
   Allow on **Private networks** only. If it doesn't ask and audio doesn't
   arrive, add the rule manually (PowerShell, no admin needed only if the
   profile allows it; usually requires admin):
   ```
   netsh advfirewall firewall add rule name="PhoneCast" dir=in action=allow protocol=UDP localport=53001
   netsh advfirewall firewall add rule name="PhoneCast-TCP" dir=in action=allow protocol=TCP localport=53000
   ```
4. Install the APK on the phone (USB install via `adb install`, or copy the
   file from `android/app/build/outputs/apk/debug/app-debug.apk` and open it —
   allow "install unknown apps" for that source).

## On the phone

1. Open **PhoneCast**, enter the PC's address (e.g. `192.168.1.20`), leave the
   ports at 53000/53001, token empty for now.
2. Press **Start**. The system shows the "Start broadcasting what's on your
   screen?" consent dialog → **Start now**. (This dialog appears every
   session; that is required behavior on Android 14+, not a bug.)
3. Open any music/video app (YouTube Music, Spotify, a local player) and
   play audio.
4. You should hear the phone's audio from the PC's default output device
   within ~1 second, mixed with anything the PC plays.

## Expected outcomes & failure modes

| Symptom | Likely cause |
|---|---|
| "Failed: receiver rejected handshake: token rejected" | receiver started with `--token X`, phone token field empty/different |
| "Failed: connection refused / timeout" | wrong PC address, receiver not running, or Windows Firewall blocking TCP 53000 |
| Phone UI shows "Streaming…" but the status card says **No audio captured for 3+ s** | the app you're playing blocks playback capture (DRM apps do this). Try a different app or a local music player |
| Audio plays a few seconds then stops, receiver logs "session closed" | the phone killed the app in the background (vivo/Funtouch aggressive management) — milestone 6 adds the guided workarounds; interim: lock PhoneCast in Recents and set battery usage to "Unrestricted" |
| Receiver log shows rising `lost` | Wi-Fi congestion; jitter buffer arrives in milestone 4 |

## What to check on the receiver side

The receiver logs at session close and every 5 s of trouble:

```
session open ... rate=48000 channels=2 frame_size=240
network stats  lost=... reordered=... overrun=... bad=...
session closed ... lost=0 reordered=0 ... underruns=...
```

Report those numbers when something looks wrong — they identify whether the
problem is the network (lost), the phone (underruns without loss), or the
buffering (reorder/overrun).

## Known not-tested items

- Everything in this document — the app compiles on this machine, but no
  device/emulator was available here, so no claim is made that any runtime
  behavior above works until you run it.
- Latency is not measured yet (milestone 4 adds the click-track test).
