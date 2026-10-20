use anyhow::{anyhow, Context, Result};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Command, Stdio};

use crate::config::Config;

pub fn service_bind_address(config: &Config) -> Result<String> {
    match config.network_access.as_str() {
        "lan" => Ok("0.0.0.0".to_owned()),
        "tailscale" => tailscale_ipv4().map(|address| address.to_string()),
        _ => Ok("127.0.0.1".to_owned()),
    }
}

/// Probe wildcard-bound services through loopback, but probe Tailscale-only
/// mode through its specific private interface.
pub fn service_probe_address(config: &Config) -> Result<String> {
    match config.network_access.as_str() {
        "tailscale" => service_bind_address(config),
        _ => Ok("127.0.0.1".to_owned()),
    }
}

pub fn service_access_host_for(config: &Config, managed_by_syncpulse: bool) -> Result<String> {
    if !managed_by_syncpulse {
        return Ok("127.0.0.1".to_owned());
    }
    match config.network_access.as_str() {
        // A local browser/WebView can still use loopback if no LAN route is
        // currently available; advertised phone links remain unavailable.
        "lan" => Ok(lan_ipv4()
            .map(|address| address.to_string())
            .unwrap_or_else(|| "127.0.0.1".to_owned())),
        "tailscale" => tailscale_ipv4().map(|address| address.to_string()),
        _ => Ok("127.0.0.1".to_owned()),
    }
}

pub fn service_probe_address_for(config: &Config, managed_by_syncpulse: bool) -> Result<String> {
    if managed_by_syncpulse {
        service_probe_address(config)
    } else {
        Ok("127.0.0.1".to_owned())
    }
}

pub fn service_probe_address_for_configured_mode(config: &Config) -> String {
    service_probe_address(config).unwrap_or_else(|_| "127.0.0.1".to_owned())
}

pub fn service_advertised_host(config: &Config, managed_by_syncpulse: bool) -> Option<String> {
    if !managed_by_syncpulse {
        return Some("127.0.0.1".to_owned());
    }
    match config.network_access.as_str() {
        "lan" => lan_ipv4().map(|address| address.to_string()),
        "tailscale" => tailscale_ipv4().ok().map(|address| address.to_string()),
        _ => Some("127.0.0.1".to_owned()),
    }
}

pub fn tailscale_ipv4() -> Result<Ipv4Addr> {
    let output = Command::new("tailscale")
        .args(["ip", "-4"])
        .stdin(Stdio::null())
        .output()
        .context("Tailscale is required for private Tailscale service access")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Tailscale is not connected. Sign in to Tailscale before starting services in Tailscale-only mode."
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .ok_or_else(|| anyhow!("Tailscale did not return a usable IPv4 address"))
}

pub fn lan_ipv4() -> Option<Ipv4Addr> {
    if let Ok(output) = Command::new("ip")
        .args(["-4", "route", "get", "1.1.1.1"])
        .stdin(Stdio::null())
        .output()
    {
        if output.status.success() {
            let output_text = String::from_utf8_lossy(&output.stdout);
            let words: Vec<_> = output_text.split_whitespace().collect();
            if let Some(source_index) = words.iter().position(|word| *word == "src") {
                if let Some(address) = words
                    .get(source_index + 1)
                    .and_then(|value| value.parse::<Ipv4Addr>().ok())
                    .filter(|address| !address.is_loopback() && !address.is_unspecified())
                {
                    return Some(address);
                }
            }
        }
    }
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
    socket.connect(SocketAddr::from(([1, 1, 1, 1], 53))).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_loopback() && !address.is_unspecified() => {
            Some(address)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_mode_is_loopback_and_lan_mode_is_explicit_all_interfaces() {
        let local = Config::default();
        assert_eq!(service_bind_address(&local).unwrap(), "127.0.0.1");

        let lan = Config {
            network_access: "lan".to_owned(),
            ..Config::default()
        };
        assert_eq!(service_bind_address(&lan).unwrap(), "0.0.0.0");
        assert_eq!(service_probe_address(&lan).unwrap(), "127.0.0.1");
    }
}
