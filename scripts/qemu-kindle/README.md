# qemu-kindle: the real Kindle build as a live desktop window

Runs the genuine **Kindle KOReader release** (`koreader-kindlepw2`, ARMv7
softfp, glibc 2.12) under `qemu-user`, with our Rust
`libkoreader-cre.so` swapped in, and shows the device framebuffer in an
X11 window (WSLg). Mouse press/drag/release are forwarded as touch
events, so page turns, menus and dialogs work.

There is **no SDL** in this path: the Kindle platform code runs
unmodified — `ffi/framebuffer_einkfb` on `/dev/fb0`, `fbink_input_scan`,
the lipc fake-event pipe, device model detection, everything.

## What the shim fakes (and why)

A PC is missing the device bits, so `fakekindle.c` is preloaded into the
guest (it is compiled with the same toolchain, so it is an ARM .so):

| Guest expects | Shim provides |
|---|---|
| `/proc/usid` | a file with a PW2 serial prefix (`B0D4…`) → device model detection |
| `/dev/fb0` | a 758×1024 8bpp file; `FBIOGET_*` ioctls filled in, eink update ioctls accepted |
| `/dev/input/event1` | a pipe plus evdev ioctls (axis ranges, capabilities) |
| tap/move input | command FIFO `state/taps`: `d x y`, `m x y`, `u`, `t x y` → evdev protocol-B events |
| `lipc-wait-event` | guest exec is impossible under qemu-user (no binfmt_misc), so exec is intercepted and the child parks; pipe write ends are duplicated and kept open so the input loop never sees EOF |

## Usage

```sh
scripts/qemu-kindle/run.sh                     # file manager
scripts/qemu-kindle/run.sh ~/juliet.epub       # open a book directly
scripts/qemu-kindle/run.sh --no-viewer ~/juliet.epub   # headless
```

The window title is `Kindle PW2 - qemu`. Click and drag inside it to
interact. Close the window to stop.

Requirements:

- kindlepw2 crosstool-NG toolchain in `~/x-tools` (override `X_TOOLS`)
- `qemu-arm` (qemu-user) — the distro package or an extracted `qemu-user` `.deb`
- an **armel** sysroot, e.g. Debian's `gcc-arm-linux-gnueabi`
  (`/usr/arm-linux-gnueabi`). The toolchain's own glibc 2.12 loader cannot
  run under qemu-user, and KOReader only needs ≤ GLIBC_2.12 symbols, so a
  newer armel rootfs is fine.
- WSLg for the window (`DISPLAY=:0`), `libX11` headers for the viewer
- `curl`, `unzip`, `python3`

Results are cached in `~/.cache/cre-qemu-kindle`
(`CRE_QEMU_CACHE` overrides): the KOReader package, the log (`reader.log`),
the shim, and the last framebuffer (`state/fb0`, `frame.pgm`).

## Notes

- KOReader logs are in `~/.cache/cre-qemu-kindle/reader.log`; our cre module
  prints stage timings there (`CRE_LIB_TRACE=1` is set by the harness).
- The `startup script has been updated` dialog is avoided by copying
  `koreader.sh` to `/var/tmp/koreader.sh`, exactly like the real launcher.
- The harness replaces `libs/libkoreader-cre.so` in the cached package;
  the original is kept as `libkoreader-cre.so.orig`.
