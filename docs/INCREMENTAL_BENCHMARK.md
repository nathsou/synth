# Incremental PnR benchmark

Measured in the cloud session on 2026-10-18 using `f17722558832c78f9a623ed794aa893d8850ce8a`
(Rust 1.88 release build), on an AMD EPYC 9V45 VM. This compares synth against
itself; it is not a Vivado or iCEcube2 comparison.

## Method

Run `python3 tools/bench_incremental.py --out /tmp/incremental-bench` after
`cargo build --release -p synth`. The output directory retains copied input
sources, each edited RTL variant, checkpoints, ASC files, command logs, binary
hash and machine-readable measurements. Use a new output directory for each run.

Four designs, seeds 1/2/3, with unchanged, small and structural edits. PicoRV32
also has a second structural edit. Each edit independently reuses the original
baseline checkpoint; there is no cumulative edit history. Fresh and incremental
runs have identical RTL, device, seed, pin constraints and clock targets.
Runs are sequential, and their order alternates across seeds. Wall times include
elaboration, synthesis, packing, PnR and ASC output; incremental time also includes
checkpoint reading/writing and any full-PnR fallback. Baseline checkpoint creation
is measured separately and excluded from speedup. There is a 180-second timeout
per command. No timing-failure override is used.

Targets: 50 MHz for RAM blinky and PicoRV32, 60 MHz for Euler31, 30 MHz for Euler48.
RAM blinky uses HX1K/VQ100; the others use HX8K/CB132. The Euler wrapper has two
clock domains; reported summary Fmax is the **minimum of their individual STA
Fmax values**, not a generated-clock-adjusted maximum board frequency. Raw results
retain both clock values. Timing estimates come from synth's own STA, not an
independent timing oracle or hardware measurements.

| Design | Small edit | Structural edit |
|---|---|---|
| RAM blinky | RAM initialization `i * 7 + 3` → `i * 7 + 5` | Add an 8-bit sum to the LED datapath |
| PicoRV32 SoC | UART comparison `tx_div == 7` → `tx_div == 6` | Enable barrel shifter; separately enable multiplier |
| Euler31 | Coin-sum target 200 → 199 | Reduce coin-processing stages 8 → 4 |
| Euler48 | Summation limit 1000 → 999 | Reduce modular arithmetic lanes 6 → 3 |

Source edit size is not a reliable measure of implementation change. A parameter
edit can replace substantial logic, and a constant edit can change synthesis
naming, mapping and packing far beyond the edited expression. The modified Euler
solvers intentionally solve different problems; original golden answers do not
apply to these variants.

## Results with the repair policy

Same method, edits and seeds, measured later on 2026-10-18 in a different cloud
VM (absolute times are not comparable with the original table below; compare
the fresh and incremental columns of one table). Changes since the original
implementation:

- **Skip hopeless repairs.** If fewer than 60% of the cells keep their place,
  incremental PnR gives up before routing and runs fresh PnR, instead of
  repairing for up to 30 s and falling back anyway.
- **Refine the repaired placement.** A cold, short-move annealing refines the
  units placed anew (all units, colder, when more than 5% of the cells are new)
  instead of leaving them where greedy repair put them.
- **Route around kept trees.** Repair routing starts with a high present
  congestion cost, so new routes avoid the kept ones rather than negotiate with
  them (Euler48 small: 12.5 s of repair routing became 1.5 s). Saved trees are
  matched by source and pruned to the sinks a net still has; a net that gained
  sinks grows its pruned tree. A repair stalled for 30 iterations gives up.
- **Protect QoR.** Checkpoints record each clock's period; an incremental result
  more than 5% slower on any clock than the checkpointed run is replaced by fresh
  PnR (as is one that misses a target).

[Per-run data](benchmarks/incremental-2026-10-18-repair.json).

| Design / edit | Fresh s | Incremental s | Speedup | Fresh Fmax MHz | Incremental Fmax MHz | Fallbacks |
|---|---:|---:|---:|---:|---:|---:|
| blinky_ram / unchanged | 0.069 | 0.070 | 0.98× | 192.6 | 192.6 | 0/3 |
| blinky_ram / small | 0.066 | 0.063 | 1.05× | 192.6 | 192.6 | 0/3 |
| blinky_ram / large | 0.083 | 0.077 | 1.08× | 192.6 | 192.6 | 0/3 |
| picosoc / unchanged | 7.264 | 0.774 | 9.38× | 66.7–74.3 | 66.7–74.3 | 0/3 |
| picosoc / small | 6.783 | 0.681 | 9.96× | 66.7–74.3 | 66.7–74.3 | 0/3 |
| picosoc / large | 9.927 | 10.144 | 0.98× | 60.5–70.4 | 60.5–70.4 | 3/3 |
| picosoc / multiply | 9.526 | 9.909 | 0.96× | 65.0–66.2 | 65.0–66.2 | 3/3 |
| euler31 / unchanged | 2.040 | 0.368 | 5.54× | 126.7–136.1 | 126.7–136.1 | 0/3 |
| euler31 / small | 2.075 | 1.145 | 1.81× | 117.0–136.9 | 125.7–140.7 | 0/3 |
| euler31 / large | 1.452 | 1.623 | 0.89× | 108.5–125.7 | 108.5–125.7 | 3/3 |
| euler48 / unchanged | 42.866 | 3.007 | 14.26× | 51.5–57.4 | 51.5–57.4 | 0/3 |
| euler48 / small | 40.980 | 4.090 | 10.02× | 50.6–59.1 | 53.4–57.4 | 0/3 |
| euler48 / large | 16.227 | 16.068 | 1.01× | 65.9–70.7 | 65.9–70.7 | 3/3 |

Every incremental result is now within the fresh results' seed spread or
better, and no edit is meaningfully slower than fresh PnR: structural edits that
keep few cells cost 0.9–1.0× a fresh build (the attempt is cheap before the
fallback). Euler48 small edits went from 3.15× to 10×, Euler31 small edits keep
their 1.8× without the former 28–33% Fmax loss, and PicoRV32 structural edits
went from 0.23–0.44× to 0.96–0.98×.

## Original implementation

### Results

All **90 builds succeeded and met their targets**. All 12 unchanged incremental
bitstreams were byte-identical to the corresponding fresh output. Packed resource
counts matched between every fresh/incremental pair. This does **not** mean all
functional checks passed: both Euler designs show UART mismatches in fresh and incremental builds
(see validation below).

[Per-run data](benchmarks/incremental-2026-10-18.json). Times are medians over three
seeds; Fmax columns give the range across those seeds.

| Design / edit | Fresh s | Incremental s | Speedup | Fresh Fmax MHz | Incremental Fmax MHz | Fallbacks |
|---|---:|---:|---:|---:|---:|---:|
| blinky_ram / unchanged | 0.035 | 0.033 | 1.07× | 192.6 | 192.6 | 0/3 |
| blinky_ram / small | 0.039 | 0.030 | 1.32× | 192.6 | 192.6 | 0/3 |
| blinky_ram / large | 0.042 | 0.038 | 1.10× | 192.6 | 192.6 | 0/3 |
| picosoc / unchanged | 3.889 | 0.364 | 10.69× | 66.7–74.3 | 66.7–74.3 | 0/3 |
| picosoc / small | 3.747 | 0.367 | 10.20× | 66.7–74.3 | 66.7–74.3 | 0/3 |
| picosoc / large | 5.384 | 23.667 | 0.23× | 60.5–70.4 | 60.5–70.4 | 3/3 |
| picosoc / multiply | 5.230 | 11.968 | 0.44× | 65.0–66.2 | 51.8–66.2 | 2/3 |
| euler31 / unchanged | 1.112 | 0.183 | 6.08× | 126.7–136.1 | 126.7–136.1 | 0/3 |
| euler31 / small | 1.100 | 0.600 | 1.83× | 117.0–136.9 | 84.2–91.9 | 0/3 |
| euler31 / large | 0.800 | 0.676 | 1.18× | 108.5–125.7 | 86.1–94.1 | 0/3 |
| euler48 / unchanged | 18.913 | 0.961 | 19.68× | 47.7–56.3 | 47.7–56.3 | 0/3 |
| euler48 / small | 17.131 | 5.446 | 3.15× | 55.9–57.0 | 52.5–56.3 | 0/3 |
| euler48 / large | 7.807 | 23.058 | 0.34× | 62.6–65.9 | 32.7–65.9 | 2/3 |

| Design / edit | LC | RAM | Reused cells (seed 1) | Reused routes (seed 1) |
|---|---:|---:|---:|---:|
| blinky_ram / unchanged | 44 | 1 | 45/45 | 77/77 |
| blinky_ram / small | 44 | 1 | 45/45 | 77/77 |
| blinky_ram / large | 44 | 1 | 32/45 | 28/84 |
| picosoc / unchanged | 1592 | 12 | 1604/1604 | 1800/1800 |
| picosoc / small | 1592 | 12 | 1604/1604 | 1800/1800 |
| picosoc / large | 1848 | 12 | 639/1860 | 182/2054 |
| picosoc / multiply | 2061 | 12 | 252/2073 | 106/2382 |
| euler31 / unchanged | 1011 | 12 | 1023/1023 | 1426/1426 |
| euler31 / small | 1009 | 12 | 846/1021 | 928/1424 |
| euler31 / large | 767 | 4 | 167/771 | 196/1014 |
| euler48 / unchanged | 6677 | 0 | 6677/6677 | 9443/9443 |
| euler48 / small | 6676 | 0 | 6674/6676 | 9283/9442 |
| euler48 / large | 3692 | 0 | 718/3692 | 845/5195 |

The substantial changes grow PicoRV32 from 1,592 LC to 1,848 LC (barrel
shifter, +16%) or 2,061 LC (multiplier, +29%). Euler31 shrinks from 1,011 LC /
12 RAM to 767 LC / 4 RAM; Euler48 shrinks from 6,677 to 3,692 LC (−45%).
The added blinky adder changes connectivity and reuse but leaves packed LC count
unchanged. PnR reuse does not improve area here because synthesis and packing
remain identical between each pair.

Fallback protects requested timing, but can erase the speed benefit. PicoRV32
barrel-shifter edits fall back on all three seeds; multiplier edits on two.
Euler48 lane reduction falls back on two seeds. For the accepted seed-1 Euler48
repair, Fmax is 32.7 MHz versus 64.4 MHz fresh (−49%), despite meeting 30 MHz.
Euler31 small edits lose roughly 28–33% of the slowest-domain Fmax against their
paired fresh runs while still meeting 60 MHz. PicoRV32 multiplier seed 3 accepts
51.8 MHz versus 65.0 MHz fresh. Conversely, small PicoRV32 and RAM initialization
edits preserve the same timing, and Euler48 small edits stay within 0.1–3.4 MHz
of fresh results.


Cell reuse counts accepted legal placements; route reuse counts trees accepted
**before repair**, including attempts later discarded by fallback. These are not
percentages guaranteed to survive in the final output. Speedup is median fresh
wall time divided by median incremental wall time; below 1 means a slowdown.
Each seed/case has one timed run, so these are exploratory measurements, not
confidence intervals. The 30–40 ms RAM-blinky runs are especially sensitive to
measurement noise.

## Functional validation

Use `python3 tools/bench_incremental_bitsim.py --out /tmp/incremental-bench`
with IceStorm, Icarus Verilog and Yosys cell models installed. The checker uses
the saved edited RTL, not the original Euler golden answers. It continues after
mismatches and exits nonzero if any case fails. These runs were separate from
timing measurements, so simulator activity did not affect benchmark runtimes.

| Seed-1 edited outputs | Result | Cycles per case |
|---|---|---:|
| RAM blinky: small / added logic | Both pass | 10,000 |
| PicoRV32: UART / barrel shifter / multiplier | All three pass | 200,000 |
| Euler31: small edit | UART mismatch | 100,000 |
| Euler31: unchanged / small / large, fresh **and** incremental | All six show UART mismatches | 2,000 |
| Euler48: small / large, fresh **and** incremental | All four show UART mismatches | 60,000 |

Euler31's unchanged and small-edited fresh/incremental outputs first disagree
with RTL at cycle 392; the large-edited pair first disagrees at cycle 388. Each
2,000-cycle run reports 16 mismatches. Euler48's small-edited pair first disagrees
at cycle 22,932 and reports 6,304 mismatches over 60,000 cycles; its large-edited
pair first disagrees at cycle 45,506 and reports 1,808 mismatches. The discrepancy
therefore exists without incremental reuse. Its root cause is unresolved; these
Euler measurements must not be presented as functionally validated results.

Reproduce the fresh-flow controls:

```sh
python3 tools/bench_incremental_bitsim.py --out /tmp/incremental-bench \
  --designs euler31 --edits unchanged small large --modes fresh incremental --cycles 2000
python3 tools/bench_incremental_bitsim.py --out /tmp/incremental-bench \
  --designs euler48 --modes fresh incremental --cycles 60000
```

[Validation output](benchmarks/incremental-validation-2026-10-18.txt) records
passes and failures. Simulation checks finite board-visible behavior, not formal
equivalence. PicoRV32 uses the existing firmware, so its passing multiplier
configuration does not establish coverage of newly enabled multiply instructions.

## What to improve next

First resolve the Euler board-level simulation discrepancies. Reproduce it in the
unchanged fresh flow, compare the synthesized netlist and decoded bitstream with
the RTL, and inspect reset/generated-clock behavior before attributing it to a
specific pass. The benchmark does not establish the root cause.


Bounding repair cost, local placement repair and a timing-margin policy (items 1–3
of the original list) are done; see "Results with the repair policy". Still open:

1. **Tune the policy on more edit histories.** The 60% reuse floor, 5% wide-edit
   threshold and 5% slowdown tolerance are set from these four designs; cumulative
   edit sequences (each from the previous checkpoint) are not benchmarked yet.
2. **Improve matching across synthesis changes.** Match with stable hierarchical
   provenance and structural signatures rather than relying primarily on generated
   names. First instrument why candidates fail to match or fail placement legality;
   the observed reuse percentages alone do not prove naming is the only cause.
   (For Euler48's small edit, synthesis is already stable: 11,540 of 11,551
   cells keep their names and parameters, and all but 13 keep their
   connections, by driver name.)
3. **Cache synthesis separately.** Even perfect PnR reuse still reruns synthesis.
   Add module/cone-level caching with dependency-aware invalidation after the
   PnR repair policy is robust. Continue to check runtime, area, timing and
   functional correctness together.

These results support incremental PnR for unchanged and well-localized edits.
They do not support a blanket speedup claim for architectural changes, nor a
claim of equal fresh-build Fmax merely because the requested target is met.
