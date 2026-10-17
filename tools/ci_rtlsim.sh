#!/usr/bin/env bash
# Differential simulation of the frontend on RISC-V SoCs (development / CI
# only): each design is elaborated by `synth elab`, written back as a Verilog
# netlist and simulated against its source with iverilog (tools/rtlsim.py).
# Needs iverilog >= 11 on PATH.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
d=$root/tests/designs
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
status=0

run() { # name, then the command; reports PASS/FAIL per design
  local name=$1; shift
  echo "== $name"
  if "$@" >"$out/$name.log" 2>&1 && grep -q PASS "$out/$name.log"; then
    grep PASS "$out/$name.log"
  else
    status=1; tail -20 "$out/$name.log"
  fi
}

# PicoRV32 SoC running a C program that prints over the UART.
p=$d/picosoc
(cd "$p" && "$synth" elab soc.v picorv32.v --top top --verilog "$out/picosoc.v" --rename top_netlist)
run picosoc bash -c "cd '$p' && python3 '$root/tools/rtlsim.py' --top top --netlist '$out/picosoc.v' \
  --clock clk --cycles 50000 --hold 5000 soc.v picorv32.v"

# SERV (bit-serial) in its servant SoC, printing "Hi, I'm Servant!".
s=$d/third_party/serv
sfiles="$s/servant/servant.v $s/servant/servant_mux.v $s/servant/servant_ram.v $s/servant/servant_timer.v \
  $s/servant/servant_gpio.v $s/servile/*.v $s/rtl/*.v"
(cd "$s/sw" && "$synth" elab $sfiles --top servant -P 'memfile="hello_uart.hex"' \
  --verilog "$out/servant.v" --rename servant_netlist)
run serv bash -c "cd '$s/sw' && python3 '$root/tools/rtlsim.py' -P 'memfile=\"hello_uart.hex\"' --top servant \
  --netlist '$out/servant.v' --clock wb_clk --reset wb_rst --cycles 200000 --hold 100 $sfiles"

# DarkRISCV SoC running its default firmware until the testbench-only
# $finish. __ICARUS__ selects the same simulation-only code paths as iverilog.
k=$d/third_party/darkriscv/rtl
(cd "$k" && "$synth" elab "$k"/*.v --top darksocv -I "$k" -D __ICARUS__ \
  --verilog "$out/darksocv.v" --rename darksocv_netlist)
run darkriscv bash -c "cd '$k' && python3 '$root/tools/rtlsim.py' --top darksocv --netlist '$out/darksocv.v' \
  --clock XCLK --reset XRES --cycles 100000 --hold 2000 -I '$k' $k/*.v"

# Hazard3 core alone, with random AHB responses (instruction fuzzing).
h=$d/third_party/hazard3/hdl
hfiles=$(grep '^file' "$h/hazard3.f" | awk -v h="$h" '{print h "/" $2}' | tr '\n' ' ')
"$synth" elab $hfiles --top hazard3_cpu_1port -I "$h" --verilog "$out/hazard3.v" --rename hazard3_cpu_1port_netlist
run hazard3 python3 "$root/tools/rtlsim.py" --top hazard3_cpu_1port --netlist "$out/hazard3.v" \
  --clock clk --clock clk_always_on --reset-low rst_n --cycles 20000 --hold 3 -I "$h" $hfiles

exit $status
