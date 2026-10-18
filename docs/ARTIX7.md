# Artix-7 native synthesis foundation

This change is a **draft foundation**, not the requested complete native
Artix-7 flow. It cannot produce an Artix-7 bitstream, and it does not establish
QoR parity with Yosys. The full native backend remains outstanding.

## Available functionality

```sh
cargo build --release -p synth
target/release/synth synth design.v --top top --family artix7 --verilog mapped.v
```

`xc7` aliases `artix7`; the default remains `ice40`. The existing Verilog
frontend, defines, include paths, top parameters and `--rename` work with
both synthesis families. Only the `synth` command accepts `--family`.

The new `arch-xc7` crate emits:

- LUT1–LUT6 with full 64-bit truth tables, output polarity selection and
  folding of operand logic into carry-propagate LUTs.
- CARRY4 groups for add/subtract/compare operations, including partial groups
  and carry outputs. Initial carry uses CYINIT; cascaded groups use CI.
- FDRE, FDSE, FDCE and FDPE, with clock inversion, INIT and enable/reset
  priority preserved. Enable-priority synchronous resets are gated before
  mapping because Xilinx reset/preset overrides CE.
- RAMB18E1 for memories with synchronous reads and at most one write port.
  Widths 1, 2, 4, 9 and 18 use data and parity initialization; width/depth
  splitting, read-port replication, independent bit enables, read enables,
  falling-edge clocks, nonzero address offsets and read-register initialization
  are supported. Same-clock transparency is implemented with bypass logic.
  Out-of-range writes are disabled rather than aliased into a valid word.
  Other memories fall back to registers and logic.

An address-register absorption guard prevents changing asynchronous reads
when writes use a different clock. Such reads remain asynchronous instead of
being updated only on the address clock.

AIG optimization, structural choices, carry-aware LUT mapping, remapping and
SAT-based don't-care optimization now live in `synth-logic::mapping`. Backends
provide carry timing and emitted-netlist cost callbacks. The carry timing
weights for Artix-7 are **mapping heuristics, not calibrated device delays**;
the printed LUT-level delay is not an Fmax prediction.

## Validation

```sh
cargo fmt --all --check
RUSTFLAGS='-D warnings' cargo clippy --workspace --all-targets
cargo test --workspace
tools/fetch_xc7_models.sh /tmp/xc7-unisim
XILINX_UNISIM=/tmp/xc7-unisim python3 tools/ci_artix7.py --out /tmp/xc7-validation
python3 tools/qor_artix7.py --out /tmp/xc7-qor
```

Yosys and Icarus are development/CI oracles only. `YOSYS_DATDIR` selects the
Yosys model directory; `SYNTH` selects the compiled binary. The Xilinx RAMB18E1
model is downloaded from `Xilinx/XilinxUnisimLibrary` at commit
`1c8e05fd1e9a79ceb8b996a0996674122eed086f` (Apache-2.0); it is not vendored or
shipped with synth. Its GSR startup is exercised in simulation.

The test harness checks combinational equivalence against Yosys's Xilinx cell
models at arithmetic widths 1, 2, 3, 4, 5, 8, 9, 17 and 33, and all LUT6 truth
table inputs. Differential tests cover both clock edges, reset/enable
priorities, asynchronous controls and initialization. RAM tests compare
against Xilinx's model across all five width modes, parity, bank and column
splits, read-port replication, masks, collision behavior, initial read values,
address-register transparency and out-of-range writes.

Seven Euler solvers (25, 31, 43, 48, 57, 92 and 97) are checked against their
known answers and exact cycle counts. `--long` additionally enables Euler 10,
whose large gate-level simulation can take many minutes. Euler 14 and 58 are
excluded from this simulation run because of their long cycle counts; they
remain in the synthesis QoR corpus. `--section primitives` and `--section euler`
select independent verification groups. CI retains the generated netlists
and reports. Deterministic emission, the `xc7` alias, unknown-family errors,
and the default iCE40 target are also checked.

## QoR methodology and current result

`tools/qor_artix7.py` synthesizes every solver in the existing Euler manifest.
Both flows receive identical source files and top modules and flattened
hierarchy. The reference is `synth_xilinx -family xc7 -flatten -noiopad
-noclkbuf`, with other defaults retained: DSP, RAM, SRL and wide mux inference
are enabled. No baseline optimizations are disabled to improve the comparison.

Standalone Yosys INV cells count as LUTs, just as synth's LUT1 inverters do.
RAMB36E1 counts as two RAMB18E1 equivalents. FF, carry, DSP, LUTRAM, SRL and
MUXF7/F8 cell counts are retained separately in `results.json`; LUT counts
alone do not describe total resource use. Tool versions, source SHA-256 hashes,
netlists, reports and single-run wall times are recorded. Wall times on a
shared development machine are indicative, not a controlled speed benchmark.
No placed area, routing success or Fmax is claimed.

Measured on 2026-10-18 with Yosys 0.52 (`fee39a3284c90249e1d9684cf6944ffbbcbb8f90`) and the PR working tree based on `a1f312d`. Each resource column is **synth / Yosys**. Full cell counts, input hashes and wall times are in [ARTIX7_QOR.json](ARTIX7_QOR.json).

| Design | LUTs incl. INV | FFs | CARRY4 | BRAM18 equivalents | DSP48 |
|---|---:|---:|---:|---:|---:|
| Euler 14 | 2974 / 3222 | 2214 / 2201 | 494 / 592 | 4 / 4 | 0 / 0 |
| Euler 25 | 549 / 543 | 187 / 187 | 180 / 143 | 0 / 0 | 0 / 0 |
| Euler 31 | 198 / 297 | 402 / 404 | 65 / 54 | 6 / 6 | 0 / 0 |
| Euler 43 | 3846 / 3404 | 1497 / 1517 | 460 / 320 | 0 / 0 | 0 / 0 |
| Euler 48 | 4054 / 3336 | 2412 / 2438 | 756 / 731 | 0 / 0 | 0 / 13 |
| Euler 57 | 1081 / 1070 | 857 / 1114 | 291 / 224 | 16 / 0 | 0 / 0 |
| Euler 58 | 4044 / 4769 | 2158 / 2030 | 207 / 225 | 0 / 0 | 0 / 0 |
| Euler 92 | 934 / 948 | 2807 / 2660 | 198 / 181 | 11 / 7 | 0 / 1 |
| Euler 97 | 634 / 621 | 445 / 445 | 132 / 125 | 0 / 0 | 0 / 3 |

LUT count is lower on four of nine designs, but five regress and several designs trade extra carries or block RAMs for other resources. For example, Yosys uses 44 RAM32M cells on Euler 57, while synth uses 16 RAMB18E1 blocks. Euler 92 also uses more FFs and RAM blocks. **These results do not meet the requested QoR criterion.**

Euler 10 was attempted but its Yosys mapping was stopped after more than ten minutes without a completed result. Its full gate-level simulation was also not completed (the initial run hit a five-minute timeout). It is omitted from the numerical comparison rather than assigned an estimated or zero result. The optional long simulation now has a 30-minute command limit.

To reproduce the nine completed cases:

```sh
python3 tools/qor_artix7.py --out /tmp/xc7-qor \
  --design 14 --design 25 --design 31 --design 43 --design 48 \
  --design 57 --design 58 --design 92 --design 97
```

Local verification passed: formatting, Clippy with warnings denied, 107 workspace tests, Rust 1.88 compatibility, all default Artix-7 formal/differential checks, and byte-identical iCE40 netlist emission against `a1f312d` for Euler 25, 31 and 97. No Artix-7 hardware or bitstream validation has been performed.

## Remaining requirements for full native support

- Import and validate an Artix-7 device/routing database (including site routing,
  package pins and speed-grade timing), starting with XC7A35T and XC7A100T.
- Implement packing and legality for slices, carry chains, control sets, RAMs,
  DSPs, clock networks and I/O banks; add XDC constraints.
- Implement placement, routing, calibrated setup/hold timing and timing repair
  for that database, with deterministic reproducibility and capacity checks.
- Generate configuration frames, ECC/CRC and startup packets; independently
  round-trip the resulting bitstreams and run them on reference hardware.
- Add DSP, LUTRAM, SRL and wide-mux mapping and close the measured synthesis
  gaps, then compare complete flows using the same constraints, parts and
  independent timing analysis.

These are pending work, not enabled by the new synthesis CLI option. The
project's [Phase 2 roadmap](ROADMAP.md#phase-2-artix-7) describes the intended
native backend and hardware exit criteria.
