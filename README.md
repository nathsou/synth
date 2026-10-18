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
are placed near their neighbors. Route trees with identical physical endpoints
are validated and reused; the router repairs affected nets and may displace
retained routes to resolve congestion. LUT pin permutations are recovered from
the final routes, and current LUT/RAM contents are always written to the bitstream.

Full timing analysis runs every time. A failed repair or missed clock target
triggers fresh PnR; corrupt/incompatible checkpoints also start fresh. Successful
runs atomically replace the checkpoint. Runs that miss timing do not replace it,
even with `--timing-allow-fail`. Specify a clock target to enforce timing; without
one, incremental builds do not promise the same Fmax as a fresh optimization.

This is incremental **PnR**, not incremental synthesis or partial FPGA
reconfiguration. Matching uses cell names, primitive parameters and pin schemas;
large synthesis changes or renaming can reduce reuse. Placement repair retains
legal existing units rather than annealing them, falling back to a full run when
necessary. Omit `--incremental` for a fresh optimization. Identical input and
checkpoint produce reproducible results, but results can depend on edit history.

`tools/ci_incremental.sh` verifies decoded bitstreams against edited RTL for RAM
contents, added/removed logic, carry-chain and pin changes (requires the CI reference
tools). `cargo test --workspace` also checks checkpoint corruption, allocation-ID
changes, routing invalidation, automatic selection and repeated saves.

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

`tools/qor.sh` runs both flows on the same sources, devices, packages and pin
constraints (HX8K CB132, the Go Board's HX1K VQ100 for `go_serv`). Fmax is
computed by `icetime` on each bitstream (median of three placement seeds),
times are wall clock on the same machine.

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
| Euler 48      |       6197 |       5826 |      1.4 s |     14.0 s |
| Euler 58      |       5958 |       5965 |      3.5 s |     31.2 s |
| Euler 92      |       1622 |       1759 |      0.7 s |      7.5 s |

Whole flow to a bitstream (logic cells, Fmax, time):

| Design            | synth cells | synth Fmax | synth time | Y+N cells | Y+N Fmax | Y+N time |
|-------------------|------------:|-----------:|-----------:|----------:|---------:|---------:|
| PicoRV32 SoC      |        1592 |   83.5 MHz |        7 s |      1549 | 63.5 MHz |     18 s |
| SERV SoC          |         548 |   86.9 MHz |        2 s |       714 | 77.4 MHz |     11 s |
| SERV, Go Board    |         581 |   89.8 MHz |        1 s |       752 | 80.5 MHz |      8 s |
| Hazard3 SoC (Cu)  |        2237 |   41.9 MHz |       13 s |      2339 | 34.4 MHz |     62 s |
| Euler 14          |        6161 |   63.3 MHz |       38 s |      6343 | 66.6 MHz |     51 s |
| Euler 25          |        1390 |   50.9 MHz |        3 s |      1743 | 56.1 MHz |     11 s |
| Euler 31          |        1011 |  128.0 MHz |        2 s |      1387 | 105.4 MHz |     11 s |
| Euler 43          |        6235 |   43.4 MHz |       41 s |      6164 | 47.4 MHz |    159 s |
| Euler 48          |        6863 |   48.1 MHz |       43 s |      6597 | 62.8 MHz |     54 s |
| Euler 57          |        2719 |   56.4 MHz |        7 s |      3602 | 62.0 MHz |     24 s |
| Euler 58          |        7423 |   59.5 MHz |       71 s |      7403 | 57.4 MHz |    102 s |
| Euler 92          |        3928 |   73.7 MHz |       12 s |      4168 | 67.3 MHz |     23 s |
| Euler 97          |        1534 |   68.8 MHz |        4 s |      1617 | 54.2 MHz |     10 s |

Every seed of every design routes. Synthesis is smaller on 14 of the 16
designs and much faster on all, the whole flow is faster than Yosys + nextpnr
on every design, and the processors and four of the nine solvers end up with
a higher Fmax; the other, arithmetic-heavy solvers (multipliers and many carry
chains, Euler 48 most of all) are where Fmax still trails nextpnr.

Artix-7 is next.

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
