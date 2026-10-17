# synth — Roadmap

Companion to [ARCHITECTURE.md](ARCHITECTURE.md). Phases have explicit **exit
criteria**. A phase is done when its criteria pass in CI (and on hardware where
stated), not when its features merely exist.

## Guiding principles

1. **Vertical slices first.** Get a bitstream running on real hardware as early as
   possible, then widen. One family end to end beats three half-done ones.
2. **Backend and frontend in parallel.** The backend is brought up on netlists
   produced by Yosys (JSON reader, dev-only), so it does not wait for our
   frontend. Each half then has an independent oracle.
3. **Measure from day one.** Every phase records QoR against the same baselines,
   so optimization work in Phase 4 is driven by data.
4. **Design for Artix-7, ship iCE40 first.** The ChipDB model and the `Arch` trait
   are validated against XC7's scale and complexity (data import and memory
   footprint) early in Phase 1, even though XC7 support lands in Phase 3. This
   avoids baking iCE40-specific assumptions into core data structures.

---

## Phase 0: Foundations

- Cargo workspace, CI (fmt, clippy, tests, `-j1` vs `-jN` determinism check),
  license, contribution guide, `docs/decisions/` (ADR log).
- `synth-core`: typed IDs and arenas, interner, deterministic hasher and ordered
  map, `Bits`/`Logic` bit vectors, diagnostics renderer, thread pool, pass timers.
- Test corpus and benchmark manifest. External designs are fetched by script and
  pinned by commit, never vendored. Licenses are recorded.
- `synth-chipdb` format v0 and `synth-dbgen` iCE40 importer (IceStorm).
- iCE40 bitstream **assembler and disassembler**, and `.asc` reader.

**Exit criteria**
- `synth dev asc2bin` output is bit-identical to `icepack` for all IceStorm
  examples and a corpus of nextpnr-produced `.asc` files.
- `synth dev bin2asc` round-trips them.
- The XC7A50T/XC7A200T import *prototype* (routing graph only) runs and its
  measured in-memory size validates the node-deduplication design (target: tens
  of MB).

---

## Phase 1: iCE40 end to end (Milestone 1)

Target board: **iCEBreaker** (iCE40UP5K: 5,280 LCs, 30 × 4 Kbit EBR, 4 × 256 Kbit
SPRAM, 8 × DSP). Secondary boards: iCE40-HX8K breakout and iCEstick.

### Track B: backend

1. `synth-netlist` mapped netlist IR; Yosys JSON reader (feature `yosys-json`).
2. iCE40 primitive library and tile legality (PLB control sets, carry chains, IO,
   global buffers, BRAM, PLL).
3. SA placer (timing-driven), IO and pin constraint handling (PCF).
4. Router: global networks, then PathFinder with A* and lookahead.
5. STA with IceStorm timing data, and clock constraints.
6. Bitstream from placed and routed design.

*Track B exit*: Yosys-synthesized blinky, UART echo and PicoRV32 SoC netlists run
on the iCEBreaker. Our STA agrees with `icetime` within a few percent. Fmax and
utilization are within 15% of nextpnr on the same netlists.

### Track F: frontend and synthesis

1. Preprocessor, lexer, parser (Verilog-2005) with diagnostics.
2. Elaboration: parameters, generate, constant functions, sizing rules.
3. Process lowering → RTL IR; RTL passes (const-prop, CSE, DCE, mux opt, width
   reduction).
4. `synth-sim` 4-state RTL simulator. Differential tests against Icarus.
5. Memory inference + `$readmemh`; DFF legalization.
6. `synth-sat`; AIG with rewriting, balancing and fraiging; priority-cut LUT4
   mapping with area recovery; combinational equivalence checking.
7. iCE40 lowering: carry chains, `SB_RAM40_4K`/`SB_SPRAM256KA`, `SB_MAC16`,
   `SB_IO`/`SB_GB`/`SB_PLL40_*`, flop variants.

*Track F exit*: the test corpus elaborates and simulates identically to Icarus,
and every synthesis pass passes `--check` equivalence. A PicoRV32 SoC synthesized by
us runs firmware correctly in `synth-sim` (gate level).

### Integration ladder

1. Blinky → 2. UART "hello" → 3. PicoRV32 SoC (firmware in BRAM via `$readmemh`,
UART) → 4. **pipelined cores**: VexRiscv (5-stage, SpinalHDL-generated Verilog),
Hazard3 (3-stage), DarkRISCV. Each runs riscv-tests, a UART shell and CoreMark.

### Milestone 1 exit criteria

- A pipelined RV32IM SoC, built **only with `synth`** (no Yosys, nextpnr or
  IceStorm at runtime), passes riscv-tests and runs CoreMark on the iCEBreaker.
- The bitstream round-trip simulation (disassemble, simulate, compare to RTL) passes
  for the whole integration ladder in CI.
- Baseline QoR is recorded against iCEcube2/Radiant and Yosys+nextpnr (LCs, BRAM,
  DSP, Fmax, runtime). Target at M1: **within 20% LCs and 20% Fmax of
  Yosys+nextpnr**. Closing the gap is Phase 4's job.
- Full flow for the SoC takes under 10 s on a laptop.
- `synth init --board icebreaker && synth build && synth prog` works for a new user.

---

## Phase 2: ECP5

Target boards: **ULX3S** (12F/25F/45F/85F) and OrangeCrab (25F/85F).

- Trellis importer: tile bits, routing, globals, timing for speed grades 6/7/8.
- Slice and PFU legality (LUT pairs, `PFUMX`/`L6MUX21`, `CCU2C`, LSR/CE/CLK
  sharing, LUTRAM mode). Mux-aware wide-function mapping.
- `DP16KD`/`PDPW16KD`, `DPR16X4`, `MULT18X18D`/`ALU54B`, `EHXPLLL`, `DCCA`, IO
  (LVCMOS, differential, DDR). LPF import.
- **Electrostatic analytical placer** + legalizer + detailed placement (needed for
  the 85F). Parallel deterministic router.
- ECP5 bitstream assembler and disassembler (bit-exact versus `ecppack`).

**Exit criteria**: the Milestone 1 SoC and a LiteX SoC (VexRiscv + SDRAM
controller) run on the ULX3S. QoR baseline is recorded against Diamond and
Yosys+nextpnr, with a target **≤ 10% behind Yosys+nextpnr** on both LUTs and Fmax.
A full 85F-scale benchmark completes within the runtime and memory budgets.

---

## Phase 3: Artix-7

Target boards: **Arty A7-35T / A7-100T**, Basys 3 (XC7A35T), Nexys A7.

- X-Ray importer with node deduplication; site-internal routing as pseudo-pips.
- Slice legality: 4 × LUT6/LUT6_2, 8 FFs, single control set, `CARRY4`, F7/F8, AX/BX
  bypass rules. SLICEM LUTRAM and SRL.
- `RAMB18E1`/`RAMB36E1`, `DSP48E1` (register absorption, cascades), `MMCME2`/`PLLE2`
  (config computation from frequencies), `BUFG`/`BUFGCTRL`/`BUFH`, clock region
  budgeting, IOB/IOSTANDARD/bank rules, `IDELAYE2`/`IDDR`/`ODDR`.
- XDC/SDC subset with a Tcl-subset interpreter.
- Hold-time fixing; LUT pin permutation in the router.
- Frames, ECC and CRC bitstream; startup sequence. Bit-exact versus
  `fasm2frames`/`xc7frames2bit`.
- **Vivado calibration harness** (EDIF + LOC/BEL + `FIXED_ROUTE` export, timing
  diff, correction tables). The feasibility spike is done early in Phase 1.

**Exit criteria**: the M1 SoC and a LiteX SoC run on the Arty A7. A large design
(≥ 50k LUTs) on XC7A100T/200T stays within runtime and memory budgets. QoR baseline
is recorded against Vivado (default and performance strategies) and openXC7.
Our STA agrees with Vivado `report_timing` within ±10% on the same placed and
routed design.

---

## Phase 4: QoR optimization

Prioritized by measured gap (dashboard driven). The expected high-impact items,
roughly in order:

| Area | Items |
|---|---|
| Logic | Mapping with structural choices; `mfs`-style resynthesis; LUT6_2 merging; F7/F8 and PFUMX-aware mapping; deeper rewriting scripts (`-O3`) |
| Arithmetic | Ternary adders (XC7); constant multipliers via CSD shift-add; DSP inference with full pipeline-register absorption, pre-adder and MAC/cascade patterns; carry-chain-aware comparators |
| Memory | BRAM output register absorption; byte-enable mapping; BRAM/LUTRAM/SRL selection by cost; width/depth splitting and cascading |
| Sequential | Retiming; register duplication; control-set reduction; FSM one-hot recoding |
| Placement | Timing-driven analytical placement with net weighting; clock-region-aware placement; macro-aware legalization; detailed-placement swaps with exact STA |
| Routing | Better lookahead; criticality-driven rip-up; LUT pin permutation; hold-aware routing |
| Physical synthesis | Post-placement critical path re-mapping and replication (the unified-flow advantage, see Architecture §2) |
| Timing model | Calibration tables per family and speed grade |

**Targets** (on the bench suite, geometric mean):

| Family | LUT/FF | Fmax |
|---|---|---|
| iCE40 | ≤ vendor | ≥ vendor and ≥ Yosys+nextpnr |
| ECP5 | ≤ vendor | ≥ vendor |
| Artix-7 | within 5% of Vivado | within 15% of Vivado first, then close further |

---

## Phase 5: Breadth and polish

- SystemVerilog subset (Ibex first, then CVA6 as a stretch goal).
- More parts: Spartan-7, Kintex-7 and Zynq-7 PL (same X-Ray infrastructure);
  ECP5-5G; additional iCE40 packages.
- Native programmer (FTDI MPSSE JTAG/SPI, flash writing).
- Incremental compilation; HTML reports; floorplan viewer; LSP for diagnostics.
- WASM build: an in-browser iCE40 playground.
- A decision on VHDL; possibly other families (Gowin via Apicula, MachXO2 via
  Trellis).

---

## First pull requests (concrete starting sequence)

| # | PR | Track |
|---|---|---|
| 1 | Workspace skeleton, CI, license, ADR template, `synth-core` IDs/arenas/hasher/maps | 0 |
| 2 | `Bits`/`Logic` bit vectors with Verilog semantics + exhaustive tests | 0 |
| 3 | Diagnostics and source map | 0 |
| 4 | ChipDB format v0 + IceStorm importer (`cargo xtask dbgen ice40`) | 0 |
| 5 | iCE40 assembler/disassembler, `.asc` I/O, bit-exact tests | 0 |
| 6 | XC7 routing-graph import prototype (memory and size measurement only) | 0 |
| 7 | Mapped netlist IR + Yosys JSON reader | B |
| 8 | iCE40 primitives + tile legality + PCF | B |
| 9 | SA placer | B |
| 10 | Router (globals + PathFinder) | B |
| 11 | STA + iCE40 timing | B |
| 12 | Blinky on hardware 🎉 | B |
| 13–20 | Preprocessor/lexer → parser → elaboration → process lowering → RTL passes → simulator → SAT/AIG/mapper → iCE40 lowering | F |

---

## Rough sizing

These are order-of-magnitude estimates to plan around, not commitments.

| Component | Rust LOC (approx.) |
|---|---|
| Core utilities, diagnostics | 5k |
| Verilog-2005 frontend (preproc, parser, elaboration, process lowering) | 20–25k |
| RTL IR + passes + memory inference | 10k |
| SAT + AIG + optimization + mapping + equivalence | 12–15k |
| Simulators | 5k |
| ChipDB + importers (3 families) | 8k |
| Placement (SA + analytical + legalization + detailed) | 10k |
| Routing + STA | 10k |
| iCE40 / ECP5 / XC7 arch crates (lowering, legality, bitstream) | 5k / 10k / 15k |
| CLI, constraints (PCF/LPF/XDC-Tcl), boards, reports | 6k |
| **Milestone 1 subset** | **~60k** |
| **All three families, pre-optimization** | **~120k** |

For one experienced engineer, Milestone 1 is roughly a year of focused work. The
Verilog frontend and debugging the end-to-end flow are the main schedule risks.
The two tracks parallelize well across two people.

---

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| XC7 timing model inaccuracy | Fmax claims are wrong; Vivado parity unattainable | Calibration harness (§10.4) spiked early; report confidence ranges; per-arc correction tables |
| Database gaps (X-Ray: some IO, clocking and DSP/BRAM modes) | Some designs unsupported | Clear "unsupported feature" diagnostics; prioritize by benchmark demand; contribute fuzzers upstream |
| Verilog semantics edge cases (sizing, signedness, X) | Silent mis-synthesis | Dedicated sizing corpus; differential simulation versus Icarus/Verilator; random fuzzing; `--check` in CI |
| Scale and memory on XC7A200T | Unusable on big parts | Node deduplication validated in Phase 0; paged router state; memory budget in CI |
| Scope creep (SV, VHDL, more families) | Milestone slip | Strict phase exit criteria; later items gated by ADR |
| Hardware damage from bad IO configuration | Board damage (low probability) | SRAM-only loading during bring-up; bank voltage and IO standard DRC before bitstream; conservative defaults |
| Single-maintainer bus factor on complex subsystems | Stalls | ADRs, design notes per crate, golden tests as executable spec |

---

## Decisions to confirm

These defaults are assumed in the plan. Each one can be changed.

1. **Implementation language: Rust** (stable, edition 2024). It fits the
   performance and parallelism requirements, gives memory safety on huge graphs,
   ships as a single static binary and can target WASM. Weak spots are graph
   ergonomics (addressed with arenas and indices) and compile times (addressed
   with crate splitting).
2. **Dependency policy**: zero crates in shipped code. Dev tooling avoids them
   too, with exceptions by ADR. Yosys, nextpnr, Icarus, Verilator and vendor tools
   are allowed as **CI-only oracles**.
3. **HDL scope**: Verilog-2005 first, a SystemVerilog subset second, VHDL
   deferred.
4. **Family order**: iCE40 → ECP5 → Artix-7, with XC7 data structures validated
   in Phase 0.
5. **Boards**: iCEBreaker, ULX3S and Arty A7 as the reference boards for the
   hardware-in-the-loop CI. This depends on what hardware is available.
6. **Project license**: Apache-2.0 OR MIT (or ISC).
7. **Programming**: delegate to `openFPGALoader` initially; native programmer in
   Phase 5.
8. **Yosys JSON input**: kept permanently as an interoperability input, or
   removed after Milestone 1.
