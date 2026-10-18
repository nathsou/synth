#!/usr/bin/env bash
# CI-only independent primitive models, Apache-2.0; not shipped with synth.
set -euo pipefail
out=${1:?usage: fetch_xc7_models.sh OUT_DIR}
mkdir -p "$out"
rev=1c8e05fd1e9a79ceb8b996a0996674122eed086f
for model in RAMB18E1 RAM32M RAM64M SRLC32E MUXF7 MUXF8; do
  curl --fail --location --retry 3 \
    "https://raw.githubusercontent.com/Xilinx/XilinxUnisimLibrary/$rev/verilog/src/unisims/$model.v" \
    -o "$out/$model.v"
done
