//! Single-instance guard.
//!
//! * `<config>/AnyDrop/instance.lock` is held with an OS file lock for the
//!   lifetime of the first instance. The OS drops the lock if the process dies,
//!   so a crash never leaves a stale lock behind.
//! * The first instance also listens on `127.0.0.1:<random>` (port written to
//!   `instance.port`). A second launch connects there, asks the running
//!   instance to show its window (forwarding any file paths it was given), then
//!   exits after telling the user AnyDrop is already running.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAGIC: &str = "ANYDROP-SHOW-1";

pub struct PrimaryInstance {
    _lock: Option<File>,
    listener: Option<TcpListener>,
}

pub enum Acquire {
    Primary(PrimaryInstance),
    /// Another instance holds the lock. `notified` = it was asked to show itself.
    AlreadyRunning {
        notified: bool,
    },
}

fn state_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("AnyDrop"))
}

pub fn acquire(forward: &[PathBuf]) -> Acquire {
    match state_dir() {
        Some(dir) => acquire_in(&dir, forward),
        None => Acquire::Primary(PrimaryInstance {
            _lock: None,
            listener: None,
        }),
    }
}

pub fn acquire_in(dir: &Path, forward: &[PathBuf]) -> Acquire {
    let unguarded = || {
        Acquire::Primary(PrimaryInstance {
            _lock: None,
            listener: None,
        })
    };
    if std::fs::create_dir_all(dir).is_err() {
        return unguarded();
    }
    let Ok(lock) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
    else {
        return unguarded();
    };
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok();
            if let Some(l) = &listener {
                if let Ok(addr) = l.local_addr() {
                    let _ = std::fs::write(dir.join("instance.port"), addr.port().to_string());
                }
            }
            Acquire::Primary(PrimaryInstance {
                _lock: Some(lock),
                listener,
            })
        }
        Err(std::fs::TryLockError::WouldBlock) => Acquire::AlreadyRunning {
            notified: notify(dir, forward).is_ok(),
        },
        // Locking unsupported (e.g. some network file systems): don't block startup.
        Err(std::fs::TryLockError::Error(_)) => unguarded(),
    }
}

fn notify(dir: &Path, forward: &[PathBuf]) -> std::io::Result<()> {
    let port: u16 = std::fs::read_to_string(dir.join("instance.port"))?
        .trim()
        .parse()
        .map_err(|_| std::io::Error::other("bad port file"))?;
    let mut s =
        TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut msg = format!("{MAGIC}\n");
    for p in forward {
        // Absolute paths, so the running instance resolves them correctly.
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        msg.push_str(&p.to_string_lossy());
        msg.push('\n');
    }
    s.write_all(msg.as_bytes())?;
    s.flush()
}

impl PrimaryInstance {
    /// Empty guard used while the app struct is being built.
    pub fn placeholder() -> Self {
        Self {
            _lock: None,
            listener: None,
        }
    }

    /// Serve "show yourself" requests from later launches on a background thread.
    pub fn listen(&mut self, on_request: impl Fn(Vec<PathBuf>) + Send + 'static) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("anydrop-instance".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut lines = BufReader::new(conn).lines();
                    if lines.next().and_then(Result::ok).as_deref() != Some(MAGIC) {
                        continue; // not one of ours
                    }
                    let paths = lines
                        .map_while(Result::ok)
                        .filter(|l| !l.is_empty())
                        .map(PathBuf::from)
                        .filter(|p| p.exists())
                        .collect();
                    on_request(paths);
                }
            });
    }
}

/// Native "already running" message box for the second launch.
pub fn tell_already_running(notified: bool) {
    let text = if notified {
        "AnyDrop이 이미 실행 중입니다.\n실행 중인 창을 앞으로 가져왔습니다."
    } else {
        "AnyDrop이 이미 실행 중입니다.\n트레이(메뉴 막대)의 AnyDrop 아이콘을 확인하세요."
    };
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("AnyDrop")
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn second_instance_is_refused_and_notifies_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let Acquire::Primary(mut first) = acquire_in(dir.path(), &[]) else {
            panic!("first launch must become primary");
        };
        let (tx, rx) = mpsc::channel();
        first.listen(move |paths| tx.send(paths).unwrap());

        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"x").unwrap();
        match acquire_in(dir.path(), std::slice::from_ref(&file)) {
            Acquire::AlreadyRunning { notified } => assert!(notified),
            Acquire::Primary(_) => panic!("second launch must be refused"),
        }
        let got = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(got, vec![std::fs::canonicalize(&file).unwrap()]);

        // Once the first instance exits, a new launch becomes primary again.
        drop(first);
        assert!(matches!(acquire_in(dir.path(), &[]), Acquire::Primary(_)));
    }
}
