//! macOS `.icns` (and PNG) writer for the AnyDrop icon. Used by
//! `examples/make-icns.rs`, which `scripts/macos-app.sh` runs to put
//! `AnyDrop.icns` into the app bundle.

/// Build a macOS `.icns` file (PNG-encoded entries, 16 px .. 1024 px incl. @2x).
pub fn icns_bytes() -> Vec<u8> {
    // (OSType, pixel size) — see Apple's icns format: ic11..ic14 are the @2x variants.
    const ENTRIES: [(&[u8; 4], u32); 10] = [
        (b"icp4", 16),
        (b"icp5", 32),
        (b"ic11", 32),
        (b"icp6", 64),
        (b"ic12", 64),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic13", 256),
        (b"ic09", 512),
        (b"ic14", 1024),
    ];
    let mut body = Vec::new();
    for (ty, size) in ENTRIES {
        let png = png_bytes(size);
        body.extend_from_slice(ty);
        body.extend_from_slice(&(png.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(&png);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Minimal RGBA PNG encoder (deflate via miniz_oxide, already in the dependency tree).
pub fn png_bytes(size: u32) -> Vec<u8> {
    let (rgba, w, h) = crate::icon::app_icon_rgba(size);
    // Raw scanlines, each prefixed with filter type 0.
    let mut raw = Vec::with_capacity(((w * 4 + 1) * h) as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 9);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &z);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
