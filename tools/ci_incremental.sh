#!/usr/bin/env bash
# Decode incremental bitstreams and simulate against each edited RTL design.
# Requires the same reference tools as ci_bitsim.sh. No runtime dependencies.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
synth=${SYNTH:-$root/target/release/synth}
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
design="$root/tests/designs/blinky_ram"
pcf="$work/pins.pcf"
cp "$design/go_board.pcf" "$pcf"
cp "$design/blinky_ram.v" "$work/base.v"
sed 's/i \* 7 + 3/i * 7 + 5/' "$work/base.v" > "$work/ram.v"
sed 's/assign led = q/assign led = (q + cnt[7:0])/' "$work/ram.v" > "$work/logic.v"
sed -e 's/23/19/g' -e 's/19:15/19:11/g' -e 's/19:16/19:12/g' "$work/logic.v" > "$work/shorter.v"

build() {
  local name=$1 rtl=$2
  "$synth" build "$rtl" --device hx1k --package vq100 --pcf "$pcf" \
    --incremental "$work/design.chk" --asc "$work/$name.asc" 2> "$work/$name.log"
  echo "== $name"
  grep -E '^info: (incremental|total)' "$work/$name.log"
  python3 "$root/tools/bitsim.py" --asc "$work/$name.asc" --pcf "$pcf" --package vq100 \
    --top top --clock clk --cycles 10000 --hold 50 "$rtl"
}
build fresh "$work/base.v"
build unchanged "$work/base.v"
cmp "$work/fresh.asc" "$work/unchanged.asc"
build ram_contents "$work/ram.v"
build added_logic "$work/logic.v"
build shorter_carry "$work/shorter.v"
build removed_logic "$work/base.v"
build repeated "$work/base.v"
cmp "$work/removed_logic.asc" "$work/repeated.asc"
# Changing pin constraints must update IO placement and invalidate old endpoints.
sed -e 's/led\[0\] 56/led[0] 57/' -e 's/led\[1\] 57/led[1] 56/' "$pcf" > "$work/new.pcf"
mv "$work/new.pcf" "$pcf"
build moved_pins "$work/base.v"
echo "all incremental bitstream simulations passed"
