#!/usr/bin/env bash
# Synthesis QoR against Yosys (development only): iCE40 cell counts of
# `synth synth` and of Yosys `synth_ice40` for the test designs and board
# examples. Needs yosys on PATH. DESIGNS="a b" selects designs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
err=${QOR_LOG:-/dev/null}
d=$root/tests/designs
s=$d/third_party/serv
h=$d/third_party/hazard3/hdl
k=$d/third_party/darkriscv/rtl
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')
sfiles=("$s"/servant/servant.v "$s"/servant/servant_mux.v "$s"/servant/servant_ram.v
  "$s"/servant/servant_timer.v "$s"/servant/servant_gpio.v "$s"/servile/*.v "$s"/rtl/*.v)

# Prints "LUT4 CARRY FF RAM" from a list of "N KIND" counts on stdin.
summarise() {
  awk '{ n[$2] += $1 }
    END {
      ff = 0; for (k in n) if (k ~ /^SB_DFF/) ff += n[k]
      printf "%6d %6d %6d %4d", n["SB_LUT4"], n["SB_CARRY"], ff, n["SB_RAM40_4K"]
    }'
}

# run NAME DIR TOP YOSYS_COMMANDS -- FILES/ARGS
# YOSYS_COMMANDS run before elaboration (to set parameters).
run() {
  local name=$1 dir=$2 top=$3 ycmds=$4; shift 5
  if [ -n "${DESIGNS:-}" ] && [[ " $DESIGNS " != *" $name "* ]]; then return; fi
  local args=("$@") files=() incs=()
  for a in "${args[@]}"; do
    case $a in -I*|-D*|-P*) ;; *.v) files+=("$a") ;; esac
  done
  for ((i = 0; i < ${#args[@]}; i++)); do
    case ${args[$i]} in
      -I) incs+=("-I${args[$((i + 1))]}") ;;
    esac
  done
  local ours yos
  ours=$(cd "$dir" && { "$synth" synth "${args[@]}" --top "$top" 2>&1 || true; } | tail -1 |
    tr ',' '\n' | sed -E 's/^ *(info: )?//' | summarise)
  yos=$(cd "$dir" && { yosys -q -p "read_verilog -defer ${incs[*]} ${files[*]}; $ycmds hierarchy -top $top;
    synth_ice40 -top $top; tee -o /dev/stdout stat" 2>"$err" || true; } |
    awk '$1 ~ /^SB_/ { print $2, $1 }' | summarise)
  printf "%-22s %s   %s\n" "$name" "$ours" "$yos"
}

printf "%-22s %6s %6s %6s %4s   %6s %6s %6s %4s\n" "" LUT4 CARRY FF RAM LUT4 CARRY FF RAM
printf "%-22s %-29s   %s\n" "design" "synth" "yosys"
run picosoc "$d/picosoc" top "" -- soc.v picorv32.v
run serv "$s/sw" servant 'chparam -set memfile "hello_uart.hex" servant;' -- "${sfiles[@]}" -P 'memfile="hello_uart.hex"'
run darkriscv "$k" darksocv "" -- "$k"/*.v -I "$k" -D __ICARUS__
run hazard3_core "$h" hazard3_cpu_1port "" -- "${hfiles[@]}" -I "$h"
e=$root/examples
run cu_hazard3 "$e/alchitry_cu_hazard3" top "" -- top.v "${hfiles[@]}" -I "$h"
run go_serv "$e/go_board_serv" top "" -- top.v "$s"/servant/{servant,servant_mux,servant_ram,servant_timer,servant_gpio}.v \
  "$s"/servile/*.v "$s"/rtl/*.v
