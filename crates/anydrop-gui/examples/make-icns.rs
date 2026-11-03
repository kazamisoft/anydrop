//! Write the AnyDrop macOS icon: `cargo run -p anydrop-gui --example make-icns -- <out.icns>`
//! Used by scripts/macos-app.sh.

#[path = "../src/icns.rs"]
mod icns;
#[path = "../src/icon.rs"]
#[allow(dead_code)]
mod icon;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "AnyDrop.icns".into());
    std::fs::write(&out, icns::icns_bytes()).expect("write icns");
    eprintln!("wrote {out}");
}
