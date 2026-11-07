//! mDNS advertising (milestone 5): announce `_phonecast._tcp.local` so a
//! sender can discover the receiver instead of typing an IP.
//!
//! The SRV record carries the TCP control port; TXT keys (docs/protocol.md)
//! carry the UDP port, protocol version, and whether a token is required.

use std::net::IpAddr;

use anyhow::{Context as _, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

pub const SERVICE_TYPE: &str = "_phonecast._tcp.local.";

/// One instance per advertised IPv4 address. mdns-sd answers queries per
/// interface, so a multi-homed PC (Wi-Fi + Ethernet) is discoverable on both.
pub fn advertise(
    ipv4s: &[IpAddr],
    tcp_port: u16,
    udp_port: u16,
    token_set: bool,
) -> Result<(ServiceDaemon, Vec<String>)> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;
    let host = hostname_label();
    let mut names = Vec::new();
    for ip in ipv4s {
        let instance = format!("PhoneCast @ {host}");
        // docs/protocol.md TXT keys.
        let mut props = std::collections::HashMap::new();
        props.insert("ver".to_string(), "1".to_string());
        props.insert("udp".to_string(), udp_port.to_string());
        props.insert(
            "auth".to_string(),
            if token_set {
                "1".to_string()
            } else {
                "0".to_string()
            },
        );
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &format!("{host}.local."),
            ip.to_string(),
            tcp_port,
            props,
        )
        .with_context(|| format!("bad service info for {ip}"))?;
        daemon
            .register(info)
            .with_context(|| format!("failed to advertise on {ip}"))?;
        names.push(format!("{instance} ({ip})"));
    }
    Ok((daemon, names))
}

/// Short, DNS-label-safe machine name for the instance/host records.
fn hostname_label() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let cleaned: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        "phonecast".to_owned()
    } else {
        cleaned.chars().take(30).collect()
    }
}

/// LAN IPv4 addresses to advertise. With a concrete `bind` (not
/// 0.0.0.0/any) that address alone is announced.
pub fn advertise_ips(bind: IpAddr) -> Vec<IpAddr> {
    if !bind.is_unspecified() {
        return vec![bind];
    }
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter(|i| !i.is_loopback())
            .filter_map(|i| match i.ip() {
                IpAddr::V4(v4) => Some(IpAddr::V4(v4)),
                IpAddr::V6(_) => None,
            })
            .collect(),
        Err(err) => {
            tracing::warn!("interface enumeration failed: {err}; mDNS off");
            Vec::new()
        }
    }
}
