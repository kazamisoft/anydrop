//! Sender / receiver engine.

use crate::paths::{sanitize_rel_path, to_wire_path, unique_path};
use crate::protocol::{
    read_msg, read_msg_opt, write_msg, DeviceInfo, FileEntry, Message, PROTOCOL_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

const CHUNK: usize = 256 * 1024;
const PART_SUFFIX: &str = ".anydrop-part";
/// How long the receiver waits for the user to accept/reject an offer.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(120);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Progress {
    pub file_index: usize,
    pub file_count: usize,
    pub path: String,
    pub file_done: u64,
    pub file_size: u64,
    pub total_done: u64,
    pub total_size: u64,
}

pub type ProgressFn = Arc<dyn Fn(&Progress) + Send + Sync>;

/// A file on the local disk scheduled for sending.
#[derive(Debug, Clone)]
pub struct LocalFile {
    pub abs: PathBuf,
    pub entry: FileEntry,
}

/// Expand the given files/directories into a flat list of regular files.
/// A directory `photos/` becomes entries `photos/...` (the directory name is kept).
pub fn collect_files(paths: &[PathBuf]) -> Result<Vec<LocalFile>> {
    let mut out = Vec::new();
    for p in paths {
        let meta =
            std::fs::metadata(p).with_context(|| format!("cannot access {}", p.display()))?;
        if meta.is_file() {
            let name = p
                .file_name()
                .ok_or_else(|| anyhow!("bad file name: {}", p.display()))?;
            out.push(LocalFile {
                abs: p.clone(),
                entry: FileEntry {
                    path: to_wire_path(Path::new(name)),
                    size: meta.len(),
                },
            });
        } else if meta.is_dir() {
            let canon = p.canonicalize()?;
            let base = canon.parent().map(Path::to_path_buf).unwrap_or_default();
            for ent in walkdir::WalkDir::new(&canon)
                .follow_links(false)
                .sort_by_file_name()
            {
                let ent = ent?;
                if !ent.file_type().is_file() {
                    continue; // skip dirs and symlinks
                }
                let rel = ent.path().strip_prefix(&base)?;
                out.push(LocalFile {
                    abs: ent.path().to_path_buf(),
                    entry: FileEntry {
                        path: to_wire_path(rel),
                        size: ent.metadata()?.len(),
                    },
                });
            }
        }
    }
    if out.is_empty() {
        bail!("nothing to send");
    }
    Ok(out)
}

async fn connect<A: ToSocketAddrs>(addr: A) -> Result<TcpStream> {
    let s = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connection timed out"))??;
    // Latency optimisation only; never fatal (see handle_connection).
    let _ = s.set_nodelay(true);
    Ok(s)
}

async fn handshake(
    rd: &mut (impl tokio::io::AsyncRead + Unpin),
    wr: &mut (impl tokio::io::AsyncWrite + Unpin),
    me: &DeviceInfo,
) -> Result<DeviceInfo> {
    write_msg(
        wr,
        &Message::Hello {
            version: PROTOCOL_VERSION,
            device: me.clone(),
        },
    )
    .await?;
    match read_msg(rd).await? {
        Message::HelloAck { version, device } => {
            if version != PROTOCOL_VERSION {
                bail!("protocol mismatch: local v{PROTOCOL_VERSION}, remote v{version}");
            }
            Ok(device)
        }
        Message::Error { message } => bail!("peer error: {message}"),
        m => bail!("unexpected reply: {m:?}"),
    }
}

/// Check that an AnyDrop receiver is listening at `addr` and return its identity.
pub async fn probe<A: ToSocketAddrs>(
    addr: A,
    me: &DeviceInfo,
    timeout: Duration,
) -> Result<DeviceInfo> {
    tokio::time::timeout(timeout, async {
        let s = connect(addr).await?;
        let (mut rd, mut wr) = s.into_split();
        handshake(&mut rd, &mut wr, me).await
    })
    .await
    .map_err(|_| anyhow!("probe timed out"))?
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Completed { files: usize, bytes: u64 },
    Rejected(String),
}

pub async fn send_files<A: ToSocketAddrs>(
    addr: A,
    me: &DeviceInfo,
    files: &[LocalFile],
    progress: Option<ProgressFn>,
) -> Result<SendOutcome> {
    let stream = connect(addr).await?;
    let (rd, wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let mut wr = BufWriter::with_capacity(CHUNK, wr);

    handshake(&mut rd, &mut wr, me).await?;

    let total_size: u64 = files.iter().map(|f| f.entry.size).sum();
    let entries: Vec<FileEntry> = files.iter().map(|f| f.entry.clone()).collect();
    write_msg(
        &mut wr,
        &Message::Offer {
            files: entries,
            total_size,
        },
    )
    .await?;

    match read_msg(&mut rd).await? {
        Message::Accept => {}
        Message::Reject { reason } => return Ok(SendOutcome::Rejected(reason)),
        Message::Error { message } => bail!("peer error: {message}"),
        m => bail!("unexpected reply: {m:?}"),
    }

    let streamed: Result<()> = async {
        let mut buf = vec![0u8; CHUNK];
        let mut total_done = 0u64;
        for (i, f) in files.iter().enumerate() {
            let mut file = File::open(&f.abs)
                .await
                .with_context(|| format!("open {}", f.abs.display()))?;
            let mut hasher = Sha256::new();
            let mut remaining = f.entry.size;
            while remaining > 0 {
                let want = remaining.min(CHUNK as u64) as usize;
                let n = file.read(&mut buf[..want]).await?;
                if n == 0 {
                    bail!("{} shrank while sending", f.abs.display());
                }
                hasher.update(&buf[..n]);
                wr.write_all(&buf[..n]).await?;
                remaining -= n as u64;
                total_done += n as u64;
                if let Some(cb) = &progress {
                    cb(&Progress {
                        file_index: i,
                        file_count: files.len(),
                        path: f.entry.path.clone(),
                        file_done: f.entry.size - remaining,
                        file_size: f.entry.size,
                        total_done,
                        total_size,
                    });
                }
            }
            let sha256 = hex::encode(hasher.finalize());
            write_msg(
                &mut wr,
                &Message::FileEnd {
                    index: i as u32,
                    sha256,
                },
            )
            .await?;
        }
        Ok(())
    }
    .await;

    if let Err(e) = streamed {
        // If the receiver aborted (disk full, checksum, ...) it sends an Error frame
        // before closing; surface that instead of a bare "broken pipe".
        if let Ok(Ok(Message::Error { message })) =
            tokio::time::timeout(Duration::from_secs(2), read_msg(&mut rd)).await
        {
            bail!("receiver reported error: {message}");
        }
        return Err(e);
    }

    match read_msg(&mut rd).await? {
        Message::Done => Ok(SendOutcome::Completed {
            files: files.len(),
            bytes: total_size,
        }),
        Message::Error { message } => bail!("receiver reported error: {message}"),
        m => bail!("unexpected reply: {m:?}"),
    }
}

// ---------------------------------------------------------------- receiver

#[derive(Debug, Clone)]
pub struct OfferRequest {
    pub peer: DeviceInfo,
    pub remote: SocketAddr,
    pub files: Vec<FileEntry>,
    pub total_size: u64,
}

pub type AcceptFn =
    Arc<dyn Fn(OfferRequest) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Always accept (useful for tests and `--yes`).
pub fn auto_accept() -> AcceptFn {
    Arc::new(|_| Box::pin(async { true }))
}

#[derive(Clone)]
pub struct ReceiverConfig {
    pub dest_dir: PathBuf,
    pub device: DeviceInfo,
    pub accept: AcceptFn,
    pub progress: Option<ProgressFn>,
}

#[derive(Debug)]
pub struct ReceivedReport {
    pub peer: DeviceInfo,
    pub saved: Vec<PathBuf>,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum ConnOutcome {
    /// A ping/probe (or discovery check) — no offer was made.
    Probe(DeviceInfo),
    Rejected(DeviceInfo),
    Received(ReceivedReport),
}

pub type ResultFn = Arc<dyn Fn(SocketAddr, Result<ConnOutcome>) + Send + Sync>;

/// Accept connections forever, handling each one in its own task.
pub async fn serve(
    listener: TcpListener,
    cfg: Arc<ReceiverConfig>,
    on_result: ResultFn,
) -> Result<()> {
    loop {
        let (stream, remote) = listener.accept().await?;
        let cfg = cfg.clone();
        let on_result = on_result.clone();
        tokio::spawn(async move {
            let r = handle_connection(stream, remote, &cfg).await;
            on_result(remote, r);
        });
    }
}

pub async fn handle_connection(
    stream: TcpStream,
    remote: SocketAddr,
    cfg: &ReceiverConfig,
) -> Result<ConnOutcome> {
    // macOS returns EINVAL from setsockopt() when the socket was already shut
    // down by the kernel (e.g. the Application Firewall killed the flow right
    // after accept). Don't fail here; the first read reports the real cause.
    if let Err(e) = stream.set_nodelay(true) {
        if e.raw_os_error() == Some(22) {
            bail!(
                "connection was closed by the OS immediately after accept ({e}). \
                 On macOS this usually means the Application Firewall is blocking \
                 incoming connections for this binary"
            );
        }
    }
    let (rd, wr) = stream.into_split();
    let mut rd = BufReader::with_capacity(CHUNK, rd);
    let mut wr = BufWriter::new(wr);

    let peer = match read_msg(&mut rd).await? {
        Message::Hello { version, device } => {
            if version != PROTOCOL_VERSION {
                let message =
                    format!("protocol mismatch: receiver v{PROTOCOL_VERSION}, sender v{version}");
                write_msg(
                    &mut wr,
                    &Message::Error {
                        message: message.clone(),
                    },
                )
                .await
                .ok();
                bail!(message);
            }
            device
        }
        m => bail!("expected hello, got {m:?}"),
    };
    write_msg(
        &mut wr,
        &Message::HelloAck {
            version: PROTOCOL_VERSION,
            device: cfg.device.clone(),
        },
    )
    .await?;

    let (files, total_size) = match read_msg_opt(&mut rd).await? {
        None => return Ok(ConnOutcome::Probe(peer)),
        Some(Message::Offer { files, total_size }) => (files, total_size),
        Some(m) => bail!("expected offer, got {m:?}"),
    };

    // Validate every path up-front, before asking the user.
    let mut rels = Vec::with_capacity(files.len());
    for f in &files {
        match sanitize_rel_path(&f.path) {
            Ok(p) => rels.push(p),
            Err(e) => {
                write_msg(
                    &mut wr,
                    &Message::Reject {
                        reason: e.to_string(),
                    },
                )
                .await
                .ok();
                bail!("offer from {} rejected: {e}", peer.name);
            }
        }
    }
    if files.iter().map(|f| f.size).sum::<u64>() != total_size {
        write_msg(
            &mut wr,
            &Message::Reject {
                reason: "size mismatch".into(),
            },
        )
        .await
        .ok();
        bail!("inconsistent offer");
    }

    let req = OfferRequest {
        peer: peer.clone(),
        remote,
        files: files.clone(),
        total_size,
    };
    let accepted = tokio::time::timeout(ACCEPT_TIMEOUT, (cfg.accept)(req))
        .await
        .unwrap_or(false);
    if !accepted {
        write_msg(
            &mut wr,
            &Message::Reject {
                reason: "declined by receiver".into(),
            },
        )
        .await?;
        return Ok(ConnOutcome::Rejected(peer));
    }
    write_msg(&mut wr, &Message::Accept).await?;

    let mut saved = Vec::with_capacity(files.len());
    let mut buf = vec![0u8; CHUNK];
    let mut total_done = 0u64;
    for (i, (f, rel)) in files.iter().zip(rels.iter()).enumerate() {
        let target = cfg.dest_dir.join(rel);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut part_name = target.clone().into_os_string();
        part_name.push(PART_SUFFIX);
        let part = PathBuf::from(part_name);

        let res: Result<PathBuf> = async {
            let mut out = BufWriter::with_capacity(CHUNK, File::create(&part).await?);
            let mut hasher = Sha256::new();
            let mut remaining = f.size;
            while remaining > 0 {
                let want = remaining.min(CHUNK as u64) as usize;
                let n = rd.read(&mut buf[..want]).await?;
                if n == 0 {
                    bail!("connection lost during {}", f.path);
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).await?;
                remaining -= n as u64;
                total_done += n as u64;
                if let Some(cb) = &cfg.progress {
                    cb(&Progress {
                        file_index: i,
                        file_count: files.len(),
                        path: f.path.clone(),
                        file_done: f.size - remaining,
                        file_size: f.size,
                        total_done,
                        total_size,
                    });
                }
            }
            out.flush().await?;
            out.into_inner().sync_all().await?;
            let local = hex::encode(hasher.finalize());
            match read_msg(&mut rd).await? {
                Message::FileEnd { index, sha256 } if index as usize == i => {
                    if !sha256.eq_ignore_ascii_case(&local) {
                        bail!("checksum mismatch for {}", f.path);
                    }
                }
                m => bail!("expected file_end #{i}, got {m:?}"),
            }
            let final_path = unique_path(target.clone());
            tokio::fs::rename(&part, &final_path).await?;
            Ok(final_path)
        }
        .await;

        match res {
            Ok(p) => saved.push(p),
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                write_msg(
                    &mut wr,
                    &Message::Error {
                        message: e.to_string(),
                    },
                )
                .await
                .ok();
                return Err(e);
            }
        }
    }

    write_msg(&mut wr, &Message::Done).await?;
    Ok(ConnOutcome::Received(ReceivedReport {
        peer,
        saved,
        bytes: total_size,
    }))
}
