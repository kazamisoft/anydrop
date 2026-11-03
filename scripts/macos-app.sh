#!/usr/bin/env bash
# Build AnyDrop.app (GUI) for macOS and ad-hoc sign it.
#   ./scripts/macos-app.sh              -> current architecture
#   ./scripts/macos-app.sh universal    -> Intel + Apple Silicon
# Result: target/AnyDrop.app
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "universal" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  cargo build --release -p anydrop-gui --target aarch64-apple-darwin
  cargo build --release -p anydrop-gui --target x86_64-apple-darwin
  BIN=target/anydrop-gui-universal
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/anydrop-gui target/x86_64-apple-darwin/release/anydrop-gui
else
  cargo build --release -p anydrop-gui
  BIN=target/release/anydrop-gui
fi

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=target/AnyDrop.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/AnyDrop"
# Same code-drawn icon as the window / tray / Windows exe (crates/anydrop-gui/src/icon.rs).
cargo run -q --release -p anydrop-gui --example make-icns -- "$APP/Contents/Resources/AnyDrop.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>AnyDrop</string>
  <key>CFBundleDisplayName</key><string>AnyDrop</string>
  <key>CFBundleIdentifier</key><string>com.kazamisoft.anydrop</string>
  <key>CFBundleExecutable</key><string>AnyDrop</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AnyDrop</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
  <!-- macOS 15+: required for the Local Network permission prompt and mDNS -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>AnyDrop은 같은 네트워크의 기기를 찾고 파일을 주고받기 위해 로컬 네트워크를 사용합니다.</string>
  <key>NSBonjourServices</key>
  <array><string>_anydrop._tcp</string></array>
</dict>
</plist>
EOF

# Ad-hoc signature: lets the Application Firewall remember "Allow incoming connections".
codesign --force --deep --sign - "$APP"
codesign --verify --verbose "$APP"

echo
echo "Built $APP"
echo "Install:  cp -R $APP /Applications/"
echo "Firewall (if enabled):"
echo "  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /Applications/AnyDrop.app"
echo "  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp /Applications/AnyDrop.app"
