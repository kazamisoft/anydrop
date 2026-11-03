//! AnyDrop icon, drawn in code so there are no binary assets to keep in sync.
//!
//! Shared by the app (window + tray icon) and by `build.rs`, which turns it into
//! a multi-size `.ico` embedded in `anydrop-gui.exe`.

/// App / tray icon drawn in code (no asset files): blue disc with a white down arrow.
pub fn app_icon_rgba(size: u32) -> (Vec<u8>, u32, u32) {
    let n = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (u, v) = ((x as f32 + 0.5) / n, (y as f32 + 0.5) / n);
            let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
            // anti-aliased disc
            let disc = ((0.48 - d) * n).clamp(0.0, 1.0);
            if disc <= 0.0 {
                continue;
            }
            let stem = (u - 0.5).abs() < 0.07 && (0.20..0.58).contains(&v);
            let head = (0.50..=0.78).contains(&v) && (u - 0.5).abs() <= (0.78 - v) * 0.95;
            let bar = (0.80..0.86).contains(&v) && (u - 0.5).abs() < 0.22;
            let (r, g, b) = if stem || head || bar {
                (255, 255, 255)
            } else {
                (0x2f, 0x80, 0xed)
            };
            let i = ((y * size + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[r, g, b, (disc * 255.0) as u8]);
        }
    }
    (px, size, size)
}

/// Build a Windows `.ico` file containing one 32-bit BGRA bitmap per size.
#[allow(dead_code)] // used by build.rs
pub fn ico_bytes(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| dib(s)).collect();
    let mut out = Vec::new();
    // ICONDIR
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        let dim = if *s >= 256 { 0u8 } else { *s as u8 }; // 0 means 256
        out.extend_from_slice(&[dim, dim, 0, 0]); // w, h, palette, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// BITMAPINFOHEADER + bottom-up BGRA pixels + (empty) 1-bpp AND mask.
fn dib(size: u32) -> Vec<u8> {
    let (rgba, w, h) = app_icon_rgba(size);
    let mask_row = (w.div_ceil(32) * 4) as usize;
    let mut out = Vec::with_capacity(40 + rgba.len() + mask_row * h as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&((h * 2) as i32).to_le_bytes()); // XOR + AND masks
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // BI_RGB, sizes, resolution, colors
    for y in (0..h).rev() {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.extend(std::iter::repeat_n(0u8, mask_row * h as usize));
    out
}
