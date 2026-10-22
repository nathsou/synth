# Native Artix-7 implementation status

synth has a native Rust path from Verilog through synthesis, packing,
placement, routing, static timing estimation, FASM and an uncompressed
Artix-7 bitstream. It is **experimental, not at the iCE40 flow's maturity**:
bitstreams have been checked against independent encoders and by simulating
the netlist reconstructed from them, but never run on hardware. The timing
estimate's routing delays are calibrated against Vivado 2025.2's timing of
the same routed designs (see below); it is still an estimate, without clock
skew, I/O or hold analysis. Missing timing characterization is reported and
fails timing acceptance by default.

[Against Vivado](#against-vivado), with Vivado's own timer judging both
tools' placed and routed results, synth uses fewer slices on nine of the ten
Euler solvers. Its synthesis is on par with Vivado's (Vivado implementing
synth's netlist is within 7% of its own flow, and ahead on four designs),
but its placement and routing are 10–25% slower under a tight clock
constraint (details below).

On nine Project Euler solvers, the native flow is also compared with Yosys's
`synth_xilinx` followed by nextpnr-xilinx, and with Yosys's netlist placed
and routed by synth's own backend. Summary (details below):

- Native packed slices are fewer than nextpnr's on all nine designs (by
  34% to 61%), and fewer than the matched backend's on six (2–5% more on
  the other three).
- The native estimate is faster than the matched backend's (same timer and
  backend, Yosys's netlist) on eight of nine designs.
- Against nextpnr's own estimates, native timing is better on six of nine;
  the two timers use different cell delay models (see below).
- End to end (Yosys included for nextpnr), native implementation is
  faster on two designs and within 1.5 times nextpnr's time on five;
  Euler 25 takes 1.7 times as long and Euler 48 three times.

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
  an address range check. A shallow memory whose reads pass through two
  register stages goes to block RAM instead, as with Vivado: the block RAM
  absorbs both stages, LUT RAM neither.
- RAMB18E1 for larger memories with synchronous reads and at most one write
  port, in TDP mode (widths 1–18) or 36-bit SDP mode, choosing the geometry
  with the fewest blocks. Initialization, parity, read enables, falling
  edges, address offsets and output initialization are supported. A
  register that alone follows the read data moves into the RAM's output
  register (DOA_REG, and DOB_REG for SDP words) with its clock enable and
  synchronous reset; the registered output is also faster (0.88 ns clock to
  output rather than 2.45).
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
  draw from each interconnect tile's own GND/VCC wires where they can. After
  carries, wide multiplexers and memories take their lanes, the remaining
  LUTs and flip-flops cluster by affinity (as VPR's AAPack): a LUT and the
  flip-flop it alone feeds form one atom, and a slice grows with the atom
  sharing the most nets with it, then fills with the least connected atoms
  left. The first-fit packing this replaced spread connected logic over the
  design and about doubled the wirelength of any placement of it, Vivado's
  included.
- **Placement.** Deterministic simulated annealing of half-perimeter
  wirelength over slice macros, block RAMs and DSPs together, with I/O
  fixed by the XDC, on VPR's schedule: the initial temperature from the
  cost changes of random moves, N^(4/3) moves per temperature within a
  window adapting toward 44% acceptance, cooling by acceptance rate, and a
  greedy finish. A carry chain moving onto single slices displaces them
  into the sites it leaves, chains of one length may swap, and a move tries
  several targets before giving up (most offsets land on a chain). Clock
  nets use BUFGs and global routing and are left out of the wirelength
  cost. Timing-driven placement predicts each connection's delay from its
  distance, fitted to the calibrated delays of routed connections.
- **Routing.** Negotiated congestion (PathFinder) with A*. The estimate is
  a lookahead: the least remaining cost and delay to a sink by wire class
  and distance, found by searching backwards from sample sinks. Nets in
  disjoint regions (quadrants, then halves) are routed concurrently, and
  each connection searches only around its own source and sink; congested
  nets keep the branches of their trees clear of overused nodes and reroute
  only the sinks they lose. Routing is timing-driven: sink criticalities
  come from the placement's predicted delays, then from the routes every
  other iteration; a connection's criticality (at most 0.99) weighs its
  delay against congestion. The router also picks each LUT input pin, so a
  critical connection takes a fast one. Address cascades between block RAM
  tiles and buffer pass-throughs are not general routing.
- **Timing estimate.** Static timing over the packed netlist using X-Ray's
  SDF cell delays (slow-corner maxima, per CARRY4 arc and per DSP register
  configuration) and nextpnr-xilinx's routing delay model of X-Ray wire RC,
  calibrated against Vivado 2025.2: X-Ray's pip delays scale by 1.144 to
  Vivado's slow corner, and the pips X-Ray leaves untimed take fitted
  delays, mainly the interconnect-to-slice inputs (515 ps into A1 down to
  none into A6), the slice output muxes and the IOB input path. Fitted on
  Euler 14's 17,000 routed connections and checked on Euler 31's, the
  error per connection against Vivado's net delays fell from 430 ps RMS to
  under 100; the estimate of a whole design is within 6% of Vivado's
  timing of the same routing on the Euler designs (it was up to 35%
  optimistic). It
  reports each clock's worst period, the critical path, and the estimate
  without block RAM and DSP paths. It has no clock skew, I/O timing or hold
  analysis.
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

**Against Vivado** (Vivado 2025.2, not needed by the executable):

```sh
python3 tools/xsim_euler.py --out /tmp/xc7-xsim
python3 tools/vivado_artix7.py --db /tmp/prjxray-db/artix7 --out /tmp/xc7-vivado \
  --fasm2bels /tmp/fasm2bels/venv/bin/python --connections /tmp/fasm2bels/conn \
  --bitread /tmp/xc7-oracle/build/tools/bitread --seeds 3
# Again with each design constrained beyond the best Fmax of that run:
python3 tools/vivado_artix7.py --db /tmp/prjxray-db/artix7 --out /tmp/xc7-vivado-tight \
  --targets /tmp/xc7-vivado/results.json --fasm2bels /tmp/fasm2bels/venv/bin/python \
  --connections /tmp/fasm2bels/conn --bitread /tmp/xc7-oracle/build/tools/bitread --seeds 3
python3 tools/vivado_report.py --vivado /tmp/xc7-vivado-tight/results.json \
  --netlist /tmp/xc7-vivado-tight/results.json --synth /tmp/xc7-vivado-tight/results.json
python3 tools/xsim_euler.py --out /tmp/xc7-xsim-bits --bitstream-dir /tmp/xc7-vivado \
  --db /tmp/prjxray-db/artix7 --bitread /tmp/xc7-oracle/build/tools/bitread \
  --fasm2bels /tmp/fasm2bels/venv/bin/python --connections /tmp/fasm2bels/conn
```

`xsim_euler.py` simulates every Euler solver's synthesized netlist with
Xilinx's UNISIM models in Vivado's simulator to its known answer and exact
cycle count (all ten pass, Euler 58 over 2.6 million cycles), and with
`--bitstream-dir` the netlists fasm2bels rebuilds from synth's routed
bitstreams (all eight without DSPs pass). It found that a LUT on a lane's
O5 output driving the clock enable or reset of its own slice's flip-flops
was routed out through the output multiplexer without that multiplexer's
O5 setting, so the flip-flops were enabled by their own data (Euler 58,
which `ci_artix7_bitsim.py` does not cover). `vivado_artix7.py` is the QoR
comparison below; Vivado's design rule checks found that the harnesses had
put single-ended clocks on the N side of clock-capable pairs, now the P
side. No hardware execution has been possible.

## QoR

### Against Vivado

`tools/vivado_artix7.py` implements each Euler solver three ways on the
XC7A100T-CSG324 with the same pins and clock, each judged by Vivado
2025.2's timer after routing:

- **Vivado**: Vivado from RTL with its default steps (`synth_design`,
  `opt_design`, `place_design`, `phys_opt_design`, `route_design`).
- **netlist**: synth's synthesized netlist through the same Vivado steps,
  comparing synthesis alone.
- **synth**: synth's native flow from source to bitstream. fasm2bels
  rebuilds the routed netlist from the bitstream with every cell's site and
  every net's route as constraints; Vivado routes only the clock and the
  few nets it cannot take as routed (those also routed inside a slice), and
  times the result. fasm2bels has no DSP48 model: Euler 48 and 97 have only
  synth's own estimate.

Each design is constrained 20% beyond the best Fmax any flow reached at
100 MHz, so that every flow works for timing (Vivado stops optimizing once
a constraint is met; synth's results do not depend on the target). synth's
figures are the median of three placement seeds. Slices are occupied
slices after routing. Results: [ARTIX7_VIVADO.json](ARTIX7_VIVADO.json).

| Design | Slices: Vivado / netlist / synth | LUTs: Vivado / netlist | FFs: Vivado / netlist / synth | MHz, Vivado's timer: Vivado / netlist / synth | synth's estimate |
|---|---:|---:|---:|---:|---:|
| Euler 10 | 1446 / 1222 / 986 | 3760 / 3963 | 1369 / 1859 / 1859 | 112.4 / 118.3 / 101.5 | 98.3 |
| Euler 14 | 1224 / 1081 / 958 | 3234 / 2934 | 2200 / 2212 / 2214 | 147.3 / 138.5 / 112.7 | 119.0 |
| Euler 25 | 167 / 183 / 138 | 665 / 515 | 187 / 187 / 187 | 144.1 / 143.8 / 125.8 | 127.5 |
| Euler 31 | 83 / 110 / 87 | 223 / 207 | 300 / 300 / 300 | 327.1 / 304.2 / 243.9 | 240.0 |
| Euler 43 | 1329 / 1104 / 1071 | 5032 / 3637 | 1523 / 1501 / 1497 | 72.3 / 96.0 / 80.5 | 81.1 |
| Euler 48 | 1244 / 1206 / 1092 | 4781 / 3229 | 2449 / 2400 / 2412 | 134.8 / 132.6 / - | 115.5 |
| Euler 57 | 390 / 374 / 284 | 1562 / 1149 | 857 / 784 / 601 | 130.3 / 137.9 / 113.4 | 107.0 |
| Euler 58 | 1029 / 1097 / 930 | 3890 / 3743 | 2023 / 2028 / 2028 | 138.8 / 144.9 / 115.8 | 118.1 |
| Euler 92 | 823 / 536 / 477 | 1427 / 879 | 1647 / 2616 / 2616 | 158.0 / 153.8 / 130.7 | 131.1 |
| Euler 97 | 211 / 211 / 198 | 792 / 545 | 443 / 443 / 445 | 85.6 / 83.7 / - | 78.5 |

- **Area.** synth uses fewer slices than Vivado on every design but
  Euler 31 (87 against 83), by 6–42% elsewhere, and Vivado's own
  implementation of synth's netlist packs it into more slices than synth
  on all ten.
- **Synthesis.** Vivado implementing synth's netlist matches or beats its
  own synthesis on Euler 10, 43, 57 and 58 (Euler 43 by 33%), and is within
  3% on 25, 48, 92 and 97; Euler 14 and 31 trail by 6–7%.
- **Placement and routing.** synth's own placement and routing remain
  10–25% slower than Vivado's under a tight constraint (Euler 43, where
  synth's netlist is much better, is 11% faster). On Euler 14, Vivado
  placing synth's slices reaches 131 MHz and routing synth's placement 120,
  against synth's 113: the gap is shared by placement and routing, with
  about 5% from packing (Vivado's own packing of the same netlist, 139).
- **Runtime.** End to end, synth takes 4–155 s per design and Vivado 44–215
  s (measured with other jobs on the machine); synth is faster on all but
  Euler 10, where its three timing rounds add about 50 s.

### Synthesis

`tools/qor_artix7.py` synthesizes the Euler corpus with identical sources
and tops, flattened. The reference is `synth_xilinx -family xc7 -flatten
-noiopad -noclkbuf` with Yosys's other defaults: DSP, RAM, SRL and wide mux
inference enabled. LUTs are LUT1–LUT6 plus Yosys's INV cells; LUT RAM,
SRL and MUXF7/F8 cells are counted separately in the JSON. Packed slices
are each netlist through synth's packer. Measured with Yosys 0.52 (`fee39a3`) at revision `4875c58`; each
column is **synth / Yosys**. Full counts: [ARTIX7_QOR.json](ARTIX7_QOR.json).

| Design | LUTs | FFs | CARRY4 | BRAM18 | DSP48 | Packed slices |
|---|---:|---:|---:|---:|---:|---:|
| Euler 14 | 2961 / 3525 | 2214 / 2201 | 494 / 592 | 4 / 4 | 0 / 0 | 964 / 1178 |
| Euler 25 | 515 / 543 | 187 / 187 | 87 / 143 | 0 / 0 | 0 / 0 | 147 / 215 |
| Euler 31 | 203 / 297 | 402 / 404 | 60 / 54 | 6 / 6 | 0 / 0 | 104 / 179 |
| Euler 43 | 3637 / 3400 | 1497 / 1517 | 339 / 320 | 0 / 0 | 0 / 0 | 922 / 901 |
| Euler 48 | 3229 / 3351 | 2412 / 2438 | 709 / 731 | 0 / 0 | 13 / 13 | 1095 / 1061 |
| Euler 57 | 1139 / 1067 | 1113 / 1114 | 155 / 224 | 0 / 0 | 0 / 0 | 406 / 483 |
| Euler 58 | 3751 / 4706 | 2028 / 2030 | 132 / 225 | 0 / 0 | 0 / 0 | 1010 / 1200 |
| Euler 92 | 880 / 966 | 2616 / 2660 | 157 / 181 | 7 / 7 | 0 / 1 | 481 / 516 |
| Euler 97 | 545 / 543 | 445 / 445 | 121 / 125 | 0 / 0 | 3 / 3 | 195 / 185 |

Euler 43 needs 7% more LUTs: Yosys implements more of its wide
multiplexing in MUXF7/F8 (610 and 222 cells). Euler 57 trades 72 LUTs for
69 fewer CARRY4s. Euler 43, 48 and 97 pack into 2–5% more slices than
Yosys's netlists, mostly from propagate LUTs replicated into carry lanes.

### Routed

`tools/pnr_artix7.py` implements each design three ways on the
XC7A100T-CSG324 with the same pin constraints and a 100 MHz clock: synth
from source; Yosys then nextpnr-xilinx (`8f178fc`, chip database from the
same X-Ray revision); and Yosys's netlist through synth's backend
("matched"). Resources are counted alike from each flow's FASM.
Results: [ARTIX7_PNR.json](ARTIX7_PNR.json).

| Design | Slices: synth / nextpnr / matched | INT pips | MHz: synth / nextpnr / matched | Seconds: synth / nextpnr |
|---|---:|---:|---:|---:|
| Euler 14 | 964 / 2266 / 1178 | 49407 / 68048 / 55324 | 105.7 / 89.9 / 114.0 | 59 / 51 |
| Euler 25 | 147 / 380 / 215 | 8770 / 7229 / 8365 | 117.5 / 90.4 / 90.0 | 22 / 13 |
| Euler 31 | 104 / 254 / 179 | 4552 / 7266 / 5440 | 254.8 / 232.7 / 253.4 | 9 / 11 |
| Euler 43 | 922 / 1398 / 901 | 50641 / 49108 / 48004 | 82.9 / 76.2 / 76.3 | 125 / 84 |
| Euler 48 | 1095 / 2364 / 1061 | 56647 / 58139 / 51862 | 125.6 / 133.9 / 111.2 | 117 / 38 |
| Euler 57 | 406 / 784 / 483 | 22094 / 21300 / 18978 | 106.0 / 108.0 / 100.0 | 28 / 19 |
| Euler 58 | 1010 / 2152 / 1200 | 55820 / 71300 / 67874 | 115.3 / 90.6 / 84.4 | 77 / 56 |
| Euler 92 | 481 / 926 / 516 | 19624 / 28446 / 21241 | 135.0 / 116.6 / 110.9 | 21 / 28 |
| Euler 97 | 195 / 446 / 185 | 9213 / 10539 / 9004 | 78.4 / 141.8 / 71.6 | 17 / 12 |

Seconds are end to end: synthesis included, Yosys's for nextpnr. The
synth columns are at revision `46b82d1`.

How to read the timing columns:

- **synth and matched** share one timer, so those two columns compare the
  synthesis results directly.
- **nextpnr's figure** comes from its own timer. It shares the routing delay
  model but has lower cell delays (about 0.1 ns per LUT and for FF clock to
  Q, against X-Ray's slow-corner SDF maxima), and it does not time block RAM
  or DSP paths.
- **Euler 97** is limited by a chain of two DSP multiplies in one cycle.
  Without block RAM and DSP paths, synth estimates 162.6 MHz and the matched
  flow 139.9. Euler 48's estimate without DSP paths is 129.8 MHz.
- **No timing is certified.**

## Remaining work

- Placement: on Euler 14, Vivado's placer leaves about 20% less wirelength
  than synth's on the same slices, and Vivado's own packing a further 30%
  less. More annealing moves no longer help (the anneal converges for its
  cost); an analytical global placement is the likely next step, after a
  force-directed start was tried and gained nothing.
- Time DSP designs with Vivado: fasm2bels has no DSP48 model, so Euler 48
  and 97 have only synth's estimate.
- Extend timing to clock skew, I/O and hold, and characterize the BRAM pips
  missing from the upstream data.
- Reduce routing time on designs with wide high-fanout critical nets
  (Euler 48: three times nextpnr's). Their sinks' A* searches are loosely
  bounded by a lookahead that classes wires without their index.
- Close the remaining synthesis gaps: Euler 43's wide multiplexing, and
  carry-lane LUT replication that costs 2–5% slices on Euler 43, 48 and 97.
- Validate on hardware with a SoC and realistic clock and I/O constraints;
  extend primitive coverage (RAMB36, DSP cascades, clocking) and verify
  legality at device capacity.

The [Phase 2 roadmap](ROADMAP.md#phase-2-artix-7) specifies the remaining
scaling, SoC and hardware exit criteria.
