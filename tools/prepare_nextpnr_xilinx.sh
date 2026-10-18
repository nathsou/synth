#!/usr/bin/env bash
# Pinned Yosys+nextpnr-xilinx baseline for tools/pnr_artix7.py (an ISC
# reference flow for comparison only; never invoked by the synth executable).
# Needs CMake, a C++ compiler, Boost and Eigen. Chip databases are generated
# from the pinned Project X-Ray database given as the second argument.
set -euo pipefail
out=${1:?usage: prepare_nextpnr_xilinx.sh OUT_DIR PRJXRAY_DB_ARTIX7 [PART...]}
db=${2:?usage: prepare_nextpnr_xilinx.sh OUT_DIR PRJXRAY_DB_ARTIX7 [PART...]}
shift 2
parts=("${@:-xc7a100tcsg324-1}")
mkdir -p "$out"
out=$(cd "$out" && pwd)
rev=8f178fc6a6d4dfbc57bef66c3ccff34d558047d5
meta=772a3d5204e82bbb1622f6a4d5d6d2fd32277bd3
src="$out/nextpnr-xilinx"
if [[ ! -e "$src" ]]; then
  git init "$src"
  git -C "$src" remote add origin https://github.com/gatecat/nextpnr-xilinx.git
  git -C "$src" fetch --depth 1 origin "$rev"
  git -C "$src" checkout --detach FETCH_HEAD
  git -C "$src" submodule update --init --depth 1 xilinx/external/nextpnr-xilinx-meta
fi
if [[ $(git -C "$src" rev-parse HEAD) != "$rev" ]] ||
   [[ $(git -C "$src/xilinx/external/nextpnr-xilinx-meta" rev-parse HEAD) != "$meta" ]]; then
  echo "Unexpected nextpnr-xilinx revision; refusing to change an existing checkout" >&2
  exit 1
fi
cmake -S "$src" -B "$out/build" -DARCH=xilinx -DBUILD_GUI=OFF -DBUILD_PYTHON=OFF \
  -DCMAKE_BUILD_TYPE=Release
cmake --build "$out/build" -j"$(nproc)"
for part in "${parts[@]}"; do
  if [[ ! -e "$out/$part.bin" ]]; then
    python3 "$src/xilinx/python/bbaexport.py" --device "$part" --xray "$db" --bba "$out/$part.bba"
    "$out/build/bba/bbasm" --l "$out/$part.bba" "$out/$part.bin"
    rm "$out/$part.bba"
  fi
done
printf 'nextpnr-xilinx: %s/build/nextpnr-xilinx, chip databases in %s\n' "$out" "$out"
