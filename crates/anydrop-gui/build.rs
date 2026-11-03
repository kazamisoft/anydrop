//! Embed the application icon and version info into anydrop-gui.exe.

#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("anydrop.ico");
    std::fs::write(
        &out,
        icon::ico_bytes(&[16, 20, 24, 32, 40, 48, 64, 128, 256]),
    )
    .unwrap();

    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().unwrap());
    res.set("ProductName", "AnyDrop");
    res.set("FileDescription", "AnyDrop - LAN/Tailscale file sharing");
    res.set("CompanyName", "kazamisoft");
    res.set("LegalCopyright", "MIT OR Apache-2.0");
    if let Err(e) = res.compile() {
        // Don't fail the whole build just because the resource compiler is missing.
        println!("cargo:warning=could not embed Windows icon: {e}");
    }
}
