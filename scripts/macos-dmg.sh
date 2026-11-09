#!/usr/bin/env bash
# Wrap a built `broxee` binary in Broxee.app and put it in a .dmg (macOS only).
#
#   ./scripts/macos-dmg.sh target/release/broxee broxee-v0.1.0-macos-arm64.dmg
#
# The app is the same binary as the CLI: launched without arguments (from Finder or the Dock)
# it opens the window and brings back the last session. The bundle is signed ad hoc, not with a
# Developer ID, so Gatekeeper still asks once (see README, "Download").
set -euo pipefail

bin=${1:?usage: macos-dmg.sh <broxee binary> <out.dmg>}
out=${2:?usage: macos-dmg.sh <broxee binary> <out.dmg>}
version=$(awk -F'"' '/^\[workspace.package\]/ { p = 1 } p && /^version/ { print $2; exit }' Cargo.toml)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/Broxee.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/broxee"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Broxee</string>
  <key>CFBundleDisplayName</key><string>Broxee</string>
  <key>CFBundleIdentifier</key><string>io.github.kikiraihan.broxee</string>
  <key>CFBundleExecutable</key><string>broxee</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature: Apple silicon refuses to run unsigned code, and a bundle whose signature
# does not cover Info.plist is reported as damaged.
codesign --force --sign - "$app"
codesign --verify --strict "$app"

ln -s /Applications "$stage/Applications"
rm -f "$out"
hdiutil create -volname Broxee -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$out" >/dev/null
echo "$out"
