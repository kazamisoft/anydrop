//! End-to-end simulations of sender <-> receiver over loopback TCP.

use anydrop_core::protocol::{
    read_msg, write_msg, DeviceInfo, FileEntry, Message, PROTOCOL_VERSION,
};
use anydrop_core::transfer::*;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use unicode_normalization::UnicodeNormalization;

type Results = Arc<Mutex<Vec<anyhow::Result<ConnOutcome>>>>;

async fn start_receiver(dest: &Path, accept: AcceptFn) -> (SocketAddr, Results) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Arc::new(ReceiverConfig {
        dest_dir: dest.to_path_buf(),
        device: DeviceInfo::local(Some("receiver".into())),
        accept,
        progress: None,
    });
    let results: Results = Arc::default();
    let r2 = results.clone();
    tokio::spawn(serve(
        listener,
        cfg,
        Arc::new(move |_, r| r2.lock().unwrap().push(r)),
    ));
    (addr, results)
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().strip_prefix(root).unwrap().to_path_buf())
        .collect();
    v.sort();
    v
}

async fn wait_results(r: &Results, n: usize) {
    for _ in 0..200 {
        if r.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("receiver did not finish");
}

#[tokio::test]
async fn sends_files_and_directories_intact() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // single big file (crosses many chunk boundaries)
    let big = pseudo_random(20 * 1024 * 1024 + 123, 7);
    std::fs::write(src.path().join("big.bin"), &big).unwrap();
    // empty file
    std::fs::write(src.path().join("empty.txt"), b"").unwrap();
    // directory tree with a macOS-style NFD Korean name
    let nfd_name: String = "사진".nfd().collect();
    let tree = src.path().join("album");
    std::fs::create_dir_all(tree.join(&nfd_name).join("deep")).unwrap();
    std::fs::write(tree.join("readme.md"), b"hello").unwrap();
    std::fs::write(
        tree.join(&nfd_name).join("deep").join("a.jpg"),
        pseudo_random(300_000, 3),
    )
    .unwrap();

    let (addr, results) = start_receiver(dst.path(), auto_accept()).await;
    let files = collect_files(&[
        src.path().join("big.bin"),
        src.path().join("empty.txt"),
        tree.clone(),
    ])
    .unwrap();
    assert_eq!(files.len(), 4);

    let last = Arc::new(AtomicU64::new(0));
    let l2 = last.clone();
    let progress: ProgressFn = Arc::new(move |p| l2.store(p.total_done, Ordering::Relaxed));

    let me = DeviceInfo::local(Some("sender".into()));
    let out = send_files(addr, &me, &files, Some(progress)).await.unwrap();
    let total = big.len() as u64 + 5 + 300_000;
    assert_eq!(
        out,
        SendOutcome::Completed {
            files: 4,
            bytes: total
        }
    );
    assert_eq!(last.load(Ordering::Relaxed), total);

    wait_results(&results, 1).await;
    assert!(matches!(
        results.lock().unwrap()[0],
        Ok(ConnOutcome::Received(_))
    ));

    let got = list_files(dst.path());
    let expect: Vec<PathBuf> = vec![
        PathBuf::from("album/readme.md"),
        ["album", "사진", "deep", "a.jpg"].iter().collect(), // NFC on the receiving side
        PathBuf::from("big.bin"),
        PathBuf::from("empty.txt"),
    ];
    let mut expect_sorted = expect.clone();
    expect_sorted.sort();
    assert_eq!(got, expect_sorted);
    assert_eq!(std::fs::read(dst.path().join("big.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read(dst.path().join(&expect[1])).unwrap(),
        pseudo_random(300_000, 3)
    );
}

#[tokio::test]
async fn existing_file_gets_numbered_copy() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("doc.txt"), b"new").unwrap();
    std::fs::write(dst.path().join("doc.txt"), b"old").unwrap();

    let (addr, _) = start_receiver(dst.path(), auto_accept()).await;
    let files = collect_files(&[src.path().join("doc.txt")]).unwrap();
    let me = DeviceInfo::local(None);
    send_files(addr, &me, &files, None).await.unwrap();
    send_files(addr, &me, &files, None).await.unwrap();

    assert_eq!(std::fs::read(dst.path().join("doc.txt")).unwrap(), b"old");
    assert_eq!(
        std::fs::read(dst.path().join("doc (1).txt")).unwrap(),
        b"new"
    );
    assert_eq!(
        std::fs::read(dst.path().join("doc (2).txt")).unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn receiver_can_decline() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("x.txt"), b"x").unwrap();

    let seen = Arc::new(Mutex::new(None));
    let s2 = seen.clone();
    let decline: AcceptFn = Arc::new(move |req: OfferRequest| {
        *s2.lock().unwrap() = Some((req.peer.name.clone(), req.files.clone()));
        Box::pin(async { false })
    });
    let (addr, _) = start_receiver(dst.path(), decline).await;
    let files = collect_files(&[src.path().join("x.txt")]).unwrap();
    let out = send_files(addr, &DeviceInfo::local(Some("alice".into())), &files, None)
        .await
        .unwrap();

    assert!(matches!(out, SendOutcome::Rejected(_)));
    let (name, offered) = seen.lock().unwrap().clone().unwrap();
    assert_eq!(name, "alice");
    assert_eq!(
        offered,
        vec![FileEntry {
            path: "x.txt".into(),
            size: 1
        }]
    );
    assert!(list_files(dst.path()).is_empty());
}

#[tokio::test]
async fn probe_returns_receiver_identity() {
    let dst = tempfile::tempdir().unwrap();
    let (addr, results) = start_receiver(dst.path(), auto_accept()).await;
    let dev = probe(
        addr,
        &DeviceInfo::local(None),
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(dev.name, "receiver");
    wait_results(&results, 1).await;
    assert!(matches!(
        results.lock().unwrap()[0],
        Ok(ConnOutcome::Probe(_))
    ));
}

/// A hand-written malicious / broken sender speaking the raw protocol.
async fn raw_sender(
    addr: SocketAddr,
    files: Vec<FileEntry>,
    body: &[u8],
    sha: &str,
    drop_early: bool,
) -> Option<Message> {
    let s = TcpStream::connect(addr).await.unwrap();
    let (mut rd, mut wr) = s.into_split();
    write_msg(
        &mut wr,
        &Message::Hello {
            version: PROTOCOL_VERSION,
            device: DeviceInfo::local(Some("evil".into())),
        },
    )
    .await
    .unwrap();
    read_msg(&mut rd).await.unwrap();
    let total_size = files.iter().map(|f| f.size).sum();
    write_msg(&mut wr, &Message::Offer { files, total_size })
        .await
        .unwrap();
    match read_msg(&mut rd).await.unwrap() {
        Message::Accept => {}
        other => return Some(other),
    }
    wr.write_all(body).await.unwrap();
    if drop_early {
        drop(wr);
        drop(rd);
        return None;
    }
    write_msg(
        &mut wr,
        &Message::FileEnd {
            index: 0,
            sha256: sha.into(),
        },
    )
    .await
    .unwrap();
    read_msg(&mut rd).await.ok()
}

#[tokio::test]
async fn path_traversal_is_rejected() {
    let base = tempfile::tempdir().unwrap();
    let dst = base.path().join("inbox");
    std::fs::create_dir(&dst).unwrap();
    let (addr, _) = start_receiver(&dst, auto_accept()).await;

    for evil in ["../escape.txt", "a/../../escape.txt", "..\\escape.txt"] {
        let reply = raw_sender(
            addr,
            vec![FileEntry {
                path: evil.into(),
                size: 3,
            }],
            b"pwn",
            "00",
            false,
        )
        .await;
        assert!(
            matches!(reply, Some(Message::Reject { .. })),
            "{evil}: {reply:?}"
        );
    }
    assert!(!base.path().join("escape.txt").exists());
    assert!(list_files(&dst).is_empty());
}

#[tokio::test]
async fn checksum_mismatch_leaves_no_file() {
    let dst = tempfile::tempdir().unwrap();
    let (addr, results) = start_receiver(dst.path(), auto_accept()).await;
    let reply = raw_sender(
        addr,
        vec![FileEntry {
            path: "a.txt".into(),
            size: 4,
        }],
        b"data",
        &"0".repeat(64),
        false,
    )
    .await;
    assert!(matches!(reply, Some(Message::Error { .. })), "{reply:?}");
    wait_results(&results, 1).await;
    assert!(results.lock().unwrap()[0].is_err());
    assert!(
        list_files(dst.path()).is_empty(),
        "{:?}",
        list_files(dst.path())
    );
}

#[tokio::test]
async fn interrupted_transfer_cleans_up_part_file() {
    let dst = tempfile::tempdir().unwrap();
    let (addr, results) = start_receiver(dst.path(), auto_accept()).await;
    raw_sender(
        addr,
        vec![FileEntry {
            path: "big.bin".into(),
            size: 1_000_000,
        }],
        &[1u8; 1000],
        "",
        true,
    )
    .await;
    wait_results(&results, 1).await;
    assert!(results.lock().unwrap()[0].is_err());
    assert!(
        list_files(dst.path()).is_empty(),
        "{:?}",
        list_files(dst.path())
    );
}

#[tokio::test]
async fn probe_any_skips_unreachable_addresses_quickly() {
    let dst = tempfile::tempdir().unwrap();
    let (good, _) = start_receiver(dst.path(), auto_accept()).await;
    // TEST-NET-1 addresses: never answer (SYN is dropped or unreachable).
    let dead: Vec<SocketAddr> = (1..=5)
        .map(|i| format!("192.0.2.{i}:47820").parse().unwrap())
        .collect();
    let mut addrs = dead.clone();
    addrs.push(good); // reachable address listed last, like a Windows box with WSL/Hyper-V adapters

    let t = std::time::Instant::now();
    let (addr, dev) = probe_any(
        &addrs,
        &DeviceInfo::local(None),
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(addr, good);
    assert_eq!(dev.name, "receiver");
    // Sequential probing would take ~5 x 2s; racing finishes in well under one timeout.
    assert!(
        t.elapsed() < std::time::Duration::from_millis(1500),
        "took {:?}",
        t.elapsed()
    );
}

/// Needs working multicast on the host; run with `cargo test -- --ignored`.
#[tokio::test]
#[ignore]
async fn browser_tracks_peers_appearing_and_leaving() {
    use anydrop_core::discovery::{Advertiser, Browser};
    let browser = Browser::start(Arc::new(|| {})).unwrap();
    let dev = DeviceInfo::local(Some("browse-test".into()));

    let adv = Advertiser::start(&dev, 47999).unwrap();
    let mut seen = false;
    for _ in 0..100 {
        if browser.peers().iter().any(|p| p.device.id == dev.id) {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen, "advertised peer never appeared");

    drop(adv); // sends mDNS goodbye
    let mut gone = false;
    for _ in 0..100 {
        if !browser.peers().iter().any(|p| p.device.id == dev.id) {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(gone, "peer was not removed after goodbye");
}
