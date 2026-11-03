//! AnyDrop core library.
//!
//! - [`protocol`]  : wire format (length-prefixed JSON control frames + raw file bytes)
//! - [`discovery`] : mDNS/DNS-SD advertise & browse on the local network
//! - [`tailscale`] : peer discovery over a Tailscale tailnet (`tailscale status --json`)
//! - [`transfer`]  : sender / receiver engine
//! - [`paths`]     : cross-platform safe file name handling (Windows <-> macOS)
//!
//! GUI front-ends (egui/Tauri) are expected to link this crate directly.

pub mod discovery;
pub mod paths;
pub mod protocol;
pub mod tailscale;
pub mod transfer;

pub use protocol::{DeviceInfo, FileEntry, DEFAULT_PORT, PROTOCOL_VERSION, SERVICE_TYPE};

/// Information about a peer that can receive files.
#[derive(Debug, Clone)]
pub struct Peer {
    pub device: DeviceInfo,
    pub addrs: Vec<std::net::SocketAddr>,
    pub source: PeerSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSource {
    Lan,
    Tailscale,
    Manual,
}

impl std::fmt::Display for PeerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PeerSource::Lan => "lan",
            PeerSource::Tailscale => "tailscale",
            PeerSource::Manual => "manual",
        })
    }
}
