#!/usr/bin/env bash
# The RISC-V benchmark of examples/weft/bench on the cores of this
# repository (development only): rv_fast in its own system, and, in systems
# made here for the comparison in a temporary directory, rv_pipe (its ROM
# and RAM enlarged to the benchmark's) and PicoRV32 (the PicoSoC of
# tests/designs/picosoc with 12 KiB of memory and the benchmark's devices,
# without and with its barrel shifter). For each: the cycles and
# instructions between the benchmark's marks, counted in simulation, its
# checksums checked; then the logic cells, block RAMs and Fmax of a build
# for the Alchitry Cu (the median of SEEDS, default "1 2 3", each the best
# of synth's four placement seeds), and the time the benchmark takes at
# that Fmax.
#
# Needs a built synth (SYNTH selects another), and clang with lld for the
# PicoRV32 image (the others use the images committed in the bench).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
synth=${SYNTH:-$root/target/release/synth}
seeds=${SEEDS:-1 2 3}
w=$root/examples/weft
b=$w/bench
pico=$root/tests/designs/picosoc
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
want="c8051c82 a22376ca 0c41a0ed 4eae9570 01180194 0017fe00 ffffa099 aa5fa60f"

# rv_pipe with the benchmark's memories: an 8 KiB ROM and a 4 KiB RAM.
sed -e "s|mem rom: \[uint<32>; 128\] init program<128>(DELAY);|mem rom: [uint<32>; 2048] init hex_file(\"$b/rom.hex\");|" \
  -e "s|mem ram: \[uint<32>; 256\];|mem ram: [uint<32>; 1024] init hex_file(\"$b/ram.hex\");|" \
  -e 's/rom\[pc\[8:2\]\]/rom[pc[12:2]]/' -e 's/ram\[result\[9:2\]\]/ram[result[11:2]]/g' \
  "$w/rv_pipe.weft" >"$tmp/rv_pipe_bench.weft"

# PicoRV32: 12 KiB of code and data from 0, the devices at 0x1000_0000.
cat >"$tmp/link.ld" <<'EOF'
MEMORY { RAM (rwx) : ORIGIN = 0x00000000, LENGTH = 12K }
SECTIONS {
    .text : { *(.text.start) *(.text*) *(.rodata*) *(.srodata*) *(.data*) *(.sdata*) } > RAM
    .bss (NOLOAD) : { . = ALIGN(4); __bss_start = .; *(.bss*) *(.sbss*) *(COMMON) . = ALIGN(4); __bss_end = .; } > RAM
    __stack_top = ORIGIN(RAM) + LENGTH(RAM);
}
EOF
clang --target=riscv32 -march=rv32i -mabi=ilp32 -O2 -ffreestanding -nostdlib -fno-builtin-memset \
  -fuse-ld=lld -Wl,-T,"$tmp/link.ld" -o "$tmp/bench.elf" "$b/start.S" "$b/bench.c"
llvm-objcopy -O binary -j .text "$tmp/bench.elf" "$tmp/fw.bin"
python3 -c "import sys; d=open(sys.argv[1],'rb').read(); d+=bytes(-len(d)%4); \
print('\n'.join('%08x'%int.from_bytes(d[i:i+4],'little') for i in range(0,len(d),4)))" \
  "$tmp/fw.bin" >"$tmp/fw.hex"
for barrel in 0 1; do
  sed -e 's/module top(input clk, input btn, output reg \[7:0\] led, output tx);/module top(input clk, output reg [7:0] led, output tx);\n  initial led = 0;\n  reg [31:0] cycles = 0;\n  always @(posedge clk) cycles <= cycles + 1;/' \
    -e "s/.BARREL_SHIFTER(0)/.BARREL_SHIFTER($barrel)/" -e "s/.STACKADDR(32'h1000)/.STACKADDR(32'h3000)/" \
    -e 's/reg \[31:0\] mem \[0:1023\];/reg [31:0] mem [0:3071];/' -e 's/mem_addr\[11:2\]/mem_addr[13:2]/g' \
    -e 's/wire is_mem = mem_addr\[31\] == 1.b0;/wire is_mem = mem_addr[28] == 1'"'"'b0;/' \
    -e "s/2'd0: if (mem_wstrb != 0 \&\& !tx_busy) begin/2'd1: if (mem_wstrb != 0 \&\& !tx_busy) begin/" \
    -e "s/2'd2: if (mem_wstrb != 0) led <= mem_wdata\[7:0\];/2'd0: if (mem_wstrb != 0) led <= mem_wdata[7:0];/" \
    -e 's/mem_rdata <= {30.b0, btn, tx_busy};/mem_rdata <= mem_addr[3] ? cycles : {31'"'"'b0, tx_busy};/' \
    "$pico/soc.v" >"$tmp/pico$barrel.v"
done
cp "$pico/picorv32.v" "$tmp/"
grep -v btn "$pico/alchitry_cu.pcf" >"$tmp/cu.pcf"

# measure NAME DIR CLKS_PER_BIT SIM-ARGS -- BUILD-ARGS
measure() {
  local name=$1 dir=$2 cpb=$3
  shift 3
  local sim=() build=()
  while [ "$1" != -- ]; do sim+=("$1"); shift; done
  shift
  build=("$@")
  # The cycles from the LEDs at 1 to the LEDs at 2.
  local at=() text
  for mark in 1 2; do
    at+=("$(cd "$dir" && "$synth" sim "${sim[@]}" --until led=$mark --cycles 20000000 2>&1 |
      sed -nE 's/^info: ([0-9]+) cycles .*stopped by --until/\1/p')")
  done
  local cycles=$((at[1] - at[0]))
  text=$(cd "$dir" && "$synth" sim "${sim[@]}" --until led=3 --cycles 20000000 --uart tx:"$cpb" --quiet 2>&1 |
    grep -E '^[0-9a-f]{8} ' || true)
  [ "$text" = "$want" ] || { echo "$name: wrong checksums: $text" >&2; return 1; }
  local lcs="" brams="" mhzs=""
  for seed in $seeds; do
    local o
    o=$(cd "$dir" && "$synth" build "${build[@]}" --freq 200 --timing-allow-fail --seed "$seed" -o "$tmp/out.bin" 2>&1)
    lcs=$(grep -oE '[0-9]+ ICESTORM_LC' <<<"$o" | cut -d' ' -f1)
    brams=$(grep -oE '[0-9]+ ICESTORM_RAM' <<<"$o" | cut -d' ' -f1)
    mhzs="$mhzs $(grep -E '^info: clock' <<<"$o" | grep -oE '[0-9.]+ MHz \(' | cut -d' ' -f1 | sort -n | head -1)"
  done
  local mhz
  mhz=$(tr ' ' '\n' <<<"$mhzs" | grep . | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  printf '%-22s %9d %6s %5s %6s %8.2f\n' "$name" "$cycles" "$lcs" "$brams" "$mhz" \
    "$(echo "$cycles / $mhz / 1000" | bc -l)"
}

printf '%-22s %9s %6s %5s %6s %8s\n' core cycles LCs BRAMs MHz ms
measure rv_fast "$b" 4 bench.weft ../rv_fast.weft ../rv32i.weft ../uart.weft ../rv_program.weft \
  --top bench -P CLKS_PER_BIT=4 -- \
  bench.weft ../rv_fast.weft ../rv32i.weft ../uart.weft ../rv_program.weft --top bench --board alchitry-cu
measure rv_pipe "$tmp" 4 rv_pipe_bench.weft "$w/rv32i.weft" "$w/rv_program.weft" "$w/uart.weft" \
  --top top -P CLKS_PER_BIT=4 -P DELAY=0 -- \
  rv_pipe_bench.weft "$w/rv32i.weft" "$w/rv_program.weft" "$w/uart.weft" --top top --board alchitry-cu \
  -P CLK_HZ=100000000 -P MAX_HZ=100000000
for barrel in 1 0; do
  label=PicoRV32
  [ $barrel = 0 ] || label="PicoRV32, barrel shifter"
  measure "$label" "$tmp" 8 "pico$barrel.v" picorv32.v --top top -- \
    "pico$barrel.v" picorv32.v --top top --device hx8k --package cb132 --pcf cu.pcf
done
echo "(rv_fast and rv_pipe run 469,461 instructions of the bench's image)"
