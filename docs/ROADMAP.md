# synth — Roadmap

Companion to [ARCHITECTURE.md](ARCHITECTURE.md). Phases have explicit **exit
criteria**. A phase is done when its criteria pass in CI (and on hardware where
stated), not when its features merely exist.

## Guiding principles

1. **Vertical slices first.** Get a bitstream running on real hardware as early as
   possible, then widen. One family end to end beats three half-done ones.
2. **Backend and frontend in parallel.** The backend is brought up on netlists
   that Yosys has mapped, read as RTLIL, so it does not wait for our frontend.
   Each half then has an independent oracle.
3. **Measure from day one.** Every phase records QoR against the same baselines,
   so optimization work in Phase 4 is driven by data.
4. **Design for Artix-7, ship iCE40 first.** Artix-7 is the second family. The
   ChipDB model and the `Arch` trait are validated against XC7's scale and
   complexity (data import and memory footprint) in Phase 0, so no iCE40-specific
   assumptions get baked into core data structures.
5. **Validate on the hardware we have.** Family order follows the reference
   boards. A family without a board can still be built and verified by bitstream
   round-trip, but it cannot pass a hardware exit criterion.

## Reference hardware

| Board | FPGA | Resources | Clock | Role |
|---|---|---|---|---|
| Nandland Go Board | iCE40HX1K-VQ100 | 1,280 LCs, 16 × 4 Kbit BRAM (8 KB) | 25 MHz | Phase 1: small designs, smoke tests |
| Alchitry Cu (v1) | iCE40HX8K-CB132 | 7,680 LCs, 32 × 4 Kbit BRAM (16 KB), PLL, no DSP | 100 MHz | Phase 1 primary (Milestone 1) |
| Nexys A7 | XC7A100T-1CSG324C | 63,400 LUT6, 135 × 36 Kbit BRAM, 240 DSP48E1, 128 MB DDR2, Ethernet | 100 MHz | Phase 2 primary |
| Alchitry Au (v1) | XC7A35T-1FTG256C | 20,800 LUT6, 50 × 36 Kbit BRAM, 90 DSP48E1, 256 MB DDR3 | 100 MHz | Phase 2 secondary |
| None | ECP5 | | | Phase 3, verified without hardware (ADR 0009) |

All four boards use an FT2232H for programming and a USB UART.

---

## Phase 0: Foundations

- Cargo workspace, CI (fmt, clippy, tests, `-j1` vs `-jN` determinism check),
  licence, contribution guide, `docs/decisions/` (ADR log).
- `synth-core`: typed IDs and arenas, interner, deterministic hasher and ordered
  map, `Bits`/`Logic` bit vectors, diagnostics renderer, thread pool, pass timers.
- Test corpus and benchmark manifest. External designs are fetched by script and
  pinned by commit, never vendored. Licences are recorded.
- `synth-chipdb` format v0 and `synth-dbgen` iCE40 importer (IceStorm, HX1K and
  HX8K first).
- iCE40 bitstream **assembler and disassembler**, and `.asc` reader.

**Exit criteria**
- `synth dev asc2bin` output is bit-identical to `icepack` for all IceStorm
  examples and a corpus of `.asc` files produced by nextpnr for HX1K and HX8K.
- `synth dev bin2asc` round-trips them.
- The XC7A35T/XC7A100T import *prototype* (routing graph only) runs, and its
  measured in-memory size validates the node-deduplication design (target: tens
  of MB).

---

## Phase 1: iCE40 end to end (Milestone 1)

Primary board: **Alchitry Cu** (HX8K). Secondary: **Go Board** (HX1K).

### Track B: backend

1. `synth-netlist` mapped netlist IR.
2. RTLIL reader, mapped-netlist subset first (`SB_*` cells from
   `yosys -p "synth_ice40; write_rtlil"`). The SRTL text format is written at the
   same time.
3. iCE40 primitive library and tile legality (PLB control sets, carry chains, IO,
   global buffers, BRAM, PLL).
4. SA placer (timing-driven), IO and pin constraint handling (PCF).
5. Router: global networks, then PathFinder with A* and lookahead.
6. STA with IceStorm timing data, and clock constraints.
7. Bitstream from placed and routed design.

*Track B exit*: Yosys-synthesized blinky and UART echo run on the Go Board and the
Cu, and a Yosys-synthesized PicoRV32 SoC runs on the Cu. Our STA agrees with
`icetime` within a few percent. Fmax and utilization are within 15% of nextpnr
on the same netlists.

### Track F: frontend and synthesis

1. Preprocessor, lexer, parser (Verilog-2005) with diagnostics.
2. Elaboration: parameters, generate, constant functions, sizing rules.
3. Shared process lowering → RTL IR; RTL passes (const-prop, CSE, DCE, mux opt,
   width reduction).
4. RTLIL word-level subset (what Amaranth emits) on top of the same lowering.
5. `synth-sim` 4-state RTL simulator. Differential tests against Icarus.
   (Done: ADR 0008; a compiled cycle simulator, 4- and 2-state, with a C backend.)
6. Memory inference + `$readmemh`; DFF legalization.
7. `synth-sat`; AIG with rewriting, balancing and fraiging; priority-cut LUT4
   mapping with area recovery; combinational equivalence checking.
8. iCE40 lowering: carry chains, `SB_RAM40_4K`, `SB_IO`, `SB_GB`, `SB_PLL40_*`,
   flop variants. The UltraPlus-only `SB_MAC16` and `SB_SPRAM256KA` wait until
   there is a UP5K board.

*Track F exit*: the test corpus elaborates and simulates identically to Icarus,
and every synthesis pass passes `--check` equivalence. A PicoRV32 SoC synthesized by
us runs firmware correctly in `synth-sim` (gate level).

### Integration ladder

- **Go Board**: 1. blinky → 2. UART "hello" → 3. SERV (bit-serial RISC-V; its
  `servant` SoC already supports the HX1K iCEstick).
- **Alchitry Cu**: 4. PicoRV32 SoC (firmware in BRAM via `$readmemh`, UART, PLL
  for the core clock) → 5. **pipelined cores**: VexRiscv (5-stage,
  SpinalHDL-generated Verilog), Hazard3 (3-stage), DarkRISCV → 6. an Amaranth
  SoC read as RTLIL.

The HX8K has no DSP blocks and 16 KB of BRAM. That shapes the target: RV32I, and
RV32IM with an iterative multiply/divide unit, with firmware sized to fit in BRAM.

### Milestone 1 exit criteria

- A pipelined RV32I(M) SoC, built **only with `synth`** (no Yosys, nextpnr or
  IceStorm at runtime), passes riscv-tests and runs Dhrystone on the Alchitry Cu.
  It also runs CoreMark if that fits in the 16 KB of BRAM.
- SERV runs on the Go Board, and an Amaranth design read as RTLIL runs on the Cu.
- The bitstream round-trip simulation (disassemble, simulate, compare to RTL)
  passes for the whole integration ladder in CI.
- Baseline QoR is recorded against iCEcube2 and Yosys+nextpnr (LCs, BRAM, Fmax,
  runtime). Target at M1: **within 20% LCs and 20% Fmax of Yosys+nextpnr**.
  Closing the gap is Phase 4's job.
- Full flow for the SoC takes under 10 s on a laptop.
- `synth init --board alchitry-cu && synth build && synth prog` works for a new
  user.

---

## Phase 2: Artix-7

Primary board: **Nexys A7** (XC7A100T). Secondary: **Alchitry Au** (XC7A35T).

- X-Ray importer with node deduplication; site-internal routing as pseudo-pips.
- Slice legality: 4 × LUT6/LUT6_2, 8 FFs, single control set, `CARRY4`, F7/F8,
  AX–DX bypass rules. SLICEM LUTRAM and SRL.
- `RAMB18E1`/`RAMB36E1`, `DSP48E1` (register absorption, cascades),
  `MMCME2`/`PLLE2` (configuration computed from frequencies),
  `BUFG`/`BUFGCTRL`/`BUFH`, clock region budgeting, IOB/IOSTANDARD/bank rules,
  `IDELAYE2`/`IDDR`/`ODDR`.
- XDC/SDC subset with a Tcl-subset interpreter, so the Digilent and Alchitry
  master XDC files work unchanged.
- **Electrostatic analytical placer** + legalizer + detailed placement (needed at
  100T scale). Parallel deterministic router.
- Hold-time fixing; LUT pin permutation in the router.
- Frames, ECC and CRC bitstream; startup sequence. Bit-exact versus
  `fasm2frames`/`xc7frames2bit`.
- **Vivado calibration harness** (EDIF + LOC/BEL + `FIXED_ROUTE` export, timing
  diff, correction tables). The feasibility spike is done early in Phase 1.

**Exit criteria**
- The Milestone 1 SoC (now RV32IM using DSPs) runs on both boards.
- A LiteX SoC (VexRiscv, UART, SPI flash, Ethernet on the Nexys A7) runs on both
  boards. Stretch goal: LiteDRAM with the Nexys A7 DDR2, which exercises
  ISERDES/OSERDES/IDELAY and careful clocking.
- A design of ≥ 50k LUTs on the XC7A100T stays within runtime and memory budgets.
- QoR baseline is recorded against Vivado (default and performance strategies) and
  openXC7.
- Our STA agrees with Vivado `report_timing` within ±10% on the same placed and
  routed design.

---

## Phase 3: ECP5 (in progress, without hardware)

ECP5 came back without a board (ADR 0009), so the phase has two modes:

- **Without hardware** (done, see [ECP5.md](ECP5.md)):
  - [x] Trellis database read at run time; routing graph with Trellis' arcs;
    LPF import.
  - [x] Assembler and disassembler bit-exact versus `ecppack`/`ecpunpack`.
  - [x] `CCU2C`, LSR/CE/CLK sharing, `DP16KD`/`PDPW16KD`, `MULT18X18D`,
    `EHXPLLL`, `DCCA`, I/O.
  - [x] The full integration ladder verified by bitstream round-trip
    simulation; timing checked against nextpnr-ecp5's.
  - [x] QoR baseline against Yosys+nextpnr (ahead on LUTs on every design,
    on Fmax on all but one).
  - [ ] `PFUMX`/`L6MUX21`, LUTRAM (`DPR16X4`), `ALU54B` and registered DSPs,
    I/O registers, bidirectional pads.
- **With hardware** (needs a board, for example a ULX3S, OrangeCrab or Colorlight
  i5/i9): the same hardware exit criteria as the other families.

**Exit criteria**: all round-trip and bit-exact checks pass. QoR baseline is
recorded against Diamond and Yosys+nextpnr, with a target **≤ 10% behind
Yosys+nextpnr**. If a board has been added, the Milestone 1 SoC and a LiteX SoC
run on it.

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
| Artix-7 | within 5% of Vivado | within 15% of Vivado first, then close further |
| ECP5 | ≤ vendor | ≥ vendor |

---

## Phase 5: Breadth and polish

- SystemVerilog subset (Ibex first, then CVA6 as a stretch goal). Revisit FIRRTL
  and VHDL based on demand.
- More parts: Spartan-7, Kintex-7 and Zynq-7 PL (same X-Ray infrastructure);
  iCE40 UltraPlus (`SB_MAC16`, SPRAM); ECP5-5G.
- Native programmer: macOS and Linux backends (see Architecture §14), Windows later.
- Incremental compilation; HTML reports; floorplan viewer; LSP for diagnostics.
- WASM build: an in-browser iCE40 playground.
- Possibly other families (Gowin via Apicula, MachXO2 via Trellis).

---

## First pull requests (concrete starting sequence)

| # | PR | Track |
|---|---|---|
| 1 | Workspace skeleton, CI, licence, ADR template, `synth-core` IDs/arenas/hasher/maps | 0 |
| 2 | `Bits`/`Logic` bit vectors with Verilog semantics + exhaustive tests | 0 |
| 3 | Diagnostics and source map | 0 |
| 4 | ChipDB format v0 + IceStorm importer (`cargo xtask dbgen ice40`) | 0 |
| 5 | iCE40 assembler/disassembler, `.asc` I/O, bit-exact tests | 0 |
| 6 | XC7A35T/XC7A100T routing-graph import prototype (memory and size measurement only) | 0 |
| 7 | Mapped netlist IR, SRTL writer, RTLIL reader (mapped-netlist subset) | B |
| 8 | iCE40 primitives + tile legality + PCF + board files for the Cu and Go Board | B |
| 9 | SA placer | B |
| 10 | Router (globals + PathFinder) | B |
| 11 | STA + iCE40 timing | B |
| 12 | Blinky on the Go Board and the Cu 🎉 | B |
| 13–20 | Preprocessor/lexer → parser → elaboration → shared process lowering → RTL passes + RTLIL word-level subset → simulator → SAT/AIG/mapper → iCE40 lowering | F |

---

## Rough sizing

These are order-of-magnitude estimates to plan around, not commitments.

| Component | Rust LOC (approx.) |
|---|---|
| Core utilities, diagnostics | 5k |
| Verilog-2005 frontend (preproc, parser, elaboration, process lowering) | 20–25k |
| RTLIL reader + SRTL reader/writer | 4k |
| RTL IR + passes + memory inference | 10k |
| SAT + AIG + optimization + mapping + equivalence | 12–15k |
| Simulators | 5k |
| ChipDB + importers (3 families) | 8k |
| Placement (SA + analytical + legalization + detailed) | 10k |
| Routing + STA | 10k |
| iCE40 / Artix-7 / ECP5 arch crates (lowering, legality, bitstream) | 5k / 15k / 10k |
| CLI, constraints (PCF/XDC-Tcl/LPF), boards, reports | 6k |
| **Milestone 1 subset** | **~60k** |
| **All three families, pre-optimization** | **~125k** |

For one experienced engineer, Milestone 1 is roughly a year of focused work. The
Verilog frontend and debugging the end-to-end flow are the main schedule risks.
The two tracks parallelize well across two people.

---

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| XC7 timing model inaccuracy | Fmax claims are wrong; Vivado parity unattainable | Calibration harness (Architecture §10.4) spiked early; report confidence ranges; per-arc correction tables |
| Database gaps (X-Ray: some IO, clocking and DSP/BRAM modes) | Some designs unsupported | Clear "unsupported feature" diagnostics; prioritize by benchmark demand; contribute fuzzers upstream |
| Verilog semantics edge cases (sizing, signedness, X) | Silent mis-synthesis | Dedicated sizing corpus; differential simulation versus Icarus/Verilator; random fuzzing; `--check` in CI |
| RTLIL has no formal spec and changes between Yosys/Amaranth versions | Reader breaks on new producer versions | Pin tested versions; corpus per version; precise diagnostics for unknown cells |
| iCE40 boards may be flash-only | Slower iteration, flash wear | Read back after write; `synth bram-patch` for firmware-only changes; images are small (~32 KB for HX1K, ~135 KB for HX8K) |
| Scale and memory on XC7A100T/200T | Unusable on big parts | Node deduplication validated in Phase 0; paged router state; memory budget in CI |
| Scope creep (SV, VHDL, FIRRTL, more families) | Milestone slip | Strict phase exit criteria; later items gated by ADR |
| Hardware damage from bad IO configuration | Board damage (low probability) | SRAM loading where the board allows it; bank voltage and IO standard DRC before bitstream; conservative defaults |
| Single-maintainer bus factor on complex subsystems | Stalls | ADRs, design notes per crate, golden tests as executable spec |

---

## Decisions

### Confirmed

1. **Implementation language: Rust** (stable, edition 2024).
2. **Dependencies**: zero crates in shipped code. Yosys, nextpnr, Icarus, Verilator
   and vendor tools are allowed as **CI-only oracles**. Yosys output (as RTLIL) is
   used to bring up the backend.
3. **Inputs**: Verilog-2005, RTLIL and SRTL. SystemVerilog, FIRRTL and VHDL are
   deferred (Architecture §6.1).
4. **Hardware**: Go Board, Alchitry Cu, Alchitry Au and Nexys A7. Family order:
   **iCE40 → Artix-7 → ECP5** (ECP5 without a board yet, ADR 0009).
5. **Programming**: openFPGALoader for now (Homebrew on macOS, packages on
   Linux). A native programmer for the FT2232H boards comes later, with macOS
   (IOKit) and Linux (usbfs) backends.
6. **Licence**: EUPL-1.2 for code; CERN-OHL-W-2.0 for HDL kept in the
   repository; tool output is not covered by either.
7. **Platforms**: macOS on Apple Silicon (main development machine) and Linux
   x86-64, both tested in CI.

Each decision is recorded in [`docs/decisions/`](decisions/).
