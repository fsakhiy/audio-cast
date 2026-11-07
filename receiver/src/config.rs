//! TOML config file. Precedence: CLI flag > config file > built-in default.
//!
//! Only fields the user actually set on the CLI override the file; that is
//! why `Effective` is assembled in main from `Option` CLI fields rather
//! than clap defaults.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Pairing token; senders must present it in the handshake. Empty = open.
    pub token: Option<String>,
    /// Address to bind TCP/UDP to. String so users can write "0.0.0.0".
    pub bind: Option<String>,
    pub tcp_port: Option<u16>,
    pub udp_port: Option<u16>,
    /// Substring of the output device name (same rule as --device).
    pub device: Option<String>,
    pub jitter_target_ms: Option<f64>,
    pub extra_delay_ms: Option<f64>,
    /// Advertise via mDNS (`_phonecast._tcp.local`). Default true.
    pub mdns: Option<bool>,
    /// Write logs here (useful when started at login with no console).
    pub log_file: Option<String>,
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        // Strip a UTF-8 BOM if a Windows editor added one; toml chokes on it.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let parsed: FileConfig =
            toml::from_str(text).with_context(|| format!("invalid TOML in {}", path.display()))?;
        Ok(parsed)
    }
}

/// Fully-resolved settings after merging CLI > file > defaults.
#[derive(Debug)]
pub struct Effective {
    pub token: String,
    pub bind: std::net::IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub device: Option<String>,
    pub jitter_target_ms: f64,
    pub extra_delay_ms: f64,
    pub mdns: bool,
    pub log_file: Option<String>,
}

impl Effective {
    pub fn build(cli: &CliOverrides, file: &FileConfig) -> Result<Self> {
        let bind_str = cli
            .bind
            .clone()
            .or_else(|| file.bind.clone())
            .unwrap_or_else(|| "0.0.0.0".into());
        let bind = bind_str
            .parse()
            .with_context(|| format!("invalid bind address {bind_str:?}"))?;
        let jitter = cli
            .jitter_target_ms
            .or(file.jitter_target_ms)
            .unwrap_or(30.0);
        if !(10.0..=200.0).contains(&jitter) {
            anyhow::bail!("jitter target must be between 10 and 200 ms (got {jitter})");
        }
        let extra = cli
            .extra_delay_ms
            .or(file.extra_delay_ms)
            .unwrap_or(0.0);
        if !(0.0..=2000.0).contains(&extra) {
            anyhow::bail!("extra delay must be between 0 and 2000 ms (got {extra})");
        }
        Ok(Self {
            token: cli.token.clone().or_else(|| file.token.clone()).unwrap_or_default(),
            bind,
            tcp_port: cli.tcp_port.or(file.tcp_port).unwrap_or(53_000),
            udp_port: cli.udp_port.or(file.udp_port).unwrap_or(53_001),
            device: cli.device.clone().or_else(|| file.device.clone()),
            jitter_target_ms: jitter,
            extra_delay_ms: extra,
            mdns: cli.mdns_off.map(|off| !off).or(file.mdns).unwrap_or(true),
            log_file: cli.log_file.clone().or_else(|| file.log_file.clone()),
        })
    }
}

/// The CLI fields that can be overridden by the config file: `None` means
/// "not passed", so the file (or default) wins.
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub token: Option<String>,
    pub bind: Option<String>,
    pub tcp_port: Option<u16>,
    pub udp_port: Option<u16>,
    pub device: Option<String>,
    pub jitter_target_ms: Option<f64>,
    pub extra_delay_ms: Option<f64>,
    pub mdns_off: Option<bool>,
    pub log_file: Option<String>,
}

/// The example config written by --write-example-config. Kept in sync with
/// the struct above; comments explain each key.
pub const EXAMPLE: &str = r#"# PhoneCast receiver configuration. Default location:
#   Windows: %APPDATA%\PhoneCast\config.toml
#   Linux:   ~/.config/phonecast/config.toml
# Every key is optional; CLI flags override this file.

# Pairing token senders must present. Empty or omitted = accept any sender
# (LAN trust). Set this if you share a network with strangers.
# token = "change-me"

# Substring of the output device name (see: phonecast-receiver --list).
# device = "headphones"

# Binding / ports (docs/protocol.md defaults).
# bind = "0.0.0.0"
# tcp_port = 53000
# udp_port = 53001

# Jitter buffer base target, ms, 10-200. Adaptive on top of this.
# jitter_target_ms = 30.0

# Manual lip-sync hold for video, ms, 0-2000.
# extra_delay_ms = 0.0

# Advertise the receiver via mDNS so the phone can discover it.
# mdns = true

# Log to this file instead of the console (for autostart).
# log_file = "phonecast.log"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_defaults() {
        let file: FileConfig = toml::from_str("").unwrap();
        let eff = Effective::build(&CliOverrides::default(), &file).unwrap();
        assert_eq!(eff.token, "");
        assert_eq!(eff.tcp_port, 53_000);
        assert_eq!(eff.udp_port, 53_001);
        assert_eq!(eff.bind.to_string(), "0.0.0.0");
        assert_eq!(eff.jitter_target_ms, 30.0);
        assert!(eff.mdns);
    }

    #[test]
    fn file_overrides_default_but_cli_overrides_file() {
        let file: FileConfig = toml::from_str("token = \"abc\"\ntcp_port = 60000\n").unwrap();
        let eff = Effective::build(&CliOverrides::default(), &file).unwrap();
        assert_eq!(eff.token, "abc");
        assert_eq!(eff.tcp_port, 60_000);

        let cli = CliOverrides {
            token: Some("xyz".into()),
            ..Default::default()
        };
        let eff = Effective::build(&cli, &file).unwrap();
        assert_eq!(eff.token, "xyz", "CLI must win");
        assert_eq!(eff.tcp_port, 60_000, "file fills what CLI omits");
    }

    #[test]
    fn rejects_unknown_keys() {
        // A typo'd key must fail loudly, not be silently ignored.
        assert!(toml::from_str::<FileConfig>("tokn = \"x\"").is_err());
    }

    #[test]
    fn example_config_is_valid() {
        let parsed: FileConfig = toml::from_str(EXAMPLE).unwrap();
        // all commented out => everything None
        assert_eq!(parsed, FileConfig::default());
    }

    #[test]
    fn mdns_off_flag_flips_correctly() {
        let file: FileConfig = toml::from_str("mdns = false").unwrap();
        let eff = Effective::build(&CliOverrides::default(), &file).unwrap();
        assert!(!eff.mdns);
        let cli = CliOverrides {
            mdns_off: Some(true),
            ..Default::default()
        };
        let eff = Effective::build(&cli, &FileConfig::default()).unwrap();
        assert!(!eff.mdns);
        // --no-mdns passed with mdns=true in file: flag wins
        let file: FileConfig = toml::from_str("mdns = true").unwrap();
        let eff = Effective::build(&cli, &file).unwrap();
        assert!(!eff.mdns);
    }

    #[test]
    fn invalid_bind_and_jitter_error_out() {
        let cli = CliOverrides {
            bind: Some("not-an-ip".into()),
            ..Default::default()
        };
        assert!(Effective::build(&cli, &FileConfig::default()).is_err());
        let cli = CliOverrides {
            jitter_target_ms: Some(5.0),
            ..Default::default()
        };
        assert!(Effective::build(&cli, &FileConfig::default()).is_err());
    }
}
