#!/usr/bin/env bash
# Build Linux packages: a tarball, a .deb and an AppImage.
#
# Usage: scripts/package/linux.sh <version> <out dir>
# Needs dpkg-deb; the AppImage step needs appimagetool on PATH (skipped with a warning
# otherwise).
set -euo pipefail

version="$1"
out="$(mkdir -p "$2" && cd "$2" && pwd)"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

arch="$(uname -m)"
case "$arch" in
  x86_64) deb_arch=amd64 ;;
  aarch64) deb_arch=arm64 ;;
  *) echo "unsupported architecture $arch" >&2; exit 1 ;;
esac

cargo build --release --locked -p goro
bin=target/release/goro
work="$(mktemp -d)"

# Tarball: the binary, desktop entry and icon.
tar_dir="$work/goro-${version}-linux-${arch}"
mkdir -p "$tar_dir"
cp "$bin" packaging/linux/goro.desktop README.md "$tar_dir/"
cp assets/icons/goro-512.png "$tar_dir/goro.png"
tar -C "$work" -czf "$out/goro-${version}-linux-${arch}.tar.gz" "$(basename "$tar_dir")"

# .deb
deb="$work/deb"
mkdir -p "$deb/DEBIAN" "$deb/usr/bin" "$deb/usr/share/applications" \
  "$deb/usr/share/icons/hicolor/512x512/apps" "$deb/usr/share/icons/hicolor/256x256/apps"
cp "$bin" "$deb/usr/bin/goro"
cp packaging/linux/goro.desktop "$deb/usr/share/applications/dev.goro.Goro.desktop"
cp assets/icons/goro-512.png "$deb/usr/share/icons/hicolor/512x512/apps/goro.png"
cp assets/icons/goro-256.png "$deb/usr/share/icons/hicolor/256x256/apps/goro.png"
cat > "$deb/DEBIAN/control" <<CONTROL
Package: goro
Version: ${version}
Architecture: ${deb_arch}
Maintainer: Goro <goro@users.noreply.github.com>
Depends: git, libxkbcommon0, libxkbcommon-x11-0, libfontconfig1, libfreetype6, libvulkan1
Section: devel
Priority: optional
Description: Instant, native review for agent-written code changes
 A fast desktop app that shows what a coding agent changed in a git repository and
 lets you stage, discard, comment and commit.
CONTROL
dpkg-deb --build --root-owner-group "$deb" "$out/goro_${version}_${deb_arch}.deb"

# AppImage
if command -v appimagetool >/dev/null; then
  appdir="$work/Goro.AppDir"
  mkdir -p "$appdir/usr/bin"
  cp "$bin" "$appdir/usr/bin/goro"
  cp packaging/linux/goro.desktop "$appdir/goro.desktop"
  cp assets/icons/goro-256.png "$appdir/goro.png"
  ln -s usr/bin/goro "$appdir/AppRun"
  ARCH="$arch" appimagetool "$appdir" "$out/Goro-${version}-${arch}.AppImage"
else
  echo "warning: appimagetool not found; skipping the AppImage" >&2
fi

rm -rf "$work"
ls -1 "$out"
