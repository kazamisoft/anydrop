//! LAN discovery via mDNS / DNS-SD (`_anydrop._tcp.local.`).
//!
//! Works on Windows 10+, macOS (Intel / Apple Silicon) and Linux without
//! Bonjour being installed: `mdns-sd` is a pure-Rust implementation.
//! Note: mDNS is link-local multicast and does **not** cross Tailscale; see
//! [`crate::tailscale`] for that.

use crate::protocol::{DeviceInfo, SERVICE_TYPE};
use crate::{Peer, PeerSource};
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Keeps the service registered while alive.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn start(device: &DeviceInfo, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        // Instance names must be unique on the link: add a short id suffix.
        let instance = format!("{}-{}", sanitize_label(&device.name), &device.id[..6]);
        let host = format!("{}.local.", sanitize_label(&device.name));
        let props = [
            ("id", device.id.as_str()),
            ("name", device.name.as_str()),
            ("os", device.os.as_str()),
            ("ver", "1"),
        ];
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, &props[..])?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
        let _ = self.daemon.shutdown();
    }
}

fn sanitize_label(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if out.trim_matches('-').is_empty() {
        "anydrop".into()
    } else {
        out
    }
}

/// Browse the LAN for `timeout` and return the peers found (excluding `self_id`).
/// Blocking — call from `spawn_blocking` in async code.
pub fn browse_blocking(timeout: Duration, self_id: Option<&str>) -> Result<Vec<Peer>> {
    let daemon = ServiceDaemon::new()?;
    let rx = daemon.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found: HashMap<String, Peer> = HashMap::new();

    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let id = info
                    .get_property_val_str("id")
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() || Some(id.as_str()) == self_id {
                    continue;
                }
                let port = info.get_port();
                // Prefer IPv4, then IPv6; skip link-local v6 (needs scope id).
                let mut addrs: Vec<SocketAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|ip| ip.to_ip_addr())
                    .filter(|ip| match ip {
                        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
                        _ => true,
                    })
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect();
                addrs.sort_by_key(|a| (a.is_ipv6(), a.ip().is_loopback()));
                let device = DeviceInfo {
                    id: id.clone(),
                    name: info.get_property_val_str("name").unwrap_or("?").to_string(),
                    os: info.get_property_val_str("os").unwrap_or("?").to_string(),
                };
                found.insert(
                    id,
                    Peer {
                        device,
                        addrs,
                        source: PeerSource::Lan,
                    },
                );
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    let mut v: Vec<Peer> = found.into_values().collect();
    v.sort_by(|a, b| a.device.name.cmp(&b.device.name));
    Ok(v)
}

pub async fn browse(timeout: Duration, self_id: Option<String>) -> Result<Vec<Peer>> {
    tokio::task::spawn_blocking(move || browse_blocking(timeout, self_id.as_deref())).await?
}
