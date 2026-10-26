# synth

A unified, self-contained FPGA toolchain written in Rust. It takes Verilog-2005,
[Weft](docs/WEFT.md) (its own HDL) or RTLIL (for example from Amaranth) to a
programmable bitstream in one tool, with no Yosys, nextpnr, Python or vendor
tools at runtime.

Initial targets, in order:

| Family | Parts | Reference boards |
|---|---|---|
| Lattice iCE40 | LP / HX / UltraPlus | Alchitry Cu (HX8K), Nandland Go Board (HX1K) |
| Xilinx Artix-7 | XC7A35T – XC7A200T | Nexys A7 (XC7A100T), Alchitry Au (XC7A35T) |
| Lattice ECP5 | LFE5U / LFE5UM / LFE5UM5G | deferred |

The goal is to match or beat vendor tools (iCEcube2/Radiant, Diamond, Vivado) on
resource use and Fmax, with deterministic, reproducible results.

```sh
cargo build --release
target/release/synth build design.v --top top \
  --device hx8k --package cb132 --pcf pins.pcf -o design.bin
```

## Weft

Weft is a small HDL that compiles straight to synth's RTL IR. It is designed so
that the usual Verilog mistakes are compile errors with a suggested fix:
arithmetic is exact (wrapping is written `+%`), indices are range-checked,
registers state their reset or power-on value, combinational signals must be
assigned on every path, and clock domains are checked, so an unsynchronized
input or a domain crossing is rejected. A module with one clock needs no domain
annotations.

```weft
module top(in clk: clock, in btn: bool, out led: bool) {
    let pressed = sync(btn);        // `btn` is asynchronous until synchronized
    reg count: uint<24> = 0;
    if pressed { count := count +% 1; }
    led = count[23];
}
```

```sh
target/release/synth build top.weft --device hx1k --package vq100 --pcf pins.pcf -o top.bin
```

On a terminal, `synth build`, `synth synth` and `synth pnr` show their
progress on the last line: the step of the flow, a bar for the whole run,
what the step is doing (the annealing round, the routing iteration and the
wires still congested) and the time so far, with the log above it.

See the [specification](docs/WEFT.md) and the [examples](examples/weft). Verilog
modules can be used from Weft through `extern module` declarations.

`synth lsp` is a language server for Weft: diagnostics as you type, hover with
types, ranges and clock domains, go to definition, references, rename,
outline, completion and semantic highlighting. See [editor support](docs/EDITORS.md)
for VS Code (an extension is in [`editors/vscode`](editors/vscode)), Neovim,
Helix and Emacs. `synth fmt` formats Weft sources (`--check` only reports the
files that would change).

## Playground

`synth playground` opens the whole flow in the browser: edit Weft or Verilog
and see the design after every pass as a schematic, from the module level down
to gates and LUTs, then the cells and routed wires on the actual iCE40 die,
the critical paths and every configuration bit, decoded. Its examples (the
Project Euler solvers among them) build for the Nandland Go, the Alchitry Cu
and Au and the Nexys A7, with `--xc7-db` for the Artix-7 boards.

```sh
target/release/synth playground        # http://127.0.0.1:8080/
tools/build_playground.sh              # or a static site, the flow in WebAssembly
```

See [docs/PLAYGROUND.md](docs/PLAYGROUND.md).

## Simulation

`synth sim` simulates Verilog (four-state, as Icarus does) or Weft
(two-state) without other tools: it compiles the elaborated design into a
program that runs only the logic each change reaches. It drives the clock,
sets inputs at given cycles, stops on a condition, prints signals, decodes a
serial line and writes VCD:

```sh
target/release/synth sim tests/designs/euler/p031.v --top euler31 \
  --set start=1 --set rst=1 --at 2:rst=0 --until done --print answer --vcd p031.vcd
target/release/synth sim soc.v picorv32.v --top top --cycles 200000 --uart tx:8
```

For long runs, `--emit-c <dir>` writes the design as a C model (`<top>.h`,
`<top>.c`) to compile with a testbench of your own, like a Verilator model.
`--attach` places external memories (a ROM on the design's address and data
ports) inside the model. The model runs only the logic that changed, or all
of it, as Verilator does, when most of it changes at each edge
(`-DSIM_SETTLE_CHANGED` or `-DSIM_SETTLE_ALL` forces one). On the NES of the
Weft examples, the C model runs twice as fast as Verilator, on the Euler
solvers 1.1 times as fast (geometric mean, from 0.6 to 2.1 times) and on the
PicoRV32 SoC 0.8 times as fast; the interpreter is 7 to 70 times slower than
the C model. `--optimize` speeds up the interpreter (it is always on for C
models), at the cost of internal signals. Rust code drives simulations
through the `synth-sim` crate. See [ADR 0008](docs/decisions/0008-simulator.md).

The same simulator checks the flow: `--level synth` simulates the iCE40
netlist synthesis makes of a design and `--level bitstream` what its
bitstream implements, decoded back to cells (ports named after the PCF);
`--compare synth` or `--compare bitstream` runs one against the RTL under
random inputs, comparing every output bit the RTL defines:

```sh
target/release/synth sim soc.v picorv32.v --top top --compare bitstream \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf --cycles 200000
```

`tools/ci_sim.sh` runs these checks on the test designs and boards in CI.

## Carry mapping

`--carry-min-width <bits>` on `synth build` or `synth synth` sets the minimum
adder width that uses a hard carry chain (default: 3). Variable comparators use
a threshold two bits higher. For example, `--carry-min-width 8` leaves short
arithmetic in LUT logic, which can reduce carry feed-in cells and improve timing
on designs with many small adders. Compare routed timing and packed cell counts
for your design; a higher threshold is not always better.

Subsequent optimization maps a carry chain into LUT logic when its distinct
boundary signals fit within the four inputs of a LUT. This is a local mapping
rule; synthesis runs once. Automatic incremental checkpoints distinguish
explicit carry thresholds. See [the QoR experiments](docs/QOR_EXPERIMENTS.md)
for results and tradeoffs.

## Incremental builds

Add `--incremental` to reuse the previous placement and routing after edits:

```sh
target/release/synth build design.v --top top \
  --device hx8k --package cb132 --pcf pins.pcf -o design.bin --incremental
```

The checkpoint is selected automatically under `.synth/checkpoints/` in the
working directory. Its key includes source **paths** (not contents), top module,
defines, parameters, include paths, PCF path, target and implementation settings.
Changing the output filename does not change the selected checkpoint. Different
designs/settings get separate checkpoints. Put bare `--incremental` after the
input files or before another option; `--incremental path.chk` (or
`--incremental=path.chk`) selects an explicit checkpoint. `synth pnr` supports
the same option.

The first run creates a checkpoint; subsequent runs still elaborate, synthesize
and pack the current design. Matching packed cells are placed at their previous
locations where legal, with carry chains handled as complete units. Edited units
are placed near their neighbors, then refined by a short annealing (the whole
placement when more than 5% of the cells are new). If fewer than 60% of the cells
can keep their place, fresh PnR runs straight away. Saved route trees are
validated, pruned to the sinks each net still has and reused; new routes avoid
the kept ones, and the router may still displace them to resolve congestion. LUT
pin permutations are recovered from the final routes, and current LUT/RAM
contents are always written to the bitstream.

Full timing analysis runs every time. A failed repair, a missed clock target, or
a clock more than 5% slower than in the checkpointed run triggers fresh PnR;
corrupt/incompatible checkpoints also start fresh. Successful runs atomically
replace the checkpoint. Runs that miss timing do not replace it, even with
`--timing-allow-fail`.

This is incremental **PnR**, not incremental synthesis or partial FPGA
reconfiguration. Matching uses cell names, primitive parameters and pin schemas;
large synthesis changes or renaming can reduce reuse. Omit `--incremental` for a
fresh optimization. Identical input and
checkpoint produce reproducible results, but results can depend on edit history.

`tools/ci_incremental.sh` verifies decoded bitstreams against edited RTL for RAM
contents, added/removed logic, carry-chain and pin changes (requires the CI reference
tools). `cargo test --workspace` also checks checkpoint corruption, allocation-ID
changes, routing invalidation, automatic selection and repeated saves.

See [the incremental benchmark](docs/INCREMENTAL_BENCHMARK.md) for small and
structural edits across four designs, including slowdowns and timing tradeoffs.

## Status

**iCE40 works end to end.** Verilog-2005 elaboration, logic synthesis (AIG
with SAT sweeping, DAG-aware rewriting, refactoring and structural choices;
sequential clean-up; LUT mapping with remapping and don't-care optimisation,
timing through carry chains, block RAM inference), packing, timing-driven
placement and routing (with LUT input permutation), static timing analysis
against clock constraints (`set_frequency` in the PCF or `--freq`) and
bitstream generation are implemented for the iCE40 HX1K and HX8K, PLLs
included: `synth_pll<IN_HZ, OUT_HZ>` (Weft or Verilog) makes a clock with the
device's PLL on iCE40 and Artix-7 alike ([docs/WEFT.md](docs/WEFT.md) §8.7).
[examples/](examples/README.md) has RISC-V systems for the Alchitry Cu
(Hazard3, 5-stage pipelined) and the Go Board (SERV), ready to program.

Every stage is checked by simulation against the source RTL on four RISC-V
cores (PicoRV32, SERV, DarkRISCV, Hazard3) and ten Project Euler solvers
([tests/designs/euler](tests/designs/euler/README.md)), and the bitstreams by
simulating the decoded configuration.

### Against Yosys + nextpnr

`tools/qor.sh` runs both flows on the same sources, devices, packages, pin
constraints (HX8K CB132, the Go Board's HX1K VQ100 for `go_serv`) and clock
target: `--freq 200` for both, above what any design reaches, so both optimise
for speed (nextpnr's results are the same as with its 12 MHz default). Fmax is
computed by `icetime` on each bitstream (median of three placement seeds),
times are wall clock on the same machine. The script records the tool
versions, keeps every tool's report (`QOR_LOG`), and checks each design's
sources against the frozen list in `tools/qor-sources.txt`.

`synth`'s own timing analysis agrees with icetime path by path, with two
known differences: icetime takes the first setup record of a cell (the
falling edge one, up to 70 ps less than the worse edge, which `synth`
uses), and stock icetime does not time paths that leave a carry chain
through the `I3` input of a cell without carry logic, which both flows
build. `tools/icetime-carry-feedout.patch` adds those paths; with it, an
earlier `synth` measured 69.4 MHz on PicoRV32 SoC (82.8 MHz with stock
icetime) and nextpnr 63.5 MHz (unchanged); no other design changed by more
than 0.3 MHz. The table below uses stock icetime.

Synthesis (iCE40 cells and time):

| Design        | synth LUT4 | Yosys LUT4 | synth time | Yosys time |
|---------------|-----------:|-----------:|-----------:|-----------:|
| PicoRV32 SoC  |       1326 |       1364 |      0.5 s |      9.1 s |
| SERV SoC      |        387 |        501 |      0.2 s |      7.2 s |
| DarkRISCV SoC |       2671 |       2861 |      1.6 s |     13.8 s |
| Hazard3 core  |       3412 |       3547 |      5.2 s |     47.8 s |
| Euler 10      |       6507 |      64445 |      5.5 s |     1544 s |
| Euler 14      |       5427 |       5696 |      1.9 s |     15.9 s |
| Euler 43      |       5589 |       5550 |      3.0 s |    121.8 s |
| Euler 48      |       6038 |       5826 |      1.5 s |     14.0 s |
| Euler 58      |       5958 |       5965 |      3.5 s |     31.2 s |
| Euler 92      |       1622 |       1759 |      0.7 s |      7.5 s |

Whole flow to a bitstream (logic cells, Fmax, time):

| Design            | synth cells | synth Fmax | synth time | Y+N cells | Y+N Fmax | Y+N time |
|-------------------|------------:|-----------:|-----------:|----------:|---------:|---------:|
| PicoRV32 SoC      |        1545 |   88.2 MHz |        4 s |      1549 | 63.5 MHz |     24 s |
| SERV SoC          |         548 |   92.5 MHz |        2 s |       714 | 77.4 MHz |     14 s |
| SERV, Go Board    |         581 |   98.2 MHz |        1 s |       752 | 80.5 MHz |     11 s |
| Hazard3 SoC (Cu)  |        2204 |   45.8 MHz |        8 s |      2339 | 34.4 MHz |     87 s |
| Euler 10          |        7311 |   56.9 MHz |       31 s |         – | does not fit | 2527 s |
| Euler 14          |        6015 |   71.1 MHz |       14 s |      6343 | 66.6 MHz |     75 s |
| Euler 25          |        1324 |   72.3 MHz |        3 s |      1743 | 56.1 MHz |     16 s |
| Euler 31          |         989 |  149.7 MHz |        2 s |      1387 | 105.4 MHz |     16 s |
| Euler 43          |        5959 |   54.0 MHz |       18 s |      6164 | 47.4 MHz |    200 s |
| Euler 48          |        6503 |   74.3 MHz |       18 s |      6597 | 62.8 MHz |     66 s |
| Euler 57          |        2629 |   64.1 MHz |        5 s |      3602 | 62.0 MHz |     36 s |
| Euler 58          |        7381 |   64.2 MHz |       29 s |      7403 | 57.4 MHz |    134 s |
| Euler 92          |        3841 |   91.2 MHz |        6 s |      4168 | 67.3 MHz |     35 s |
| Euler 97          |        1467 |   83.8 MHz |        3 s |      1617 | 54.2 MHz |     13 s |

`synth build` places and routes four placement seeds at once, on threads of
their own (4 cores here), and keeps the best timed (`--tries 1` places one).
Every seed of every design routes. Synthesis is smaller on 14 of the 16
designs and much faster on all; the whole flow is faster than Yosys +
nextpnr on every design, from 3.7 times (Euler 48) to 13 (SERV on the Go
Board), and every design ends up with a higher Fmax, by 3% (Euler 57) to
55% (Euler 97). Yosys maps Euler 10 to 64,445 LUTs, which no iCE40 holds.

### Experimental native Artix-7 flow

The Rust backend supports synthesis, slice/carry/RAM/DSP packing, placement,
routing and configuration-frame generation:

```sh
tools/fetch_xc7_db.sh /tmp/prjxray-db
target/release/synth build design.v --top top --family artix7 \
  --db /tmp/prjxray-db/artix7 --device xc7a35tftg256-1 \
  --xdc design.xdc -o design.bit --timing-allow-fail
```

`--family xc7` is an alias; omitting `--family` keeps iCE40. `synth` can emit
mapped Verilog, and `pnr` accepts mapped RTLIL. External tools are used only
for independent tests; the runtime reads the pinned CC0 device database.

**Artix-7 has not reached iCE40 maturity.** Against Vivado 2025.2 on the Euler
solvers, with Vivado's timer judging both, synth uses fewer slices on six of
ten and its synthesis is on par, but its placement and routing are 1–12%
slower under a tight clock (3% on average), in less time on average. Physical tests compare native frames/packets
against Project X-Ray on 35T and 100T, and netlists rebuilt from the
bitstreams simulate correctly, but hardware behavior remains unvalidated.
Builds require the explicit timing override. Advanced clocks/I/O, RAMB36, DSP
cascades and incremental checkpoints remain unsupported. See [the detailed
status and validation report](docs/ARTIX7.md) for the supported XDC subset and
remaining criteria.

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): pipeline, IRs, algorithms,
  device database, verification strategy.
- [docs/ROADMAP.md](docs/ROADMAP.md): phases, exit criteria, risks and open
  decisions.

## Licence

`synth` is licensed under the [EUPL-1.2](LICENSE). HDL kept in this repository
(example designs, test SoCs, soft IP) is licensed under CERN-OHL-W-2.0, as marked
in those files.

Bitstreams, netlists, reports and other output that `synth` produces from your
designs are yours. They are not covered by the licence of the toolchain.

## Acknowledgements

Device knowledge comes from the open bitstream documentation projects:
[Project IceStorm](https://github.com/YosysHQ/icestorm),
[Project Trellis](https://github.com/YosysHQ/prjtrellis) and
[Project X-Ray](https://github.com/f4pga/prjxray).
