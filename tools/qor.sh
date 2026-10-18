#!/usr/bin/env bash
# Quality of results against Yosys + nextpnr (development only).
#
# For every design, synthesis alone: iCE40 cells and run time of `synth synth`
# and of Yosys `synth_ice40`. Then, for the designs with a board (or few
# enough pins), the whole flow to a bitstream: logic cells, Fmax of the
# bitstream as icetime computes it (the same timing model for both), and run
# time of `synth build` and of Yosys + nextpnr-ice40.
#
# Both flows get the same device, package, pin constraints, sources and
# clock target: FREQ MHz (default 200, above what any design reaches, so
# both optimise for speed), passed as --freq with --timing-allow-fail to
# each. Place and route runs once per seed in SEEDS (default "1 2 3"); the
# table gives Fmax as min/median/max over the seeds and median times. A
# failed step (no bitstream) shows as FAIL rather than as numbers; "met k"
# counts the bitstreams whose icetime Fmax meets FREQ. Peak memory (MB) is
# the largest of the runs. The output starts with the tool versions and
# ends with a hash of each design's sources, checked against the frozen
# benchmark in tools/qor-sources.txt (a changed design is marked).
#
# Needs yosys, nextpnr-ice40 and icetime on PATH (ICETIME selects another
# icetime binary); ICETIME_CHIPDB names icetime's chip database directory if
# it doesn't find its own. Stock icetime misses paths that leave a carry
# chain into the I3 input of a logic cell without carry logic (both flows
# build such cells): tools/icetime-carry-feedout.patch adds them, enabled
# by ICETIME_FEEDOUT=1.
# DESIGNS="a b" selects designs; FLOW=synth stops after synthesis, FLOW=pnr
# skips the synthesis table, FLOW=sources only hashes the sources;
# QOR_LOG=dir keeps every tool's output there, icetime's reports included.
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

# measure OUT CMD...: runs CMD and writes "status seconds peak-MB" to OUT.
measure() {
  local out=$1
  shift
  python3 -c '
import resource, subprocess, sys, time
t = time.time()
rc = subprocess.call(sys.argv[2:])
mem = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024
with open(sys.argv[1], "w") as f:
    f.write("%d %.1f %.0f\n" % (rc, time.time() - t, mem))
' "$out" "$@"
}
# Fields of a measure result: status, time, memory.
field() { awk -v k="$2" '{ print $k }' "$1"; }

# Prints "LUT4 CARRY FF RAM" from "N KIND" counts on stdin.
summarise() {
  awk '{ n[$2] += $1 }
    END {
      ff = 0; for (k in n) if (k ~ /^SB_DFF/) ff += n[k]
      printf "%6d %6d %5d %4d", n["SB_LUT4"], n["SB_CARRY"], ff, n["SB_RAM40_4K"]
    }'
}

# Fmax of a bitstream by icetime; its report goes next to the bitstream.
fmax() {
  local dev=$1 pkg=$2 asc=$3 db=()
  [ -n "${ICETIME_CHIPDB:-}" ] && db=(-C "$ICETIME_CHIPDB/chipdb-${dev#hx}.txt")
  "${ICETIME:-icetime}" -d "$dev" -P "$pkg" "${db[@]}" -t "$asc" >"$asc.icetime" 2>&1 || true
  sed -n 's/.*Total path delay: .* (\(.*\) MHz).*/\1/p' "$asc.icetime" | tail -1
}
freq=${FREQ:-200}
# How many of the numbers on stdin reach FREQ.
met() { awk -v f="$freq" '$1 >= f { n++ } END { print n + 0 }'; }

# "min/median/max" of numbers on stdin ("-" if none).
spread() {
  sort -g | awk '{ v[NR] = $1 } END {
    if (NR == 0) { print "-"; exit }
    printf "%.1f/%.1f/%.1f", v[1], v[int((NR + 1) / 2)], v[NR] }'
}
median() { sort -g | awk '{ v[NR] = $1 } END { if (NR) print v[int((NR + 1) / 2)]; else print "-" }'; }

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
design darkriscv "$k" darksocv hx8k cb132 none "" "" -- "$k"/*.v -I "$k" -D __ICARUS__
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

# Prints a hash of each design's sources (its files, headers in its include
# directories, memory images and pin constraints), marking those that differ
# from the frozen benchmark.
sources() {
  echo
  echo "Sources (* differs from tools/qor-sources.txt):"
  for n in "${names[@]}"; do
    selected "$n" || continue
    load "$n"
    local h want
    h=$(cd "$dir" && {
      cat "${files[@]}"
      for i in "${incs[@]}"; do cat "${i#-I}"/*.vh 2>/dev/null || true; done
      cat ./*.hex 2>/dev/null || true
      [ ! -f "$pcf" ] || cat "$pcf"
    } | sha256sum | cut -c1-12)
    want=$(awk -v n="$n" '$1 == n { print $2 }' "$root/tools/qor-sources.txt" 2>/dev/null || true)
    printf "%-14s %s%s\n" "$n" "$h" "$([ "$h" = "$want" ] || echo ' *')"
  done
}

echo "synth $(git -C "$root" describe --always --dirty 2>/dev/null || echo '?'); $(yosys -V 2>&1 | head -1);" \
  "$(nextpnr-ice40 --version 2>&1 | head -1); icetime $(command -v "${ICETIME:-icetime}")" \
  "${ICETIME_FEEDOUT:+(carry feed-outs timed)}; FREQ $freq MHz"
echo

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

[ "${FLOW:-}" = sources ] && { sources; exit 0; }
if [ "${FLOW:-}" != pnr ]; then
printf "Synthesis: iCE40 cells, time (s) and peak memory (MB)\n"
printf "%-14s %6s %6s %5s %4s %6s %5s   %6s %6s %5s %4s %6s %5s\n" "" LUT4 CARRY FF RAM time mem LUT4 CARRY FF RAM time mem
printf "%-14s %-38s   %s\n" "design" "synth" "yosys"
failed() { printf "%27s %6s %5s" FAIL "$(field "$1" 2)" "$(field "$1" 3)"; }
for n in "${names[@]}"; do
  selected "$n" || continue
  load "$n"
  (cd "$dir" && measure "$logs/$n.synth.m" "$synth" synth "${args[@]}" --top "$top" 2>"$logs/$n.synth.log")
  if [ "$(field "$logs/$n.synth.m" 1)" = 0 ]; then
    ours=$(tail -1 "$logs/$n.synth.log" | tr ',' '\n' | sed -E 's/^ *(info: )?//' | summarise)
    ours="$ours $(printf "%6s %5s" "$(field "$logs/$n.synth.m" 2)" "$(field "$logs/$n.synth.m" 3)")"
  else
    ours=$(failed "$logs/$n.synth.m")
  fi
  (cd "$dir" && measure "$logs/$n.yosys.m" yosys -q -p "$yread synth_ice40 -top $top; tee -o $logs/$n.ystat stat" \
    >"$logs/$n.yosys.log" 2>&1)
  if [ "$(field "$logs/$n.yosys.m" 1)" = 0 ]; then
    yos=$(awk '$1 ~ /^SB_/ { print $2, $1 }' "$logs/$n.ystat" | summarise)
    yos="$yos $(printf "%6s %5s" "$(field "$logs/$n.yosys.m" 2)" "$(field "$logs/$n.yosys.m" 3)")"
  else
    yos=$(failed "$logs/$n.yosys.m")
  fi
  printf "%-14s %s   %s\n" "$n" "$ours" "$yos"
done

fi

[ "${FLOW:-}" = synth ] && { sources; exit 0; }
seeds=(${SEEDS:-1 2 3})
printf "\nPlace and route (%d seeds, target %s MHz): logic cells, Fmax min/median/max\n" "${#seeds[@]}" "$freq"
printf "(MHz, icetime), median time (s) and peak memory (MB); FAIL counts runs without\n"
printf "a bitstream, met the bitstreams meeting the target\n"
printf "%-14s %6s %17s %6s %5s   %6s %17s %6s %6s %5s\n" "" cells Fmax time mem cells Fmax yosys nextpnr mem
printf "%-14s %-38s   %s\n" "design" "synth" "yosys + nextpnr"
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
  # Yosys once (deterministic), nextpnr per seed.
  (cd "$dir" && measure "$logs/$n.yflow.m" yosys -q -p "$yread synth_ice40 -top $top -json $logs/$n.json" \
    >"$logs/$n.yflow.log" 2>&1)
  ofail=0 nfail=0 ocells="" ncells="" ofm=() nfm=() ot=() nt=() omem=0 nmem=0
  for seed in "${seeds[@]}"; do
    b=$logs/$n.s$seed
    rm -f "$b.asc" "$b.nasc"
    (cd "$dir" && measure "$b.build.m" "$synth" build "${args[@]}" --top "$top" --device "$dev" \
      --package "$pkg" "${pcfargs[@]}" --seed "$seed" --freq "$freq" --timing-allow-fail \
      -o "$b.bin" --asc "$b.asc" >"$b.build.log" 2>&1)
    ot+=("$(field "$b.build.m" 2)")
    omem=$(( $(field "$b.build.m" 3) > omem ? $(field "$b.build.m" 3) : omem ))
    if [ "$(field "$b.build.m" 1)" = 0 ] && [ -f "$b.asc" ]; then
      ocells=$(sed -n 's/.* \([0-9]*\) ICESTORM_LC.*/\1/p' "$b.build.log" | tail -1)
      ofm+=("$(fmax "$dev" "$pkg" "$b.asc")")
    else
      ofail=$((ofail + 1))
    fi
    if [ "$(field "$logs/$n.yflow.m" 1)" = 0 ]; then
      (cd "$dir" && measure "$b.nextpnr.m" nextpnr-ice40 "--$dev" --package "$pkg" "${npcf[@]}" --seed "$seed" \
        --freq "$freq" --timing-allow-fail --json "$logs/$n.json" --asc "$b.nasc" >"$b.nextpnr.log" 2>&1)
      nt+=("$(field "$b.nextpnr.m" 2)")
      nmem=$(( $(field "$b.nextpnr.m" 3) > nmem ? $(field "$b.nextpnr.m" 3) : nmem ))
      if [ "$(field "$b.nextpnr.m" 1)" = 0 ] && [ -f "$b.nasc" ]; then
        ncells=$(sed -n 's/.*ICESTORM_LC: *\([0-9]*\).*/\1/p' "$b.nextpnr.log" | head -1)
        nfm+=("$(fmax "$dev" "$pkg" "$b.nasc")")
      else
        nfail=$((nfail + 1))
      fi
    else
      nfail=$((nfail + 1))
    fi
  done
  ofmax=$(printf "%s\n" "${ofm[@]}" | { grep . || true; } | spread)
  nfmax=$(printf "%s\n" "${nfm[@]}" | { grep . || true; } | spread)
  [ "$ofail" -gt 0 ] && ofmax="$ofmax FAIL$ofail"
  [ "$nfail" -gt 0 ] && nfmax="$nfmax FAIL$nfail"
  omet=$(printf "%s\n" "${ofm[@]}" | { grep . || true; } | met)
  nmet=$(printf "%s\n" "${nfm[@]}" | { grep . || true; } | met)
  [ "$omet" -gt 0 ] && ofmax="$ofmax met$omet"
  [ "$nmet" -gt 0 ] && nfmax="$nfmax met$nmet"
  printf "%-14s %6s %17s %6s %5s   %6s %17s %6s %6s %5s\n" "$n" "${ocells:--}" "$ofmax" \
    "$(printf "%s\n" "${ot[@]}" | median)" "$omem" "${ncells:--}" "$nfmax" \
    "$(field "$logs/$n.yflow.m" 2)" "$(printf "%s\n" "${nt[@]}" | median)" "$nmem"
done

sources
