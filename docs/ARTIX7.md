# Native Artix-7 implementation status

synth has a native Rust path from Verilog through synthesis, packing,
placement, routing, static timing estimation, FASM and an uncompressed
Artix-7 bitstream. It is **experimental, not at the iCE40 flow's maturity**:
bitstreams have been checked against independent encoders and by simulating
the netlist reconstructed from them, but never run on hardware, and the
timing estimate is uncalibrated. Missing timing characterization is reported
and fails timing acceptance by default.

On nine Project Euler solvers, the native flow is compared with Yosys's
`synth_xilinx` followed by nextpnr-xilinx, and with Yosys's netlist placed
and routed by synth's own backend. Summary (details below):

- Native packed slices are fewer than nextpnr's on all nine designs (by
  32% to 57%), and fewer than the matched backend's on six.
- The native estimate is faster than the matched backend's (same timer and
  backend, Yosys's netlist) on eight of nine designs.
- Against nextpnr's own estimates, native timing is better on six of nine;
  the two timers use different cell delay models (see below).
- Native implementation is slower: 3 to 4 times nextpnr's wall time on
  the largest designs.

## Synthesis

```sh
cargo build --release -p synth
target/release/synth synth design.v --top top --family artix7 --verilog mapped.v
```

`xc7` aliases `artix7`; the default remains `ice40`. The Verilog frontend,
defines, include paths, top parameters and `--rename` work with both
families. `build`, `pnr`, and `synth` accept `--family`.

The `arch-xc7` crate emits:

- LUT1–LUT6, and MUXF7/MUXF8 for seven- and eight-input functions. After
  the LUT6 mapping is selected, it is remapped once with cuts of up to eight
  inputs, each costing its LUT6s plus the dedicated muxes, and kept only if
  it needs fewer LUTs at no greater depth. Unused inputs never use the muxes.
- CARRY4 chains for add/subtract/compare, with operand logic folded into
  the propagate LUTs; short additions may stay in LUTs (see QoR below).
- FDRE/FDSE/FDCE/FDPE with clock inversion, INIT and enable/reset priority.
- SRLC32E shift registers for chains of at least three flip-flops with one
  clock, enable and no live reset, whose stages have no other readers.
- RAM32M/RAM64M distributed RAM for memories of at most 64 words with
  asynchronous reads and one write port: other-address reads use ports A to
  C and the write-address port D; writes are grouped by enable and gated by
  an address range check.
- RAMB18E1 for larger memories with synchronous reads and at most one write
  port, in TDP mode (widths 1–18) or 36-bit SDP mode, choosing the geometry
  with the fewest blocks. Initialization, parity, read enables, falling
  edges, address offsets and output initialization are supported.
- DSP48E1 multipliers with signed/unsigned extension, partial products for
  wider operands and truncation. An addition of a product with no other use
  folds into the post-adder (P = A·B + C) when the product fits one DSP.
  Small or sparse constant multiplies stay in logic.

AIG optimization, structural choices, carry-aware LUT mapping and don't-care
optimization are in `synth-logic::mapping`. Carry/DSP timing weights in the
mapper are heuristics; the printed LUT-level delay is not an Fmax.

Short arithmetic is evaluated with minimum carry widths of 3, 4 and 5, and
the selection retried with more priority cuts; a candidate is kept only if
no LUT, FF, CARRY4, BRAM18 or DSP48 count increases, its estimated delay
does not increase, and something improves. Exact duplicate LUTs and
identical carry chains are shared.

## Native physical flow

```sh
tools/fetch_xc7_db.sh /tmp/prjxray-db
target/release/synth build design.v --top top --family artix7 \
  --db /tmp/prjxray-db/artix7 --device xc7a35tftg256-1 \
  --xdc design.xdc --fasm design.fasm -o design.bit --timing-allow-fail
# Also tested with --device xc7a100tcsg324-1.
```

The database is CC0 Project X-Ray data pinned to
`0a0addedd73e7e4139d52a6d8db4258763e0f1f3`; the executable reads it
directly. Python, Yosys, nextpnr and vendor tools are not runtime
dependencies. `pnr mapped.il` implements a mapped RTLIL netlist (such as
Yosys's) with the same options, including Yosys's MUXF7/F8, SRL16E/SRLC32E,
RAM32M/RAM64M and registered or post-adding DSP48E1s.

- **Device import.** Frame geometry, package pins, site templates and tile
  connectivity; tile wires are collapsed into routing nodes. The 100T has
  3.78 million nodes and 39.9 million edges. Bidirectional pips get the
  feature of each direction; default pseudo-pips are fixed connections.
- **Packing.** Slices hold four LUT6s, each optionally sharing its site
  with a LUT5 on O5 (partners chosen by shared inputs), and eight FFs (the
  second bank fed from O5 or the bypass input). Control sets, carry
  macros, MUXF7/F8 lanes and SLICEM-only LUT RAM and shift registers (which
  share the slice's write clock and enable) are legalized. Constant sinks
  draw from each interconnect tile's own GND/VCC wires where they can.
- **Placement.** Deterministic simulated annealing of half-perimeter
  wirelength over slice macros, block RAMs and DSPs together, with I/O
  fixed by the XDC. Clock nets use BUFGs and global routing and are left out
  of the wirelength cost.
- **Routing.** Negotiated congestion (PathFinder) with A*. The estimate is
  a lookahead: the least remaining cost and delay to a sink by wire class
  and distance, found by searching backwards from sample sinks. Nets in
  disjoint regions (quadrants, then halves) are routed concurrently;
  congested nets keep the branches of their trees clear of overused nodes
  and reroute only the sinks they lose. Routing is timing-driven: sink
  criticalities come from the placement's predicted delays, then from the
  routes, and one round reroutes critical nets after the first legal
  routing. Address cascades between block RAM tiles and buffer
  pass-throughs are not general routing.
- **Timing estimate.** Static timing over the packed netlist using X-Ray's
  SDF cell delays (slow-corner maxima, per CARRY4 arc and per DSP register
  configuration) and nextpnr-xilinx's routing delay model of X-Ray wire RC.
  It reports each clock's worst period, the critical path, and the estimate
  without block RAM and DSP paths (which nextpnr-xilinx does not time). It
  has no clock skew, I/O timing or hold analysis and is not calibrated.
- **Constraints.** An explicit XDC subset: `set_property` (including
  `-dict`), `get_ports` and `create_clock`; every port needs PACKAGE_PIN and
  LVCMOS33. Unknown commands or properties are errors.
- **Bitstreams.** Native FASM assembly with alias and conflict checks,
  frames with ECC, CRC, startup packets and row padding; `unpack-xc7`
  checks part ID, frame boundaries, ECC and CRC.

The 35T uses X-Ray's shared 50T fabric, as the reference tools do; site
legality at device capacity is not independently validated. PLL/MMCM,
RAMB36, DSP cascades, advanced I/O and incremental checkpoints are not
implemented. `--timing-allow-fail` is an explicit bring-up override: without
it a build writes its bitstream and exits unsuccessfully, never reporting
uncertified timing as passing.

## Validation

```sh
cargo fmt --all --check
RUSTFLAGS='-D warnings' cargo clippy --workspace --all-targets
cargo test --workspace
tools/fetch_xc7_models.sh /tmp/xc7-unisim
XILINX_UNISIM=/tmp/xc7-unisim python3 tools/ci_artix7.py --out /tmp/xc7-validation
tools/prepare_xc7_oracle.sh /tmp/xc7-oracle
/tmp/xc7-oracle/venv/bin/python tools/ci_artix7_physical.py \
  --db /tmp/prjxray-db/artix7 --xray /tmp/xc7-oracle/prjxray \
  --xray-tools /tmp/xc7-oracle/build/tools --euler --yosys --out /tmp/xc7-physical
tools/prepare_fasm2bels.sh /tmp/fasm2bels /tmp/xc7-oracle/prjxray
python3 tools/ci_artix7_bitsim.py --physical /tmp/xc7-physical \
  --db /tmp/prjxray-db/artix7 --bitread /tmp/xc7-oracle/build/tools/bitread \
  --fasm2bels /tmp/fasm2bels/venv/bin/python --connections /tmp/fasm2bels/conn \
  --unisim /tmp/xc7-unisim --out /tmp/xc7-bitsim
```

Yosys, Icarus, Project X-Ray, fasm2bels and the Xilinx simulation models are
test oracles only, pinned by the scripts above; see
[THIRD_PARTY.md](../THIRD_PARTY.md).

**Synthesis** (`ci_artix7.py`, 62 checks): combinational equivalence against
Yosys's Xilinx cell models at arithmetic widths 1 to 33 and all LUT6 inputs;
differential tests of clock edges, reset/enable priorities and
initialization; block RAM tests against Xilinx's RAMB18E1 model across all
widths, parity, banking, masks, collisions and initial values; LUT RAM and
shift registers against Xilinx's RAM32M/RAM64M/SRLC32E models; fourteen
multiplier and eight multiply-add configurations (signed, truncated, up to
64 by 64 bits) on 2,000 vectors each; and seven Euler solvers (25, 31, 43,
48, 57, 92, 97) simulated to their known answers and exact cycle counts.

**Encoding** (`ci_artix7_physical.py`, 30 checks on the 35T and 100T): an
inverter, I/O, counter, mixed FF controls, shared carries, LUT RAM, shift
registers, initialized TDP/SDP block RAM including a three-bank design,
DSPs, Yosys netlists with MUXF7/F8, SRL16E and a registered post-adding
DSP48E1, and Euler 31 and 97. Independent X-Ray FASM assembly must match
every native frame, and X-Ray's C++ packet writer and decoder must agree
with the native bitstream including ECC. `--full-luts` randomizes every
LUT INIT in the fabric. Results: [ARTIX7_PHYSICAL.json](ARTIX7_PHYSICAL.json).

**Function** (`ci_artix7_bitsim.py`, 24 checks): fasm2bels reconstructs a
netlist from each routed bitstream, which is simulated against the RTL for
2,000 cycles of random inputs with Xilinx's models, comparing every output
bit the RTL defines. All non-DSP designs above pass on both parts (fasm2bels
has no DSP48 model; DSP designs are skipped). fasm2bels does not carry
SLICEM clock inversion to LUT RAM and shift registers, which the harness
restores. This checks the configured circuit, but through fasm2bels's
interpretation of the bits, not hardware. Results:
[ARTIX7_BITSIM.json](ARTIX7_BITSIM.json).

No hardware execution or vendor timing validation has been possible: no
Vivado installation or board was available.

## QoR

### Synthesis

`tools/qor_artix7.py` synthesizes the Euler corpus with identical sources
and tops, flattened. The reference is `synth_xilinx -family xc7 -flatten
-noiopad -noclkbuf` with Yosys's other defaults: DSP, RAM, SRL and wide mux
inference enabled. LUTs are LUT1–LUT6 plus Yosys's INV cells; LUT RAM,
SRL and MUXF7/F8 cells are counted separately in the JSON. Packed slices
are each netlist through synth's packer. Measured with Yosys 0.52 (`fee39a3`) at revision `c9d0731`; each
column is **synth / Yosys**. Full counts: [ARTIX7_QOR.json](ARTIX7_QOR.json).

| Design | LUTs | FFs | CARRY4 | BRAM18 | DSP48 | Packed slices |
|---|---:|---:|---:|---:|---:|---:|
| Euler 14 | 2961 / 3525 | 2214 / 2201 | 494 / 592 | 4 / 4 | 0 / 0 | 992 / 1207 |
| Euler 25 | 515 / 543 | 187 / 187 | 87 / 143 | 0 / 0 | 0 / 0 | 174 / 246 |
| Euler 31 | 203 / 297 | 402 / 404 | 60 / 54 | 6 / 6 | 0 / 0 | 109 / 178 |
| Euler 43 | 3637 / 3400 | 1497 / 1517 | 339 / 320 | 0 / 0 | 0 / 0 | 944 / 919 |
| Euler 48 | 3229 / 3351 | 2412 / 2438 | 709 / 731 | 0 / 0 | 13 / 13 | 1139 / 1097 |
| Euler 57 | 1139 / 1067 | 1113 / 1114 | 155 / 224 | 0 / 0 | 0 / 0 | 431 / 523 |
| Euler 58 | 3751 / 4706 | 2028 / 2030 | 132 / 225 | 0 / 0 | 0 / 0 | 1021 / 1232 |
| Euler 92 | 880 / 966 | 2616 / 2660 | 157 / 181 | 7 / 7 | 0 / 1 | 484 / 527 |
| Euler 97 | 545 / 543 | 445 / 445 | 121 / 125 | 0 / 0 | 3 / 3 | 202 / 193 |

Euler 43 needs 7% more LUTs: Yosys implements more of its wide
multiplexing in MUXF7/F8 (610 and 222 cells). Euler 57 trades 72 LUTs for
69 fewer CARRY4s. Euler 48 and 97 have fewer or equal LUTs but pack into
more slices, from the LUTs replicated into carry lanes.

### Routed

`tools/pnr_artix7.py` implements each design three ways on the
XC7A100T-CSG324 with the same pin constraints and a 100 MHz clock: synth
from source; Yosys then nextpnr-xilinx (`8f178fc`, chip database from the
same X-Ray revision); and Yosys's netlist through synth's backend
("matched"). Resources are counted alike from each flow's FASM.
Results: [ARTIX7_PNR.json](ARTIX7_PNR.json).

| Design | Slices: synth / nextpnr / matched | INT pips | MHz: synth / nextpnr / matched | Seconds: synth / nextpnr |
|---|---:|---:|---:|---:|
| Euler 14 | 992 / 2266 / 1207 | 53257 / 68048 / 61122 | 104.5 / 89.9 / 87.9 | 236 / 51 |
| Euler 25 | 174 / 380 / 246 | 9402 / 7229 / 9519 | 111.4 / 90.4 / 72.9 | 25 / 12 |
| Euler 31 | 109 / 254 / 178 | 5017 / 7266 / 6416 | 234.1 / 232.7 / 189.0 | 10 / 11 |
| Euler 43 | 944 / 1398 / 919 | 58809 / 49108 / 55141 | 83.3 / 76.2 / 71.6 | 319 / 76 |
| Euler 48 | 1139 / 2364 / 1097 | 61444 / 58139 / 56418 | 118.3 / 133.9 / 116.8 | 125 / 36 |
| Euler 57 | 431 / 784 / 523 | 23170 / 21300 / 20530 | 93.8 / 108.0 / 86.2 | 57 / 18 |
| Euler 58 | 1021 / 2152 / 1232 | 64869 / 71300 / 73314 | 104.2 / 90.6 / 84.9 | 193 / 56 |
| Euler 92 | 484 / 926 / 527 | 23351 / 28446 / 24223 | 119.5 / 116.6 / 89.7 | 31 / 28 |
| Euler 97 | 202 / 446 / 193 | 10810 / 10539 / 9656 | 76.5 / 141.8 / 77.8 | 17 / 11 |

How to read the timing columns:

- **synth and matched** share one timer, so those two columns compare the
  synthesis results directly.
- **nextpnr's figure** comes from its own timer. It shares the routing delay
  model but has lower cell delays (about 0.1 ns per LUT and for FF clock to
  Q, against X-Ray's slow-corner SDF maxima), and it does not time block RAM
  or DSP paths.
- **Euler 97** is limited by a chain of two DSP multiplies in one cycle.
  Without block RAM and DSP paths, synth estimates 137.9 MHz and the matched
  flow 142.7.
- **No timing is certified.**

## Remaining work

- Calibrate timing against vendor tools or hardware (cell corners, clock
  skew, I/O and hold), and characterize the BRAM pips missing from the
  upstream data.
- Reduce implementation time on large designs (3–4 times nextpnr's now),
  dominated by the first routing pass.
- Close the remaining synthesis gaps: Euler 43's wide multiplexing, and
  carry-lane LUT replication that costs slices on Euler 48 and 97.
- Validate on hardware with a SoC and realistic clock and I/O constraints;
  extend primitive coverage (RAMB36, DSP cascades, clocking) and verify
  legality at device capacity.

The [Phase 2 roadmap](ROADMAP.md#phase-2-artix-7) specifies the remaining
scaling, SoC and hardware exit criteria.
