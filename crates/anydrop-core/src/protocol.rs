//! AnyDrop wire protocol (v1)
//!
//! Every control message is a *frame*: `u32 big-endian length` + `UTF-8 JSON`.
//! File contents are streamed as raw bytes between frames (length known from the Offer).
//!
//! ```text
//! Sender                                   Receiver
//!   | -- Hello{version, device} ----------->  |
//!   | <------------ HelloAck{version, device} |
//!   | -- Offer{files[], total_size} -------->  |   (ping/probe closes the socket here)
//!   | <-------------- Accept | Reject{reason}  |   (receiver may ask the user)
//!   | == for each file i:                      |
//!   |    raw bytes (files[i].size) ---------->  |
//!   |    FileEnd{index, sha256} ------------->  |   (receiver verifies SHA-256)
//!   | <---------------- Done | Error{message}  |
//! ```

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
/// Default TCP port. Open this port in the firewall on the receiving side.
pub const DEFAULT_PORT: u16 = 47820;
/// DNS-SD service type used for LAN discovery.
pub const SERVICE_TYPE: &str = "_anydrop._tcp.local.";
/// Upper bound for a control frame (offer of ~100k files fits easily).
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Random per-process id, used to filter out ourselves during discovery.
    pub id: String,
    /// Human readable name (defaults to hostname).
    pub name: String,
    /// e.g. "macos-aarch64", "windows-x86_64"
    pub os: String,
}

impl DeviceInfo {
    pub fn local(name: Option<String>) -> Self {
        let name = name.unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "anydrop-device".into())
        });
        Self {
            id: random_id(),
            name,
            os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }
}

fn random_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    /// Relative path using '/' as separator, NFC normalized. e.g. "photos/2024/a.jpg"
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        version: u32,
        device: DeviceInfo,
    },
    HelloAck {
        version: u32,
        device: DeviceInfo,
    },
    Offer {
        files: Vec<FileEntry>,
        total_size: u64,
    },
    Accept,
    Reject {
        reason: String,
    },
    FileEnd {
        index: u32,
        sha256: String,
    },
    Done,
    Error {
        message: String,
    },
}

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        bail!("frame too large ({} bytes)", body.len());
    }
    w.write_u32(body.len() as u32).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Returns `Ok(None)` on clean EOF before a frame starts.
pub async fn read_msg_opt<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Message>> {
    let len = match r.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if len > MAX_FRAME {
        bail!("peer sent oversized frame ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.context("reading frame body")?;
    let msg = serde_json::from_slice(&buf).context("decoding frame")?;
    Ok(Some(msg))
}

pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message> {
    read_msg_opt(r)
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed by peer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = Message::Offer {
            files: vec![FileEntry {
                path: "한글/파일.txt".into(),
                size: 42,
            }],
            total_size: 42,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).await.unwrap();
        let mut rd = &buf[..];
        assert_eq!(read_msg(&mut rd).await.unwrap(), msg);
        assert!(read_msg_opt(&mut rd).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let buf = (MAX_FRAME as u32 + 1).to_be_bytes();
        let mut rd = &buf[..];
        assert!(read_msg(&mut rd).await.is_err());
    }
}
