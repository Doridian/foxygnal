#!/bin/sh
# Build the plugins in plugins/ for wasm32-unknown-unknown and refresh the
# prebuilt copies the host's tests use (crates/foxygnal-plugin/tests/fixtures).
#
#   scripts/build-plugins.sh
#
# Needs the wasm32-unknown-unknown target (`rustup target add
# wasm32-unknown-unknown`) and network access the first time (crates).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/plugins"
cargo build --release --target wasm32-unknown-unknown
out="$root/plugins/target/wasm32-unknown-unknown/release"
fixtures="$root/crates/foxygnal-plugin/tests/fixtures"
mkdir -p "$fixtures"
cp "$out/foxygnal_example.wasm" "$fixtures/example.wasm"
cp "$out/foxygnal_misbehave.wasm" "$fixtures/misbehave.wasm"
ls -l "$fixtures"/*.wasm
