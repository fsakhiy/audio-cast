# Running the receiver at login

Both paths are no-admin, per-user, and per-OS-isolated in
`receiver/src/platform.rs`. Pick yours.

## Windows 10/11

1. Build: `cargo build --release -p phonecast-receiver`
   (needs CMake on PATH for the opus crate, e.g. `~/tools/cmake/bin`).
2. Install autostart (writes the `HKCU\...\Run` value `PhoneCast`):
   ```
   .\target\release\phonecast-receiver.exe --install-autostart
   ```
   This also creates `%APPDATA%\PhoneCast\config.toml` if missing — edit
   it (device, token, ports, log file) and restart the process to apply.
3. Remove again: `phonecast-receiver.exe --remove-autostart`.

**Firewall (once, admin PowerShell):** inbound on Private networks only:
```
netsh advfirewall firewall add rule name=PhoneCast-UDP dir=in action=allow protocol=UDP localport=53001 profile=private
netsh advfirewall firewall add rule name=PhoneCast-TCP dir=in action=allow protocol=TCP localport=53000 profile=private
```
(The receiver also prints these two lines at every console start.)

A started-but-headless receiver writes nothing to a console; set
`log_file = "phonecast.log"` in the config to keep a trail (it lands next
to the binary when started from the Run key — or give an absolute path).

## Linux

Either let the receiver manage a systemd **user** service:
```
cargo build --release -p phonecast-receiver
./target/release/phonecast-receiver --install-autostart
```
or install the example unit manually — see the comments in
`packaging/phonecast-receiver.service`.

Check: `systemctl --user status phonecast-receiver`,
logs: `journalctl --user -u phonecast-receiver`. User services start at
login; enable lingering (`loginctl enable-linger $USER`) if you want it
running before login.

## Config file

Default locations:

- Windows: `%APPDATA%\PhoneCast\config.toml`
- Linux: `~/.config/phonecast/config.toml`

Precedence: **CLI flag > config file > default**. `--write-example-config`
produces a commented template. Unknown keys are rejected on purpose — a
typo'd option should fail loudly, not be ignored.

## Pairing token

`token = "..."` in the config (or `--token`). Senders presenting a
different token get `Status::TokenRejected` and a visible error. With no
token set the receiver trusts the LAN; mDNS TXT then advertises `auth=0`,
so the phone UI can warn you before connecting. Discovery of the receiver
itself is via mDNS (`_phonecast._tcp.local`) — see docs/protocol.md.

## Not (yet) provided

- System tray icon: the background-process autostart route is what's
  implemented; tray is optional milestone-6 polish.
- Machine-wide (all users) autostart — deliberately out of scope; it
  would need admin rights.
