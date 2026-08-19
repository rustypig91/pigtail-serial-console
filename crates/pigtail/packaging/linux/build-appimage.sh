#!/usr/bin/env bash
#
# Build a distro-agnostic AppImage from an already-built pigtail binary.
#
#   build-appimage.sh <path-to-pigtail> <version> [output-dir]
#
# Produces pigtail-v<version>-x86_64.AppImage in the output directory
# (default: the current directory).
set -euo pipefail

BIN="${1:?usage: build-appimage.sh <path-to-pigtail> <version> [output-dir]}"
VERSION="${2:?usage: build-appimage.sh <path-to-pigtail> <version> [output-dir]}"
OUT_DIR="$(cd "${3:-.}" && pwd)"

LINUX_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(dirname "$LINUX_DIR")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/AppDir"

install -Dm755 "$BIN" "$APPDIR/usr/bin/pigtail"
install -Dm644 "$LINUX_DIR/pigtail.desktop" "$APPDIR/usr/share/applications/pigtail.desktop"
for size in 16 32 48 64 128 256; do
    install -Dm644 "$PKG_DIR/icons/hicolor/${size}x${size}/apps/pigtail.png" \
        "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/pigtail.png"
done

# linuxdeploy and its AppImage output plugin. "continuous" is a rolling build;
# it is the channel upstream publishes and the one their docs point at.
BASE="https://github.com/linuxdeploy"
curl -sSfLo "$WORK/linuxdeploy" \
    "$BASE/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
curl -sSfLo "$WORK/linuxdeploy-plugin-appimage" \
    "$BASE/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage"
chmod +x "$WORK/linuxdeploy" "$WORK/linuxdeploy-plugin-appimage"
export PATH="$WORK:$PATH"

# winit and glutin dlopen their X11/xkb libraries, so they do not show up in the
# binary's ELF needed-list and linuxdeploy cannot discover them on its own.
# GL/EGL are deliberately left out: those must come from the host's drivers.
ldconfig_bin=""
for candidate in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
    if command -v "$candidate" >/dev/null 2>&1; then
        ldconfig_bin="$candidate"
        break
    fi
done

extra_libs=()
if [ -n "$ldconfig_bin" ]; then
    cache="$("$ldconfig_bin" -p)"
    for lib in libX11.so.6 libXcursor.so.1 libXi.so.6 libXrandr.so.2 \
        libxkbcommon.so.0 libxkbcommon-x11.so.0; do
        path="$(printf '%s\n' "$cache" | awk -v l="$lib" '$1 == l && /x86-64/ { print $NF; exit }')"
        if [ -n "$path" ]; then
            extra_libs+=("--library=$path")
        else
            echo "warning: $lib not found on this system, not bundling it" >&2
        fi
    done
else
    echo "warning: ldconfig not found, relying on linuxdeploy's own detection" >&2
fi

# GitHub runners have no FUSE, so the tools must unpack themselves to run.
export APPIMAGE_EXTRACT_AND_RUN=1
export VERSION
export OUTPUT="pigtail-v${VERSION}-x86_64.AppImage"

cd "$WORK"
linuxdeploy \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/pigtail" \
    --desktop-file "$APPDIR/usr/share/applications/pigtail.desktop" \
    --icon-file "$PKG_DIR/icons/hicolor/256x256/apps/pigtail.png" \
    "${extra_libs[@]}" \
    --output appimage

mkdir -p "$OUT_DIR"
mv "$WORK/$OUTPUT" "$OUT_DIR/$OUTPUT"
echo "wrote $OUT_DIR/$OUTPUT"
