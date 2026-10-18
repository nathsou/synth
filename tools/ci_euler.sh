#!/usr/bin/env bash
# Project Euler solvers (development / CI): each solver's RTL, and its
# synthesised iCE40 netlist simulated with Yosys's cell models (from
# YOSYS_DATDIR, default /usr/share/yosys), must give the answer after exactly
# the cycle count of the original HDLean design (tests/designs/euler/designs.txt).
# Runs longer than MAX_RUN cycles (default 200000: P14 and P58 take minutes)
# are skipped unless LONG=1. ONLY="25 31" selects problems.
# Needs iverilog >= 11 on PATH.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs/euler
cells=${YOSYS_DATDIR:-/usr/share/yosys}/ice40/cells_sim.v
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
status=0

# run NAME GENERATION EXPECT CYCLES WIDTH SOLVER FILES...
run() {
  local name=$1 gen=$2 expect=$3 cycles=$4 width=$5 solver=$6; shift 6
  iverilog "-g$gen" -DSOLVER="$solver" -DANSWER_WIDTH="$width" -DEXPECT="64'd$expect" \
    -DMAX_CYCLES=$((cycles + 10)) -o "$out/$name.vvp" "$d/tb.v" "$@" >"$out/$name.log" 2>&1 &&
    vvp -n "$out/$name.vvp" >>"$out/$name.log" 2>&1 || true
  if grep -q "PASS answer $expect after $cycles cycles" "$out/$name.log"; then
    echo "PASS $name: $expect after $cycles cycles"
  else
    status=1
    echo "FAIL $name (expected $expect after $cycles cycles)"
    grep -v "sorry:" "$out/$name.log" | tail -5
  fi
}

while read -r n width answer cycles extra; do
  case $n in \#*|'') continue ;; esac
  if [ -n "${ONLY:-}" ] && [[ " $ONLY " != *" $n "* ]]; then continue; fi
  if [ "$cycles" -gt "${MAX_RUN:-200000}" ] && [ -z "${LONG:-}" ]; then
    echo "skip p0$n ($cycles cycles; LONG=1 runs it)"
    continue
  fi
  src=("$d/p0$n.v")
  [ -n "$extra" ] && src+=("$d/$extra")
  run "p0$n-rtl" 2005 "$answer" "$cycles" "$width" "euler$n" "${src[@]}"
  if "$synth" synth "${src[@]}" --top "euler$n" --verilog "$out/p0$n.v" --rename "euler${n}_net" \
    >"$out/p0$n-synth.log" 2>&1; then
    run "p0$n-netlist" 2012 "$answer" "$cycles" "$width" "euler${n}_net" "$out/p0$n.v" "$cells"
  else
    status=1
    echo "FAIL p0$n: synthesis"
    tail -5 "$out/p0$n-synth.log"
  fi
done <"$d/designs.txt"
exit $status
