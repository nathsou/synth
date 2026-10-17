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
