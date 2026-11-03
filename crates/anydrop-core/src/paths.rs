//! Cross-platform path handling.
//!
//! * The sender converts local paths into a portable relative form:
//!   '/' separators and **NFC** Unicode (macOS often yields NFD names, which show
//!   up as broken Hangul jamo such as "ㅎㅏㄴㄱㅡㄹ" on Windows).
//! * The receiver never trusts the peer: `..`, absolute paths, drive letters,
//!   Windows-reserved characters and device names are neutralised so a file can
//!   never be written outside the chosen download directory.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Normalise a single component received from the network so that it is a valid
/// file name on Windows, macOS and Linux alike.
fn sanitize_component(c: &str) -> Option<String> {
    let c: String = c.nfc().collect();
    let mut s: String = c
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            ch if (ch as u32) < 0x20 => '_',
            ch => ch,
        })
        .collect();
    // Windows silently drops trailing dots/spaces.
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() || s == "." || s == ".." {
        return None;
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    Some(s)
}

/// Convert an untrusted relative path from the wire into a safe relative `PathBuf`.
pub fn sanitize_rel_path(p: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for raw in p.split(['/', '\\']) {
        if raw.is_empty() || raw == "." {
            continue;
        }
        if raw == ".." {
            bail!("path traversal rejected: {p:?}");
        }
        match sanitize_component(raw) {
            Some(c) => out.push(c),
            None => bail!("invalid path component in {p:?}"),
        }
    }
    if out.as_os_str().is_empty() {
        bail!("empty path");
    }
    Ok(out)
}

/// Build the portable wire path from local relative components.
pub fn to_wire_path(rel: &Path) -> String {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().nfc().collect()),
            _ => None,
        })
        .collect();
    parts.join("/")
}

/// If `path` exists, return "name (1).ext", "name (2).ext", ... that does not exist.
pub fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()));
    for i in 1.. {
        let cand = parent.join(format!("{stem} ({i}){}", ext.as_deref().unwrap_or("")));
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_rejected() {
        assert!(sanitize_rel_path("../etc/passwd").is_err());
        assert!(sanitize_rel_path("a/../../b").is_err());
        assert!(sanitize_rel_path("..\\..\\Windows\\x.dll").is_err());
        assert!(sanitize_rel_path("").is_err());
    }

    #[test]
    fn absolute_and_drive_become_relative() {
        assert_eq!(
            sanitize_rel_path("/etc/passwd").unwrap(),
            PathBuf::from("etc").join("passwd")
        );
        assert_eq!(
            sanitize_rel_path("C:\\x\\y.txt").unwrap(),
            PathBuf::from("C_").join("x").join("y.txt")
        );
    }

    #[test]
    fn windows_unsafe_names() {
        assert_eq!(
            sanitize_rel_path("what?.txt").unwrap(),
            PathBuf::from("what_.txt")
        );
        assert_eq!(
            sanitize_rel_path("CON.txt").unwrap(),
            PathBuf::from("_CON.txt")
        );
        assert_eq!(
            sanitize_rel_path("trail. ").unwrap(),
            PathBuf::from("trail")
        );
    }

    #[test]
    fn nfd_hangul_becomes_nfc() {
        let nfd: String = "한글.txt".nfd().collect();
        assert_ne!(nfd, "한글.txt");
        assert_eq!(sanitize_rel_path(&nfd).unwrap(), PathBuf::from("한글.txt"));
        assert_eq!(to_wire_path(Path::new(&nfd)), "한글.txt");
    }
}
