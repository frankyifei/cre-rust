#!/usr/bin/env bash
# Builds the cre-lua crate as a LuaJIT loadable module.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo build --release -p cre-lua

cp target/release/libcre_lua.so target/release/libkoreader-cre.so
