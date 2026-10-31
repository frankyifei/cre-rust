#!/usr/bin/env bash
# Builds the cre-lua crate as the LuaJIT loadable module for Kindle PW2
# (ARMv7 Cortex-A9, softfp EABI, glibc 2.12) using the NiLuJe crosstool-NG
# toolchain in ~/x-tools (override with X_TOOLS).
#
# Target choice: armv7-unknown-linux-gnueabi (soft-float ABI) because the
# toolchain is configured with -mfloat-abi=softfp, matching the device's
# system libc and LuaJIT. The prebuilt rust-std targets glibc 2.17, but ELF
# symbol version bindings are resolved at link time against this toolchain's
# glibc 2.12 sysroot, so the module only requires GLIBC_2.4/2.12 on device.
#
# Differences from the generic ARM build:
#   * `old-glibc` feature enables a getauxval fallback: Rust std calls
#     getauxval(AT_MINSIGSTKSZ) for the thread signal-stack size and glibc
#     only exported it from 2.16, so glibc-2.12 devices would fail to load
#     the module without it.
#   * the produced binary is stripped (debug info is useless on device).
#
# Like official KOReader builds, the .so intentionally leaves the Lua C API
# symbols undefined; they are resolved from the host LuaJIT process at
# dlopen time. libgcc_s.so.1 (unwinder) is a NEEDED dependency, exactly as
# in the upstream libkoreader-cre.so.
set -euo pipefail

cd /home/frank/src/kindle-project

X_TOOLS="${X_TOOLS:-$HOME/x-tools/arm-kindlepw2-linux-gnueabi}"
TOOL_PREFIX="${TOOL_PREFIX:-arm-kindlepw2-linux-gnueabi}"
TOOL="${X_TOOLS}/bin/${TOOL_PREFIX}"
TARGET="${TARGET:-armv7-unknown-linux-gnueabi}"

gcc="${TOOL}-gcc"
ar="${TOOL}-ar"
nm="${TOOL}-nm"
strip="${TOOL}-strip"
readelf="${TOOL}-readelf"
objdump="${TOOL}-objdump"

for tool in "$gcc" "$ar" "$nm" "$strip" "$readelf" "$objdump"; do
    if [ ! -x "$tool" ]; then
        echo "error: missing toolchain binary: $tool" >&2
        echo "set X_TOOLS to the crosstool-NG install prefix" >&2
        exit 1
    fi
done

target_env="${TARGET//-/_}"
target_env="${target_env^^}"

export "CARGO_TARGET_${target_env}_LINKER=${gcc}"
export "CARGO_TARGET_${target_env}_AR=${ar}"
export "CARGO_TARGET_${target_env}_NM=${nm}"
export CC_armv7_unknown_linux_gnueabi="$gcc"
export CXX_armv7_unknown_linux_gnueabi="${TOOL}-g++"
export AR_armv7_unknown_linux_gnueabi="$ar"
export CROSS_COMPILE="${TOOL_PREFIX}-"
export LINUX=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-${TOOL_PREFIX}}"

# VFP vs soft-float:
#   The default rustc target is "+soft-float": every float op is a libgcc
#   call. Removing that feature (with +vfp3) keeps the softfp ABI but emits
#   VFP instructions - much faster. The softfp ABI only guarantees 8-byte
#   alignment at public interfaces, and VFP loads fault on unaligned data,
#   so plain VFP crashed on real hardware (SIGBUS). +strict-align makes the
#   compiler align every VFP access, which keeps the speed and fixes the
#   fault (validated under qemu's strict default CPU, which reproduced it).
#   Set CRE_NO_VFP=1 for the safe all-software-float build.
if [ "${CRE_NO_VFP:-0}" = "1" ]; then
    echo "== building software-float variant (CRE_NO_VFP=1)"
else
    export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=cortex-a9 -C target-feature=+vfp3,-soft-float,+strict-align"
fi

# LTO + one codegen unit: the module is ~21 MB without it, which costs RSS
# (executed pages) and flash space; thin LTO typically removes a third.
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-thin}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-1}"

cargo build --release --target "$TARGET" -p cre-lua --features old-glibc

release_dir="${CARGO_TARGET_DIR}/${TARGET}/release"
out="${release_dir}/libkoreader-cre.so"
cp "${release_dir}/libcre_lua.so" "$out"
"$strip" --strip-unneeded "$out"

echo
echo "== built: $out =="
ls -la "$out"
file "$out" 2>/dev/null || true

max_glibc="$("$objdump" -T "$out" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -V -u | tail -1)"
echo "max required glibc symbol version: ${max_glibc:-none} (device has 2.12)"

needed="$("$readelf" -d "$out" | grep NEEDED | sed -E 's/.*\[(.*)\]/\1/' | tr '\n' ' ')"
echo "NEEDED: $needed"

# Weak symbols ("w") are optional: Rust falls back when the old libc lacks
# them. Only hard undefineds outside libc/GCC versions are suspicious.
bad_undefined="$("$nm" -D --undefined-only "$out" \
    | grep -vE ' lua' \
    | grep -vE '^ +w ' \
    | grep -vE 'GLIBC_|GCC_' \
    || true)"
if [ -n "$bad_undefined" ]; then
    echo "WARNING: unexpected non-libc undefined symbols:" >&2
    echo "$bad_undefined" >&2
fi

if ! "$nm" -D --defined-only "$out" | grep -q ' T luaopen_cre$'; then
    echo "error: luaopen_cre is not exported" >&2
    exit 1
fi
lua_undefined="$("$nm" -D --undefined-only "$out" | grep -cE ' lua' || true)"
echo "exported entry point: luaopen_cre"
echo "undefined Lua API symbols (resolved by host LuaJIT): $lua_undefined"
echo
echo "deploy: copy to the device as koreader/libs/libkoreader-cre.so"
