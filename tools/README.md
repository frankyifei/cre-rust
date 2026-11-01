# Tools

This directory holds the development tools. They are not part of the
`libkoreader-cre.so` module.

| Tool | Use |
|---|---|
| `qemu-kindle/` | Run the genuine Kindle KOReader build on a Linux desktop |
| `qemu-kindle/plugin/` | KOReader input and script plugin for the harness |
| `qemu-kindle/examples/` | Example `QK_SCRIPT` files |

## Quick start: run KOReader with QEMU

From the repository root:

```sh
tools/qemu-kindle/run.sh /path/to/book.epub
```

The script does these steps:

1. Builds the ARM module with `scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh`.
2. Downloads the Kindle KOReader package on the first run.
3. Installs the module into the package.
4. Starts KOReader under `qemu-arm`.
5. Opens an X11 window with the e-ink framebuffer.

Use `--headless` to run without a window:

```sh
tools/qemu-kindle/run.sh --headless /path/to/book.epub
```

The run log is `~/.cache/cre-qemu-kindle/reader.log`.

### Requirements

- `qemu-user` (`qemu-arm`)
- an armel sysroot, for example `/usr/arm-linux-gnueabi`
  (`apt install gcc-arm-linux-gnueabi`)
- a display for the window. WSLg works.
- `libx11-dev`
- the koxtoolchain for the module build

### Change the configuration

The harness reads a JSON file. See
[`qemu-kindle/README.md`](qemu-kindle/README.md) and
[`qemu-kindle/config.example.json`](qemu-kindle/config.example.json).

Common options:

| Option | Effect |
|---|---|
| `--cre-lib FILE` | Use a prebuilt module |
| `--no-viewer` | Do not open a window (use the wrapper `--headless`) |
| `--config FILE` | Use a JSON configuration file |
| `--cache-dir DIR` | Use another package cache |

## Debug plugin

`qemu-kindle/plugin/qkinput.koplugin` is installed into the guest package on
each run.

It does two jobs:

1. It converts viewer mouse events to KOReader gesture events.
2. It runs a Lua script when the book is ready.

Set `QK_SCRIPT` to a Lua file. The file must return a function. KOReader calls
the function with the reader UI:

```lua
return function(ui)
    -- ui.document, ui.menu, ui.rolling, ...
end
```

Use `UIManager:scheduleIn()` so KOReader can finish its startup work:

```lua
local UIManager = require("ui/uimanager")
local logger = require("logger")

return function(ui)
    UIManager:scheduleIn(3, function()
        logger.info("pages", ui.document:getPageCount())
    end)
end
```

Run an example:

```sh
QK_SCRIPT="$PWD/tools/qemu-kindle/examples/goto-page.lua" \
    tools/qemu-kindle/run.sh fixtures/koreader-test-data/leaves.epub
```

The examples:

- `examples/goto-page.lua`: log the page count and go to page 20.
- `examples/dump-toc.lua`: log the TOC and go to the first entry.

Use this hook to test a chapter jump, a font change, a page turn, or a timing
problem.

## Directory layout

```text
tools/
  README.md
  qemu-kindle/
    README.md            Full harness documentation
    run.sh               Repository wrapper (build module + run)
    qk                   Harness command-line tool
    config.example.json  Configuration example
    fakekindle.c         ARM shim for the device interfaces
    viewer.c             X11 framebuffer viewer
    plugin/              KOReader input and script plugin
    examples/            QK_SCRIPT examples
```

## Notes

- Keep paths configurable. Do not hardcode a home directory.
- `qk` uses `~/.cache/qemu-kindle` by default. `run.sh` uses
  `~/.cache/cre-qemu-kindle`. Set `CRE_QEMU_CACHE` to change it.
- The harness keeps the original module as `libkoreader-cre.so.orig`.
