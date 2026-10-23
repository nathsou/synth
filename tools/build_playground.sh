#!/usr/bin/env bash
# Builds the playground as a static site: the web app and the whole flow
# compiled to WebAssembly, runnable from any static file server.
#
#   tools/build_playground.sh [out-dir]    (default: target/playground)
#
# Requires the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown). `synth playground` serves the same app without
# WebAssembly, running the flow natively.
#
# With XC7_DB set to Project X-Ray's artix7 directory (tools/fetch_xc7_db.sh),
# it also bundles the database for the Artix-7 boards: one xc7db/<part>.tar.gz
# per part (about 4 MB each), which the page fetches on its first run for it.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out=${1:-$root/target/playground}
cd "$root"
cargo build --release --target wasm32-unknown-unknown -p synth-playground \
  --config profile.release.debug=false --config profile.release.strip=true
mkdir -p "$out"
# Absolute: the database is archived from within $XC7_DB.
out=$(cd "$out" && pwd)
find crates/synth-playground/web -maxdepth 1 -type f -exec cp {} "$out/" \;
cp target/wasm32-unknown-unknown/release/synth_playground.wasm "$out/"
if [ -n "${XC7_DB:-}" ]; then
  mkdir -p "$out/xc7db"
  # The parts of the playground's boards, with their fabrics.
  for pf in xc7a35tftg256-1:xc7a50t xc7a100tcsg324-1:xc7a100t; do
    part=${pf%%:*} fabric=${pf#*:}
    (cd "$XC7_DB" && find . -maxdepth 1 -type f -printf '%P\n' | sort > "$out/xc7db/.list" &&
      tar czf "$out/xc7db/$part.tar.gz" -T "$out/xc7db/.list" timings "$part" "$fabric")
  done
  rm -f "$out/xc7db/.list"
  echo "playground: Artix-7 database $(du -sh "$out/xc7db" | cut -f1)"
else
  echo "playground: warning: XC7_DB is not set, so the Artix-7 boards will not run" \
    "(tools/fetch_xc7_db.sh <dir>, then XC7_DB=<dir>/artix7)" >&2
fi
echo "playground: $out ($(du -h "$out/synth_playground.wasm" | cut -f1) of WebAssembly)"
