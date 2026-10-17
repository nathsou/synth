#!/usr/bin/env bash
# Quality of results against Yosys + nextpnr (development only).
#
# For every design, synthesis alone: iCE40 cells and run time of `synth synth`
# and of Yosys `synth_ice40`. Then, for the designs with a board (or few
# enough pins), the whole flow to a bitstream: logic cells, Fmax of the
# bitstream as icetime computes it (the same timing model for both), and run
# time of `synth build` and of Yosys + nextpnr-ice40.
#
# Needs yosys, nextpnr-ice40 and icetime on PATH; ICETIME_CHIPDB names
# icetime's chip database directory if it doesn't find its own.
# DESIGNS="a b" selects designs; FLOW=synth stops after synthesis;
# QOR_LOG=dir keeps every tool's output there.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
logs=${QOR_LOG:-$(mktemp -d)}
mkdir -p "$logs"
[ -z "${QOR_LOG:-}" ] && trap 'rm -rf "$logs"' EXIT
d=$root/tests/designs
s=$d/third_party/serv
h=$d/third_party/hazard3/hdl
k=$d/third_party/darkriscv/rtl
eu=$d/euler
e=$root/examples
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')
sfiles=("$s"/servant/servant.v "$s"/servant/servant_mux.v "$s"/servant/servant_ram.v
  "$s"/servant/servant_timer.v "$s"/servant/servant_gpio.v "$s"/servile/*.v "$s"/rtl/*.v)

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }

# Prints "LUT4 CARRY FF RAM" from "N KIND" counts on stdin.
summarise() {
  awk '{ n[$2] += $1 }
    END {
      ff = 0; for (k in n) if (k ~ /^SB_DFF/) ff += n[k]
      printf "%6d %6d %5d %4d", n["SB_LUT4"], n["SB_CARRY"], ff, n["SB_RAM40_4K"]
    }'
}

# Fmax of a bitstream by icetime.
fmax() {
  local dev=$1 pkg=$2 asc=$3 db=()
  [ -n "${ICETIME_CHIPDB:-}" ] && db=(-C "$ICETIME_CHIPDB/chipdb-${dev#hx}.txt")
  icetime -d "$dev" -P "$pkg" "${db[@]}" -t "$asc" 2>/dev/null |
    sed -n 's/.*Total path delay: .* (\(.*\) MHz).*/\1/p' | tail -1
}

names=()
declare -A spec
# design NAME DIR TOP DEVICE PACKAGE PCF YOSYS_DEFINES YOSYS_COMMANDS -- SYNTH_ARGS
# PCF "-": no pin constraints; "none": synthesis only (too many pins to place).
# YOSYS_COMMANDS run before elaboration (to set parameters).
design() {
  local name=$1
  names+=("$name")
  spec[$name]=$(printf '%q ' "$@")
}

design picosoc "$d/picosoc" top hx8k cb132 alchitry_cu.pcf "" "" -- soc.v picorv32.v
design serv "$s/sw" servant hx8k cb132 - "" 'chparam -set memfile "hello_uart.hex" servant;' -- \
  "${sfiles[@]}" -P 'memfile="hello_uart.hex"'
design darkriscv "$k" darksocv hx8k cb132 - "" "" -- "$k"/*.v -I "$k" -D __ICARUS__
design hazard3_core "$h" hazard3_cpu_1port hx8k cb132 none "" "" -- "${hfiles[@]}" -I "$h"
design cu_hazard3 "$e/alchitry_cu_hazard3" top hx8k cb132 alchitry_cu.pcf "" "" -- \
  top.v "${hfiles[@]}" -I "$h"
design go_serv "$e/go_board_serv" top hx1k vq100 go_board.pcf "" "" -- top.v \
  "$s"/servant/{servant,servant_mux,servant_ram,servant_timer,servant_gpio}.v "$s"/servile/*.v "$s"/rtl/*.v
while read -r n width _ _ extra; do
  case $n in \#*|'') continue ;; esac
  src=(euler_top.v report.v "p0$n.v")
  [ -n "$extra" ] && src+=("$extra")
  design "euler$n" "$eu" euler_top hx8k cb132 alchitry_cu.pcf \
    "-DSOLVER=euler$n -DANSWER_WIDTH=$width" "" -- "${src[@]}" -D "SOLVER=euler$n" -D "ANSWER_WIDTH=$width"
done <"$eu/designs.txt"

selected() { [ -z "${DESIGNS:-}" ] || [[ " $DESIGNS " == *" $1 "* ]]; }

# Parses a design's spec into the variables used below.
load() {
  eval "set -- ${spec[$1]}"
  name=$1 dir=$2 top=$3 dev=$4 pkg=$5 pcf=$6 ydefs=$7 ycmds=$8
  shift 9
  args=("$@") files=() incs=()
  for a in "${args[@]}"; do
    case $a in *.v) files+=("$a") ;; esac
  done
  for ((i = 0; i < ${#args[@]}; i++)); do
    [ "${args[$i]}" = -I ] && incs+=("-I${args[$((i + 1))]}")
  done
  yread="read_verilog -defer ${incs[*]} $ydefs ${files[*]}; $ycmds hierarchy -top $top;"
}

printf "Synthesis: iCE40 cells and time (s)\n"
printf "%-14s %6s %6s %5s %4s %6s   %6s %6s %5s %4s %6s\n" "" LUT4 CARRY FF RAM time LUT4 CARRY FF RAM time
printf "%-14s %-32s   %s\n" "design" "synth" "yosys"
for n in "${names[@]}"; do
  selected "$n" || continue
  load "$n"
  t=$(now)
  ours=$(cd "$dir" && { "$synth" synth "${args[@]}" --top "$top" 2>"$logs/$n.synth.log" || true; } &&
    tail -1 "$logs/$n.synth.log" | tr ',' '\n' | sed -E 's/^ *(info: )?//' | summarise)
  ot=$(elapsed "$t")
  t=$(now)
  yos=$(cd "$dir" && { yosys -q -p "$yread synth_ice40 -top $top; tee -o /dev/stdout stat" \
    2>"$logs/$n.yosys.log" || true; } | awk '$1 ~ /^SB_/ { print $2, $1 }' | summarise)
  yt=$(elapsed "$t")
  printf "%-14s %s %6s   %s %6s\n" "$n" "$ours" "$ot" "$yos" "$yt"
done

[ "${FLOW:-}" = synth ] && exit 0
printf "\nPlace and route: logic cells, Fmax (MHz, icetime) and time (s)\n"
printf "%-14s %6s %7s %6s   %6s %7s %6s %6s\n" "" cells Fmax time cells Fmax yosys nextpnr
printf "%-14s %-22s   %s\n" "design" "synth" "yosys + nextpnr"
for n in "${names[@]}"; do
  selected "$n" || continue
  load "$n"
  [ "$pcf" = none ] && continue
  pcfargs=() npcf=()
  if [ "$pcf" != - ]; then
    pcfargs=(--pcf "$pcf")
    npcf=(--pcf "$pcf")
  else
    npcf=(--pcf-allow-unconstrained)
  fi
  t=$(now)
  (cd "$dir" && "$synth" build "${args[@]}" --top "$top" --device "$dev" --package "$pkg" "${pcfargs[@]}" \
    -o "$logs/$n.bin" --asc "$logs/$n.asc" >"$logs/$n.build.log" 2>&1) || true
  ot=$(elapsed "$t")
  ocells=$(sed -n 's/.* \([0-9]*\) ICESTORM_LC.*/\1/p' "$logs/$n.build.log" | tail -1)
  ofmax=$( [ -f "$logs/$n.asc" ] && fmax "$dev" "$pkg" "$logs/$n.asc")
  t=$(now)
  (cd "$dir" && yosys -q -p "$yread synth_ice40 -top $top -json $logs/$n.json" >"$logs/$n.yflow.log" 2>&1) || true
  yt=$(elapsed "$t")
  t=$(now)
  (cd "$dir" && nextpnr-ice40 "--$dev" --package "$pkg" "${npcf[@]}" --json "$logs/$n.json" \
    --asc "$logs/$n.nasc" >"$logs/$n.nextpnr.log" 2>&1) || true
  nt=$(elapsed "$t")
  ncells=$(sed -n 's/.*ICESTORM_LC: *\([0-9]*\).*/\1/p' "$logs/$n.nextpnr.log" | head -1)
  nfmax=$( [ -f "$logs/$n.nasc" ] && fmax "$dev" "$pkg" "$logs/$n.nasc")
  printf "%-14s %6s %7s %6s   %6s %7s %6s %6s\n" "$n" "${ocells:--}" "${ofmax:--}" "$ot" \
    "${ncells:--}" "${nfmax:--}" "$yt" "$nt"
done
