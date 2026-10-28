#!/usr/bin/env bash
set -euo pipefail

cd /home/frank/src/kindle-project

target="${TARGET:-arm-unknown-linux-gnueabi}"
cross_prefix="${CROSS_PREFIX:-arm-linux-gnueabi}"
gcc="${GCC:-/usr/bin/${cross_prefix}-gcc}"
ar="${AR:-/usr/bin/${cross_prefix}-ar}"
nm="${NM:-/usr/bin/${cross_prefix}-nm}"

target_env="${target//-/_}"
target_env="${target_env^^}"

export "CARGO_TARGET_${target_env}_LINKER=${gcc}"
export "CARGO_TARGET_${target_env}_AR=${ar}"
export "CARGO_TARGET_${target_env}_NM=${nm}"

export CROSS_COMPILE="${cross_prefix}-"
export LINUX=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-${target}}"

cargo build --release --target "$target" -p cre-lua

release_dir="${CARGO_TARGET_DIR}/${target}/release"
cp "${release_dir}/libcre_lua.so" "${release_dir}/libkoreader-cre.so"
