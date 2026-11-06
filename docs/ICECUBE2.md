# iCE40: QoR against iCEcube2

`tools/icecube2_ice40.py` implements each design of the iCE40 benchmark
(the RISC-V systems and the Euler solvers of [tools/qor.sh](../tools/qor.sh))
with synth and with Lattice iCEcube2 2020.12, on the same device, package,
pin constraints, sources and clock targets, and times both bitstreams with
the same static timing analysis.

## Method

- **synth**: `synth build --freq 200 --timing-allow-fail`, seeds 1, 2 and 3;
  the median run by Fmax is reported (each run is itself the best of synth's
  four parallel placement seeds).
- **iCEcube2**: LSE synthesis (`-optimization_goal Timing`, the other options
  as iCEcube2's IDE sets them), then the back end as the IDE runs it:
  `edifparser`, `sbtplacer` (standard effort), `packer`, `sbrouter` (timing
  driven, LUT pin permutation), `netlister`, `sbtimer` and `bitmap`.
  Clocks are constrained in the SDC that LSE writes for the back end
  (`impl.scf`): every clock at 200 MHz, as for synth. iCEcube2 has no seed.
- **Divided clocks.** The Euler solvers' UART reporter runs on the clock
  divided by two (a register, `slow`); both flows constrain it at 100 MHz
  (`set_frequency slow 100` for synth). The MHz columns give the fastest
  the clock port can run with every clock meeting timing (the reporter's
  Fmax counting twice).
- **One timer for both.** `synth time` decodes each bitstream back to cells
  and routes and times it with synth's analysis (IceStorm's worst-case
  models, as `synth build` reports them), including the LUT cascades and
  block RAM address cascades iCEcube2 uses and synth's own flow did not.
  On synth's bitstreams it gives exactly what `synth build` reports. On
  iCEcube2's it agrees with iCEcube2's own sign-off timer, sbtimer (HX8K,
  worst case, 1.14 V, 85 °C):

  | Design | `synth time` | sbtimer |
  |---|---:|---:|
  | Euler 14 | 66.8 MHz | 67.6 MHz |
  | Euler 25 | 57.8 MHz | 57.8 MHz |
  | Euler 31 | 157.9 MHz | 160.4 MHz |
  | Euler 57 | 52.7 MHz | 52.9 MHz |
  | Euler 92 | 72.3 MHz | 71.9 MHz |
  | Euler 97 | 73.9 MHz | 74.2 MHz |

  so the comparison does not depend on which of the two times it.
- **Cells** are logic cells (`ICESTORM_LC`) and block RAMs counted in the
  decoded bitstreams, alike for both flows.
- iCEcube2's tools are single-threaded and ran 6 or 7 at a time; synth's
  times are with the same machine loaded and are not controlled.

## Results

| Design | synth LCs | iCEcube2 LCs | synth MHz | iCEcube2 MHz | synth time | iCEcube2 time |
|---|---:|---:|---:|---:|---:|---:|
| PicoRV32 SoC | 1544 | – | 84.5 | LSE: no result in 2 h | 5 s | – |
| SERV SoC | 547 | – | 112.6 | LSE: cells not mapped | 1 s | – |
| Hazard3 SoC (Cu) | 2203 | – | 49.4 | LSE: cells not mapped | 10 s | – |
| SERV, Go Board | 580 | – | 114.3 | LSE: no result in 2 h | 1 s | – |
| Euler 10 | 7099 | – | 68.0 | does not fit | 33 s | – |
| Euler 14 | 6014 (-6%) | 6401 | 73.9 (+11%) | 66.8 | 18 s | 692 s |
| Euler 25 | 1324 (-37%) | 2100 | 73.6 (+27%) | 57.8 | 5 s | 413 s |
| Euler 31 | 988 (-8%) | 1078 | 161.8 (+2%) | 157.9 | 2 s | 314 s |
| Euler 43 | 5959 | – | 55.4 | does not fit (14,567 LUTs) | 22 s | – |
| Euler 48 | 6503 | – | 75.2 | does not fit | 20 s | – |
| Euler 57 | 2628 (-21%) | 3318 | 67.0 (+27%) | 52.7 | 4 s | 218 s |
| Euler 58 | 7381 | – | 67.2 | does not fit | 16 s | – |
| Euler 92 | 3840 (-1%) | 3878 | 93.2 (+29%) | 72.3 | 7 s | 227 s |
| Euler 97 | 1467 (-8%) | 1596 | 90.7 (+23%) | 73.9 | 3 s | 142 s |

HX8K CB132 (the Go Board's SERV: HX1K VQ100); MHz is the fastest the clock
port can run with every clock meeting timing (the Euler reporter's clock
counting at twice its Fmax; on Hazard3 the PLL's core clock); synth's
figures are the median of seeds 1-3; times are wall clock with other jobs
running. Results: [ICE40_ICECUBE2.json](ICE40_ICECUBE2.json).

- **iCEcube2 implements 6 of the 14 designs**; synth all 14 (the reasons
  are below).
- On those six, synth uses **1-37% fewer logic cells** and reaches a
  **2-29% higher Fmax** (geometric mean 19%), in 2-18 s against 142-692 s.
- Euler 31 is the closest: both flows' critical paths are the same, two
  LUTs and a 16-bit carry chain, and synth is 2.5% faster; before the LUT
  input change below its median was 156.6 MHz, 0.8% slower.
- The iCEcube2 bitstreams were checked by simulation too: decoded, Euler
  31's matches its RTL but for the initial values LSE drops (below).

## iCEcube2's synthesis

iCEcube2 synthesizes with Synplify Pro by default and also ships LSE. Both
were run (`--synthesis lse`, `--synthesis synplify`) with the same back end
and constraints: the table above is LSE's, the one below Synplify Pro's.
Synplify Pro's FlexNet license check reads the host ID of an interface
named `eth0` (`lmutil lmhostid` must print the MAC the license is locked
to); where the Ethernet interface has another name, a dummy `eth0` with
that MAC serves (`sudo ip link add eth0 type dummy && sudo ip link set eth0
address <MAC>`; it does not survive a reboot).

### With Synplify Pro

| Design | synth LCs | Synplify LCs | synth MHz | Synplify MHz: `synth time` / sbtimer |
|---|---:|---:|---:|---:|
| PicoRV32 SoC | 1544 (-16%) | 1836 | 84.5 | 69.9 / 72.7 (does not run, below) |
| SERV SoC | 547 | 526 | 112.6 | 129.5 / 129.4 (register file removed, below) |
| Hazard3 SoC (Cu) | 2203 (-28%) | 3047 | 49.4 | 45.1 / 41.7 |
| SERV, Go Board | 580 | – | 114.3 | placer: no site for a global buffer |
| Euler 10 | 7099 | – | 68.0 | RAMs as 46,080 flip-flops |
| Euler 14 | 6014 | – | 73.9 | does not fit (8,970 LUTs) |
| Euler 25 | 1324 (-23%) | 1723 | 73.6 | 62.7 / 62.5 |
| Euler 31 | 988 (-8%) | 1072 | 161.8 | 119.6 / 119.7 |
| Euler 43 | 5959 | – | 55.4 | does not fit (15,217 LUTs) |
| Euler 48 | 6503 | – | 75.2 | does not fit |
| Euler 57 | 2628 (-23%) | 3421 | 67.0 | 54.3 / 54.5 |
| Euler 58 | 7381 | – | 67.2 | does not fit (8,333 LUTs) |
| Euler 92 | 3840 | – | 93.2 | RAMs as flip-flops |
| Euler 97 | 1467 (-15%) | 1730 | 90.7 | 69.6 / 83.0 |

Results: [ICE40_ICECUBE2_SYNPLIFY.json](ICE40_ICECUBE2_SYNPLIFY.json).

- **Synplify Pro implements 7 of the 14 designs.** On the five that work
  and run, synth uses 8-28% fewer logic cells and is faster by either
  timer: by 9% (Euler 97, against sbtimer) to 35% (Euler 31).
- **SERV SoC.** Synplify Pro's netlist is 4% smaller and 15% faster than
  synth's because it is missing the CPU's register file: it removes the
  block RAM that holds it ("does not drive other instances"), and synth's
  critical path starts at that RAM. Decoded and simulated against its RTL,
  its bitstream does not run.
- **PicoRV32 SoC.** Decoded and simulated, Synplify Pro's bitstream does
  not run the firmware either (the LEDs stay 0; the RTL ends at 152). Part
  of it is that Synplify ignores register initial values and the UART line
  idles high only by its initial value; why the CPU stops is not
  established. The decoder reads every block RAM mode as iCEcube2 builds
  it (one RAM per width, built by iCEcube2 and Synplify Pro, matches its
  RTL), and Synplify Pro's Euler 25, 31, 57 and 97 bitstreams match their
  RTL but for initial values. iCEcube2's simulation models
  (`verilog/sb_ice_lc.v`) would settle it; synth's Verilog front end does
  not read them (`pulldown`, user-defined primitives).
- **Timers.** sbtimer times the clock network; `synth time`, like icetime,
  takes ideal clocks. On LSE's netlists they agree within 2%, on Synplify
  Pro's they differ by up to 19% (Euler 97), either way. synth is ahead by
  both.
- Synplify Pro, like LSE, drops non-zero register initial values (warning
  FX1039), and initializes memories only by `$readmemh`, not in loops.

### LSE

LSE fails on half the benchmark:

- **Too big.** Euler 43 maps to 14,567 LUTs (the HX8K has 7,680), and Euler
  10, 48 and 58 do not fit the device's logic blocks after packing, where
  synth fits each with 7,099, 5,959, 6,503 and 7,381 logic cells.
- **Not mapped.** On Hazard3 and on the SERV SoC, LSE leaves generic gates
  (`OR2`, `MUX21`, MachXO2 primitives) and unmapped RAM ports
  (`ClockedWritePort_*`, `ReadPort_*`) in its netlist, which iCEcube2's back
  end rejects.
- **No result.** LSE does not finish the PicoRV32 SoC or the SERV SoC for
  the Go Board within two hours (synth takes seconds).
- **Initial values.** LSE drops non-zero register initial values: decoded
  and simulated against its RTL, iCEcube2's Euler 31 bitstream holds the
  serial line low until the reset ends, where the RTL starts it at 1.

## What the comparison changed in synth

Each change was measured against the previous revision over all 14 designs
and five seeds (synth's own analysis, median by design, geometric mean).

- **LUT cascades.** A logic cell's LUT output reaches input 2 of the next
  cell in its tile directly (`lutff_<z>/lout`), with no local track or input
  multiplexer: about 0.6 ns less than the usual route, and one local track
  saved. iCEcube2 uses it (77 times on Euler 31); synth's chip database had
  the wires and its flow left them unused. The router now takes a cascade
  for the net of a combinational cell, and the placer counts it: a
  cascaded connection has no routing delay, a share of moves line a
  critical connection up from slot z to z + 1, and after annealing each
  tile's cells are ordered for the most critical connections. The delay
  estimates leave the cascade out (it reaches one input of a tile only).
- **Accumulations as adder trees.** A chain of additions in series,
  conditional or not (`if (z[t]) cnt = cnt + 1` in a loop, the shape of
  counts and sums), becomes a balanced tree of additions of its terms, each
  as narrow as its operands: a 16-bit population count is 4.4 LUT levels
  instead of 23, in 31 LUTs and 14 carries instead of 55 and 30 (LSE: 24
  levels).
- **LUT input speed in routing.** The four inputs of a LUT are not
  equally fast: `in3` is the fastest, `in0` 133 ps slower through the LUT
  and 196 ps slower to the flip-flop. The timing analysis counted it, but
  the router chose among the inputs (the LUT input permutation) as if they
  were the same; now the permutation edges of each placed cell carry the
  difference, so a critical connection takes a fast input. On Euler 31
  the critical path's last LUT went to `in0` on some seeds and `in3` on
  others: its clock port reaches 160.0 or 161.8 MHz on every seed now
  (156.6 to 160.0 before), and the change alone gains 2.1% (geometric mean).
- **Clock targets by signal.** `set_frequency slow 100` also names a clock
  made by a register by its signal's name (`slow` for `slow$dff[3]`).

Together, against the revision before (`b20a636`):

| Design | Fmax before | after | Logic cells before | after |
|---|---:|---:|---:|---:|
| PicoRV32 SoC | 78.9 MHz | 84.0 MHz (+6.5%) | 1545 | 1545 |
| SERV SoC | 92.6 MHz | 110.9 MHz (+19.8%) | 548 | 548 |
| Hazard3 SoC (Cu) | 45.7 MHz | 49.4 MHz (+8.1%) | 2204 | 2204 |
| SERV, Go Board | 97.6 MHz | 110.5 MHz (+13.2%) | 581 | 581 |
| Euler 10 | 57.4 MHz | 69.0 MHz (+20.2%) | 7311 | 7100 |
| Euler 14 | 71.1 MHz | 72.4 MHz (+1.8%) | 6015 | 6015 |
| Euler 25 | 72.0 MHz | 74.2 MHz (+3.1%) | 1324 | 1324 |
| Euler 31 | 146.7 MHz | 156.1 MHz (+6.4%) | 989 | 989 |
| Euler 43 | 53.8 MHz | 55.7 MHz (+3.5%) | 5959 | 5959 |
| Euler 48 | 73.7 MHz | 73.3 MHz (-0.5%) | 6503 | 6503 |
| Euler 57 | 63.8 MHz | 65.3 MHz (+2.4%) | 2629 | 2629 |
| Euler 58 | 64.1 MHz | 68.1 MHz (+6.2%) | 7381 | 7381 |
| Euler 92 | 89.9 MHz | 93.4 MHz (+3.9%) | 3841 | 3841 |
| Euler 97 | 83.5 MHz | 89.3 MHz (+6.9%) | 1467 | 1467 |

Fmax is the slowest clock here, every clock at 200 MHz (the median of seeds
1-5; synth's analysis). The geometric mean gains 7.1%; logic cells go down
0.2%, and only Euler 48 is slower, by 0.5%, within its spread over seeds
(73-79 MHz either way). Euler 10 gains most from the adder trees (its
critical path counts the bits of a 16-bit word), the SERV systems from the
cascades. Every netlist and bitstream still matches its RTL in simulation
(`tools/ci_sim.sh`), and the test suite passes.

## Running it

```sh
cargo build --release
tools/icecube2_ice40.py                       # every design, results in target/icecube2
tools/icecube2_ice40.py --designs euler31 serv --flows synth
tools/icecube2_ice40.py --icecube2-from target/icecube2   # synth again, iCEcube2's results kept
tools/icecube2_ice40.py --table               # the table of the last run
synth time design.bin --device hx8k --package cb132 --pcf pins.pcf --freq 200
```

`ICECUBE2` names the installation (default `~/lscc/iCEcube2.2020.12`) and
`LM_LICENSE_FILE` the license (default: the one iCEcube2's `config.xml`
names). `--goal Area`, `--effort high` and `--lut-permutation` select other
LSE and placer options; `--timeout` bounds each tool run (default 30
minutes).
