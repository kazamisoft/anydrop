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
use std::sync::{Arc, Mutex};
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
                if let Some(p) = peer_from(&info) {
                    if Some(p.device.id.as_str()) != self_id {
                        found.insert(p.device.id.clone(), p);
                    }
                }
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

fn peer_from(info: &mdns_sd::ResolvedService) -> Option<Peer> {
    let id = info
        .get_property_val_str("id")
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return None;
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
    addrs.sort_by_key(|a| (a.is_ipv6(), a.ip().is_loopback(), *a));
    addrs.dedup();
    let device = DeviceInfo {
        id,
        name: info.get_property_val_str("name").unwrap_or("?").to_string(),
        os: info.get_property_val_str("os").unwrap_or("?").to_string(),
    };
    Some(Peer {
        device,
        addrs,
        source: PeerSource::Lan,
    })
}

pub async fn browse(timeout: Duration, self_id: Option<String>) -> Result<Vec<Peer>> {
    tokio::task::spawn_blocking(move || browse_blocking(timeout, self_id.as_deref())).await?
}

/// Long-lived LAN browser: one mDNS daemon that keeps listening and updates the
/// peer set only when devices appear (announce) or disappear (goodbye / TTL
/// expiry). Use this in long-running apps instead of polling [`browse`].
pub struct Browser {
    daemon: ServiceDaemon,
    peers: Arc<Mutex<HashMap<String, Peer>>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl Browser {
    pub fn start(on_change: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let b = Self {
            daemon: ServiceDaemon::new()?,
            peers: Arc::default(),
            on_change,
        };
        b.spawn_listener()?;
        Ok(b)
    }

    fn spawn_listener(&self) -> Result<()> {
        let rx = self.daemon.browse(SERVICE_TYPE)?;
        let peers = self.peers.clone();
        let on_change = self.on_change.clone();
        std::thread::Builder::new()
            .name("anydrop-mdns".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let changed = match ev {
                        ServiceEvent::ServiceResolved(info) => match peer_from(&info) {
                            Some(p) => {
                                let mut m = peers.lock().unwrap();
                                let key = info.get_fullname().to_string();
                                let same = m.get(&key).is_some_and(|old| {
                                    old.device == p.device && old.addrs == p.addrs
                                });
                                m.insert(key, p);
                                !same
                            }
                            None => false,
                        },
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            peers.lock().unwrap().remove(&fullname).is_some()
                        }
                        ServiceEvent::SearchStopped(_) => break,
                        _ => false,
                    };
                    if changed {
                        on_change();
                    }
                }
            })?;
        Ok(())
    }

    /// Re-send mDNS queries now (e.g. user pressed "refresh"). Known peers are kept.
    pub fn refresh(&self) -> Result<()> {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        self.spawn_listener()
    }

    /// Current peers, de-duplicated by device id, sorted by name.
    pub fn peers(&self) -> Vec<Peer> {
        let m = self.peers.lock().unwrap();
        let mut by_id: HashMap<String, Peer> = HashMap::new();
        for p in m.values() {
            by_id
                .entry(p.device.id.clone())
                .or_insert_with(|| p.clone());
        }
        let mut v: Vec<Peer> = by_id.into_values().collect();
        v.sort_by(|a, b| {
            a.device
                .name
                .to_lowercase()
                .cmp(&b.device.name.to_lowercase())
                .then(a.device.id.cmp(&b.device.id))
        });
        v
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}
