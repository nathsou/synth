# synth — Architecture

Status: **design draft**. This document describes the target architecture. The
phased plan for getting there is in [ROADMAP.md](ROADMAP.md).

---

## 1. Goals and non-goals

### Goals

1. **One self-contained tool** (`synth`) that goes from HDL to a programmable
   bitstream for:
   - Lattice **iCE40** (LP/HX/UltraPlus),
   - Lattice **ECP5** (LFE5U / LFE5UM / LFE5UM5G), deferred: out of scope until
     there is hardware to validate on (see the roadmap),
   - Xilinx **Artix-7** (XC7A35T/50T/100T/200T; Spartan-7, Kintex-7 and Zynq-7 PL
     come almost for free later because they share the 7-series fabric and database).
2. **No external tools at runtime.** No Yosys, ABC, nextpnr, Python, Tcl, or vendor
   tools. The shipped binary depends only on the Rust standard library and the OS.
   The device data comes from the open bitstream documentation projects (§10). They
   are imported at build time into our own format and shipped with the tool.
3. **Match vendor quality of results (QoR)**: resource use (LUT/FF/BRAM/DSP),
   Fmax, and power-on correctness. We measure this continuously against
   iCEcube2/Radiant, Diamond, Vivado and Yosys+nextpnr (§13.8). On iCE40 and ECP5
   we aim to beat the vendor tools. On Artix-7, matching Vivado is a long-term goal.
4. **Deterministic.** The same inputs and seed give a bit-identical bitstream,
   whatever the thread count, platform or run.
5. **Easy to use.** `synth build --board alchitry-cu top.v` should just work, with
   good diagnostics, readable reports and sensible defaults.

### Non-goals (for now)

- Native SystemVerilog, VHDL and FIRRTL frontends (deferred, §6.1). Meanwhile,
  SystemVerilog and VHDL designs can reach `synth` as RTLIL, by running Yosys
  (with yosys-slang or the GHDL plugin) on the user's side.
- Transceivers (GTP, ECP5 SERDES), PCIe, partial reconfiguration, bitstream
  encryption.
- Vendor IP generators (MIG, CoreGen). Users instantiate primitives directly.
- A GUI. Reports are text, JSON and later HTML. A floorplan viewer may come later.
- Simulation as a product feature. An internal simulator exists for verification
  (§13) and may be exposed later as `synth sim`.

---

## 2. Why a unified toolchain

Yosys + nextpnr proved that an open flow can be competitive. They are our main
references, and in CI they serve as **test oracles**. They are never runtime
dependencies. What we can do differently comes from keeping the whole flow in **one
process with one data model**:

- **One timing model everywhere.** LUT mapping, packing, placement and routing all
  see the same delay model. There is no lossy JSON handoff and no guessing about
  what the other tool will do.
- **Physical synthesis.** After placement, critical paths can be re-mapped,
  registers duplicated, logic retimed and LUT pins permuted, all against real
  placement and routing information and then re-legalized incrementally. In a
  split flow this is impractical.
- **Name preservation.** Constraints (`get_cells`, `get_nets`), timing reports and
  debug output keep referring to source-level names through every stage.
- **Incremental compilation** (later). We can cache per-module elaboration and
  synthesis results, and keep the placement of unchanged logic.
- **One configuration surface.** One project file, one constraint language, one
  report format across all FPGA families.

---

## 3. Pipeline overview

```
 Verilog-2005        RTLIL, SRTL         Constraints: synth.toml, PCF, LPF, XDC/SDC
      │                   │                              │
      ▼                   ▼                              │
 ┌─────────┐       ┌─────────────┐                       │
 │ Verilog │       │  low-level  │  Verilog: preprocess, │
 │ frontend│       │   readers   │  parse, elaborate     │
 └────┬────┘       └──────┬──────┘  RTLIL / SRTL: §6.4   │
      └───┬───────────────┘                              │
          ▼  shared process lowering                     │
 ┌─────────────────┐   const-prop, CSE, DCE, width       │
 │  RTL IR (word)  │   reduction, FSM / memory / arith   │
 │                 │   inference, DFF legalization       │
 └────────┬────────┘                                     │
          ▼                                              │
 ┌─────────────────┐   carry chains, DSP, BRAM / LUTRAM  │
 │  Arch lowering  │   / SRL, IO buffers, global clocks  │
 └────────┬────────┘                                     │
          ▼                                              │
 ┌─────────────────┐   AIG optimization with choices,    │
 │ Logic IR (AIG + │   timing-aware LUT mapping, LUT     │
 │ boxes + FFs)    │   merging, don't-care resynthesis   │
 └────────┬────────┘                                     │
          ▼                                              ▼
 ┌─────────────────┐                             ┌──────────────┐
 │ Mapped netlist  │◄────────────────────────────│    ChipDB    │
 │ (device prims)  │                             │ (mmap-able)  │
 └────────┬────────┘                             └──────┬───────┘
          ▼                                             │
 ┌─────────────────┐   macro pre-pack → global place →  │
 │ Physical design │   legalize → detailed place →      │
 │                 │◄──route clocks → route signals ────┘
 │                 │   → hold fix → post-route opt
 └────────┬────────┘          ▲
          │                   │  STA (incremental) drives every stage
          ▼                   │
 ┌─────────────────┐
 │ Config features │──► bitstream assembler (per family) ──► .bin / .bit
 │  (FASM-like)    │◄── bitstream disassembler (used for verification)
 └─────────────────┘
```

Every arrow between IRs can be dumped (`--emit rtl|aig|netlist|place|route|fasm`;
`--emit rtl` writes SRTL, §6.4) and, in `--check` mode, verified (§13.3). An
RTLIL input made only of device primitives (a netlist that Yosys has already
mapped) skips synthesis and enters at the mapped netlist.

---

## 4. Workspace layout

This is a Cargo workspace. Crates are split along stable interfaces so that
compile times stay reasonable and dependencies stay acyclic.

| Crate | Role |
|---|---|
| `synth` (bin) | CLI, project file, board catalog, driving the flow |
| `synth-core` | Typed IDs and arenas, string interning, deterministic hashing and maps, bit vectors (2- and 4-state), diagnostics, thread pool, instrumentation |
| `synth-verilog` | Preprocessor, lexer, parser, AST (Verilog-2005, then SystemVerilog) |
| `synth-elab` | Verilog elaboration: hierarchy, constant evaluation, generate, producing RTL IR |
| `synth-rtlil` | RTLIL reader (Amaranth output, Yosys netlists) |
| `synth-weft` | Weft (native HDL): lexer, parser, type and domain checking, elaboration to RTL IR |
| `synth-rtl` | Word-level RTL IR, the SRTL text format (read and write), the process-lowering engine shared by the Verilog and RTLIL paths, and RTL passes (opt, memory, FSM, arithmetic inference) |
| `synth-sat` | CDCL SAT solver (incremental, with assumptions) |
| `synth-logic` | AIG, optimization, cut enumeration, LUT mapping, equivalence checking |
| `synth-sim` | Compiled cycle simulator for RTL and netlists, 4- or 2-state, with an interpreter and a C backend (ADR 0008) |
| `synth-netlist` | Mapped netlist IR shared by every backend stage |
| `synth-chipdb` | ChipDB binary format: types, reader, queries |
| `synth-dbgen` | Offline importers (IceStorm, Trellis, X-Ray) that write ChipDB files. Build time only |
| `synth-arch` | The `Arch` trait and shared architecture helpers |
| `synth-place` | Placers (SA, analytical), legalizer, detailed placement |
| `synth-route` | Router (negotiated congestion, timing-driven), global clock routing |
| `synth-timing` | STA engine, SDC/XDC constraint model, reports |
| `arch-ice40`, `arch-ecp5`, `arch-xc7` | Primitive libraries, lowering rules, packing and legality, timing glue, bitstream assembler and disassembler |
| `synth-lsp` | Weft language server (`synth lsp`) |
| `synth-playground` | The flow as data for the web playground (`synth playground`, or WebAssembly), and the web app itself |

Other top-level directories: `boards/` (board catalog, TOML), `tests/` (designs and
golden outputs), `bench/` (QoR suite and harness), `editors/` (editor
integrations), `docs/`.

---

## 5. Foundations (`synth-core`)

- **Arenas and typed indices.** All graphs (AST, RTL, AIG, netlist, routing) use
  `u32` newtype IDs into `Vec`-backed arenas. There is no `Rc<RefCell<…>>`.
  Deletion uses tombstones and periodic compaction. Hot data (placement
  coordinates, router costs) is stored as a struct of arrays.
- **Deterministic collections.** We write our own Fx-style hasher, with no random
  seeding, plus an insertion-ordered map and set. Iteration order never depends on
  hashes, pointer values or thread scheduling.
- **Interned names.** Hierarchical names are stored as a path trie
  (`parent_id + segment`). Millions of names cost little memory and compare in O(1).
- **Bit vectors.** Arbitrary-width 2-state (`Bits`) and 4-state (`Logic`) values,
  with full Verilog arithmetic semantics, are used for constant evaluation and
  simulation.
- **Diagnostics.** A source map over all files and macro expansions, with spans on
  every AST node. Rendering is rustc-style (labels, notes, suggestions).
- **Parallelism.** A small work-stealing pool on `std::thread::scope`. Parallel
  algorithms must reduce deterministically: partition, compute, merge in a fixed
  order.
- **Instrumentation.** Wall time and peak memory per pass, plus per-pass statistics
  (`--stats json`), feed the QoR dashboard.

---

## 6. Frontends (`synth-verilog`, `synth-elab`, `synth-rtlil`)

### 6.1 Input languages

| Input | Level | When | Unlocks |
|---|---|---|---|
| **Verilog-2005** (IEEE 1364), synthesizable subset | HDL | Milestone 1 | Hand-written RTL; PicoRV32, Hazard3, DarkRISCV, SERV; Verilog generated by SpinalHDL (VexRiscv), Migen/LiteX, Clash, Bluespec, and Chisel (through `firtool` with Verilog lowering options) |
| **RTLIL** (Yosys text IR) | Word-level RTL | Milestone 1 | Amaranth designs directly, without Yosys; backend bring-up on netlists mapped by Yosys; SystemVerilog (yosys-slang) and VHDL (GHDL plugin) through a Yosys run on the user's side |
| **SRTL** (our RTL IR's text form) | Word-level RTL | Milestone 1 | IR dumps and golden tests; a documented, stable target for generators and new HDLs |
| **Weft** (native HDL, [spec](WEFT.md), ADR 0006) | HDL | Implemented | New designs without Verilog's pitfalls: exact arithmetic, checked clock domains, declared registers and memories; compiled straight to the RTL IR |
| SystemVerilog, synthesizable subset | HDL | Later, demand-driven | Ibex, CVA6 and most modern RTL, natively |
| FIRRTL | HDL-level IR | Deferred | Chisel without `firtool` |
| VHDL | HDL | Deferred | NEORV32 and others, natively |

The Verilog-2005 subset covers modules, parameters, `generate`, constant
functions, `always`/`assign`, signed arithmetic, `$clog2`, `$readmemh/b`,
`initial` for power-on values and memory init, attributes (`(* keep *)` and
similar), and `` `define/`ifdef/`include ``.

**Why not FIRRTL yet.** FIRRTL is not a low-level format. It has aggregate types,
width and reset inference and `when` blocks, and its spec moves with Chisel
releases, so supporting it means writing a second frontend. The lowered form that
`firtool` produces (the CIRCT `hw`/`comb`/`seq` dialects) is MLIR, whose text form
has no stability guarantee. For Chisel, `firtool` → Verilog → `synth` is the
practical path. A FIRRTL reader can be revisited after Milestone 1 if there is
demand.

**Vendor primitive instantiation** (`SB_PLL40_CORE`, `EHXPLLL`, `MMCME2_BASE`,
`BUFG`, `IBUFDS` and so on) works from every input. Each `arch-*` crate declares its
primitives (ports, parameters with defaults) to the frontends.

### 6.2 Preprocessor, lexer, parser

- **Hand-written.** No parser generator. A recursive-descent parser with Pratt
  parsing for expressions gives the best control over error recovery and
  diagnostics, and needs no dependencies.
- The preprocessor is integrated with the lexer: macros with arguments, include
  paths, `` `line ``, `` `default_nettype ``, `` `timescale `` (ignored). Every
  token keeps its expansion chain for diagnostics.
- The AST lives in arenas. Parsing is parallel per file.

### 6.3 Elaboration

- **Hierarchy and parameters.** Resolve modules, apply overrides and `defparam`
  (limited support), and create one specialized module per unique parameter set
  (memoized by a hash of the parameters).
- **Constant evaluation.** A 4-state interpreter over AST expressions and constant
  functions (with loop and recursion limits).
- **Generate.** Unroll `for` and `if`/`case` generate blocks into named scopes.
- **Expression sizing and signedness** follow LRM §5.4/§5.5 exactly (self- versus
  context-determined operands). This is a classic source of synthesis bugs, so it
  gets a dedicated test corpus.
- **Process lowering** by symbolic execution of each `always` block over an
  environment that maps each variable to a value. The engine lives in `synth-rtl`
  and also lowers RTLIL processes (§6.4):
  - Blocking assignments update the environment. Non-blocking assignments are
    collected and committed at the end of the block.
  - `if`/`case` produce muxes (`case` produces parallel muxes when it is
    provably parallel or `unique`). Bounded loops are unrolled.
  - Classification: combinational (an incomplete assignment produces a latch
    warning, and an error under `always_comb`), edge-triggered, and async-reset
    patterns (`posedge clk or posedge rst`). Clock enables are recognized from mux
    feedback and synchronous resets from priority structure.
- **Initial values.** `initial` blocks and declaration initializers become flop
  init values or memory contents. `$readmemh` is evaluated at elaboration time.
  This matters for milestone 1: firmware lives in BRAM.

### 6.4 Low-level RTL inputs (`synth-rtlil`, `synth-rtl`)

**RTLIL reader.**
- It parses the RTLIL text format:
  - modules, wires (width, offset, `upto`, signedness, ports), `connect`;
  - cells with parameters;
  - attributes (`src` is mapped to our source spans for diagnostics);
  - `memory` objects;
  - `process` blocks (`switch`/`case`, plus `sync` rules for Yosys output that
    has not been through `proc`).
- Cells supported:
  - Yosys word-level cells: `$add` … `$pmux`, `$shift`/`$shiftx`, the
    `$dff`/`$adff`/`$sdff`/`$dffe` families, `$mem_v2`/`$memrd_v2`/`$memwr_v2`/`$meminit_v2`,
    `$tribuf`.
  - Gate-level cells (`$_AND_` and similar).
  - Vendor cells by name (`SB_LUT4`, `SB_CARRY`, `LUT6`, `FDRE` and so on), mapped
    to our primitive library.
- It covers what Amaranth emits today: combinational processes, `$dff`/`$adff`
  cells, and `memory` objects with `_v2` ports and `$meminit_v2`. Processes go
  through the same lowering engine as Verilog `always` blocks.
- It has two uses:
  1. Backend bring-up: netlists produced by `yosys -p "synth_ice40; write_rtlil"`
     before our own frontend is ready.
  2. A permanent way in for Amaranth and other RTLIL producers.
- RTLIL has no formal spec. We pin the tested Yosys and Amaranth versions and keep
  a corpus generated with each. Unknown cells or constructs produce a clear
  diagnostic rather than a guess.

**SRTL.** The text form of our own RTL IR:
- It maps one-to-one to the in-memory IR and round-trips exactly. It is versioned,
  with a documented grammar and a stability promise from 1.0.
- `--emit rtl` writes it. Pass-level unit tests read it back (input SRTL → pass →
  expected SRTL).
- It costs little because we need it for testing anyway, and it gives new HDLs and
  generators a target that we control.

---

## 7. RTL IR (`synth-rtl`)

A flattened, word-level netlist. Hierarchy is preserved in names, and keeping it is
optional for reporting.

- **Signals** are sequences of bits. Each bit is a net bit or a constant
  (0/1/x/z). They are stored compactly as runs of `(net, offset, width)`.
- **Cells:**
  - Bitwise and reduction: `not and or xor xnor`, `reduce_*`, `logic_not`.
  - Arithmetic: `add sub neg mul`, `div/mod` (constant divisor first), shifts
    (constant and variable), comparisons (signed and unsigned).
  - Selection: `mux`, `pmux` (one-hot select), `bmux`/`demux`.
  - State: `dff` (clock polarity, enable, sync/async set/reset, init value),
    `latch`, and `memory` (width, depth, init, N read ports (sync or async, enable,
    transparency, reset), M write ports (clock, per-bit or per-byte enable,
    priority)).
  - Other: `instance` (primitive or black box), `tristate`.
- **Passes** (each idempotent and individually testable):
  - Constant propagation and peephole algebra; structural hashing (CSE); dead code
    elimination.
  - Mux-tree simplification; width reduction (sign and zero extension analysis,
    unused high bits).
  - Arithmetic extraction: adder trees, `a*b+c` (MAC), comparator sharing.
  - Memory: fold registers into synchronous read ports, merge ports, decide
    BRAM, LUTRAM, SRL or FF using arch cost tables.
  - FSM extraction and re-encoding (one-hot or binary by cost; later phase).
  - DFF legalization against the arch's flop feature set (see §8).

---

## 8. Arch lowering (`arch-*`)

Each arch lowers word-level cells into primitives and leaves random logic for the
logic IR. Rules are written in Rust against the RTL IR API (a pattern DSL can come
later if rules multiply).

| Concern | iCE40 | ECP5 | Artix-7 |
|---|---|---|---|
| Add/sub/compare | `SB_CARRY` + LUT4 (the carry shares LUT inputs I1/I2) | `CCU2C` | `CARRY4` + LUT6 (propagate/generate), ternary adders via LUT6+carry |
| Multiply | `SB_MAC16` (UP5K) or soft | `MULT18X18D`, `ALU54B` | `DSP48E1` (register absorption: A/B/M/P regs, pre-adder, cascades) |
| Block RAM | `SB_RAM40_4K` (256×16 … 2048×2), `SB_SPRAM256KA` (UP5K) | `DP16KD`, `PDPW16KD` | `RAMB18E1`, `RAMB36E1` (output register absorption, cascading) |
| LUT RAM / SRL | n/a | `DPR16X4` (SLICEA/B with SLICEC) | `RAM32M`, `RAM64M`, `RAMD*`, `SRL16E`/`SRLC32E` |
| Wide functions | LUT4 trees | `PFUMX` (LUT5), `L6MUX21` (LUT6/7) | `MUXF7`, `MUXF8`, `LUT6_2` dual output |
| Flops | `SB_DFF[N][E][R\|S\|SR\|SS]` | `TRELLIS_FF` (per-slice LSR/CE/CLK) | `FDRE/FDSE/FDCE/FDPE` (per-slice control set) |
| IO | `SB_IO` (pin types, DDR) | `TRELLIS_IO`, `IDDRX1F`/`ODDRX1F` | `IBUF/OBUF/IOBUF/IBUFDS`, `IDDR/ODDR`, `IDELAYE2`, `ISERDESE2/OSERDESE2` (later) |
| Clocks | `SB_GB`, `SB_PLL40_*`, `SB_HFOSC` | `DCCA`, `EHXPLLL`, `OSCG` | `BUFG/BUFGCTRL/BUFH`, `MMCME2`, `PLLE2` |

Global buffers are inserted automatically for clocks and very high-fanout resets
and enables. PLL and MMCM configurations can be computed from requested
frequencies with a helper (`synth pll --in 12M --out 50M`).

---

## 9. Logic synthesis (`synth-logic`, `synth-sat`)

This is where most of the LUT count and logic depth comes from, so it gets serious
investment.

### 9.1 Representation

- An **AIG** (and-inverter graph) with complemented edges, structural hashing and
  levelization.
- Flops, carry chains, DSPs, BRAMs and black boxes are **boxes**. Their outputs are
  AIG inputs and their inputs are AIG outputs. Boxes carry timing (arrival and
  required times, internal arcs), so mapping is timing-aware *across* them. This is
  the approach ABC9 uses.
- An extension point for XAG/MIG (XOR- and majority-aware graphs) for
  arithmetic-heavy logic, if measurements justify it.

### 9.2 Optimization

- **Balancing** for depth.
- **Rewriting** with precomputed optimal implementations of the 222 NPN classes of
  4-input functions. The library is generated in-repo by exhaustive search, not
  imported.
- **Refactoring** (collapse a cone, re-factor) and **resubstitution** (with
  simulation and SAT-validated don't-cares).
- **SAT sweeping / fraiging**: merge equivalent nodes found by random simulation
  signatures and proven by SAT.
- **Structural choices**: keep alternative structures so the mapper can choose
  between them ("lossless" synthesis).
- Scripts are expressed as pass pipelines in Rust, with presets `-O1/-O2/-O3`
  (runtime versus QoR).

### 9.3 LUT mapping

- K-feasible **priority cuts** (a bounded set of C cuts per node) with
  truth-table computation.
- Pass 1 is **delay-optimal** mapping that includes box delays and placement-free
  wire estimates. Then come **area recovery** passes (area flow, then exact local
  area) under required-time constraints.
- Mapping happens **over choices**.
- Per-family parameters:
  - iCE40: K=4.
  - ECP5: K=4, plus mux-aware mapping of 5- to 7-input functions onto
    LUT4 pairs + `PFUMX`/`L6MUX21`, using a cost model aware of slice and PFU
    sharing.
  - XC7: K=6 plus **LUT6_2 merging** (two functions sharing ≤5 inputs in one LUT6)
    and F7/F8 mux mapping.
- **Post-mapping**: don't-care-based LUT resynthesis (an `mfs`-style windowed
  SAT approach) and LUT pin assignment (critical input on the fastest pin) after
  routing.

### 9.4 SAT solver

A CDCL solver with two watched literals, EVSIDS, phase saving, Luby/glucose-style
restarts, LBD-based clause deletion, recursive learned-clause minimization and
incremental assumptions. It is used for fraiging, resubstitution, `mfs`, and
equivalence checking of our own transformations (§13.3). Writing it ourselves is
justified: it is roughly 2k lines and central to both QoR and correctness.

### 9.5 Sequential optimization (QoR phase)

- Retiming (Leiserson–Saxe min-period, constrained by init values, IO and control
  sets).
- Register duplication for high fanout.
- Control-set reduction (fold rare CE/SR into logic so slices pack densely; this is
  important on XC7 and ECP5).
- FSM re-encoding.

---

## 10. Device database (`synth-chipdb`, `synth-dbgen`)

### 10.1 Sources

| Family | Upstream | License | Contents we use |
|---|---|---|---|
| iCE40 | Project IceStorm | ISC | Tile grid, routing, config bits; timing derived from iCEcube2 |
| ECP5 | Project Trellis + `prjtrellis-db` | ISC / CC0-1.0 | Tile bits, routing per tile type, globals; timing per speed grade (derived from Diamond) |
| Artix-7 | Project X-Ray + `prjxray-db` | ISC / CC0-1.0 | Tile grid, tile connections, tile types (wires, pips, sites), segbits, packages and pins; timing (cell SDF + wire/pip RC) for -1/-2/-3 grades. XC7A35T shares the XC7A50T die |

These are documentation databases. Re-deriving them would require the vendor
tools anyway, so importing them is the "really good reason" exception to the
no-dependency rule. Importing happens **offline** (`cargo xtask dbgen`); end users
never need Python or the upstream repos.

### 10.2 Unified model

- **Tile grid**: tiles `(x, y, type, name)`.
- **Tile types** (stored once each): bels (with pins), local wires, local pips
  (with timing class and config feature), site-internal routing as pseudo-pips,
  LUT route-throughs as pseudo-pips.
- **Nodes**: sets of electrically connected tile wires. They are **deduplicated by
  shape** (relative tile offsets plus wire indices) and anchored per tile.
  Without deduplication, Xilinx databases reach gigabytes (the archived
  nextpnr-xilinx DBs are "several GB" for large parts). With it we target tens of
  MB for XC7A200T.
- **Config features**: feature name → bits per tile type (FASM-compatible
  naming), so bitstream assembly is data-driven.
- **Timing**: cell arc tables per bel type, configuration and speed grade; wire
  and pip delay classes (intrinsic delay plus RC for Elmore-style estimates on
  XC7, table delays on iCE40/ECP5).
- **Packages**: pin name ↔ IO bel, bank, and differential pairs.

### 10.3 Format and distribution

- A versioned, little-endian, flat format: arrays of POD structs with relative
  offsets, so it is zero-copy after a single `read()`. mmap is optional and comes
  later.
- One file per die (and package and speed grade overlay) per family.
- iCE40 databases are small enough to **embed in the binary** (`include_bytes!`), so
  a single file supports iCE40. ECP5 and XC7 databases ship as data files in the
  release archive and can be embedded with a cargo feature for offline builds.

### 10.4 Timing calibration (dev-time only)

Open timing data for XC7 is the weakest link. Calibration loop:
1. Export our placed and routed design to the vendor tool: an EDIF netlist plus
   LOC/BEL constraints and `FIXED_ROUTE` properties for Vivado (the free edition
   supports Artix-7).
2. Run `report_timing` / SDF write-out in the vendor tool.
3. Diff against our STA per arc and fit correction tables that are stored in the
   ChipDB.

The same approach works with Diamond (ECP5) and iCEcube2/Radiant (iCE40). Feasibility
of the Vivado import path is an early risk item (see the roadmap).

---

## 11. Physical implementation

### 11.1 Model

- The **physical design** is the mapped netlist plus `cell → bel` placement, plus
  `net → routing tree` (a list of pips), plus per-cell configuration.
- **BEL-level placement with arch legality checks.** We do not rigidly pre-pack
  into clusters. Each arch provides an *incremental tile validity* check:
  - iCE40 PLB: 8 LCs sharing CLK (and polarity), CE and SR.
  - ECP5 slice/PFU: LSR/CLK/CE sharing, LUT-pair/mux constraints, RAM-mode slices.
  - XC7 slice: one CLK/CE/SR control set for all 8 FFs, LUT/FF pairing and
    shared bypass inputs (`AX`…`DX`), carry alignment, LUT6_2 input sharing,
    SLICEM-only RAM/SRL.

  This gives the placer freedom and lets packing quality improve with placement.
- **Macros** (rigid relative placement): carry chains, F7/F8 trees, LUT6_2 pairs,
  DSP and BRAM cascades. Light pre-packing (LUT→FF with single fanout) is used only
  where it never hurts.

### 11.2 Placement

1. **Simulated annealing** (first, for milestone 1). VPR-style adaptive schedule,
   range limiting, timing-driven cost (wirelength + criticality-weighted delay).
   This is enough for iCE40 (≤ 7,680 LCs).
2. **Electrostatic analytical placement** (ePlace/elfPlace family) for ECP5 and XC7
   scale:
   - Weighted-average wirelength model.
   - Density from an electrostatic potential solved by DCT (our own FFT).
   - Nesterov's method with preconditioning.
   - Separate density maps per resource type (LUT, FF, carry, BRAM, DSP, IO).
   - Timing-driven net weighting; region and clock-region constraints.
3. **Legalization**: macro- and cluster-aware greedy legalization, with min-cost
   flow for column resources (BRAM, DSP, IO).
4. **Detailed placement**: low-temperature annealing, independent-set matching and
   timing-driven swaps with exact incremental STA.

### 11.3 Routing

- **Globals first**: dedicated clock networks (iCE40 global buffers, ECP5 primary
  clocks via DCC, XC7 BUFG → HROW → BUFH/leaf with clock-region budgets).
- **Signals**:
  - Negotiated congestion (PathFinder) with timing-driven cost.
  - **Connection-based** incremental rip-up: only congested or critical
    connections are rerouted.
  - **A\*** with precomputed lookahead tables per wire class and `(dx, dy)`.
  - Growing bounding boxes.
  - Deterministic parallel routing over spatially disjoint net batches.
- LUT route-throughs and **LUT input permutation** as router freedom: the LUT INIT
  is re-permuted to match, which is a big win on XC7.
- Router state is dense arrays for small devices and paged per tile region for
  large ones (XC7A200T has tens of millions of wires).
- **Hold fixing** by detour routing (needed on XC7).

### 11.4 Static timing analysis (`synth-timing`)

- A timing graph over cells and routing (estimated before routing, exact after),
  with incremental update.
- Constraints: an SDC/XDC subset (`create_clock`, `create_generated_clock`,
  `set_clock_groups`, `set_false_path`, `set_multicycle_path`,
  `set_input/output_delay`, `set_max_delay`) parsed by a small Tcl-subset
  interpreter. Clocks generated by PLL/MMCM are derived automatically from their
  configuration.
- Setup and hold, clock skew from global network delays, multi-clock paths.
- Outputs: criticality for every optimization stage; reports per clock (Fmax,
  worst paths, slack histogram) in text and JSON.

---

## 12. Bitstream generation

- Common intermediate: a **config feature set** (FASM-compatible, dumpable with
  `--emit fasm`). It is diffable against upstream tooling.
- **iCE40**: tile bits → CRAM/BRAM banks → command stream with CRC-16, equivalent
  to `icepack`, including warm-boot multi-image.
- **ECP5**: tile bits → frames → command stream (IDCODE check, CRC-16, optional
  compression, SPI mode and usercode options), equivalent to `ecppack`.
- **XC7**: frames addressed by FAR (block type, half, row, column, minor),
  per-frame ECC, CRC, type 1/2 packets, configuration register sequence (IDCODE,
  COR0, CTL0, CMD, startup), BRAM init frames. Equivalent to `fasm2frames` +
  `xc7frames2bit`.
- **Disassemblers** for all three (bitstream → features → physical netlist). They
  enable round-trip verification (§13.5) and quick BRAM content patching
  (`synth bram-patch`, which swaps firmware without re-running place and route).

---

## 13. Verification strategy

Correctness is the first QoR metric: a fast bitstream that is wrong is worthless.

1. **Unit and property tests** per crate. A small in-house property-testing helper
   (no external dependency).
2. **Frontend conformance**: parsing corpora (such as `sv-tests`) and simulation of
   elaborated RTL against Icarus/Verilator outputs. Those tools are dev-only
   oracles.
3. **Translation validation** (`--check`). SAT-based combinational equivalence
   after each logic pass (RTL→AIG, every AIG transform, mapping, lowering).
   Sequential passes (retiming, FSM recoding) are checked by induction with
   register correspondence. This is always on in CI and off by default for users.
4. **Random design fuzzing**: an in-house random Verilog generator (in the spirit
   of VlogHammer/Verismith). Each design is simulated as RTL, as a synthesized
   netlist, and as a disassembled bitstream, and the results are compared.
5. **Bitstream round trip**: disassemble our bitstream → netlist → simulate against
   the original testbench. This tests the *whole* flow without hardware. We also
   check bit-exactness against upstream assemblers (`icepack`, `ecppack`,
   `fasm2frames`) for identical feature sets.
6. **Golden tests**: small designs with checked-in feature dumps and reports.
   Changes appear as reviewable diffs.
7. **Hardware in the loop**: a self-hosted CI runner with the reference boards:
   - Nandland Go Board (iCE40HX1K) and Alchitry Cu (iCE40HX8K).
   - Alchitry Au (XC7A35T) and Nexys A7 (XC7A100T).

   All four use an FT2232H, which provides both the programming interface and a
   USB UART on one cable.
   - The Artix-7 boards load SRAM over JTAG.
   - The iCE40 boards appear to be wired like the iCEstick, with the FT2232H on the
     SPI flash. Their tests will then write and read back flash (to be checked on
     the boards). Typical flash endurance (~100k erase cycles) is enough for CI.

   Tests run firmware and check UART output (riscv-tests, Dhrystone/CoreMark,
   memory tests). There is no ECP5 board yet. Until one is added, ECP5 is verified
   by bitstream round-trip only (item 5).
8. **QoR regression** (`bench/`): the benchmark suite runs on every merge. It
   records LUT/FF/BRAM/DSP counts, Fmax per clock, runtime and peak memory, and
   compares them with stored baselines from vendor tools and Yosys+nextpnr. CI
   flags regressions beyond a threshold.

---

## 14. User experience

```
synth init --board alchitry-cu           # scaffold synth.toml + pin constraints
synth build                              # uses synth.toml
synth build top.v --board nexys-a7-100t --clock clk=100MHz -o out.bit
synth build soc.il --board alchitry-cu   # RTLIL, e.g. from Amaranth
synth check                              # parse + elaborate + lint only
synth report timing|util|clocks          # from the last build
synth prog [--flash]                     # program the board (see below)
synth bram-patch out.bin --mem rom=fw.hex
synth db list|info <device>
```

- **Project file** `synth.toml`: top module, sources, include dirs, defines,
  board or device, clocks, pins, options.
- **Board catalog** (`boards/*.toml`): device, package, speed grade, oscillator
  pins and frequencies, named pins (LEDs, UART, buttons), IO standards, programmer.
- **Constraints**: native TOML, plus import of **PCF** (iCE40), **LPF** (ECP5) and
  **XDC/SDC** subset (Xilinx), so existing projects migrate directly.
- **Integration targets**: a LiteX toolchain backend, an Amaranth platform hook,
  and apio-style workflows. These are cheap and drive adoption.
- **Programming**: `synth prog` delegates to `openFPGALoader` for now. It is
  optional and its use is clearly reported.
  - openFPGALoader has board entries for the Alchitry Au (`alchitry_au`) and the
    Nexys A7 (`nexys_a7_100`).
  - The Cu and the Go Board are programmed through their SPI flash, with
    openFPGALoader's generic FT2232 cable or `iceprog` (to be verified on the
    boards).
- **Native programmer** (later). All four reference boards use the FT2232H, so
  one implementation covers them:
  - FTDI MPSSE over a small USB transport;
  - SPI flash access for iCE40;
  - JTAG SRAM loading for 7-series (UG470).

  The USB transport needs a backend per OS, with no libusb: usbfs ioctls on Linux,
  IOKit through FFI on macOS (the main development platform, Apple Silicon).
  Windows comes last because of WinUSB driver setup. Writing 7-series flash needs
  a SPI-over-JTAG bridge bitstream, which `synth` can build itself.

---

## 15. Engineering policies

- **Dependency policy.** The shipped crates use `std` only. Dev-only tooling
  (`synth-dbgen`, bench harness, CI scripts) should also avoid dependencies, but
  may take one with a written justification in `docs/decisions/`. External tools
  (Yosys, nextpnr, Icarus, Verilator, vendor tools) are allowed **only** as test
  oracles in CI.
- **Rust**: edition 2024, stable toolchain, MSRV pinned. `unsafe` is confined to
  ChipDB zero-copy views and optional SIMD kernels, each with a safety comment and
  tests.
- **Determinism rules**: no hash-order iteration, no time-based seeds, and
  deterministic parallel reductions. A CI job runs every build with `-j1` and
  `-jN` and compares bitstreams.
- **Performance budgets** (tracked in `bench/`):
  - iCE40 RISC-V SoC full flow < 10 s.
  - 50k-LUT ECP5/XC7 design < 3–5 min on 8 cores.
  - Peak memory < 4 GB for XC7A200T.
- **Platforms**: macOS on Apple Silicon and Linux x86-64 are first-class (both in
  CI). Windows should build but is not tested yet.
- **Licensing** (decided, ADR 0004):
  - Toolchain code under **EUPL-1.2**.
  - HDL in the repository (example designs, test SoCs, soft IP) under
    **CERN-OHL-W-2.0**.
  - A README statement that bitstreams, netlists and reports produced by `synth`
    are not covered by the toolchain licence.

  Both licences are compatible with the ISC/CC0 databases. Vendor primitive
  simulation models are written from the public documentation, never copied.

---

## 16. Key references

- Mishchenko, Cho, Chatterjee, Brayton: *Combinational and sequential mapping with
  priority cuts* (ICCAD'07). *DAG-aware AIG rewriting* (DAC'06). *Improvements to
  technology mapping for LUT-based FPGAs* (FPGA'06).
- Brayton, Mishchenko: *ABC: An academic industrial-strength verification tool*
  (CAV'10).
- McMurchie, Ebeling: *PathFinder* (FPGA'95). Betz, Rose: *VPR* (FPL'97).
  Murray et al.: *VTR 8* (TRETS'20), including the router lookahead and
  connection-based routing.
- Lu et al.: *ePlace* (TODAES'15). Meng, Li, Lin, Pan: *elfPlace* (ICCAD'19).
  Gort, Anderson: *HeAP* (FPL'12).
- Leiserson, Saxe: *Retiming synchronous circuitry* (1991).
- Eén, Sörensson: *MiniSat* (SAT'03). Audemard, Simon: *Glucose* (IJCAI'09).
- Project IceStorm, Project Trellis, Project X-Ray documentation. The Yosys,
  nextpnr and VTR source code (references only).
