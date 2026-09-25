#!/usr/bin/env bash
# Build Goro.app (universal: Apple silicon + Intel), sign, notarize, and zip it.
#
# Usage: scripts/package/macos.sh <version> <out dir>
#
# Signing and notarization run when these are set (release CI provides them):
#   APPLE_SIGNING_IDENTITY   "Developer ID Application: …" in the keychain
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD   for notarytool
# Without them the app is ad-hoc signed (runs locally; Gatekeeper will warn elsewhere).
set -euo pipefail

version="$1"
out="$(mkdir -p "$2" && cd "$2" && pwd)"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

# GORO_MAC_TARGETS narrows the build (e.g. to the native target when testing locally).
targets=(${GORO_MAC_TARGETS:-aarch64-apple-darwin x86_64-apple-darwin})
binaries=()
for target in "${targets[@]}"; do
  cargo build --release --locked -p goro --target "$target"
  binaries+=("target/$target/release/goro")
done

work="$(mktemp -d)"
app="$work/Goro.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/goro" "${binaries[@]}"
iconutil -c icns -o "$app/Contents/Resources/goro.icns" assets/icons/goro.iconset

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Goro</string>
  <key>CFBundleDisplayName</key><string>Goro</string>
  <key>CFBundleIdentifier</key><string>dev.goro.Goro</string>
  <key>CFBundleExecutable</key><string>goro</string>
  <key>CFBundleIconFile</key><string>goro</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$app"
else
  echo "warning: APPLE_SIGNING_IDENTITY not set; ad-hoc signing (not distributable)" >&2
  codesign --force --sign - "$app"
fi

zip="$out/Goro-${version}-macos-universal.zip"
ditto -c -k --keepParent "$app" "$zip"

if [[ -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" ]]; then
  xcrun notarytool submit "$zip" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" --wait
  xcrun stapler staple "$app"
  rm "$zip"
  ditto -c -k --keepParent "$app" "$zip"
else
  echo "warning: notarization credentials not set; skipping notarization" >&2
fi

rm -rf "$work"
echo "$zip"
