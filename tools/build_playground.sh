#!/usr/bin/env bash
# Builds the playground as a static site: the web app and the whole flow
# compiled to WebAssembly, runnable from any static file server.
#
#   tools/build_playground.sh [out-dir]    (default: target/playground)
#
# Requires the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown). `synth playground` serves the same app without
# WebAssembly, running the flow natively.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out=${1:-$root/target/playground}
cd "$root"
cargo build --release --target wasm32-unknown-unknown -p synth-playground \
  --config profile.release.debug=false --config profile.release.strip=true
mkdir -p "$out"
find crates/synth-playground/web -maxdepth 1 -type f -exec cp {} "$out/" \;
cp target/wasm32-unknown-unknown/release/synth_playground.wasm "$out/"
echo "playground: $out ($(du -h "$out/synth_playground.wasm" | cut -f1) of WebAssembly)"
