> Historical experiment: automatic threshold selection was withdrawn after review. The current default again uses one synthesis pass with carry threshold 3. The measurements below describe the withdrawn policy.

# Default QoR: carry mapping and placement experiments

The default now explores carry thresholds 3 and 8 before place-and-route. It accepts threshold 8 only when the mapped critical path is **strictly shallower** and the packed logic-cell count does not increase. Otherwise it retains the original netlist. RTL simplification and memory mapping are shared; only one netlist is placed and routed. `--carry-min-width 3` forces the previous policy, and an explicit threshold disables exploration.

This keeps the large arithmetic wins without applying a fixed threshold to every design. The mapping estimate is a selection heuristic, not a guarantee of routed timing. Small area savings with unchanged estimated depth are deliberately insufficient: their placement perturbation caused the most persistent regressions.

## Measurement contract

Baseline source: `a1f312d9ff67e8147789877dbe06554a4cd85334`. The PR is based on Weft-enabled main `47a49156711437ea23d341d54f18e43352d26238`; control builds on PicoSoC, Euler25 and Euler31 matched the original baseline bitstreams. Frozen benchmark RTL and PCFs are unchanged. Targets are HX8K CB132, except Go Board SERV on HX1K VQ100. Every full PnR run uses 200 MHz, permits timing failure, and uses no incremental checkpoint. Fmax is measured by the same carry-feedout-patched icetime, not by different tools' internal timing estimates. No hardware Fmax measurements are claimed.

The follow-up matrix contains 233 run conditions (215 new runs and 18 reused controls), with 0 pending and 0 failed. The earlier study had 266 attempts and one nextpnr placement timeout. [Raw follow-up measurements](qor/followup-results.csv) include baseline and fixed-8 controls. Screening wall times share CPUs and are not controlled speed comparisons.

## Default policy: three-seed validation

Each row is the median of seeds 1, 2 and 3. Cell counts do not vary by seed.

| Design | Fixed 3 MHz | Automatic MHz | Change | Fixed 3 LCs | Automatic LCs |
|---|---:|---:|---:|---:|---:|
| picosoc | 69.38 | 69.38 | +0.0% | 1592 | 1592 |
| serv | 85.31 | 85.31 | +0.0% | 548 | 548 |
| cu_hazard3 | 40.79 | 40.79 | +0.0% | 2237 | 2237 |
| go_serv | 92.69 | 92.69 | +0.0% | 581 | 581 |
| euler14 | 62.47 | 62.47 | +0.0% | 6161 | 6161 |
| euler25 | 53.68 | 69.88 | +30.2% | 1390 | 1376 |
| euler31 | 137.59 | 137.59 | +0.0% | 1011 | 1011 |
| euler43 | 43.98 | 51.71 | +17.6% | 6235 | 5871 |
| euler48 | 62.37 | 62.37 | +0.0% | 6704 | 6704 |
| euler57 | 57.95 | 57.95 | +0.0% | 2719 | 2719 |
| euler58 | 57.70 | 57.70 | +0.0% | 7423 | 7423 |
| euler92 | 79.69 | 86.77 | +8.9% | 3928 | 3836 |
| euler97 | 76.36 | 75.51 | -1.1% | 1509 | 1490 |

Geometric mean across 13 designs: **Fmax +3.92%**, **packed LCs -0.82%**. The selected threshold-8 designs are Euler25, Euler43, Euler92 and Euler97. All other designs retain bit-identical baseline outputs across the three seeds.

## Additional seeds not used to select the policy

The four changed designs were run on additional seeds. These are direct automatic-policy builds, not best-seed selection.

| Design | Seeds | Fixed 3 median MHz | Automatic median MHz | Change |
|---|---|---:|---:|---:|
| euler25 | 4–6 | 53.88 | 69.92 | +29.8% |
| euler43 | 4–6 | 44.15 | 52.82 | +19.6% |
| euler92 | 4–6 | 74.06 | 85.21 | +15.1% |
| euler97 | 4–8 | 74.10 | 78.63 | +6.1% |

Across all six measured seeds, Euler25 improves 29.97%, Euler43 19.68% and Euler92 10.95%. Across eight seeds, Euler97 improves 3.91%. These small samples describe this benchmark set; they do not establish universal gains.

## Why a fixed threshold of 8 regresses

| Design | Fixed 3, eight-seed median MHz | Fixed 8, eight-seed median MHz | Change |
|---|---:|---:|---:|
| cu_hazard3 | 41.52 | 40.36 | -2.8% |
| euler31 | 133.10 | 127.11 | -4.5% |
| euler97 | 74.44 | 77.35 | +3.9% |

- **Euler31:** the worst icetime path is in the half-rate UART reporter, not the solver's 17-bit arithmetic pipeline. Across eight seeds, the native critical-path reports give median reporter routing delay 5.19 → 5.67 ns, while median cell delay stays roughly flat at 2.23 → 2.21 ns. Mapping depth is unchanged at 4.72 LUT levels and packed cells fall only 1011 → 988. The new policy rejects this area-only change. The original unscaled icetime metric is retained for fair comparison; reporter timing is not silently excluded to improve the score.
- **Hazard3:** the mapped critical depth increases 17.00 → 17.15 despite cells falling 2237 → 2224. In seed 1, the critical path changes from 8 LUT outputs and 58 carry outputs to 13 LUT outputs and 29 carry outputs; its routing component grows 11.05 → 14.26 ns. This is a change in the selected critical path, not proof that the same path had 29 carry cells removed. Restricting the threshold change to variable-operand adders, or preserving increment/decrement chains, reproduces the baseline bitstream; preserving those chains reproduces it for all eight seeds. Small counter remapping can perturb global optimization, packing and placement, including paths outside the modified counter. The new policy rejects the deeper candidate.
- **Euler97:** the initial −1.1% three-seed result was not stable. Eight seeds give +3.9%; the mapped estimate improves 9.84 → 9.00 LUT levels. The automatic policy keeps this candidate. This is why the small three-seed loss is reported rather than used to special-case the design.

The large wins have a different signature: Euler25 reduces mapped depth 12.19 → 10.61 and carry feed-in cells 50 → 8; Euler43 reduces depth 15.71 → 15.00 and packed cells 6235 → 5871. The default evaluates these properties instead of recognizing benchmark names.

## Further optimization attempts

All rows in this table compare seed 1 with seed 1 on all 13 designs. They are screening results, not three-seed default claims.

| Variant | Geometric-mean Fmax change | Worst design change |
|---|---:|---:|
| weight75 | +0.35% | -21.27% |
| exp12 | +0.61% | -11.55% |
| weight75exp12 | +0.93% | -5.88% |
| keepinc8 | +3.40% | -9.92% |
| polish4 | +0.93% | -0.35% |

- `weight75`: placement timing weight 0.75 instead of 0.5. Its promising focus-set gains did not generalize: Euler31 loses 21.3% in seed 1.
- `exp12`: criticality exponent 12 instead of 8; the combined `weight75exp12` also remains experimental.
- `const8` / `var8`: change only constant-operand or variable-operand adders. The variable-only rule restores Hazard3/Euler31 but loses the Euler43 gain and increases Euler25 area. `keepinc8` preserves increment/decrement chains; it restores Hazard3 but worsens Euler31's eight-seed median to 123.04 MHz versus 133.10 MHz. No one operand-type rule solves the suite.
- `polish4`: after normal annealing, refresh criticalities between four additional greedy passes and keep only a lower estimated worst period. Euler31 improves in seed 1, but its eight-seed median rises only 0.70%, with a 7.4% seed-2 loss. Neither this nor its timing-weight/carry combinations was promoted to the default.

Combining fixed carry width 8 with timing weight 0.75 was also tested. These are single-seed comparisons against fixed width 8 at the normal weight, not against the automatic default.

| Design | Carry8 MHz | Carry8 + weight0.75 MHz | Change |
|---|---:|---:|---:|
| picosoc | 72.89 | 69.85 | -4.2% |
| euler25 | 72.37 | 70.33 | -2.8% |
| euler43 | 52.94 | 53.50 | +1.1% |
| euler57 | 61.40 | 59.95 | -2.4% |
| euler92 | 86.82 | 88.16 | +1.5% |

The combined settings have mixed effects. They are useful design-specific trials, not evidence for a global default change.

## Isolated runtime and final-binary verification

One serial paired build per design, seed 1, on an otherwise idle four-CPU container. Both sides use the final PR binary: fixed threshold 3 versus automatic selection. This is a controlled snapshot, not a multi-repetition runtime estimate. The final implementation shares RTL preparation between candidates. Every ASC is checked against the prior benchmark result.

| Design | Fixed 3 seconds | Automatic seconds | Change |
|---|---:|---:|---:|
| picosoc | 6.74 | 6.94 | +3.0% |
| serv | 1.67 | 1.97 | +18.0% |
| cu_hazard3 | 12.86 | 15.37 | +19.5% |
| go_serv | 1.42 | 1.57 | +10.6% |
| euler14 | 33.35 | 34.03 | +2.0% |
| euler25 | 2.47 | 5.23 | +111.7% |
| euler31 | 1.87 | 2.07 | +10.8% |
| euler43 | 38.81 | 45.37 | +16.9% |
| euler48 | 40.00 | 46.09 | +15.2% |
| euler57 | 6.64 | 8.39 | +26.5% |
| euler58 | 55.86 | 63.89 | +14.4% |
| euler92 | 10.65 | 12.46 | +17.0% |
| euler97 | 3.93 | 4.43 | +12.8% |

All 26 final-binary bitstreams match their earlier measurements. Total suite build time 216.3 → 247.8 seconds (+14.6%); geometric-mean per-design build time +19.3%. Use `--carry-min-width 3` to retain the old synthesis budget.

## Validation and reproduction

- 160 workspace tests pass, including the new automatic BCD candidate selection test, explicit override checks, invalid-width checks, and incremental checkpoint isolation/reuse.
- Euler25, Euler43, Euler92 and Euler97 pass source-versus-mapped simulation with the expected answers and cycle counts. Prior fixed-threshold CPU/Euler and decoded-bitstream probes also passed; final bitstream identity connects those results to the selected candidates.
- Formatting and diff checks pass. Clippy with warnings denied passes for the changed `arch-ice40` and `synth` crates using `--no-deps`. Whole-workspace clippy is blocked by three pre-existing `only_used_in_recursion` warnings in Weft (`elab/expr.rs:368`, `elab/pattern.rs:260`, `types.rs:174`), all unchanged from current main.
- Automatic checkpoints use a new policy namespace; explicit thresholds remain separate. No timing targets or benchmark RTL were changed.

Build the PR and run the comparator with the patched timer configured:

```sh
cargo build --release
export ICETIME=/path/to/patched/icetime
export ICETIME_CHIPDB=/path/to/icestorm/chipdb
python3 tools/qor_compare.py --synth target/release/synth \
  --seeds 1,2,3 --out work/default-qor
# Force the fixed-8 candidate instead:
python3 tools/qor_compare.py --synth target/release/synth \
  --candidate-flags="--carry-min-width 8" --out work/fixed8-qor
```

The comparator defaults to serial execution, retains commands, binary hashes, ASC hashes and full logs, and records failures rather than dropping them. `--workers` permits faster QoR screening but invalidates controlled wall-time comparisons. The frozen input hashes are in `tools/qor-sources.txt`; tool provenance is in [the manifest](qor/manifest.json).

To reproduce the rejected policies, apply [the experimental patch](qor/experimental-policies.patch) in a separate checkout of this PR and rebuild. Keep `--candidate-flags="--carry-min-width 3"` to bypass automatic exploration, then use `--candidate-env QOR_ADDER_POLICY=const8`, `var8`, or `keepinc8`; `QOR_ADD_MIN=8` or `QOR_CMP_MIN=10`; `QOR_CRIT_EXP=12`; or `QOR_POLISH=4`. Timing-weight combinations use the normal `--timing-weight 0.75` CLI flag. The patch is a development harness and is not applied to production code. When using the development-only `SYNTH_DUMP_AIG` aid, force a carry width to identify the dumped candidate; automatic exploration visits both candidates and the last dump is the threshold-8 graph.
