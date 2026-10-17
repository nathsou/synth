#!/usr/bin/env bash
# Differential simulation of RISC-V SoCs (development / CI only).
#
#   MODE=elab  (default) `synth elab` output vs the source RTL: checks the
#              frontend (RTL_PASSES="dff memff ..." also applies RTL passes).
#   MODE=synth `synth synth` output (iCE40 primitives, simulated with Yosys's
#              models from YOSYS_DATDIR, default /usr/share/yosys) vs the
#              two-state elaborated RTL: checks synthesis. Two-state because
#              synthesised logic has no x: uninitialised registers start at 0.
#
# Needs iverilog >= 11 on PATH.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
mode=${MODE:-elab}
status=0
passes=()
for p in ${RTL_PASSES:-}; do passes+=(--pass "$p"); done

# check NAME DIR TOP SIM_ARGS -- SYNTH_ARGS -- RTL_FILES
# Runs in DIR (for relative memory files); SIM_ARGS go to rtlsim.py.
check() {
  local name=$1 dir=$2 top=$3 sim=$4; shift 4
  local args=() files=()
  [ "$1" = -- ] && shift
  while [ "$1" != -- ]; do args+=("$1"); shift; done
  shift
  files=("$@")
  echo "== $name"
  local ok=1
  (
    cd "$dir"
    if [ "$mode" = synth ]; then
      # Parameter overrides were applied by elaboration.
      sim=$(echo "$sim" | sed -E 's/-P [^ ]+//g')
      "$synth" elab "${args[@]}" --top "$top" --pass two-state --verilog "$out/$name.ref.v" --rename "$top"
      "$synth" synth "${args[@]}" --top "$top" --verilog "$out/$name.v" --rename "${top}_netlist"
      # shellcheck disable=SC2086
      python3 "$root/tools/rtlsim.py" --lib "${YOSYS_DATDIR:-/usr/share/yosys}/ice40/cells_sim.v" \
        --top "$top" --netlist "$out/$name.v" $sim "$out/$name.ref.v"
    else
      "$synth" elab "${args[@]}" --top "$top" "${passes[@]}" --verilog "$out/$name.v" --rename "${top}_netlist"
      # shellcheck disable=SC2086
      python3 "$root/tools/rtlsim.py" --top "$top" --netlist "$out/$name.v" $sim "${files[@]}"
    fi
  ) >"$out/$name.log" 2>&1 || ok=0
  if [ $ok = 1 ] && grep -q PASS "$out/$name.log"; then
    grep PASS "$out/$name.log"
  else
    status=1
    grep -v "sorry:" "$out/$name.log" | tail -20
  fi
}

# PicoRV32 SoC running a C program that prints over the UART.
p=$d/picosoc
check picosoc "$p" top "--clock clk --cycles 50000 --hold 5000" \
  -- soc.v picorv32.v -- soc.v picorv32.v

# SERV (bit-serial) in its servant SoC, printing "Hi, I'm Servant!".
s=$d/third_party/serv
sfiles=("$s"/servant/servant.v "$s"/servant/servant_mux.v "$s"/servant/servant_ram.v \
  "$s"/servant/servant_timer.v "$s"/servant/servant_gpio.v "$s"/servile/*.v "$s"/rtl/*.v)
check serv "$s/sw" servant "--clock wb_clk --reset wb_rst --cycles 200000 --hold 100 -P memfile=\"hello_uart.hex\"" \
  -- "${sfiles[@]}" -P 'memfile="hello_uart.hex"' -- "${sfiles[@]}"

# DarkRISCV SoC running its default firmware. __ICARUS__ selects the same
# simulation-only code paths as iverilog (which stop the source at $finish).
k=$d/third_party/darkriscv/rtl
check darkriscv "$k" darksocv "--clock XCLK --reset XRES --cycles 100000 --hold 2000 -I $k" \
  -- "$k"/*.v -I "$k" -D __ICARUS__ -- "$k"/*.v

# Hazard3 core alone, with random AHB responses (instruction fuzzing).
h=$d/third_party/hazard3/hdl
mapfile -t hfiles < <(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}')
check hazard3 "$h" hazard3_cpu_1port \
  "--clock clk --clock clk_always_on --reset-low rst_n --cycles 20000 --hold 3 -I $h" \
  -- "${hfiles[@]}" -I "$h" -- "${hfiles[@]}"

exit $status
