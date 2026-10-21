#!/usr/bin/env bash
# In-process simulation checks with `synth sim` (development / CI only): the
# designs of ci_rtlsim.sh (MODE=synth) and ci_bitsim.sh, each synthesized
# netlist and each bitstream (built by synth, decoded back to cells) against
# its RTL under random inputs, and the Project Euler solvers' answers and
# cycle counts in RTL (both modes) and as netlists. Needs only a built
# `synth` (no Icarus, Yosys or IceStorm), which also makes it fast; the
# Icarus-based scripts stay as independent oracles.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs
e=$d/third_party
status=0

# check NAME DIR ARGS...: runs `synth sim ARGS` in DIR, which must PASS.
check() {
  local name=$1 dir=$2; shift 2
  local out
  if out=$(cd "$dir" && "$synth" sim "$@" --quiet 2>&1) && grep -q PASS <<<"$out"; then
    echo "PASS $name"
  else
    status=1
    echo "FAIL $name"
    tail -12 <<<"$out"
  fi
}

# Synthesized netlists against their RTL (as ci_rtlsim.sh MODE=synth).
p=$d/picosoc
check "picosoc netlist" "$p" soc.v picorv32.v --top top --compare synth --cycles 50000 --hold 5000
sfiles=("$e"/serv/servant/servant.v "$e"/serv/servant/servant_mux.v "$e"/serv/servant/servant_ram.v \
  "$e"/serv/servant/servant_timer.v "$e"/serv/servant/servant_gpio.v "$e"/serv/servile/*.v "$e"/serv/rtl/*.v)
check "serv netlist" "$e/serv/sw" "${sfiles[@]}" --top servant -P 'memfile="hello_uart.hex"' \
  --compare synth --clock wb_clk --reset wb_rst --cycles 200000 --hold 100
k=$e/darkriscv/rtl
check "darkriscv netlist" "$k" "$k"/*.v -I "$k" --top darksocv --compare synth --clock XCLK --reset XRES \
  --cycles 100000 --hold 2000
h=$e/hazard3/hdl
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')
check "hazard3 netlist" "$h" "${hfiles[@]}" -I "$h" --top hazard3_cpu_1port --compare synth \
  --clock clk --clock clk_always_on --reset-low rst_n --cycles 20000 --hold 3

# Bitstreams against their RTL (as ci_bitsim.sh, synth-only builds).
b=$d/blinky_ram
check "blinky_ram hx1k bitstream" "$b" blinky_ram.v --top top --compare bitstream \
  --device hx1k --package vq100 --pcf go_board.pcf --cycles 100000 --hold 50
check "blinky_ram hx8k bitstream" "$b" blinky_ram.v --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 100000 --hold 50
check "picosoc bitstream" "$p" soc.v picorv32.v --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 200000 --hold 5000
g=$root/examples/go_board_serv
check "go_board_serv bitstream" "$g" top.v "${sfiles[@]}" --top top --compare bitstream \
  --device hx1k --package vq100 --pcf go_board.pcf --cycles 150000 --hold 1000
c=$root/examples/alchitry_cu_hazard3
check "alchitry_cu_hazard3 bitstream" "$c" -I "$h" top.v "${hfiles[@]}" --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 400000 --hold 1 --set rst_n=1

# Project Euler: answers after the original cycle counts (plus two cycles of
# reset), RTL in both modes and the synthesized netlist.
eu=$d/euler
while read -r n width answer cycles extra; do
  case $n in \#*|'') continue ;; esac
  if [ "$cycles" -gt "${MAX_RUN:-200000}" ] && [ -z "${LONG:-}" ]; then
    echo "skip p0$n ($cycles cycles; LONG=1 runs it)"
    continue
  fi
  src=("p0$n.v")
  [ -n "$extra" ] && src+=("$extra")
  for level in "rtl --four-state" "rtl --two-state" "synth"; do
    # shellcheck disable=SC2086
    if out=$(cd "$eu" && "$synth" sim "${src[@]}" --top "euler$n" --set start=1 --set rst=1 --at 2:rst=0 \
        --until done --print answer --level $level 2>&1) &&
      grep -q "answer = $answer " <<<"$out" && grep -q "info: $((cycles + 2)) cycles" <<<"$out"; then
      echo "PASS p0$n $level: $answer after $cycles cycles"
    else
      status=1
      echo "FAIL p0$n $level (expected $answer after $cycles cycles)"
      tail -5 <<<"$out"
    fi
  done
done <"$eu/designs.txt"
exit $status
