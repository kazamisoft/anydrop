//! `anydrop` — send/receive files between Windows, macOS and Linux on a LAN or Tailscale.

use anydrop_core::discovery::{self, Advertiser};
use anydrop_core::transfer::{
    self, auto_accept, collect_files, AcceptFn, ConnOutcome, OfferRequest, ProgressFn,
    ReceiverConfig, SendOutcome,
};
use anydrop_core::{tailscale, DeviceInfo, Peer, DEFAULT_PORT};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "anydrop",
    version,
    about = "AirDrop-like file sharing for Windows / macOS / Linux (LAN + Tailscale)"
)]
struct Cli {
    /// Device name shown to others (default: hostname)
    #[arg(short, long, global = true)]
    name: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wait for incoming files (advertises itself on the LAN via mDNS)
    Receive {
        /// Download directory (default: ~/Downloads/AnyDrop)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Accept every offer without asking
        #[arg(short = 'y', long)]
        yes: bool,
        /// Do not advertise via mDNS (direct IP / Tailscale only)
        #[arg(long)]
        no_mdns: bool,
    },
    /// Discover receivers on the LAN and the Tailscale network
    List {
        /// Seconds to wait for LAN (mDNS) answers
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Skip Tailscale peer probing
        #[arg(long)]
        no_tailscale: bool,
    },
    /// Send files/folders. TARGET = device name | IP | host[:port] | Tailscale name
    Send {
        target: String,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Seconds to search the LAN when TARGET is a device name
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,
    },
    /// Check that a receiver is reachable
    Ping {
        target: String,
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let me = DeviceInfo::local(cli.name);
    match cli.cmd {
        Cmd::Receive {
            dir,
            port,
            yes,
            no_mdns,
        } => receive(me, dir, port, yes, no_mdns).await,
        Cmd::List {
            timeout,
            port,
            no_tailscale,
        } => {
            let peers = discover_all(&me, Duration::from_secs(timeout), port, !no_tailscale).await;
            if peers.is_empty() {
                println!("No AnyDrop receivers found. (Is `anydrop receive` running? Firewall open for TCP {port} / UDP 5353?)");
            }
            for p in peers {
                let addrs: Vec<String> = p.addrs.iter().map(|a| a.to_string()).collect();
                println!(
                    "{:<24} {:<18} {:<10} {}",
                    p.device.name,
                    p.device.os,
                    p.source,
                    addrs.join(", ")
                );
            }
            Ok(())
        }
        Cmd::Send {
            target,
            paths,
            port,
            timeout,
        } => send(me, &target, paths, port, Duration::from_secs(timeout)).await,
        Cmd::Ping { target, port } => {
            let (addr, _) = resolve_target(&me, &target, port, Duration::from_secs(3)).await?;
            let t = std::time::Instant::now();
            let dev = transfer::probe(addr, &me, Duration::from_secs(5)).await?;
            println!(
                "{} ({}) at {addr} answered in {:?}",
                dev.name,
                dev.os,
                t.elapsed()
            );
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ receive

async fn receive(
    me: DeviceInfo,
    dir: Option<PathBuf>,
    port: u16,
    yes: bool,
    no_mdns: bool,
) -> Result<()> {
    let dir = match dir {
        Some(d) => d,
        None => dirs::download_dir()
            .or_else(dirs::home_dir)
            .context("no home directory")?
            .join("AnyDrop"),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen on TCP {port} (already in use?)"))?;
    let _adv = if no_mdns {
        None
    } else {
        Some(Advertiser::start(&me, port)?)
    };

    println!("AnyDrop receiver \"{}\" ({})", me.name, me.os);
    println!(
        "  listening : 0.0.0.0:{port}{}",
        if no_mdns { "" } else { "  (mDNS advertised)" }
    );
    println!("  saving to : {}", dir.display());
    println!(
        "  mode      : {}",
        if yes {
            "auto-accept"
        } else {
            "ask before accepting"
        }
    );
    println!("Press Ctrl+C to stop.\n");

    let accept: AcceptFn = if yes { auto_accept() } else { prompt_accept() };
    let cfg = Arc::new(ReceiverConfig {
        dest_dir: dir,
        device: me,
        accept,
        progress: Some(receive_progress()),
    });

    let on_result = Arc::new(|remote: SocketAddr, r: Result<ConnOutcome>| match r {
        Ok(ConnOutcome::Received(rep)) => {
            println!(
                "✔ received {} file(s), {} from {}",
                rep.saved.len(),
                human(rep.bytes),
                rep.peer.name
            );
            for p in rep.saved.iter().take(20) {
                println!("    {}", p.display());
            }
            if rep.saved.len() > 20 {
                println!("    ... and {} more", rep.saved.len() - 20);
            }
        }
        Ok(ConnOutcome::Rejected(peer)) => println!("✘ declined offer from {}", peer.name),
        Ok(ConnOutcome::Probe(_)) => {}
        Err(e) => {
            eprintln!("✘ transfer from {remote} failed: {e:#}");
            if cfg!(target_os = "macos") && format!("{e:#}").contains("Application Firewall") {
                eprintln!("  fix: sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add \"$(which anydrop || echo ./anydrop)\"");
                eprintln!("       sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp <path-to-anydrop>");
            }
        }
    });

    tokio::select! {
        r = transfer::serve(listener, cfg, on_result) => r,
        _ = tokio::signal::ctrl_c() => { println!("\nbye"); Ok(()) }
    }
}

fn prompt_accept() -> AcceptFn {
    // Serialize prompts so two simultaneous senders don't interleave on stdin.
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    Arc::new(move |req: OfferRequest| {
        let lock = lock.clone();
        Box::pin(async move {
            let _g = lock.lock().await;
            println!(
                "\n📥 {} ({}, {}) wants to send {} file(s), {}:",
                req.peer.name,
                req.peer.os,
                req.remote.ip(),
                req.files.len(),
                human(req.total_size)
            );
            for f in req.files.iter().take(10) {
                println!("    {}  ({})", f.path, human(f.size));
            }
            if req.files.len() > 10 {
                println!("    ... and {} more", req.files.len() - 10);
            }
            tokio::task::spawn_blocking(|| {
                print!("Accept? [y/N] ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).ok();
                matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            })
            .await
            .unwrap_or(false)
        })
    })
}

fn bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner} [{bar:30}] {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta} {msg}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb
}

fn receive_progress() -> ProgressFn {
    let state: Arc<Mutex<Option<ProgressBar>>> = Arc::default();
    Arc::new(move |p| {
        let mut g = state.lock().unwrap();
        if g.as_ref()
            .map(|b| b.length() != Some(p.total_size))
            .unwrap_or(true)
        {
            *g = Some(bar(p.total_size));
        }
        let b = g.as_ref().unwrap();
        b.set_position(p.total_done);
        b.set_message(format!(
            "[{}/{}] {}",
            p.file_index + 1,
            p.file_count,
            p.path
        ));
        if p.total_done >= p.total_size {
            b.finish_and_clear();
            *g = None;
        }
    })
}

// --------------------------------------------------------------------- send

async fn send(
    me: DeviceInfo,
    target: &str,
    paths: Vec<PathBuf>,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let files = collect_files(&paths)?;
    let total: u64 = files.iter().map(|f| f.entry.size).sum();
    let (addr, name) = resolve_target(&me, target, port, timeout).await?;
    println!(
        "Sending {} file(s), {} to {name} ({addr}) — waiting for the receiver to accept...",
        files.len(),
        human(total)
    );

    let pb = bar(total);
    let pb2 = pb.clone();
    let progress: ProgressFn = Arc::new(move |p| {
        pb2.set_position(p.total_done);
        pb2.set_message(format!(
            "[{}/{}] {}",
            p.file_index + 1,
            p.file_count,
            p.path
        ));
    });
    let started = std::time::Instant::now();
    let out = transfer::send_files(addr, &me, &files, Some(progress)).await;
    pb.finish_and_clear();
    match out? {
        SendOutcome::Completed { files, bytes } => {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            println!(
                "✔ sent {files} file(s), {} in {secs:.1}s ({}/s)",
                human(bytes),
                human((bytes as f64 / secs) as u64)
            );
            Ok(())
        }
        SendOutcome::Rejected(reason) => bail!("receiver declined: {reason}"),
    }
}

/// Resolve TARGET into a socket address:
/// 1. `ip` / `ip:port` / `host:port`
/// 2. device name found on the LAN (mDNS)
/// 3. Tailscale peer host name (`tailscale status`)
/// 4. plain DNS / MagicDNS host name
async fn resolve_target(
    me: &DeviceInfo,
    target: &str,
    port: u16,
    timeout: Duration,
) -> Result<(SocketAddr, String)> {
    if let Ok(sa) = target.parse::<SocketAddr>() {
        return Ok((sa, target.into()));
    }
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok((SocketAddr::new(ip, port), target.into()));
    }
    let explicit_port = target
        .rsplit_once(':')
        .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p)));
    if let Some((host, p)) = explicit_port {
        return dns(host, p).await;
    }

    let lan = discovery::browse(timeout, Some(me.id.clone()))
        .await
        .unwrap_or_default();
    if let Some(peer) = pick(&lan, target)? {
        let mut errors = Vec::new();
        for a in &peer.addrs {
            match transfer::probe(*a, me, Duration::from_secs(2)).await {
                Ok(_) => return Ok((*a, peer.device.name.clone())),
                Err(e) => errors.push(format!("  {a}: {e:#}")),
            }
        }
        bail!(
            "{} was found via mDNS but none of its addresses answered:\n{}\n\
             hint: if the receiver logs \"closed by the OS immediately after accept\", \
             allow anydrop in its firewall (macOS: System Settings > Network > Firewall)",
            peer.device.name,
            errors.join("\n")
        );
    }

    if let Ok(ts) = tailscale::list_peers().await {
        if let Some(p) = ts.iter().find(|p| p.host_name.eq_ignore_ascii_case(target)) {
            if let Some(ip) = p
                .tailscale_ips
                .iter()
                .copied()
                .min_by_key(|ip| ip.is_ipv6())
            {
                return Ok((
                    SocketAddr::new(ip, port),
                    format!("{} (tailscale)", p.host_name),
                ));
            }
        }
    }

    dns(target, port)
        .await
        .with_context(|| format!("\"{target}\" not found on LAN, Tailscale or DNS"))
}

async fn dns(host: &str, port: u16) -> Result<(SocketAddr, String)> {
    let mut it = tokio::net::lookup_host((host, port)).await?;
    let mut addrs: Vec<SocketAddr> = it.by_ref().collect();
    addrs.sort_by_key(|a| a.is_ipv6());
    let a = *addrs.first().context("no address")?;
    Ok((a, host.into()))
}

fn pick<'a>(peers: &'a [Peer], target: &str) -> Result<Option<&'a Peer>> {
    if let Some(p) = peers
        .iter()
        .find(|p| p.device.name.eq_ignore_ascii_case(target))
    {
        return Ok(Some(p));
    }
    let t = target.to_lowercase();
    let m: Vec<&Peer> = peers
        .iter()
        .filter(|p| p.device.name.to_lowercase().starts_with(&t))
        .collect();
    match m.len() {
        0 => Ok(None),
        1 => Ok(Some(m[0])),
        _ => bail!(
            "\"{target}\" is ambiguous: {}",
            m.iter()
                .map(|p| p.device.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn discover_all(me: &DeviceInfo, timeout: Duration, port: u16, with_ts: bool) -> Vec<Peer> {
    let lan = discovery::browse(timeout, Some(me.id.clone()));
    let ts = async {
        if with_ts {
            tailscale::discover(me, port, Duration::from_millis(1500))
                .await
                .unwrap_or_default()
        } else {
            vec![]
        }
    };
    let (lan, ts) = tokio::join!(lan, ts);
    let mut all = lan.unwrap_or_default();
    for p in ts {
        // Same device visible on both LAN and Tailscale -> keep both addresses.
        if let Some(e) = all.iter_mut().find(|e| e.device.id == p.device.id) {
            e.addrs.extend(p.addrs);
        } else {
            all.push(p);
        }
    }
    all.retain(|p| p.device.id != me.id);
    all
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
