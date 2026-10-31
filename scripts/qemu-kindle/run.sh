#!/usr/bin/env bash
# Repo wrapper around tools/qemu-kindle/qk.
# Builds our cre module and opens the Kindle KOReader emulator window.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE="${CRE_QEMU_CACHE:-$HOME/.cache/cre-qemu-kindle}"
TARGET="$REPO/target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so"
exec env \
    QK_CRE_BUILD_SCRIPT="$REPO/scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh" \
    QK_CRE_BUILD_OUTPUT="$TARGET" \
    "$REPO/tools/qemu-kindle/qk" --cache-dir "$CACHE" run "$@"
