//! OS-specific glue, isolated per the project rule: autostart, config
//! locations, firewall guidance. Everything here is gated by
//! `cfg(target_os)`; the pure command/contents builders are unit-tested
//! (executing them obviously requires the target OS).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// Default config file location, or None on unsupported OSes (then only
/// --config works).
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("PhoneCast").join("config.toml"))
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("phonecast").join("config.toml"))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// The Windows registry command that registers (or overwrites) the Run-key
/// autostart entry. Pure builder so the shape is testable; `run` executes.
#[cfg(windows)]
pub(crate) fn autostart_reg_command(exe: &Path, args: &[String]) -> Vec<String> {
    let value = format!("\"{}\" {}", exe.display(), args.join(" "));
    vec![
        "reg".into(),
        "add".into(),
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run".into(),
        "/v".into(),
        "PhoneCast".into(),
        "/t".into(),
        "REG_SZ".into(),
        "/d".into(),
        value,
        "/f".into(),
    ]
}

#[cfg(windows)]
pub(crate) fn autostart_reg_delete_command() -> Vec<String> {
    vec![
        "reg".into(),
        "delete".into(),
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run".into(),
        "/v".into(),
        "PhoneCast".into(),
        "/f".into(),
    ]
}

/// systemd user unit contents for Linux autostart. Pure builder, testable.
#[cfg(not(windows))]
pub(crate) fn systemd_user_unit(exe: &Path, args: &[String]) -> String {
    format!(
        "[Unit]\n\
         Description=PhoneCast audio receiver\n\
         After=network-online.target sound.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} {args}\n\
         Restart=on-failure\n\
         RestartSec=3\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        args = args.join(" "),
    )
}

#[cfg(not(windows))]
pub(crate) fn systemd_user_unit_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Some(
        base.join("systemd")
            .join("user")
            .join("phonecast-receiver.service"),
    )
}

/// Install login autostart for the receiver. On both OSes the strategy is
/// "start with --config <default config>" so behavior is user-tunable
/// without touching the autostart entry; ensure the config file exists by
/// writing the example if absent.
pub fn install_autostart() -> Result<String> {
    let exe = std::env::current_exe().context("could not determine own executable path")?;
    let cfg = default_config_path()
        .context("no config location known on this OS; --config must be passed some other way")?;
    if let Some(dir) = cfg.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    if !cfg.exists() {
        std::fs::write(&cfg, crate::config::EXAMPLE)
            .with_context(|| format!("failed to write example config to {}", cfg.display()))?;
    }
    let args = vec!["--config".into(), cfg.display().to_string()];

    #[cfg(windows)]
    {
        // HKCU Run key: per-user, survives logout, needs no admin.
        let cmd = autostart_reg_command(&exe, &args);
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .context("failed to run reg.exe")?;
        if !status.success() {
            bail!("reg.exe failed with {status}; registry entry not created");
        }
        Ok(format!(
            "PhoneCast added to login autostart (HKCU Run key).\nConfig: {}\nRemove with: phonecast-receiver --remove-autostart\n\
             Note: inbound UDP 53001 / TCP 53000 must be allowed on Private networks:",
            cfg.display()
        ))
    }
    #[cfg(not(windows))]
    {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            bail!(
                "no user D-Bus session found; systemd user services need a login session (or install packaging/phonecast-receiver.service manually)"
            );
        }
        let path = systemd_user_unit_path().context("HOME not set")?;
        std::fs::create_dir_all(path.parent().unwrap())
            .with_context(|| format!("failed to create {}", path.parent().unwrap().display()))?;
        std::fs::write(&path, systemd_user_unit(&exe, &args))
            .with_context(|| format!("failed to write {}", path.display()))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run(
            "systemctl",
            &["--user", "enable", "--now", "phonecast-receiver.service"],
        )?;
        Ok(format!(
            "systemd user service enabled and started.\nUnit: {}\nConfig: {}\nLogs: journalctl --user -u phonecast-receiver\nRemove with: phonecast-receiver --remove-autostart",
            path.display(),
            cfg.display()
        ))
    }
}

/// Reverse of [`install_autostart`]; tolerant of "was never installed".
pub fn remove_autostart() -> Result<String> {
    #[cfg(windows)]
    {
        let cmd = autostart_reg_delete_command();
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .context("failed to run reg.exe")?;
        if !status.success() {
            // reg delete returns failure if the value is absent; treat as
            // already-clean rather than erroring.
            return Ok("PhoneCast was not in the Run key (nothing removed).".into());
        }
        Ok("PhoneCast removed from login autostart.".into())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "phonecast-receiver.service"])
            .status();
        if let Some(path) = systemd_user_unit_path() {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }
        run("systemctl", &["--user", "daemon-reload"])?;
        Ok("systemd user service disabled and unit file removed.".into())
    }
}

#[cfg(not(windows))]
fn run(prog: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {prog}"))?;
    if !status.success() {
        bail!("{prog} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(windows)]
pub fn firewall_hint(udp_port: u16, tcp_port: u16) -> String {
    // Shown on every console start on Windows; the receiver is useless
    // without the inbound rules on most machines. Private profile only.
    format!(
        "if audio does not arrive, allow the ports on Private networks (admin PowerShell):\n  \
         netsh advfirewall firewall add rule name=PhoneCast-UDP dir=in action=allow protocol=UDP localport={udp_port} profile=private\n  \
         netsh advfirewall firewall add rule name=PhoneCast-TCP dir=in action=allow protocol=TCP localport={tcp_port} profile=private"
    )
}

#[cfg(not(windows))]
pub fn firewall_hint(_udp_port: u16, _tcp_port: u16) -> String {
    String::new()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn reg_command_quoting_and_shape() {
        let cmd = autostart_reg_command(
            Path::new(r"C:\Program Files\PhoneCast\phonecast-receiver.exe"),
            &[
                "--config".into(),
                r"C:\Users\u\AppData\Roaming\PhoneCast\config.toml".into(),
            ],
        );
        assert_eq!(cmd[0], "reg");
        assert_eq!(cmd[1], "add");
        assert!(cmd.iter().any(|a| a.ends_with(r"\Run")));
        assert!(cmd.iter().any(|a| a == "PhoneCast"));
        let value = &cmd[cmd.len() - 2];
        assert!(
            value.starts_with(r#""C:\Program Files"#),
            "exe path must be quoted: {value}"
        );
        assert!(value.contains("--config"));
    }

    #[test]
    fn config_path_uses_appdata() {
        // Environment-dependent; just assert the shape when APPDATA exists.
        if let Some(p) = default_config_path() {
            assert_eq!(p.file_name().unwrap(), "config.toml");
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn unit_contents_have_execstart_and_restart() {
        let unit = systemd_user_unit(
            Path::new("/home/u/.cargo/bin/phonecast-receiver"),
            &[
                "--config".into(),
                "/home/u/.config/phonecast/config.toml".into(),
            ],
        );
        assert!(unit.contains("ExecStart=/home/u/.cargo/bin/phonecast-receiver --config /home/u/.config/phonecast/config.toml"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("[Install]"));
    }
}
