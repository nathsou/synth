#!/usr/bin/env bash
# ECP5 simulation checks with `synth sim --family ecp5` (development / CI
# only): synthesised netlists and bitstreams (built by synth on an
# LFE5U-25F, decoded back to cells) against their RTL under random inputs,
# and Project Euler solvers' answers and cycle counts from their bitstreams
# (logic, block RAMs and multipliers). Needs a built `synth` and Project
# Trellis' database (Debian/Ubuntu's fpga-trellis-database, or
# SYNTH_TRELLIS_DB).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs
e=$d/third_party
dev=(--family ecp5 --device LFE5U-25F)
status=0

# check NAME DIR ARGS...: runs `synth sim ARGS` in DIR, which must PASS.
check() {
  local name=$1 dir=$2; shift 2
  local out
  if out=$(cd "$dir" && "$synth" sim "$@" "${dev[@]}" --quiet 2>&1) && grep -q PASS <<<"$out"; then
    echo "PASS $name"
  else
    status=1
    echo "FAIL $name"
    grep -v "has no LOCATE" <<<"$out" | tail -12
  fi
}

p=$d/picosoc
sfiles=("$e"/serv/servant/servant.v "$e"/serv/servant/servant_mux.v "$e"/serv/servant/servant_ram.v \
  "$e"/serv/servant/servant_timer.v "$e"/serv/servant/servant_gpio.v "$e"/serv/servile/*.v "$e"/serv/rtl/*.v)
k=$e/darkriscv/rtl
h=$e/hazard3/hdl
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')

# Synthesised netlists against their RTL.
check "picosoc netlist" "$p" soc.v picorv32.v --top top --compare synth --cycles 50000 --hold 5000
check "serv netlist" "$e/serv/sw" "${sfiles[@]}" --top servant -P 'memfile="hello_uart.hex"' \
  --compare synth --clock wb_clk --reset wb_rst --cycles 200000 --hold 100
check "darkriscv netlist" "$k" "$k"/*.v -I "$k" --top darksocv --compare synth --clock XCLK --reset XRES \
  --cycles 100000 --hold 2000
check "hazard3 netlist" "$h" "${hfiles[@]}" -I "$h" --top hazard3_cpu_1port --compare synth \
  --clock clk --clock clk_always_on --reset-low rst_n --cycles 20000 --hold 3

# Bitstreams against their RTL.
check "blinky_ram bitstream" "$d/blinky_ram" blinky_ram.v --top top --compare bitstream \
  --cycles 100000 --hold 50
check "picosoc bitstream" "$p" soc.v picorv32.v --top top --compare bitstream --cycles 200000 --hold 5000
check "serv bitstream" "$e/serv/sw" "${sfiles[@]}" --top servant -P 'memfile="hello_uart.hex"' \
  --compare bitstream --clock wb_clk --reset wb_rst --cycles 200000 --hold 100
check "darkriscv bitstream" "$k" "$k"/*.v -I "$k" --top darksocv --compare bitstream --clock XCLK \
  --reset XRES --cycles 100000 --hold 2000
c=$root/examples/alchitry_cu_hazard3
check "alchitry_cu_hazard3 bitstream" "$c" -I "$h" top.v "${hfiles[@]}" --top top --compare bitstream \
  --cycles 400000 --hold 1 --set rst_n=1

# Project Euler: answers from the bitstreams after the original cycle
# counts (plus two cycles of reset).
eu=$d/euler
while read -r n width answer cycles extra; do
  case $n in \#*|'') continue ;; esac
  if [ "$cycles" -gt "${MAX_RUN:-200000}" ] && [ -z "${LONG:-}" ]; then
    echo "skip p0$n ($cycles cycles; LONG=1 runs it)"
    continue
  fi
  src=("p0$n.v")
  [ -n "$extra" ] && src+=("$extra")
  if out=$(cd "$eu" && "$synth" sim "${src[@]}" --top "euler$n" --set start=1 --set rst=1 --at 2:rst=0 \
      --until done --print answer --level bitstream "${dev[@]}" 2>&1) &&
    grep -q "answer = $answer " <<<"$out" && grep -q "info: $((cycles + 2)) cycles" <<<"$out"; then
    echo "PASS p0$n bitstream: $answer after $cycles cycles"
  else
    status=1
    echo "FAIL p0$n bitstream (expected $answer after $cycles cycles)"
    grep -v "has no LOCATE" <<<"$out" | tail -5
  fi
done < "$eu/designs.txt"
exit $status
