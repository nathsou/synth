# Native Artix-7 implementation status

The draft PR includes a native Rust path from Verilog through packing,
placement, routing, FASM, and an uncompressed Artix-7 bitstream. It is
**experimental, not at the iCE40 flow's maturity**. Hardware functionality,
calibrated timing closure, and QoR parity with Yosys remain unproven. Missing
timing characterization is reported and fails timing acceptance by default.

## Available functionality

```sh
cargo build --release -p synth
target/release/synth synth design.v --top top --family artix7 --verilog mapped.v
```

`xc7` aliases `artix7`; the default remains `ice40`. The existing Verilog
frontend, defines, include paths, top parameters and `--rename` work with
both synthesis families. `build`, `pnr`, and `synth` accept `--family`.

The new `arch-xc7` crate emits:

- LUT1–LUT6 with full 64-bit truth tables, output polarity selection and
  folding of operand logic into carry-propagate LUTs.
- CARRY4 groups for add/subtract/compare operations, including partial groups
  and carry outputs. Initial carry uses CYINIT; cascaded groups use CI.
- FDRE, FDSE, FDCE and FDPE, with clock inversion, INIT and enable/reset
  priority preserved. Enable-priority synchronous resets are gated before
  mapping because Xilinx reset/preset overrides CE.
- RAMB18E1 for memories with synchronous reads and at most one write port.
  Widths 1, 2, 4, 9 and 18 use TDP mode; width 36 uses simple dual-port
  (SDP) mode with 512 words per block. The mapper minimizes the number of
  blocks across width and depth, preserving independent write enables by
  splitting into narrower blocks when necessary. Data and parity initialization,
  read-port replication, read enables,
  falling-edge clocks, nonzero address offsets and read-register initialization
  are supported. Same-clock transparency is implemented with bypass logic.
  Out-of-range writes are disabled rather than aliased into a valid word.
  Other memories fall back to registers and logic.
- Combinational DSP48E1 multipliers, with signed/unsigned operand extension,
  partial products for wider operands, and result truncation. Small or sparse
  constant multipliers remain in logic. DSP registers, accumulators, preadders
  and dedicated cascades are not implemented.

An address-register absorption guard prevents changing asynchronous reads
when writes use a different clock. Such reads remain asynchronous instead of
being updated only on the address clock.

AIG optimization, structural choices, carry-aware LUT mapping, remapping and
window-based don't-care optimization now live in `synth-logic::mapping`. Backends
provide hard-block timing and emitted-netlist cost callbacks. The carry/DSP timing
weights for Artix-7 are **mapping heuristics, not calibrated device delays**;
the printed LUT-level delay is not an Fmax prediction.

Short arithmetic is evaluated with minimum carry widths of 3, 4 and 5,
skipping alternatives that cannot change the lowered graph. A candidate is
retained only when no LUT, FF, CARRY4, BRAM18 or DSP48 count increases, its
estimated delay does not increase, and at least one measure improves. This
lets three-bit additions fit directly into LUT6s and exposes neighboring
logic to optimization. The selected arithmetic partition is also tried with
16 priority cuts per node instead of 8, under the same acceptance rule. This
can require up to four mapping runs. A further candidate shares identical
carry chains (including subtraction/comparison pairs and commuted operands),
and is accepted under the same resource and delay constraints.

After selection, exact duplicate LUT functions are merged with canonical pin
ordering, including propagate LUTs created after the main mapper. Dedicated
carry inputs still require a LUT in their own slice lane: the packer replicates
the function there instead of adding a buffer after the shared LUT. Thus a
lower synthesis LUT count does not imply the same reduction after packing.
These are synthesis/packing heuristics, not guarantees about routed timing.
`--pack-stats` on `synth --family artix7` reports native packed slice, LUT and
CARRY4 counts without a device database; the QoR report records them separately.

## Native physical flow

```sh
tools/fetch_xc7_db.sh /tmp/prjxray-db
target/release/synth build design.v --top top --family artix7 \
  --db /tmp/prjxray-db/artix7 --device xc7a35tftg256-1 \
  --xdc design.xdc --fasm design.fasm -o design.bit --timing-allow-fail
# Also tested with --device xc7a100tcsg324-1.
```

The database is CC0 Project X-Ray data pinned to
`0a0addedd73e7e4139d52a6d8db4258763e0f1f3`. The executable reads it directly;
Python, Yosys, nextpnr and vendor executables are not runtime dependencies.
`pnr mapped.il` accepts the same physical options. `.bit` outputs have a
deterministic header; other output suffixes select the raw configuration stream.

Implemented components:

- Device frame geometry, package pins, site templates and tile connectivity.
  Union-find collapses tile wires into routing nodes. Templates share pip
  descriptions; the router uses a compact sparse graph. The 35T import has
  2,092,063 nodes and about 21.4 million directed edges; the 100T import has
  3,779,095 nodes and about 39.2 million edges. Import and routing currently
  use hundreds of MB, not the roadmap's tens-of-MB target.
- Four LUT6 and four primary FF slots per slice, common clock/enable/reset
  legality, contiguous carry-chain macros, direct FF input bypasses, carry
  output legalization, RAMB18E1 TDP/36-bit SDP and combinational DSP48E1 placement. The
  second FF bank, LUT5 sharing, RAMB36, SRL, LUTRAM and wide-mux packing are
  not implemented. Unsupported DSP register/control modes fail explicitly.
- Deterministic seeded macro placement with HPWL annealing, RAM/DSP placement,
  explicit package-pin placement, BUFG insertion and native PathFinder
  routing. Only configuration-backed pips and explicitly modeled site
  pass-throughs can be routed. Clock distribution enable bits are emitted.
- An explicit XDC subset: `set_property` (including `-dict`), `get_ports`,
  and `create_clock`. Every port needs PACKAGE_PIN and LVCMOS33. DRIVE, SLEW
  and PULLTYPE are supported. Unknown commands/properties and nonexistent or
  duplicate pins are errors. This does not execute Tcl or support complete
  vendor board constraint files, generated clocks or I/O timing constraints.
- Native FASM scalar/vector parsing and assembly, alias offsets, negative
  configuration bits and conflict detection. Frames include ECC, CRC checks,
  startup packets and row padding. `pack-fasm` assembles FASM;
  `unpack-xc7` reads full uncompressed streams and checks part ID, frame
  boundaries, ECC and CRC. It exports frames, not a reverse-mapped FASM file.
- An SDF reader preserving propagation, interconnect, device, setup/hold and
  recovery/removal records. The coverage audit reports missing routed pip
  delays. This is **not STA**: speed-grade models, distributed wire RC,
  setup/hold analysis and timing-driven repair still need integration.

The 35T uses X-Ray's shared 50T fabric, as the reference tools do. The data
has no separate 35T disabled-site mask; utilization limits and site legality
at device capacity have not been independently validated. 50T/200T geometry
is recognized but has not passed the physical regression suite. Incremental
checkpoints, calibrated Fmax, PLL/MMCM and advanced I/O are not implemented.

`--timing-allow-fail` is an explicit bring-up override. Without it, a build
writes its diagnostic bitstream and exits unsuccessfully; it never reports
uncertified timing as passing.

## Independent physical validation

```sh
tools/prepare_xc7_oracle.sh /tmp/xc7-oracle
/tmp/xc7-oracle/venv/bin/python tools/ci_artix7_physical.py \
  --db /tmp/prjxray-db/artix7 --xray /tmp/xc7-oracle/prjxray \
  --xray-tools /tmp/xc7-oracle/build/tools \
  --euler --full-luts --out /tmp/xc7-physical
```

The oracle setup needs Git, a C++ compiler, CMake and Python with venv.
Project X-Ray is pinned to `c9f02d8576042325425824647ab5555b1bc77833`;
Python dependencies are pinned in `tools/xc7-test-requirements.txt`.
These are test dependencies only. See [THIRD_PARTY.md](../THIRD_PARTY.md).

The suite implements an inverter, clocked counter, mixed reset/enable/clock
edge registers, shared carry-propagate functions, initialized TDP and SDP block RAM
(including a three-bank design covering both RAMB18 site variants),
paired DSP multipliers, single-site
boundary I/O, and optionally
Euler 31 and 97 on both 35T and 100T. For each routed design, independent X-Ray FASM
assembly must match every native configuration frame. X-Ray's C++ packet
writer generates a second bitstream, and its C++ decoder must recover the
same bits, including ECC, from both streams. Counter builds also check
byte-for-byte repeatability and the timing-failure gate. `--full-luts`
randomizes every LUT INIT in the fabric to exercise configuration addressing.
Sources, XDC, FASM, bitstreams, frames and reports are retained. The latest
local [physical validation report](ARTIX7_PHYSICAL.json) records 24 passing
checks, source/constraint hashes and independent tool revisions. The updated
workspace passes 193 tests, warnings-denied Clippy and Rust 1.88 checks. The
merged playground also passes WebAssembly Clippy and web app tests; the Weft
blinky example synthesizes to Artix-7 primitives.

A 35T full-fabric run covered 1,044,018 enabled LUT INIT bits and all 5,408
frames. The raw native startup/frame stream with CRC packets disabled also
matched `xc7frames2bit` byte-for-byte after synchronization. The regular
suite uses CRC-enabled native streams. These checks establish configuration
encoding agreement, **not functional correctness of the placed circuit**.
There has been no hardware execution or independent vendor timing validation;
no Vivado installer or validation runner was available in the development
environment. The CI job retains the generated artifacts for further validation.

The branch includes main's selective CI policy. Artix-7 simulations share
the single optimized `ci` profile build with the other simulation suites,
reuse cached pinned device data/oracles, and run only for relevant changes
after the PR leaves draft (or on a manual run). Crate tests still run on draft
PRs. The merged Weft/playground path retains its optimization-stage observer
and WebAssembly-compatible clock.

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
address-register transparency and out-of-range writes. Fourteen multiplier
configurations cover signed/unsigned products, the 25-by-18 signed DSP boundary,
multi-DSP products up to 64-by-64 bits, and truncated outputs. Each is compared
against RTL using the independent Yosys DSP48E1 simulation model on 2,000
directed/random input pairs, in addition to Rust integer-semantics checks.

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
The native timed command also gathers packing statistics. `synth_packed`
reports the native packer's resource counts, including legalization and
replication; there is no matched Yosys packing baseline. No placed area or
Fmax is claimed.

Measured on 2026-10-18 with Yosys 0.52 (`fee39a3284c90249e1d9684cf6944ffbbcbb8f90`) and the PR working tree based on `5714ca0` (including main at `67504cb`), using a frozen `ci` profile binary. Each resource column is **synth / Yosys**. Full cell counts, input hashes and wall times are in [ARTIX7_QOR.json](ARTIX7_QOR.json).

| Design | LUTs incl. INV | FFs | CARRY4 | BRAM18 equivalents | DSP48 |
|---|---:|---:|---:|---:|---:|
| Euler 14 | 2968 / 3222 | 2214 / 2201 | 494 / 592 | 4 / 4 | 0 / 0 |
| Euler 25 | 492 / 543 | 187 / 187 | 125 / 143 | 0 / 0 | 0 / 0 |
| Euler 31 | 198 / 297 | 402 / 404 | 65 / 54 | 6 / 6 | 0 / 0 |
| Euler 43 | 3819 / 3404 | 1497 / 1517 | 339 / 320 | 0 / 0 | 0 / 0 |
| Euler 48 | 3230 / 3336 | 2412 / 2438 | 709 / 731 | 0 / 0 | 13 / 13 |
| Euler 57 | 1073 / 1070 | 857 / 1114 | 291 / 224 | 8 / 0 | 0 / 0 |
| Euler 58 | 4027 / 4769 | 2158 / 2030 | 192 / 225 | 0 / 0 | 0 / 0 |
| Euler 92 | 910 / 948 | 2807 / 2660 | 173 / 181 | 7 / 7 | 0 / 1 |
| Euler 97 | 547 / 621 | 445 / 445 | 126 / 125 | 0 / 0 | 3 / 3 |

LUT count is lower on seven of nine designs, but two regress and several designs trade extra carries or block RAMs for other resources. For example, Yosys uses 44 RAM32M cells on Euler 57, while synth uses 8 RAMB18E1 blocks. Euler 92 matches the BRAM count but still uses more FFs. **These results do not meet the requested QoR criterion.**

The 36-bit SDP experiment reduces Euler 57 from 16 to 8 RAMB18 blocks
and Euler 92 from 11 to 7, matching Yosys's BRAM count on Euler 92. The other
seven designs' BRAM counts, and every design's LUT/FF/carry/DSP and native
packed slice/LUT/carry counts, are unchanged. The change uses both data and
parity buses and maps all four write-enable lanes to paired site pins.
Vendor-model simulation covers 24/32/35/36/37-bit words, initialization,
falling edges, masks, read-port replication and a three-bank memory. Physical
encoding covers width splitting and depth banking on both supported parts,
following the pinned nextpnr-Xilinx/X-Ray references; it still needs
hardware validation. This is a general RAM geometry choice, with no
benchmark-specific rules or changes to Yosys settings.

Exact LUT sharing reduces Euler 48 from 3,548 to 3,230 LUTs (9.0%) and
Euler 97 from 600 to 547 (8.8%), with unchanged FF/CARRY4/BRAM/DSP counts.
Euler 58 saves one LUT and Euler 92 saves eleven. These reductions are at
synthesis; carry-lane replication can consume the saved LUTs during packing.
The native packed counts are recorded separately for every benchmark.
Euler 97 still packs into 205 slices, as before this change; its 53-LUT
synthesis reduction is not a 53-LUT physical saving.

The QoR investigation compared [ABC's priority-cut mapper](https://github.com/berkeley-abc/abc/blob/master/src/map/if/ifMap.c)
and [Yosys's Xilinx arithmetic mapping](https://github.com/YosysHQ/yosys/blob/fee39a3284c90249e1d9684cf6944ffbbcbb8f90/techlibs/xilinx/arith_map.v).
Regenerating cuts during exact-area recovery was tested and rejected: it
improved none of the nine designs and increased LUT counts on seven.
Unconditionally changing carry sharing or the mapper's cost feedback also
found smaller LUT counts with worse carry counts/delay on Euler 43. The final
implementation retains the original candidate in those cases. No benchmark
sources or Yosys optimization settings were changed.

Euler 10 was attempted but its Yosys mapping was stopped after more than ten minutes without a completed result. Its full gate-level simulation was also not completed (the initial run hit a five-minute timeout). It is omitted from the numerical comparison rather than assigned an estimated or zero result. The optional long simulation now has a 30-minute command limit.

To reproduce the nine completed cases:

```sh
python3 tools/qor_artix7.py --out /tmp/xc7-qor \
  --design 14 --design 25 --design 31 --design 43 --design 48 \
  --design 57 --design 58 --design 92 --design 97
```

The synthesis foundation passed formatting, Clippy with warnings denied, 107 workspace tests, Rust 1.88 compatibility, the default Artix-7 formal/differential checks, and byte-identical iCE40 netlist emission against `a1f312d` for Euler 25, 31 and 97. Physical-flow validation is described separately above; these synthesis resource counts do not include packing legalization.

## Remaining requirements for iCE40 maturity and requested QoR

- Complete and independently calibrate timing analysis, including the BRAM
  pip types lacking characterization in the upstream data, wire RC and clock
  skew. Implement timing-driven placement/routing and hold repair.
- Validate physical behavior from bitstream readback simulation and reference
  hardware, with a SoC and realistic clock/I/O constraints. Frame agreement
  alone cannot establish that the configured circuit implements the RTL.
- Complete primitive/constraint coverage, device-capacity legality, compact
  database storage and incremental checkpoints.
- Close the measured synthesis regressions, including LUTRAM/SRL mapping,
  then compare matched physical flows and constraints against Yosys plus a
  reference backend. The present measurements do not meet QoR parity.

The [Phase 2 roadmap](ROADMAP.md#phase-2-artix-7) specifies further scaling,
SoC and hardware exit criteria. This PR must remain a draft until its stated
acceptance criteria are met.
