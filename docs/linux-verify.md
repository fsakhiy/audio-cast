# Verifying the receiver build on Linux

The milestone-1 target includes "confirm it also builds on Linux". This could
not be verified from the Windows dev machine (no Linux environment available;
`alsa-sys` — cpal's Linux backend — cannot cross-compile without a Linux
sysroot). Run these steps once on any Linux box (or a container):

```sh
# Debian/Ubuntu dependencies for cpal's ALSA backend
sudo apt install pkg-config libasound2-dev

cd <repo>
cargo test                              # tone generator unit tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -- --list                     # enumerate output devices
cargo run -- --device <name-part> --duration 5   # play the test tone
```

Expected: same behavior as Windows — device list, default/`--device`
selection, tone playback to the chosen sink, clean errors for bad configs.

Notes:

- WSL2 alone is **not** enough for the playback line: WSL has no audio
  output device by default, so `--list` will show nothing (or a dummy). Use
  WSL only for the build/test/clippy steps, and a real Linux session
  (PipeWire/ALSA) for the audible check.
- WSL2 install, if desired: `wsl --install` in an elevated PowerShell, then
  inside it `curl https://sh.rustup.rs -sSf | sh`.
