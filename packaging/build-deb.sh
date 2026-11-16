#!/usr/bin/env bash
# Build a Debian package for ISOForge.
#
# Usage: packaging/build-deb.sh [version] [output-dir]
#
# The version defaults to the version in Cargo.toml. The binary must already
# have been built with `cargo build --release --locked`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ -n "${1:-}" ]; then
    VERSION="${1#v}"
else
    VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
OUT_DIR="${2:-dist}"
ARCH="amd64"
BIN="$ROOT/target/release/isoforge"
PKGROOT="$ROOT/build/deb/isoforge_${VERSION}_${ARCH}"

if [ ! -x "$BIN" ]; then
    echo "Binary not found: $BIN" >&2
    echo "Run: cargo build --release --locked" >&2
    exit 1
fi

rm -rf "$PKGROOT"
install -Dm755 "$BIN" "$PKGROOT/usr/bin/isoforge"
install -Dm644 data/io.github.isoforge.ISOForge.desktop \
    "$PKGROOT/usr/share/applications/io.github.isoforge.ISOForge.desktop"
install -Dm644 data/io.github.isoforge.ISOForge.metainfo.xml \
    "$PKGROOT/usr/share/metainfo/io.github.isoforge.ISOForge.metainfo.xml"
mkdir -p "$PKGROOT/usr/share/icons"
cp -a data/icons/hicolor "$PKGROOT/usr/share/icons/"
install -Dm644 packaging/debian/copyright \
    "$PKGROOT/usr/share/doc/isoforge/copyright"

# Runtime dependencies: libraries linked by the binary plus the external tools
# invoked by the editor. dpkg-shlibdeps is used when available so the library
# versions match the build host (the release workflow builds on Ubuntu 24.04).
DEPENDS=""
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
    TMPCTL="$(mktemp -d)"
    trap 'rm -rf "$TMPCTL"' EXIT
    mkdir -p "$TMPCTL/debian"
    cat > "$TMPCTL/debian/control" <<'EOF'
Source: isoforge
Section: utils
Priority: optional
Maintainer: Caio Rodrigo <contato@caiorodrigocev.com.br>

Package: isoforge
Architecture: any
Depends: ${shlibs:Depends}, ${misc:Depends}
Description: ISOForge
 ISO image editor.
EOF
    SHLIBS="$(cd "$TMPCTL" && LC_ALL=C dpkg-shlibdeps -O "$BIN" 2>/dev/null \
        | sed -n 's/^shlibs:Depends=//p' || true)"
    if [ -n "$SHLIBS" ]; then
        # gtk4-rs 0.9 targets GTK 4.10 APIs even when the build host is newer.
        if echo "$SHLIBS" | grep -q 'libgtk-4-1'; then
            SHLIBS="$(echo "$SHLIBS" \
                | sed -E 's/libgtk-4-1 \(>= [^)]+\)/libgtk-4-1 (>= 4.10)/')"
        else
            SHLIBS="$SHLIBS, libgtk-4-1 (>= 4.10)"
        fi
        DEPENDS="$SHLIBS, xorriso, p7zip-full, genisoimage"
    fi
fi

if [ -z "$DEPENDS" ]; then
    DEPENDS="libc6 (>= 2.39), libgcc-s1, libglib2.0-0t64, libgtk-4-1 (>= 4.10), libpango-1.0-0, xorriso, p7zip-full, genisoimage"
fi

INSTALLED_SIZE="$(du -sk "$PKGROOT/usr" | cut -f1)"
mkdir -p "$PKGROOT/DEBIAN"
cat > "$PKGROOT/DEBIAN/control" <<EOF
Package: isoforge
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Caio Rodrigo <contato@caiorodrigocev.com.br>
Installed-Size: ${INSTALLED_SIZE}
Section: utils
Priority: optional
Homepage: https://github.com/CaioRodrigoCEVDEV/isoForge
Depends: ${DEPENDS}
Description: native ISO image editor with boot-preserving rebuild
 ISOForge opens ISO9660/UDF images, extracts a temporary workspace,
 lets the user add or remove files, and rebuilds a new image while
 preserving El Torito BIOS/UEFI boot metadata when the format is
 supported.
 .
 External tools xorriso, 7z and genisoimage are required at runtime.
EOF

mkdir -p "$PKGROOT/usr/share/doc/isoforge"
cat > "$PKGROOT/usr/share/doc/isoforge/changelog.Debian" <<EOF
isoforge (${VERSION}) unstable; urgency=medium

  * Release ${VERSION}.

 -- Caio Rodrigo <contato@caiorodrigocev.com.br>  $(date -R)
EOF
gzip -9n "$PKGROOT/usr/share/doc/isoforge/changelog.Debian"

# Canonical package permissions, independent of the checkout's umask.
find "$PKGROOT/usr" -type d -exec chmod 0755 {} +
find "$PKGROOT/usr" -type f -exec chmod 0644 {} +
chmod 0755 "$PKGROOT/usr/bin/isoforge"

cat > "$PKGROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
fi
exit 0
EOF
cp "$PKGROOT/DEBIAN/postinst" "$PKGROOT/DEBIAN/postrm"
chmod 0755 "$PKGROOT/DEBIAN/postinst" "$PKGROOT/DEBIAN/postrm"

mkdir -p "$OUT_DIR"
DEB_PATH="$OUT_DIR/isoforge_${VERSION}_${ARCH}.deb"
rm -f "$DEB_PATH"
dpkg-deb --root-owner-group --build "$PKGROOT" "$DEB_PATH"
echo "Built: $DEB_PATH"
