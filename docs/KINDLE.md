# Kindle Paperwhite 2 (glibc 2.12)

The module has to load and run on a stock Kindle Paperwhite 2: an i.MX6
SoloLite (Cortex-A9, ARMv7), a **softfp** ABI, **glibc 2.12** and a LuaJIT
host. The prebuilt Rust standard library for `armv7-unknown-linux-gnueabi` is
built for glibc 2.17, so several compatibility measures are needed before the
module can run there. This document lists them.

## The target and the toolchain

- Target: `armv7-unknown-linux-gnueabi`, the **soft-float ABI** variant. A
  hard-float (`gnueabihf`) build does not load: the device's libc and LuaJIT
  use `ld-linux.so.3`, not `ld-linux-armhf.so.3`.
- Toolchain: the KOReader `koxtoolchain` crosstool-NG toolchain
  (`arm-kindlepw2-linux-gnueabi`), whose sysroot has glibc 2.12.
- The linker and `ar`/`nm`/`strip` from that toolchain are selected through
  `CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_LINKER` and friends by
  `scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh`.

The prebuilt `rust-std` is built for glibc 2.17, but ELF symbol-version
bindings are resolved at **link time** against the toolchain's 2.12 sysroot.
That is why the module can be linked against an older libc than the one Rust
shipped with, as long as no symbol newer than 2.12 is required.

## VFP without leaving the softfp ABI

The default Rust target is `+soft-float`: every float operation is a libgcc
call, which is slow for text shaping and painting. The build enables VFP3
while keeping the softfp ABI:

```text
-C target-cpu=cortex-a9 -C target-feature=+vfp3,-soft-float,+strict-align
```

`+strict-align` is not optional. The softfp ABI only guarantees 8-byte
alignment at public interfaces, and VFP loads fault on unaligned addresses:
an earlier build without it crashed with `SIGBUS` on the device. `+strict-align`
makes the compiler align every VFP access, keeping the speed and fixing the
fault. `CRE_NO_VFP=1` builds the all-software-float fallback instead.

`CRE_NEON=1` is the variant built for the current device deployment; it adds
`+neon` to the same flags.

## The `getauxval` shim

The only link-time symbol gap found on glibc 2.12 is `getauxval`:

- Rust's standard library calls `getauxval(AT_MINSIGSTKSZ)` from
  `sigstack_size()` while setting up the alternate signal stack for every
  thread;
- glibc only started exporting `getauxval` in **2.16**.

Without a definition the module fails to load with
`undefined symbol: getauxval`, regardless of the routing of the rest of the
symbols.

`cre-lua` therefore has an `old-glibc` cargo feature, enabled by the Kindle
build script, that compiles a small `getauxval` replacement:

- it reads `/proc/self/auxv` (pairs of native words terminated by `AT_NULL`);
- it returns 0 when the entry is absent, matching glibc's semantics;
- it is `#[no_mangle] extern "C"`, so it satisfies Rust std and harmlessly
  shadows the newer libc function on newer devices;
- it is compiled only for Kindle builds and only exercises the 32-bit ARM
  layout of the auxiliary vector.

Source: `crates/cre-lua/src/lib.rs`, `mod old_glibc`.

## Symbol-version and dependency audit

The build script does not trust the result; after linking and stripping it
checks:

1. **Highest required glibc symbol**: `objdump -T` must report a version no
   newer than `GLIBC_2.12`.
2. **NEEDED libraries**: only the set the device provides.
3. **Undefined symbols**: weak symbols are allowed (Rust falls back when an
   old libc lacks them); every hard undefined symbol outside
   `GLIBC_*`/`GCC_*`/Lua is reported.
4. **Entry point**: `luaopen_cre` must be exported, and the Lua API symbols
   must stay undefined so the host LuaJIT resolves them at `dlopen` time, as
   upstream `libkoreader-cre.so` does.

Current results (VFP and NEON builds):

| Check | Result |
|---|---|
| Highest glibc version | `GLIBC_2.12` (also `GLIBC_2.4`) |
| `getauxval` | defined in the module (`T getauxval`) |
| NEEDED | `libgcc_s.so.1`, `librt.so.1`, `libpthread.so.0`, `libm.so.6`, `libdl.so.2`, `libc.so.6`, `ld-linux.so.3` |
| Hard non-libc undefined | none |
| Exported entry point | `luaopen_cre` |

`librt.so.1` is expected: glibc 2.12 still provides `clock_gettime` there
(newer glibc versions moved it to libc). `libgcc_s.so.1` is the unwinder, the
same dependency upstream has. `ld-linux.so.3` confirms the softfp ABI.

## Size on flash and in RAM

Executable pages count toward the process RSS on the device, and the module
lives on the Kindle's flash, so the release profile uses:

```text
CARGO_PROFILE_RELEASE_LTO=thin
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
```

and the resulting `.so` is stripped. Without it the module is about 21 MB;
with thin LTO and one codegen unit it is about 19.7 MB (VFP) / 19.8 MB (NEON),
most of which is Rust std linked statically plus the font/SVG/zip/image
dependencies.

## Build variants

| Variant | Flags |
|---|---|
| VFP (default) | `+vfp3,-soft-float,+strict-align` |
| NEON (current device build) | `+vfp3,+neon,-soft-float,+strict-align` |
| Software float fallback | target default (`+soft-float`), `CRE_NO_VFP=1` |
| `vello_cpu` painter | `CRE_VELLO_CPU=1` (adds the backend) |

Each variant gets its own `CARGO_TARGET_DIR`, so switching variants does not
mix build artifacts.

## Verifying a build

```sh
X_TOOLS="$HOME/x-tools/arm-kindlepw2-linux-gnueabi"
TOOL="$X_TOOLS/bin/arm-kindlepw2-linux-gnueabi"
SO=target-arm-kindlepw2-linux-gnueabi-neon/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so

"$TOOL-objdump" -T "$SO" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -V -u | tail -1
"$TOOL-readelf" -d "$SO" | grep NEEDED
"$TOOL-nm" -D "$SO" | grep -E ' getauxval$| T luaopen_cre$'
```

Expected: `GLIBC_2.12`, the NEEDED list above, `getauxval` and `luaopen_cre`
defined.

## Trade-offs

**Pros**

- The module loads on a stock Paperwhite 2 with glibc 2.12 and the softfp
  ABI, without replacing the system libc or using a special launcher.
- VFP3 stays enabled, so shaping and painting do not pay the all-soft-float
  cost; `+strict-align` keeps that safe on real hardware.
- The compatibility is auditable: the build script fails loudly if a new
  symbol requires a glibc newer than 2.12 or if the entry point disappears.
- The same source still builds for x86_64 and for the soft-float fallback.

**Cons**

- The Kindle build needs a specific crosstool-NG toolchain; a plain
  `cargo build --target armv7-unknown-linux-gnueabi` is not enough.
- The prebuilt `rust-std` is built for 2.17, so every Rust toolchain upgrade
  has to be re-audited for new libc symbols; the `getauxval` shim is the kind
  of gap this can expose.
- `+strict-align` gives up some performance compared with unaligned VFP
  accesses, and the `old-glibc` shim is 32-bit Linux specific.
- Thin LTO and one codegen unit make the cross build slower than a debug
  build, and every variant needs its own target directory.
- The module is still about 20 MB, because Rust std and the image/font
  dependencies are linked statically into a `cdylib`.
