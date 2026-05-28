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

TARGET_CONF="/usr/share/fcitx5/addon/openless.conf"

case "$PKG" in
    *.deb)
        TARGET_LIB="/usr/lib/x86_64-linux-gnu/fcitx5/libopenless.so"
        echo "[inject-fcitx5] Injecting into deb: $PKG"
        TMPDIR=$(mktemp -d)
        trap 'rm -rf "$TMPDIR"' EXIT
        dpkg-deb -R "$PKG" "$TMPDIR"
        mkdir -p "$TMPDIR/$(dirname "$TARGET_LIB")"
        mkdir -p "$TMPDIR/$(dirname "$TARGET_CONF")"
        cp "$SO_SRC" "$TMPDIR/$TARGET_LIB"
        cp "$CONF_SRC" "$TMPDIR/$TARGET_CONF"
        dpkg-deb -b "$TMPDIR" "$PKG"
        echo "[inject-fcitx5] Done — deb updated"
        ;;
    *.rpm)
        TARGET_LIB="/usr/lib64/fcitx5/libopenless.so"
        echo "[inject-fcitx5] Injecting into rpm: $PKG"
        TMPDIR=$(mktemp -d)
        trap 'rm -rf "$TMPDIR"' EXIT
        cd "$TMPDIR"
        rpm2cpio "$PKG" | cpio -idm 2>/dev/null || true
        mkdir -p "$(dirname ".$TARGET_LIB")"
        mkdir -p "$(dirname ".$TARGET_CONF")"
        cp "$SO_SRC" ".$TARGET_LIB"
        cp "$CONF_SRC" ".$TARGET_CONF"
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
    */AppDir|*/appdir|*.AppDir)
        TARGET_LIB="/usr/lib/x86_64-linux-gnu/fcitx5/libopenless.so"
        # Inject into AppDir before it's packaged into AppImage.
        # Must be a directory, not an existing .AppImage file.
        if [ ! -d "$PKG" ]; then
            echo "[inject-fcitx5] AppImage injection only supports AppDir (directory), not a packaged .AppImage file. Skipping."
            exit 0
        fi
        echo "[inject-fcitx5] Injecting into AppDir: $PKG"
        mkdir -p "$PKG/$(dirname "$TARGET_LIB")"
        mkdir -p "$PKG/$(dirname "$TARGET_CONF")"
        cp "$SO_SRC" "$PKG/$TARGET_LIB"
        cp "$CONF_SRC" "$PKG/$TARGET_CONF"
        echo "[inject-fcitx5] Done — AppDir updated"
        ;;
    *)
        echo "[inject-fcitx5] Unknown package format: $PKG — skipping"
        exit 1
        ;;
esac
