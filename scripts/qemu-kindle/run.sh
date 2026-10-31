#!/usr/bin/env bash
# Live Kindle emulator: runs the genuine Kindle KOReader release (kindlepw2
# package) under qemu-user with our Rust cre module, and shows its eink
# framebuffer in an X11 window (WSLg). Mouse clicks/drags become touch events.
#
# Usage:
#   scripts/qemu-kindle/run.sh [book] [--no-viewer]
#
# Env:
#   KO_VERSION     KOReader release (default v2026.07.1)
#   QEMU           qemu-arm binary (default: qemu-arm from PATH)
#   QEMU_SYSROOT   armel rootfs for the guest loader (default /usr/arm-linux-gnueabi)
#   CRE_QEMU_CACHE cache dir (default ~/.cache/cre-qemu-kindle)
#   VIEWER_SCALE   window scale (default 0.72)
#   NO_VIEWER=1    headless (framebuffer still dumped to $CACHE/frame.pgm)
#
# The harness does NOT use SDL: the kindle platform runs unmodified
# (einkfb framebuffer, fbink input scan, lipc fake-event pipe). A guest
# LD_PRELOAD shim (fakekindle.c) supplies /proc/usid, /dev/fb0 and an evdev
# touchscreen; pipe write ends are kept alive because KOReader's fake-event
# child cannot exec lipc-wait-event under qemu-user (no binfmt_misc).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE="${CRE_QEMU_CACHE:-$HOME/.cache/cre-qemu-kindle}"
KO_VERSION="${KO_VERSION:-v2026.07.1}"
KO_URL="https://github.com/koreader/koreader/releases/download/${KO_VERSION}/koreader-kindlepw2-${KO_VERSION}.zip"

BOOK=""
NO_VIEWER="${NO_VIEWER:-0}"
while [ $# -gt 0 ]; do
    case "$1" in
    --no-viewer) NO_VIEWER=1 ;;
    *) BOOK="$1" ;;
    esac
    shift
done

TC="${X_TOOLS:-$HOME/x-tools/arm-kindlepw2-linux-gnueabi}"
CC="$TC/bin/arm-kindlepw2-linux-gnueabi-gcc"
[ -x "$CC" ] || { echo "error: missing $CC" >&2; exit 1; }

QEMU="${QEMU:-$(command -v qemu-arm || true)}"
SYSROOT="${QEMU_SYSROOT:-/usr/arm-linux-gnueabi}"
if [ -z "$QEMU" ] || [ ! -x "$SYSROOT/lib/ld-linux.so.3" ]; then
    echo "error: need qemu-arm (qemu-user) and an armel sysroot." >&2
    echo "       Set QEMU and QEMU_SYSROOT; e.g. extract qemu-user .deb and" >&2
    echo "       install gcc-arm-linux-gnueabi (armel rootfs)." >&2
    exit 1
fi

mkdir -p "$CACHE"
KO="$CACHE/koreader"
if [ ! -x "$KO/luajit" ]; then
    echo "== downloading KOReader ${KO_VERSION} (kindlepw2) =="
    curl -fSL --max-time 600 -o "$CACHE/ko.zip" "$KO_URL"
    unzip -q -o "$CACHE/ko.zip" -d "$CACHE"
fi

echo "== building cre module =="
CARGO_TARGET_DIR="$REPO/target-arm-kindlepw2-linux-gnueabi" \
    bash "$REPO/scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh" >/dev/null
CRE="$REPO/target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so"
[ -f "$KO/libs/libkoreader-cre.so.orig" ] || cp "$KO/libs/libkoreader-cre.so" "$KO/libs/libkoreader-cre.so.orig"
cp "$CRE" "$KO/libs/libkoreader-cre.so"

STATE="$CACHE/state"
SHIM="$CACHE/shim"
mkdir -p "$STATE" "$SHIM"
echo "== building guest shim + viewer =="
"$CC" -shared -fPIC -O2 -o "$SHIM/fakekindle.so" "$REPO/scripts/qemu-kindle/fakekindle.c" -ldl -lpthread
if [ "$NO_VIEWER" = 0 ]; then
    cc -O2 -o "$CACHE/qemu-kindle-viewer" "$REPO/scripts/qemu-kindle/viewer.c" -lX11
fi

printf 'B0D4123456789012\n' >"$STATE/usid"
rm -f "$STATE/taps"
mkfifo "$STATE/taps"
python3 - "$STATE/fb0" <<'PY'
import sys
with open(sys.argv[1], "wb") as f:
    f.truncate(758 * 1024)
PY

# Keep the "startup script updated" dialog away, like the real launcher does.
if [ -f "$KO/koreader.sh" ]; then
    cp -f "$KO/koreader.sh" /var/tmp/koreader.sh
    chmod 777 /var/tmp/koreader.sh
fi

mkdir -p "$CACHE/home"
LOG="$CACHE/reader.log"
echo "== launching Kindle KOReader under qemu =="
env HOME="$CACHE/home" \
    FAKEKINDLE_DIR="$STATE" \
    LD_LIBRARY_PATH="$KO/libs" \
    LD_PRELOAD="$SHIM/fakekindle.so" \
    CRE_LIB_TRACE=1 \
    "$QEMU" -L "$SYSROOT" "$KO/luajit" "$KO/reader.lua" ${BOOK:+"$BOOK"} \
    >"$LOG" 2>&1 &
QEMU_PID=$!
echo "qemu pid $QEMU_PID, log $LOG"

cleanup() {
    kill "$QEMU_PID" 2>/dev/null || true
    wait "$QEMU_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ "$NO_VIEWER" = 1 ]; then
    echo "headless; Ctrl-C to stop. Framebuffer: $STATE/fb0"
    wait "$QEMU_PID"
else
    echo "== viewer window (close it to stop) =="
    env DISPLAY="${DISPLAY:-:0}" "$CACHE/qemu-kindle-viewer" \
        "$STATE/fb0" "$STATE/taps" "${VIEWER_SCALE:-0.72}" "Kindle PW2 - qemu"
fi

echo "== done; cre trace =="
grep -E 'cre-lib:' "$LOG" | tail -10 || true
