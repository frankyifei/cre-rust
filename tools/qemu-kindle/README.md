# qk — Kindle KOReader on your desktop

`qk` runs the **genuine Kindle KOReader build** (`koreader-kindlepw2`)
under `qemu-user` and shows its e-ink framebuffer in a window. Mouse
press/drag/release become touch events, so page turns, menus and dialogs
work. You can point it at your own `libkoreader-cre.so`, which makes it a
fast way to try a replacement renderer without touching a device.

There is no SDL: the Kindle platform code runs unmodified
(`ffi/framebuffer_einkfb` on `/dev/fb0`, `fbink_input_scan`, the lipc
fake-event pipe, model detection). A small preloaded ARM shim supplies
the device internals a PC lacks.

## Requirements

- `qemu-arm` (qemu-user); on Debian/Ubuntu: `apt install qemu-user`
- an **armel** sysroot: `apt install gcc-arm-linux-gnueabi` gives
  `/usr/arm-linux-gnueabi`. (KOReader only needs ≤ GLIBC_2.12 symbols, and
  the Kindle toolchain's glibc 2.12 loader cannot run under qemu-user, so
  a newer armel rootfs is the right thing.)
- an ARM cross compiler to build the shim: the same `gcc-arm-linux-gnueabi`,
  a kindlepw2 crosstool-NG toolchain, or any soft-float ARM gcc
- `libX11` headers for the viewer (`apt install libx11-dev`), plus a
  display (X11/WSLg)
- a `libkoreader-cre.so` to test, or a script that builds one

## Quick start

From the repository root:

```sh
tools/qemu-kindle/run.sh ~/books/juliet.epub
```

`run.sh` builds this repository's ARM module and installs it. It uses
`~/.cache/cre-qemu-kindle` for the package cache. Set `CRE_QEMU_CACHE` to
change the cache directory.

Run without a window:

```sh
tools/qemu-kindle/run.sh --headless ~/books/juliet.epub
```

To use `qk` directly, install the module yourself or set `cre_lib` in the
configuration. `qk run` uses `~/.cache/qemu-kindle` by default. `qk shot`
writes the last frame to a PNG.

## Configuration

No paths are hardcoded to a particular machine. Settings resolve as
defaults → config file → `QK_*` environment variables → CLI flags.

Config file: `~/.config/qemu-kindle/config.json` (or `--config FILE`):

```json
{
  "ko_version": "v2026.07.1",
  "cache_dir": "~/.cache/qemu-kindle",
  "triplet": "arm-kindlepw2-linux-gnueabi",
  "toolchain": "~/x-tools/arm-kindlepw2-linux-gnueabi",
  "cc": "",
  "qemu": "qemu-arm",
  "sysroot": "/usr/arm-linux-gnueabi",
  "cre_lib": "~/src/cre-rust/target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so",
  "cre_build_script": "",
  "display": ":0",
  "viewer_scale": 0.72,
  "viewer_cc": "cc",
  "title": "Kindle - qemu",
  "startup_script_dir": "/var/tmp",
  "timeout": 0,
  "state_dir": "",
  "log_file": ""
}
```

Key options:

| Key | Meaning |
|---|---|
| `cre_lib` | prebuilt module to install into the package |
| `cre_build_script` | instead, run a script to build the module |
| `package_zip` / `package_dir` | use a local package instead of downloading |
| `triplet`, `toolchain`, `cc` | find/build the ARM shim |
| `qemu`, `sysroot` | guest runtime |
| `display`, `viewer_scale` | window behaviour (`--no-viewer` for headless) |
| `startup_script_dir` | where `koreader.sh` is mirrored (avoids the update dialog on Kindle; `/var/tmp` on real devices) |
| `state_dir` | serial/framebuffer/tap FIFO location |
| `tap_delay` | optional one-shot tap after N seconds |

Environment equivalents: `QK_CRE_LIB`, `QK_TRIPLET`, `QK_QEMU`,
`QK_SYSROOT`, `QK_CACHE_DIR`, `QK_DISPLAY`, …

## Commands

- `qk run [book]` — run; `--no-viewer` for headless
- `qk fetch` — download the KOReader package only
- `qk build` — build the shim/viewer and install the module only
- `qk config` — print the effective configuration (with `_config_path`)
- `qk shot [out.png]` — convert the current framebuffer to PNG

## How input works

The viewer writes mouse actions to a FIFO:

```
d X Y    touch down
m X Y    touch move (throttled to ~30 Hz)
u        touch up
t X Y    complete tap
```

Two consumers exist, and the harness routes viewer input to the plugin one:

- **qkinput.koplugin** (default, installed into the package): polls the FIFO
  and broadcasts ordinary KOReader `Gesture` events (`tap`, `swipe`). This is
  what makes clicks work in the window. It is inert unless `QK_TAP_FIFO` is
  set.
- the guest shim also knows how to emulate a Linux protocol-B evdev
  touchscreen (`ABS_MT_*`, `BTN_TOUCH`, `SYN_REPORT`) on the same FIFO
  protocol, but under qemu-user KOReader's Lua gesture layer does not act on
  those synthetic evdev events, so the shim reads a separate, unused FIFO
  (`FAKEKINDLE_TAP_FIFO`) to avoid stealing the plugin's input.

Taps work end to end (the page turns). Drags are emitted as `swipe` gestures;
whether they page-turn depends on KOReader's swipe settings in this build.

### Scripted actions (`QK_SCRIPT`)

Synthesising menu taps is slow and brittle. `QK_SCRIPT=<file.lua>` makes
`qkinput.koplugin` load a Lua file once the document is ready and call the
function it returns with the reader UI, so any KOReader action can be driven
through its own modules:

```lua
local UIManager = require("ui/uimanager")
local Event = require("ui/event")

return function(ui)
    local toc = ui.document:getToc()
    UIManager:scheduleIn(2, function()
        local item = toc[#toc]
        ui.link:addCurrentLocationToStack()
        ui:handleEvent(Event:new("GotoXPointer", item.xpointer, item.xpointer))
    end)
end
```

`logger.info(...)` output lands in the run's `reader.log`, which makes it
usable for timing a path (`os.clock()` around the call) as well as for
reproducing a bug. The hook is inert unless `QK_SCRIPT` is set.

## Notes and limitations

- The package's `libs/libkoreader-cre.so` is replaced; the original is
  kept as `libkoreader-cre.so.orig`.
- qemu-user cannot exec guest binaries without `binfmt_misc`. KOReader's
  Kindle fake-event child runs `lipc-wait-event`, so the shim parks that
  exec and keeps pipe write ends alive; the input loop then simply sees
  an idle touchscreen.
- Rendering is grayscale 758×1024 (PW2). Other Kindle models work by
  changing the serial prefix in `prepare_state()` and the fb geometry
  constants; they are intentionally not exposed yet.
- Timing under qemu (TCG) is much slower than hardware; use it for
  behaviour and relative costs, not absolute performance. Defaults are
  `-tb-size 256 -cpu cortex-a9` (matches the Paperwhite 2's i.MX6
  SoloLite / Cortex-A9); the plain `any` CPU was unstable with the VFP
  code our module now emits.
- The Kindle build compiles with `-C target-feature=+vfp3,-soft-float`:
  rustc declares the armv7-gnueabi target soft-float, which turned every
  float operation into a libgcc call. The softfp ABI is preserved (f64
  still passed in integer registers), only the math uses VFPv3-D16.
