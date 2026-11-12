#!/usr/bin/env bash
# In-process simulation checks with `synth sim` (development / CI only): the
# designs of ci_rtlsim.sh (MODE=synth) and ci_bitsim.sh, each synthesized
# netlist and each bitstream (built by synth, decoded back to cells) against
# its RTL under random inputs, and the Project Euler solvers' answers and
# cycle counts in RTL (both modes), as netlists and as C models (with a C
# compiler, CC, also through `synth sim --compiled`). Needs only a built
# `synth` (no Icarus, Yosys or IceStorm), which also makes it fast; the
# Icarus-based scripts stay as independent oracles. The checks run JOBS at a
# time (default: one per core), their output in order.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs
e=$d/third_party
cc=${CC:-cc}
have_cc=$(command -v "$cc" >/dev/null && echo 1 || true)
max_jobs=${JOBS:-$(nproc 2>/dev/null || echo 2)}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
njobs=0

# job CMD...: runs CMD in the background, at most max_jobs at a time; a
# check fails by returning non-zero.
job() {
  njobs=$((njobs + 1))
  while [ "$(jobs -rp | wc -l)" -ge "$max_jobs" ]; do wait -n || true; done
  ( set +e; "$@"; echo $? >"$tmp/$njobs.rc" ) >"$tmp/$njobs.out" 2>&1 &
}

# check NAME DIR ARGS...: runs `synth sim ARGS` in DIR, which must PASS.
check() {
  local name=$1 dir=$2; shift 2
  local out
  if out=$(cd "$dir" && "$synth" sim "$@" --quiet 2>&1) && grep -q PASS <<<"$out"; then
    echo "PASS $name"
  else
    echo "FAIL $name"
    tail -12 <<<"$out"
    return 1
  fi
}

# Synthesized netlists against their RTL (as ci_rtlsim.sh MODE=synth).
p=$d/picosoc
job check "picosoc netlist" "$p" soc.v picorv32.v --top top --compare synth --cycles 50000 --hold 5000
sfiles=("$e"/serv/servant/servant.v "$e"/serv/servant/servant_mux.v "$e"/serv/servant/servant_ram.v \
  "$e"/serv/servant/servant_timer.v "$e"/serv/servant/servant_gpio.v "$e"/serv/servile/*.v "$e"/serv/rtl/*.v)
job check "serv netlist" "$e/serv/sw" "${sfiles[@]}" --top servant -P 'memfile="hello_uart.hex"' \
  --compare synth --clock wb_clk --reset wb_rst --cycles 200000 --hold 100
k=$e/darkriscv/rtl
job check "darkriscv netlist" "$k" "$k"/*.v -I "$k" --top darksocv --compare synth --clock XCLK --reset XRES \
  --cycles 100000 --hold 2000
h=$e/hazard3/hdl
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')
job check "hazard3 netlist" "$h" "${hfiles[@]}" -I "$h" --top hazard3_cpu_1port --compare synth \
  --clock clk --clock clk_always_on --reset-low rst_n --cycles 20000 --hold 3

# Bitstreams against their RTL (as ci_bitsim.sh, synth-only builds).
b=$d/blinky_ram
job check "blinky_ram hx1k bitstream" "$b" blinky_ram.v --top top --compare bitstream \
  --device hx1k --package vq100 --pcf go_board.pcf --cycles 100000 --hold 50
job check "blinky_ram hx8k bitstream" "$b" blinky_ram.v --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 100000 --hold 50
job check "picosoc bitstream" "$p" soc.v picorv32.v --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 200000 --hold 5000
g=$root/examples/go_board_serv
job check "go_board_serv bitstream" "$g" top.v "${sfiles[@]}" --top top --compare bitstream \
  --device hx1k --package vq100 --pcf go_board.pcf --cycles 150000 --hold 1000
c=$root/examples/alchitry_cu_hazard3
job check "alchitry_cu_hazard3 bitstream" "$c" -I "$h" top.v "${hfiles[@]}" --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 400000 --hold 1 --set rst_n=1

# Project Euler: answers after the original cycle counts (plus two cycles of
# reset), RTL in both modes, compiled, and the synthesized netlist.
eu=$d/euler

# euler N ANSWER CYCLES EXTRA LEVEL...: one solver at one level.
euler() {
  local n=$1 answer=$2 cycles=$3 extra=$4 out
  shift 4
  local src=("p0$n.v")
  [ -n "$extra" ] && src+=("$extra")
  if out=$(cd "$eu" && "$synth" sim "${src[@]}" --top "euler$n" --set start=1 --set rst=1 --at 2:rst=0 \
      --until done --print answer --level "$@" 2>&1) &&
    grep -q "answer = $answer " <<<"$out" && grep -q "info: $((cycles + 2)) cycles" <<<"$out"; then
    echo "PASS p0$n $*: $answer after $cycles cycles"
  else
    echo "FAIL p0$n $* (expected $answer after $cycles cycles)"
    tail -5 <<<"$out"
    return 1
  fi
}

while read -r n width answer cycles extra; do
  case $n in \#*|'') continue ;; esac
  # Compiled, even the long ones run in seconds.
  [ -n "$have_cc" ] && job euler "$n" "$answer" "$cycles" "$extra" rtl --compiled
  if [ "$cycles" -gt "${MAX_RUN:-200000}" ] && [ -z "${LONG:-}" ]; then
    job echo "skip p0$n interpreted ($cycles cycles; LONG=1 runs it)"
    continue
  fi
  job euler "$n" "$answer" "$cycles" "$extra" rtl --four-state
  job euler "$n" "$answer" "$cycles" "$extra" rtl --two-state
  job euler "$n" "$answer" "$cycles" "$extra" synth
done <"$eu/designs.txt"

# Project Euler as C models (--emit-c), in both schedules and the adaptive
# one: answers and cycle counts after reset, every design.
# cmodel N ANSWER CYCLES EXTRA
cmodel() {
  local n=$1 answer=$2 cycles=$3 extra=$4 status=0
  local src=("p0$n.v")
  [ -n "$extra" ] && src+=("$extra")
  local t=euler$n
  local w=$tmp/$t
  if ! (cd "$eu" && "$synth" sim "${src[@]}" --top "$t" --emit-c "$w" --quiet >/dev/null 2>&1); then
    echo "FAIL p0$n C model: --emit-c"
    return 1
  fi
  cat >"$w/main.c" <<EOF
#include <stdio.h>
#include "$t.h"
static ${t}_t m;
static void cycle(void) { ${t}_set_clk(&m, 1); ${t}_eval(&m); ${t}_set_clk(&m, 0); ${t}_eval(&m); }
int main(void) {
  long c = 0;
  ${t}_init(&m); ${t}_set_clk(&m, 0); ${t}_set_start(&m, 1); ${t}_set_rst(&m, 1); ${t}_eval(&m);
  cycle(); cycle(); ${t}_set_rst(&m, 0);
  do { cycle(); c++; } while (!${t}_get_done(&m) && c < 100000000);
  printf("%llu %ld\n", (unsigned long long)${t}_get_answer(&m), c);
  return 0;
}
EOF
  for sched in "" -DSIM_SETTLE_ALL -DSIM_SETTLE_CHANGED; do
    # shellcheck disable=SC2086
    if "$cc" -O1 $sched -o "$w/sim" "$w/main.c" "$w/$t.c" 2>"$w/cc.log" &&
      [ "$("$w/sim")" = "$answer $cycles" ]; then
      echo "PASS p0$n C model ${sched:-adaptive}: $answer after $cycles cycles"
    else
      status=1
      echo "FAIL p0$n C model ${sched:-adaptive} (expected $answer after $cycles cycles)"
      tail -5 "$w/cc.log"
      [ -x "$w/sim" ] && "$w/sim"
    fi
  done
  return $status
}

if [ -n "$have_cc" ]; then
  while read -r n width answer cycles extra; do
    case $n in \#*|'') continue ;; esac
    job cmodel "$n" "$answer" "$cycles" "$extra"
  done <"$eu/designs.txt"
else
  job echo "skip C models (no C compiler: $cc)"
fi

wait
status=0
for i in $(seq "$njobs"); do
  cat "$tmp/$i.out"
  [ "$(cat "$tmp/$i.rc" 2>/dev/null)" = 0 ] || status=1
done
exit $status
