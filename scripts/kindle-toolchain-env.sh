#!/usr/bin/env bash
# Source this before building KOReader/cre for Kindle PW2 with the
# crosstool-NG toolchain in ~/x-tools.
#
#   source scripts/kindle-toolchain-env.sh
#   cd ~/koreader && ./kodev build kindlepw2
#
# or, for the cre module only:
#   ./scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh
#
# koreader-base detects the toolchain by looking for
# `arm-kindlepw2-linux-gnueabi-gcc` on PATH and then sets CHOST itself; no
# separate environment script is shipped by crosstool-NG.

export X_TOOLS="${X_TOOLS:-$HOME/x-tools/arm-kindlepw2-linux-gnueabi}"
export TOOL_PREFIX="${TOOL_PREFIX:-arm-kindlepw2-linux-gnueabi}"

if [ ! -x "$X_TOOLS/bin/${TOOL_PREFIX}-gcc" ]; then
    echo "kindle-toolchain-env: missing $X_TOOLS/bin/${TOOL_PREFIX}-gcc" >&2
    return 1 2>/dev/null || exit 1
fi

case ":$PATH:" in
    *":$X_TOOLS/bin:"*) ;;
    *) export PATH="$X_TOOLS/bin:$PATH" ;;
esac

export CHOST="${CHOST:-$TOOL_PREFIX}"
export CROSS_COMPILE="${CROSS_COMPILE:-$TOOL_PREFIX-}"

echo "kindle-toolchain-env: $("$X_TOOLS/bin/${TOOL_PREFIX}-gcc" -dumpmachine) (CHOST=$CHOST)"
