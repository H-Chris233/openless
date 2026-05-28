#!/usr/bin/env bash
# Inject fcitx5 plugin files into Linux packages at system paths.
# Usage: ./inject-fcitx5-plugin.sh <package-path>
#
# Supports: .deb, .rpm, AppImage (AppDir)
set -euo pipefail

PKG="$1"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="$SCRIPT_DIR/linux-fcitx5-plugin/build_release"
SO_SRC="$PLUGIN_DIR/libopenless.so"
CONF_SRC="$PLUGIN_DIR/openless.conf"

if [ ! -f "$SO_SRC" ] || [ ! -f "$CONF_SRC" ]; then
    echo "[inject-fcitx5] Plugin not built — run build.sh first. Skipping."
    exit 0
fi

TARGET_LIB="/usr/lib/x86_64-linux-gnu/fcitx5/libopenless.so"
TARGET_CONF="/usr/share/fcitx5/addon/openless.conf"

case "$PKG" in
    *.deb)
        echo "[inject-fcitx5] Injecting into deb: $PKG"
        TMPDIR=$(mktemp -d)
        trap 'rm -rf "$TMPDIR"' EXIT
        dpkg-deb -R "$PKG" "$TMPDIR"
        mkdir -p "$TMPDIR/usr/lib/x86_64-linux-gnu/fcitx5"
        mkdir -p "$TMPDIR/usr/share/fcitx5/addon"
        cp "$SO_SRC" "$TMPDIR/$TARGET_LIB"
        cp "$CONF_SRC" "$TMPDIR/$TARGET_CONF"
        dpkg-deb -b "$TMPDIR" "$PKG"
        echo "[inject-fcitx5] Done — deb updated"
        ;;
    *.rpm)
        echo "[inject-fcitx5] Injecting into rpm: $PKG"
        TMPDIR=$(mktemp -d)
        trap 'rm -rf "$TMPDIR"' EXIT
        cd "$TMPDIR"
        rpm2cpio "$PKG" | cpio -idm 2>/dev/null || true
        mkdir -p ".$TARGET_LIB" ".$TARGET_CONF" 2>/dev/null || true
        rmdir ".$TARGET_LIB" ".$TARGET_CONF" 2>/dev/null || true
        mkdir -p "$(dirname ".$TARGET_LIB")"
        mkdir -p "$(dirname ".$TARGET_CONF")"
        cp "$SO_SRC" ".$TARGET_LIB"
        cp "$CONF_SRC" ".$TARGET_CONF"
        # rpmrebuild is the safest way to repack
        if command -v rpmrebuild &>/dev/null; then
            rpmrebuild -np -d "$TMPDIR" "$PKG" 2>/dev/null || {
                echo "[inject-fcitx5] rpmrebuild failed — rpm injection not available, skipping"
                exit 0
            }
        else
            echo "[inject-fcitx5] rpmrebuild not found — install it for rpm injection support. Skipping."
            exit 0
        fi
        echo "[inject-fcitx5] Done — rpm updated"
        ;;
    *.AppImage|*/AppDir|*/appdir)
        # AppImage / AppDir: add files to the AppDir before packaging
        echo "[inject-fcitx5] Injecting into AppDir: $PKG"
        if [ -d "$PKG" ]; then
            APPDIR="$PKG"
        else
            # Extract AppImage to get AppDir
            TMPDIR=$(mktemp -d)
            trap 'rm -rf "$TMPDIR"' EXIT
            cd "$TMPDIR"
            "$PKG" --appimage-extract 2>/dev/null || {
                echo "[inject-fcitx5] Cannot extract AppImage — skipping"
                exit 0
            }
            APPDIR="$TMPDIR/squashfs-root"
        fi
        mkdir -p "$APPDIR/$TARGET_LIB" 2>/dev/null || true
        rmdir "$APPDIR/$TARGET_LIB" 2>/dev/null || true
        mkdir -p "$APPDIR/$(dirname "$TARGET_LIB")"
        mkdir -p "$APPDIR/$(dirname "$TARGET_CONF")"
        cp "$SO_SRC" "$APPDIR/$TARGET_LIB"
        cp "$CONF_SRC" "$APPDIR/$TARGET_CONF"
        echo "[inject-fcitx5] Done — AppDir updated"
        ;;
    *)
        echo "[inject-fcitx5] Unknown package format: $PKG — skipping"
        exit 1
        ;;
esac
