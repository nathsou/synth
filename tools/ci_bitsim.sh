#!/usr/bin/env bash
# Bitstream-level regression (development/CI only): build test designs
# either with synth alone (`synth build`) or with Yosys synthesis and synth
# place and route, and check the decompiled bitstream against the RTL in
# simulation (tools/bitsim.py).
#
# Requires: yosys, fpga-icestorm (icebox_vlog), iverilog, and a built
# `synth` binary (cargo build --release).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
synth="$root/target/release/synth"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

run() { # name top device package pcf cycles hold rtl...
  local name=$1 top=$2 dev=$3 pkg=$4 pcf=$5 cycles=$6 hold=$7; shift 7
  echo "== $name ($dev $pkg)"
  (cd "$work" && yosys -q -p "synth_ice40 -top $top; cd $top; write_rtlil -selected $name.il" "$@")
  "$synth" pnr "$work/$name.il" --device "$dev" --package "$pkg" --pcf "$pcf" --asc "$work/$name.asc" 2>&1 | grep -E "^info: (packed|placement|routing|total)"
  python3 "$root/tools/bitsim.py" --asc "$work/$name.asc" --pcf "$pcf" --package "$pkg" \
    --top "$top" --clock clk --cycles "$cycles" --hold "$hold" "$@" | head -12
}

build() { # name top device package pcf cycles hold rtl... (synth only)
  local name=$1 top=$2 dev=$3 pkg=$4 pcf=$5 cycles=$6 hold=$7; shift 7
  echo "== $name ($dev $pkg, synth only)"
  "$synth" build "$@" --top "$top" --device "$dev" --package "$pkg" --pcf "$pcf" --asc "$work/$name.asc" 2>&1 \
    | grep -E "^info: (mapped|packed|routing|total)|error"
  python3 "$root/tools/bitsim.py" --asc "$work/$name.asc" --pcf "$pcf" --package "$pkg" \
    --top "$top" --clock clk --cycles "$cycles" --hold "$hold" "$@" | head -12
}

d="$root/tests/designs/blinky_ram"
run blinky_ram_hx1k top hx1k vq100 "$d/go_board.pcf" 100000 50 "$d/blinky_ram.v"
run blinky_ram_hx8k top hx8k cb132 "$d/alchitry_cu.pcf" 100000 50 "$d/blinky_ram.v"
build blinky_ram_synth_hx1k top hx1k vq100 "$d/go_board.pcf" 100000 50 "$d/blinky_ram.v"
p="$root/tests/designs/picosoc"
(cd "$p" && build picosoc_synth_hx8k top hx8k cb132 "$p/alchitry_cu.pcf" 200000 5000 soc.v picorv32.v)
echo "all bitstream simulations passed"
