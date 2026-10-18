# synth

A unified, self-contained FPGA toolchain written in Rust. It takes Verilog-2005
or RTLIL (for example from Amaranth) to a programmable bitstream in one tool, with
no Yosys, nextpnr, Python or vendor tools at runtime.

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
bitstream generation are implemented for the iCE40 HX1K and HX8K.
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
build. `tools/icetime-carry-feedout.patch` adds those paths; with it,
PicoRV32 SoC measures 69.4 MHz for `synth` (82.8 MHz with stock icetime)
and 63.5 MHz for nextpnr (unchanged); no other design changes by more
than 0.3 MHz.

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
| PicoRV32 SoC      |        1592 |   82.8 MHz |        6 s |      1549 | 63.5 MHz |     18 s |
| SERV SoC          |         548 |   85.3 MHz |        2 s |       714 | 77.4 MHz |     11 s |
| SERV, Go Board    |         581 |   92.7 MHz |        2 s |       752 | 80.5 MHz |      8 s |
| Hazard3 SoC (Cu)  |        2237 |   40.8 MHz |       12 s |      2339 | 34.4 MHz |     56 s |
| Euler 14          |        6161 |   62.5 MHz |       34 s |      6343 | 66.6 MHz |     43 s |
| Euler 25          |        1390 |   53.7 MHz |        3 s |      1743 | 56.1 MHz |     10 s |
| Euler 31          |        1011 |  137.6 MHz |        2 s |      1387 | 105.4 MHz |     11 s |
| Euler 43          |        6235 |   44.0 MHz |       43 s |      6164 | 47.4 MHz |    146 s |
| Euler 48          |        6704 |   62.4 MHz |       45 s |      6597 | 62.8 MHz |     49 s |
| Euler 57          |        2719 |   58.0 MHz |        8 s |      3602 | 62.0 MHz |     23 s |
| Euler 58          |        7423 |   57.7 MHz |       68 s |      7403 | 57.4 MHz |     84 s |
| Euler 92          |        3928 |   79.9 MHz |       12 s |      4168 | 67.3 MHz |     23 s |
| Euler 97          |        1509 |   76.4 MHz |        5 s |      1617 | 54.2 MHz |      9 s |

Every seed of every design routes. Synthesis is smaller on 14 of the 16
designs and much faster on all, the whole flow is faster than Yosys + nextpnr
on every design, and the processors and four of the nine solvers end up with
a higher Fmax, Euler 48 level with nextpnr; Euler 14, 25, 43 and 57 still
trail by 4 to 7%.

### Experimental Artix-7 synthesis

The native synthesis foundation can emit Xilinx 7-series primitives:

```sh
target/release/synth synth design.v --top top --family artix7 --verilog mapped.v
```

`--family xc7` is an alias; omitting `--family` keeps iCE40. The Artix-7
path maps LUT1–LUT6, CARRY4, flip-flops and synchronous block RAMs entirely
in Rust, with no external tools at runtime.

**This is not full Artix-7 support.** Native packing, placement, routing,
constraints, timing analysis and bitstream generation are not implemented;
`build` and `pnr` still target iCE40. DSP, LUTRAM, SRL and wide-mux inference
also remain to be implemented. Yosys QoR parity has not been achieved.
See [the Artix-7 status and validation report](docs/ARTIX7.md).

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
