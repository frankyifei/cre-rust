#!/usr/bin/env bash
# Run the Kindle KOReader build under QEMU with this repository's cre module.
#
# Usage:
#   tools/qemu-kindle/run.sh [--headless] [BOOK]
#
# The harness builds the ARM module, installs it into the KOReader package, and
# starts KOReader. The first run downloads the package.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE="${CRE_QEMU_CACHE:-$HOME/.cache/cre-qemu-kindle}"
TARGET="$REPO/target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so"

VIEWER=()
if [ "${1:-}" = "--headless" ]; then
    VIEWER=(--no-viewer)
    shift
fi

exec env \
    QK_CRE_BUILD_SCRIPT="$REPO/scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh" \
    QK_CRE_BUILD_OUTPUT="$TARGET" \
    "$REPO/tools/qemu-kindle/qk" --cache-dir "$CACHE" ${VIEWER[@]+"${VIEWER[@]}"} run "$@"
