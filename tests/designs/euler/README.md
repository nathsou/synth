# Project Euler solvers

Ten Project Euler problems solved in hardware, ported to Verilog-2005 from the
HDLean designs in [nathsou/hdlean](https://github.com/nathsou/hdlean)
(`Designs/Euler`, MIT licence). Each design aims at the shortest time to the
answer for the Alchitry Cu (iCE40-HX8K): few cycles and a high clock. The
ports keep HDLean's architectures register for register: every solver gives
the same answer after exactly the same number of cycles as the original.

| Problem | Design | Answer | Cycles |
|---|---|---:|---:|
| 10. Summation of primes | `p010.v` | 142913828922 | 131,016 |
| 14. Longest Collatz sequence | `p014.v` | 837799 | 364,431 |
| 25. 1000-digit Fibonacci number | `p025.v` | 4782 | 2,390 |
| 31. Coin sums | `p031.v` | 73682 | 209 |
| 43. Sub-string divisibility | `p043.v` | 16695334890 | 363 |
| 48. Self powers | `p048.v` | 9110846700 | 22,758 |
| 57. Square root convergents | `p057.v` | 153 | 8,571 |
| 58. Spiral primes | `p058.v` | 26241 | 2,628,585 |
| 92. Square digit chains | `p092.v` | 8581146 | 1,201 |
| 97. Large non-Mersenne prime | `p097.v` | 8739992577 | 389 |

Every solver is a module `eulerNN (clk, rst, start, done, answer)`: held in
reset, then running until `done` rises, with `answer` valid from then on. The
algorithms are described at the top of each file.

- `bcd.v`: BCD addition (one binary carry chain resolves the decimal carries)
  and doubling, for P25 and P57.
- `report.v`: a cycle counter and a UART reporter that sends
  `<answer> <cycles>\r\n` at 115200 baud.
- `euler_top.v`, `alchitry_cu.pcf`: the board shell for the Alchitry Cu. The
  solver runs on the 100 MHz oscillator (HDLean's shell uses the PLL), the
  reporter on 50 MHz.
- `euler_board.v`: the board-neutral shell of the playground (a PLL's
  clock, the reporter on the same clock); `p031_board.v` puts P31 in it on
  a 25 MHz board clock, for the PLL checks of `tools/ci_ecp5.sh`.
- `tb.v`, `designs.txt`: the testbench and the expected results.

`tools/ci_euler.sh` checks every solver's RTL and its synthesised netlist in
simulation; `tools/qor.sh` compares the board designs with Yosys + nextpnr.

Building one for the board:

```sh
cd tests/designs/euler
synth build euler_top.v report.v p031.v --top euler_top \
  -D SOLVER=euler31 -D ANSWER_WIDTH=17 \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf -o euler31.bin
```

## Alchitry Au with the Io shield

`euler_au_io.v` and `alchitry_au_io.xdc` are the shell for the Alchitry Au
(V1, XC7A35T) with the Io V1 shield. The pins come from Alchitry Labs' Au
pinout and Io V1 constraints, with the buttons and switches pulled down.
The solver runs as on the Cu and the same UART report goes to the USB
serial port. The shield shows the result in decimal, four digits at a
time from the least significant: any button pages through the rest (the
Au's LEDs show the page, one-hot). The first DIP switch shows the cycle
count instead, and the 24 shield LEDs show the low 24 bits in binary.

The Artix-7 flow is experimental: its timing estimate is uncalibrated, so
builds need `--timing-allow-fail`, and its bitstreams have not been run on
hardware yet. Load to SRAM first (it is lost at power-off). The database
comes from `tools/fetch_xc7_db.sh` (see `docs/ARTIX7.md`).

```sh
tools/fetch_xc7_db.sh /tmp/prjxray-db
cd tests/designs/euler
synth build euler_au_io.v report.v p031.v --top euler_au_io \
  -D SOLVER=euler31 -D ANSWER_WIDTH=17 \
  --family artix7 --db /tmp/prjxray-db/artix7 --device xc7a35tftg256-1 \
  --xdc alchitry_au_io.xdc --timing-allow-fail -o euler31_au.bit
openFPGALoader -b alchitry_au euler31_au.bit   # to SRAM
picocom -b 115200 /dev/ttyUSB1                 # "73682 209"
```

`-o name.bin` writes the raw stream that Alchitry's loader takes instead of
a `.bit` file. For another problem, take its answer width and extra
sources (`bcd.v` for 25 and 57) from `designs.txt`.
