#!/usr/bin/env bash
# Build `target/bundle/Pulsar Link.app`: the release binary in an app bundle.
#
# macOS grants Bluetooth per app, keyed to the bundle ID; a bare binary started at login is
# prompted for by its path, with no explanation. The ID must match `autostart::ID`.
#
#   BINARY=path   bundle this binary (a universal one, say) instead of building one
#   IDENTITY=name sign with this Developer ID, hardened runtime, for notarization;
#                 without it, signed ad hoc: enough for this computer
set -euo pipefail
# A relative BINARY is the caller's, not the repo root's.
if [[ -n "${BINARY:-}" ]]; then BINARY=$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY"); fi
cd "$(dirname "$0")/.."

ID=com.alwaysepic.pulsar-link
grep -q "\"$ID\"" crates/pulsar-link/src/autostart.rs \
  || { echo "bundle ID $ID is not autostart::ID" >&2; exit 1; }
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/pulsar-link/Cargo.toml | head -1)

if [[ -z "${BINARY:-}" ]]; then
  cargo build --release -p pulsar-link
  BINARY=target/release/pulsar-link
fi

APP="target/bundle/Pulsar Link.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/pulsar-link"
# The licences travel with every copy. A signed build is one for others, so it must carry
# the dependencies' notices too (scripts/notices.sh).
cp LICENSE "$APP/Contents/Resources/"
# Rendered from icon.svg by scripts/app-icon.sh; committed, so a release needs no renderer.
cp crates/pulsar-link/icon/AppIcon.icns "$APP/Contents/Resources/"
if [[ -f THIRD-PARTY-NOTICES.txt ]]; then
  cp THIRD-PARTY-NOTICES.txt "$APP/Contents/Resources/"
elif [[ -n "${IDENTITY:-}" ]]; then
  echo "THIRD-PARTY-NOTICES.txt is missing: run scripts/notices.sh" >&2
  exit 1
fi
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$ID</string>
  <key>CFBundleName</key><string>Pulsar Link</string>
  <key>CFBundleDisplayName</key><string>Pulsar Link</string>
  <key>CFBundleExecutable</key><string>pulsar-link</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <!-- A background program: no Dock icon, no menu bar. Opening it opens the page. -->
  <key>LSUIElement</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key>
  <string>Pulsar Link talks to your Pulsar controller over Bluetooth to show the game's screen on its VMU and keep your saves.</string>
</dict>
</plist>
PLIST
if [[ -n "${IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --sign "$IDENTITY" --identifier "$ID" "$APP"
else
  codesign --force --sign - --identifier "$ID" "$APP"
fi
echo "$APP"
