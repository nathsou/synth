# Carry mapping by LUT input support: QoR experiments

Updated from main `67504cb`, including the CI cost optimizations and synthesis-observer API. The previous automatic 3-versus-8 threshold policy has been removed.

The proposed change lowers a carry box into combinational logic when its distinct, nonconstant boundary signals fit within the target LUT input count. On iCE40 that count is four. Constants consume no input, and opposite polarities of the same signal share one input. Thus every sum/carry output can be implemented in one LUT; the ordinary mapper can merge it with surrounding logic. Larger-support boxes retain dedicated carry hardware.

This runs inside the existing simplification loop, with one synthesis and one PnR flow. It adds no alternative-netlist selection, width sweep, benchmark-specific rule, or placement tuning. The four-input bound comes from the architecture. It is a local implementation opportunity, not a guarantee that global placement or Fmax improves.

## Measurement contract

13 frozen board designs, identical RTL/PCFs, matched 200 MHz targets, timing failures permitted, independent full PnR runs, and the same carry-feedout-patched icetime. All 13 new-main baseline seed-1 bitstreams exactly match the earlier baseline. Existing baseline runs are reused where available; the raw CSV identifies every reused row. Screening jobs share CPUs, so their wall times are not controlled performance measurements. No hardware Fmax measurements are claimed.

## Candidate results

### Original seeds 1–3

Each Fmax is the median for the stated seeds. Cell counts do not vary by seed.

| Design | Baseline MHz | LUT-sized carry MHz | Change | Baseline LCs | Candidate LCs |
|---|---:|---:|---:|---:|---:|
| picosoc | 69.38 | 69.75 | +0.53% | 1592 | 1579 |
| serv | 85.31 | 85.52 | +0.25% | 548 | 548 |
| cu_hazard3 | 40.79 | 42.98 | +5.37% | 2237 | 2214 |
| go_serv | 92.69 | 94.47 | +1.92% | 581 | 581 |
| euler14 | 62.47 | 60.74 | -2.77% | 6161 | 6015 |
| euler25 | 53.68 | 66.71 | +24.27% | 1390 | 1324 |
| euler31 | 137.59 | 127.16 | -7.58% | 1011 | 989 |
| euler43 | 43.98 | 49.26 | +12.01% | 6235 | 5959 |
| euler48 | 62.37 | 61.21 | -1.86% | 6704 | 6503 |
| euler57 | 57.95 | 60.59 | +4.56% | 2719 | 2629 |
| euler58 | 57.70 | 60.87 | +5.49% | 7423 | 7381 |
| euler92 | 79.69 | 85.72 | +7.57% | 3928 | 3841 |
| euler97 | 76.36 | 80.54 | +5.47% | 1509 | 1467 |

Geometric mean over 13 complete designs: **Fmax +3.99%**, **packed cells -2.12%**.

### Additional seeds 4–6

Each Fmax is the median for the stated seeds. Cell counts do not vary by seed.

| Design | Baseline MHz | LUT-sized carry MHz | Change | Baseline LCs | Candidate LCs |
|---|---:|---:|---:|---:|---:|
| picosoc | 75.99 | 65.01 | -14.45% | 1592 | 1579 |
| serv | 87.51 | 84.46 | -3.49% | 548 | 548 |
| cu_hazard3 | 42.38 | 43.91 | +3.61% | 2237 | 2214 |
| go_serv | 90.69 | 90.80 | +0.12% | 581 | 581 |
| euler14 | 61.37 | 62.31 | +1.53% | 6161 | 6015 |
| euler25 | 53.88 | 65.70 | +21.94% | 1390 | 1324 |
| euler31 | 133.97 | 125.48 | -6.34% | 1011 | 989 |
| euler43 | 44.15 | 50.57 | +14.54% | 6235 | 5959 |
| euler48 | 59.65 | 66.74 | +11.89% | 6704 | 6503 |
| euler57 | 58.96 | 60.59 | +2.76% | 2719 | 2629 |
| euler58 | 57.91 | 59.18 | +2.19% | 7423 | 7381 |
| euler92 | 74.06 | 82.93 | +11.98% | 3928 | 3841 |
| euler97 | 74.37 | 78.81 | +5.97% | 1509 | 1467 |

Geometric mean over 13 complete designs: **Fmax +3.61%**, **packed cells -2.12%**.

### All six seeds

Each Fmax is the median for the stated seeds. Cell counts do not vary by seed.

| Design | Baseline MHz | LUT-sized carry MHz | Change | Baseline LCs | Candidate LCs |
|---|---:|---:|---:|---:|---:|
| picosoc | 70.62 | 66.94 | -5.22% | 1592 | 1579 |
| serv | 85.57 | 84.88 | -0.80% | 548 | 548 |
| cu_hazard3 | 41.16 | 43.62 | +6.00% | 2237 | 2214 |
| go_serv | 91.69 | 92.84 | +1.25% | 581 | 581 |
| euler14 | 61.92 | 61.53 | -0.64% | 6161 | 6015 |
| euler25 | 53.78 | 66.20 | +23.10% | 1390 | 1324 |
| euler31 | 134.48 | 126.32 | -6.07% | 1011 | 989 |
| euler43 | 44.04 | 49.79 | +13.06% | 6235 | 5959 |
| euler48 | 60.88 | 66.22 | +8.77% | 6704 | 6503 |
| euler57 | 58.45 | 60.59 | +3.65% | 2719 | 2629 |
| euler58 | 57.80 | 59.88 | +3.59% | 7423 | 7381 |
| euler92 | 77.50 | 83.50 | +7.74% | 3928 | 3841 |
| euler97 | 75.28 | 79.68 | +5.85% | 1509 | 1467 |

Geometric mean over 13 complete designs: **Fmax +4.38%**, **packed cells -2.12%**.

## Other clean approaches tested

Each screening result below uses seed 1 on all 13 designs; these are not six-seed claims. All prototypes were evaluated separately against the unchanged baseline.

| Prototype | Mechanism | Geomean Fmax | Geomean LCs | Worst Fmax change |
|---|---|---:|---:|---:|
| [entry](qor/entry.patch) | Preserve the carry-generating first bit, avoiding a fabric feed-in | -0.99% | -0.42% | -22.61% |
| [support](qor/support.patch) | Remove functionally redundant variables from LUT cuts | -0.32% | +0.01% | -11.08% |
| [timing](qor/timing.patch) | Exclude LUT input timing arcs absent from the truth table | +0.27% | +0.00% | -4.24% |
| [entrydelay](qor/entrydelay.patch) | Account for carry feed-in cell delay in the mapper | +0.02% | +0.00% | -0.63% |

Preserving carry entry and minimizing LUT supports produce significant individual regressions and do not improve the suite aggregate in this screen. The timing fixes have small effects and were not bundled into the carry-mapping proposal. Their mathematical/timing regression tests pass, but correctness alone does not establish a routed QoR improvement.

## Interpretation and remaining regressions

For Euler25, 77 small-support carry boxes become ordinary logic. Carry feed-in cells fall from 50 to 11, packed cells fall from 1390 to 1324, and mapped depth falls from 12.19 to 10.30 LUT levels. This recovers much of the earlier arithmetic gain without changing the global carry-width threshold.

All 13 designs use no more packed cells, but the change still perturbs packing and placement. In the original three-seed sample Euler31 regresses; its previously diagnosed limiting path is in the half-rate UART reporter. Euler48 has a large seed-1 loss which becomes smaller in the three-seed median. The tables retain all losses and additional-seed results; no design or seed is selected away. A local LUT implementation bound is not a routed-timing proof.

Six-seed critical-path diagnosis is retained in [the path CSV](qor/clean-regression-paths.csv). Euler31 keeps the same 4.72-level mapped estimate; median native critical-path routing delay rises from 5.165 to 5.605 ns, while cell delay changes only 2.200 to 2.245 ns. The limiting clock remains the half-rate reporter clock. PicoSoC also keeps its 8.26-level estimate, but median routing delay rises 7.590 → 7.885 ns and cell delay 6.655 → 7.045 ns. These are independently computed medians of the worst native paths, which may differ between builds; they are not measurements of one identical path. This supports treating the remaining regressions as limitations of local mapping and global placement, rather than claiming that a one-LUT implementation guarantees better routed timing.

## Runtime and final-binary verification

One serial paired build per design, seed 1, after benchmark/compiler/simulation queues drained. Both binaries use their defaults. This is a controlled snapshot, not a repeated runtime estimate.

| Design | Baseline seconds | Candidate seconds | Change |
|---|---:|---:|---:|
| picosoc | 5.83 | 6.39 | +9.5% |
| serv | 1.52 | 1.52 | -0.1% |
| cu_hazard3 | 12.10 | 11.15 | -7.9% |
| go_serv | 1.42 | 1.32 | -7.1% |
| euler14 | 31.77 | 31.56 | -0.7% |
| euler25 | 2.57 | 2.87 | +11.7% |
| euler31 | 1.97 | 2.02 | +2.6% |
| euler43 | 40.81 | 38.39 | -5.9% |
| euler48 | 43.73 | 36.53 | -16.5% |
| euler57 | 6.88 | 6.99 | +1.5% |
| euler58 | 69.22 | 71.04 | +2.6% |
| euler92 | 11.60 | 13.97 | +20.4% |
| euler97 | 4.33 | 4.03 | -6.9% |

Total suite build time: 233.8 → 227.8 seconds (-2.6%); geometric-mean per-design time -0.2%. All 26 final-binary ASC hashes match their study measurements.

## Validation

- 172 workspace tests passed; after adding a further variable-carry/carry-only case, all 35 logic tests passed (173 distinct tests in total). Exhaustive carry tests cover sums, carry-out, constants, repeated/complemented operands, variable carry-in, and retaining boxes whose support exceeds the LUT size.
- A dedicated arithmetic probe exercises six converted boxes and passes decoded-bitstream versus RTL simulation for 10,000 cycles with observed output transitions.
- Seven benchmark Euler solvers (25/31/43/48/57/92/97) pass source-versus-mapped simulation with exact answers and cycle counts. Complete-answer simulation does not cover the long-running 14/58 solvers or Euler10, which does not fit the benchmark target.
- PicoSoC (50,000 cycles), SERV (200,000), DarkRISCV (100,000), and Hazard3 (20,000) pass mapped-netlist differential simulation. Changed-crate clippy with warnings denied, formatting, and diff checks pass.

## Reproduction

Build baseline commit `b227bf4` and the final PR in separate worktrees. The comparator requires both binaries, since a fixed carry-width override does not disable the new local optimization. Configure the same patched timer used in the earlier study:

```sh
export ICETIME=/path/to/patched/icetime
export ICETIME_CHIPDB=/path/to/icestorm/chipdb
python3 tools/qor_compare.py --baseline-synth /path/to/baseline/synth \
  --synth target/release/synth --seeds 1,2,3,4,5,6 --out work/clean-qor
```

The comparator retains commands, binary hashes, ASC hashes and logs, and reports failures. It defaults to serial execution. Reproduce each rejected prototype by applying its linked patch with `git apply --unidiff-zero` independently to baseline commit `b227bf4`, rebuilding, and comparing against the baseline binary. The patches are development artifacts, not applied production code. Frozen source hashes are in `tools/qor-sources.txt`.

Raw measurements: [CSV](qor/clean-results.csv), [tool and binary provenance](qor/manifest.json). The [earlier threshold study](https://github.com/nathsou/synth/blob/7a2024c48efc74ed6a0bf2d9668468c8c969b761/docs/QOR_EXPERIMENTS.md) remains available in history and describes the withdrawn implementation.
