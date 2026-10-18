#!/usr/bin/env bash
# CI-only independent primitive model, Apache-2.0; not shipped with synth.
set -euo pipefail
out=${1:?usage: fetch_xc7_models.sh OUT_DIR}
mkdir -p "$out"
rev=1c8e05fd1e9a79ceb8b996a0996674122eed086f
curl --fail --location --retry 3 \
  "https://raw.githubusercontent.com/Xilinx/XilinxUnisimLibrary/$rev/verilog/src/unisims/RAMB18E1.v" \
  -o "$out/RAMB18E1.v"
