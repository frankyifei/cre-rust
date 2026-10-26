#!/usr/bin/env bash
# Builds the cre-lua crate as a LuaJIT loadable module.
set -euo pipefail

cd /home/frank/src/kindle-project

cargo build --release -p cre-lua

cp target/release/libcre_lua.so target/release/libkoreader-cre.so
