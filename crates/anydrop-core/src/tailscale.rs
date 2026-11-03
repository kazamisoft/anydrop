//! Tailscale peer discovery.
//!
//! mDNS multicast does not travel across a tailnet, so we ask the local
//! Tailscale client for its peer list (`tailscale status --json`) and probe each
//! online peer on the AnyDrop port. Traffic over Tailscale is already
//! end-to-end encrypted by WireGuard.

use crate::protocol::DeviceInfo;
use crate::transfer::probe;
use crate::{Peer, PeerSource};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    #[serde(default)]
    peer: HashMap<String, TsPeer>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct TsPeer {
    #[serde(default)]
    pub host_name: String,
    #[serde(rename = "DNSName", default)]
    pub dns_name: String,
    #[serde(rename = "OS", default)]
    pub os: String,
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<IpAddr>,
    #[serde(default)]
    pub online: bool,
}

fn candidate_binaries() -> Vec<String> {
    let mut v = vec!["tailscale".to_string()];
    if cfg!(target_os = "macos") {
        v.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
        v.push("/usr/local/bin/tailscale".into());
        v.push("/opt/homebrew/bin/tailscale".into());
    }
    if cfg!(windows) {
        v.push(r"C:\Program Files\Tailscale\tailscale.exe".into());
    }
    v
}

/// Parse the JSON produced by `tailscale status --json` (public for tests).
pub fn parse_status(json: &[u8]) -> Result<Vec<TsPeer>> {
    let st: Status = serde_json::from_slice(json).context("parsing tailscale status")?;
    Ok(st.peer.into_values().collect())
}

pub async fn list_peers() -> Result<Vec<TsPeer>> {
    let mut last_err = anyhow!("tailscale CLI not found");
    for bin in candidate_binaries() {
        match tokio::process::Command::new(&bin)
            .args(["status", "--json"])
            .output()
            .await
        {
            Ok(out) if out.status.success() => return parse_status(&out.stdout),
            Ok(out) => last_err = anyhow!("{bin}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => last_err = anyhow!("{bin}: {e}"),
        }
    }
    Err(last_err)
}

/// Probe every online tailnet peer on `port` and return the ones running AnyDrop.
pub async fn discover(me: &DeviceInfo, port: u16, timeout: Duration) -> Result<Vec<Peer>> {
    let peers = list_peers().await?;
    let mut tasks = Vec::new();
    for p in peers.into_iter().filter(|p| p.online) {
        // Prefer the 100.x IPv4 address.
        let Some(ip) = p
            .tailscale_ips
            .iter()
            .copied()
            .min_by_key(|ip| ip.is_ipv6())
        else {
            continue;
        };
        let me = me.clone();
        tasks.push(tokio::spawn(async move {
            let addr = SocketAddr::new(ip, port);
            probe(addr, &me, timeout).await.ok().map(|device| Peer {
                device,
                addrs: vec![addr],
                source: PeerSource::Tailscale,
            })
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(Some(p)) = t.await {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_json() {
        let json = br#"{
          "Self": {"HostName": "me"},
          "Peer": {
            "nodekey:abc": {"HostName": "MacBook-Pro", "DNSName": "macbook-pro.tail1234.ts.net.",
                            "OS": "macOS", "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"], "Online": true},
            "nodekey:def": {"HostName": "WIN-PC", "OS": "windows", "TailscaleIPs": ["100.64.0.2"], "Online": false}
          }
        }"#;
        let mut peers = parse_status(json).unwrap();
        peers.sort_by(|a, b| a.host_name.cmp(&b.host_name));
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].host_name, "MacBook-Pro");
        assert!(peers[0].online);
        assert_eq!(peers[0].tailscale_ips[0].to_string(), "100.101.102.103");
        assert!(!peers[1].online);
    }
}
