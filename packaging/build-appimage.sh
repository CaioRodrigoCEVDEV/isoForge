#!/usr/bin/env bash
# Build an AppImage for ISOForge using linuxdeploy and its GTK plugin.
#
# Usage: packaging/build-appimage.sh [version] [output-dir] [tools-dir]
#
# The version defaults to the version in Cargo.toml. The binary must already
# have been built with `cargo build --release --locked`.
#
# Required in tools-dir (or via LINUXDEPLOY / LINUXDEPLOY_PLUGIN_GTK):
#   linuxdeploy
#   linuxdeploy-plugin-gtk.sh
#
# External runtime tools (xorriso, 7z, genisoimage) are not bundled.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ -n "${1:-}" ]; then
    VERSION="${1#v}"
else
    VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
OUT_DIR="${2:-dist}"
TOOLS_DIR="${3:-tools}"

LINUXDEPLOY_BIN="$(realpath -m "${LINUXDEPLOY:-$TOOLS_DIR/linuxdeploy}")"
GTK_PLUGIN="$(realpath -m "${LINUXDEPLOY_PLUGIN_GTK:-$TOOLS_DIR/linuxdeploy-plugin-gtk.sh}")"
WORK="$ROOT/build/appimage"
APPDIR="$WORK/AppDir"
BIN="$ROOT/target/release/isoforge"

if [ ! -x "$BIN" ]; then
    echo "Binary not found: $BIN" >&2
    echo "Run: cargo build --release --locked" >&2
    exit 1
fi
if [ ! -x "$LINUXDEPLOY_BIN" ]; then
    echo "linuxdeploy not found/executable: $LINUXDEPLOY_BIN" >&2
    exit 1
fi
if [ ! -x "$GTK_PLUGIN" ]; then
    echo "linuxdeploy GTK plugin not found/executable: $GTK_PLUGIN" >&2
    exit 1
fi

rm -rf "$WORK"
mkdir -p "$APPDIR/usr/bin" \
    "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons" \
    "$APPDIR/usr/share/metainfo"

install -Dm755 "$BIN" "$APPDIR/usr/bin/isoforge"
install -Dm644 data/io.github.isoforge.ISOForge.desktop \
    "$APPDIR/usr/share/applications/io.github.isoforge.ISOForge.desktop"
install -Dm644 data/io.github.isoforge.ISOForge.metainfo.xml \
    "$APPDIR/usr/share/metainfo/io.github.isoforge.ISOForge.metainfo.xml"
cp -a data/icons/hicolor "$APPDIR/usr/share/icons/"
install -Dm644 data/icons/hicolor/256x256/apps/io.github.isoforge.ISOForge.png \
    "$APPDIR/io.github.isoforge.ISOForge.png"
ln -sf io.github.isoforge.ISOForge.png "$APPDIR/.DirIcon"

export ARCH=x86_64
export DEPLOY_GTK_VERSION=4
export APPIMAGE_EXTRACT_AND_RUN=1
export PATH="$(dirname "$GTK_PLUGIN"):$PATH"

(
    cd "$WORK"
    "$LINUXDEPLOY_BIN" \
        --appdir "$APPDIR" \
        --executable "$BIN" \
        --desktop-file "$ROOT/data/io.github.isoforge.ISOForge.desktop" \
        --icon-file "$ROOT/data/icons/hicolor/256x256/apps/io.github.isoforge.ISOForge.png" \
        --plugin gtk \
        --output appimage
)

APPIMAGE="$(find "$WORK" -maxdepth 1 -type f -name '*.AppImage' -print -quit)"
if [ -z "$APPIMAGE" ]; then
    echo "linuxdeploy did not produce an AppImage" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
OUT_PATH="$OUT_DIR/ISOForge-${VERSION}-x86_64.AppImage"
rm -f "$OUT_PATH"
mv "$APPIMAGE" "$OUT_PATH"
chmod 0755 "$OUT_PATH"
echo "Built: $OUT_PATH"
