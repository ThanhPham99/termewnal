#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)}"
if [[ -z "$VERSION" ]]; then
    echo "error: unable to detect workspace version from Cargo.toml" >&2
    exit 1
fi

ARCH="amd64"
TAR_ARCH="x86_64"
DIST_DIR="$ROOT_DIR/dist"
PKG_ROOT="$ROOT_DIR/target/pkg"
DEB_STAGE="$PKG_ROOT/termewnal_${VERSION}_${ARCH}"
TAR_NAME="termewnal-${VERSION}-linux-${TAR_ARCH}"
TAR_STAGE="$PKG_ROOT/$TAR_NAME"

mkdir -p "$DIST_DIR"
rm -rf "$DEB_STAGE" "$TAR_STAGE"

echo "==> Building Termewnal v${VERSION} release binary..."
cargo build --release -p termewnal-ui --bin termewnal

BIN_PATH="${CARGO_TARGET_DIR:-$ROOT_DIR/target}/release/termewnal"
if [[ ! -x "$BIN_PATH" ]]; then
    echo "error: expected release binary at $BIN_PATH" >&2
    exit 1
fi

DESKTOP_ENTRY='[Desktop Entry]
Type=Application
Name=Termewnal
GenericName=Terminal Emulator
Comment=Pure-Rust native terminal emulator and multi-protocol SSH/SFTP workspace
Exec=/usr/bin/termewnal %F
Icon=termewnal
Terminal=false
Categories=System;TerminalEmulator;Network;
Keywords=terminal;ssh;sftp;serial;telnet;zmodem;docker;
StartupNotify=true
StartupWMClass=termewnal'

echo "==> Staging Debian package (.deb)..."
mkdir -p \
    "$DEB_STAGE/DEBIAN" \
    "$DEB_STAGE/usr/bin" \
    "$DEB_STAGE/usr/share/applications" \
    "$DEB_STAGE/usr/share/doc/termewnal"

cp "$BIN_PATH" "$DEB_STAGE/usr/bin/termewnal"
chmod 0755 "$DEB_STAGE/usr/bin/termewnal"
if command -v strip >/dev/null 2>&1; then
    strip --strip-unneeded "$DEB_STAGE/usr/bin/termewnal" || true
fi

printf '%s\n' "$DESKTOP_ENTRY" > "$DEB_STAGE/usr/share/applications/termewnal.desktop"
chmod 0644 "$DEB_STAGE/usr/share/applications/termewnal.desktop"
cp "$ROOT_DIR/README.md" "$DEB_STAGE/usr/share/doc/termewnal/README.md"

for size in 32 64 128 256; do
    icon_dir="$DEB_STAGE/usr/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$icon_dir"
    cp "$ROOT_DIR/assets/termewnal-icon-${size}.png" "$icon_dir/termewnal.png"
    chmod 0644 "$icon_dir/termewnal.png"
done
mkdir -p "$DEB_STAGE/usr/share/icons/hicolor/512x512/apps"
cp "$ROOT_DIR/assets/termewnal-icon.png" "$DEB_STAGE/usr/share/icons/hicolor/512x512/apps/termewnal.png"
chmod 0644 "$DEB_STAGE/usr/share/icons/hicolor/512x512/apps/termewnal.png"

INSTALLED_SIZE_KB="$(du -sk "$DEB_STAGE/usr" | awk '{print $1}')"
cat > "$DEB_STAGE/DEBIAN/control" <<EOF
Package: termewnal
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${INSTALLED_SIZE_KB}
Depends: libc6, libgcc-s1, libxkbcommon0, libfontconfig1
Recommends: libwayland-client0, libx11-6, libdbus-1-3, libvulkan1
Maintainer: Thanh Pham <thanhpham110699@gmail.com>
Homepage: https://thanhpham99.github.io/apps/termewnal/
Description: Pure-Rust native terminal emulator and multi-protocol workspace
 Termewnal is a GPU-accelerated native Rust terminal emulator and workspace for
 SSH, resilient tunnels, SFTP+, Serial, Telnet, ZMODEM, and Docker containers,
 inspired by Eugeny/tabby.
EOF
chmod 0644 "$DEB_STAGE/DEBIAN/control"

DEB_OUT="$DIST_DIR/termewnal_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$DEB_STAGE" "$DEB_OUT"

echo "==> Staging portable tarball (.tar.gz)..."
mkdir -p "$TAR_STAGE/icons"
cp "$DEB_STAGE/usr/bin/termewnal" "$TAR_STAGE/termewnal"
chmod 0755 "$TAR_STAGE/termewnal"
cp "$ROOT_DIR/README.md" "$TAR_STAGE/README.md"
for size in 32 64 128 256; do
    cp "$ROOT_DIR/assets/termewnal-icon-${size}.png" "$TAR_STAGE/icons/termewnal-${size}.png"
done
cp "$ROOT_DIR/assets/termewnal-icon.png" "$TAR_STAGE/icons/termewnal-512.png"

cat > "$TAR_STAGE/termewnal.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Termewnal
GenericName=Terminal Emulator
Comment=Pure-Rust native terminal emulator and multi-protocol SSH/SFTP workspace
Exec=termewnal %F
Icon=termewnal
Terminal=false
Categories=System;TerminalEmulator;Network;
Keywords=terminal;ssh;sftp;serial;telnet;zmodem;docker;
StartupNotify=true
StartupWMClass=termewnal
EOF

cat > "$TAR_STAGE/install-user.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
APP_DIR="${HOME}/.local/share/applications"
ICON_BASE="${HOME}/.local/share/icons/hicolor"

mkdir -p "$BIN_DIR" "$APP_DIR"
install -m 0755 "$HERE/termewnal" "$BIN_DIR/termewnal"
sed "s|^Exec=termewnal|Exec=${BIN_DIR}/termewnal|" "$HERE/termewnal.desktop" > "$APP_DIR/termewnal.desktop"

for size in 32 64 128 256 512; do
    mkdir -p "$ICON_BASE/${size}x${size}/apps"
    install -m 0644 "$HERE/icons/termewnal-${size}.png" "$ICON_BASE/${size}x${size}/apps/termewnal.png"
done

echo "Installed Termewnal to ${BIN_DIR}/termewnal"
EOF
chmod 0755 "$TAR_STAGE/install-user.sh"

TAR_OUT="$DIST_DIR/${TAR_NAME}.tar.gz"
tar -C "$PKG_ROOT" -czf "$TAR_OUT" "$TAR_NAME"

(
    cd "$DIST_DIR"
    sha256sum "$(basename "$DEB_OUT")" "$(basename "$TAR_OUT")" > SHA256SUMS-linux.txt
)

echo "==> Built Linux packages:"
ls -lh "$DEB_OUT" "$TAR_OUT" "$DIST_DIR/SHA256SUMS-linux.txt"
