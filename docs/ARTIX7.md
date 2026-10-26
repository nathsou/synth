# Native Artix-7 implementation status

synth has a native Rust path from Verilog through synthesis, packing,
placement, routing, static timing estimation, FASM and an uncompressed
Artix-7 bitstream. It is **experimental, not at the iCE40 flow's maturity**:
bitstreams have been checked against independent encoders and by simulating
the netlist reconstructed from them, but never run on hardware. The timing
estimate's routing delays are calibrated against Vivado 2025.2's timing of
the same routed designs (see below); it is still an estimate, with clock
skew between clock regions and hold repair, but without I/O timing. Missing timing characterization is reported and
fails timing acceptance by default.

[Against Vivado](#against-vivado), with Vivado's own timer judging both
tools' placed and routed results, synth uses fewer slices on six of the ten
Euler solvers. Its synthesis is on par with Vivado's (Vivado implementing
synth's netlist is within 3% of its own flow, and ahead on five designs),
but its placement and routing are 1–12% slower under a tight clock
constraint, 3% on average (details below), in less time on average.

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
  Before mapping, constants added in sequence fold (`i + 3 - C` takes one
  chain, not two in series), and chains of running maxima or minima of six
  bits or more (`if (best < x[k]) best = x[k], arg = n[k]`) become balanced
  trees with the same winner on ties. On Euler 31 and 14, whose critical
  paths these were, they gained 6% and 11% by Vivado's timer.
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
does not increase, and something improves. The candidates are mapped on
threads of their own and compared in a fixed order, so the selection does
not depend on the number of threads. Exact duplicate LUTs and identical
carry chains are shared.

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
  left (`synth synth --pack-stats` reports this packing). The first-fit
  packing it replaced spread connected logic over the design and about
  doubled the wirelength of any placement of it, Vivado's included.
  Implementation packs by placement instead: each atom is first a slice of
  its own, weighted by its share of a slice's LUTs or flip-flops, the
  analytical placer (below) places them all, and each slice then grows
  with the atoms within three tiles of its own position, preferring shared
  nets. Against clustering by shared nets alone this gained 1-9% by
  Vivado's timer on every Euler design (Euler 31 and 57 9%, 58 7%), for
  9-36% more slices. Carry DI enters through the lane's X input rather
  than a pass-through LUT on O5 (2% by the estimate), and a dynamic
  carry-in through AX rather than an extra carry below the chain. Packing
  is also timing-driven: the first packing is timed with a
  fixed delay between slices, and the LUTs and flip-flops driving
  connections of criticality 0.7 or more are packed again on outputs that
  leave the slice directly, off a lane's O5 and its second flip-flop bank.
  Those leave through the lane's output multiplexer, about 185 ps slower
  (Vivado times a LUT read from them in 300 ps instead of 124); before,
  they were on most of Vivado's worst paths of Euler 14, 43, 57 and 58.
  With the timer below, this gained 2.1% by Vivado's timer (geometric
  mean over six seeds of eight designs, every design faster, Euler 10 and
  92 by 4%), for at most 1% more slices; the new timer alone was 1% slower.
- **Placement.** Analytical global placement (HeAP, as in nextpnr) over
  slice macros, block RAMs and DSPs together, with I/O fixed by the XDC:
  quadratic wirelength under the bound-to-bound net model, solved per axis
  by conjugate gradients, is spread over the sites of each kind by
  recursive cuts of the regions that overflow, and each spread placement
  anchors the next solution with a weight growing every iteration. Every
  five iterations the spread placement is legalized (each macro onto the
  free site nearest its position) and timed, and connections pull in
  proportion to their criticality to the fourth power. Simulated annealing
  then refines the legalized placement on VPR's schedule from a local
  window and a low temperature: N^(4/3) moves per temperature within a
  window adapting toward 44% acceptance, cooling by acceptance rate, and a
  greedy finish, minimizing half-perimeter wirelength and each
  connection's predicted delay weighted by its criticality. Predicted
  delays come from a table of the routing graph's fastest delays from a
  slice to each slice by tile offset (Dijkstra from the device's centre,
  0.6 s), within 2% of routed connections' mean in a tile, where the
  piecewise linear fit of distance it replaced was 30% under nearby and
  two to three times over tens of rows away.
  A carry chain moving onto single slices displaces them into the sites it
  leaves, chains of one length may swap, and a move tries several targets
  before giving up (most offsets land on a chain). Clock nets use BUFGs
  and global routing and are left out of the costs; slices go only where a
  global clock reaches (under X-Ray's U-turn clock rows, two columns of
  fifty slices per part have none). Spreading counts a row as two tile
  columns, as the routed delays do, so that a design pressed into a corner
  by its pins spreads wide rather than tall. Against annealing from a
  random placement, the analytical start raised synth's estimate by 3%
  (geometric mean of the medians of five seeds of Euler 14, 25, 31, 57, 58
  and 92), and 1.6% by Vivado's timer of the median seed of each. Four
  placement seeds are implemented in parallel threads sharing the device
  data, and the one with the best timing estimate is kept (`--tries`,
  default 4; one in WebAssembly): three gained 1-5% by the estimate over a
  single seed, and the fourth adds 6% to the average runtime (72 s per
  Euler design against Vivado's 86; six seeds took as long as Vivado).
  The estimate after placement predicts the routed one too loosely to
  choose seeds before routing them.
- **Lane order.** Each slice output has direct pips into six of its tile's
  LUT pins (L_A into M_A3, L_A5, M_B3, L_B5, L_C1 and L_D1, for instance),
  about 180 ps against 350-850 ps through other wires, and in Vivado's
  worst paths of synth's bitstreams hops within one tile took 0.4-0.8 ns.
  Placement and the timer count a tile's connections as free whatever
  their lanes, so after detailed placement each tile with a critical
  connection inside it reorders its slices' movable lanes and trades them
  between its two slices: every order of each slice's lanes, then every
  swap across them, two passes, keeping the order whose inputs (weighted by
  criticality to the eighth power, each on its fastest free pin) arrive
  soonest. Delays come from a search of each tile template's routing graph
  from each output to each pin within a tile of it. This gained 1.3% by
  Vivado's timer over six seeds of the eight Euler designs (140.5 to 142.3
  MHz; Euler 92 +2.0%, 25 and 58 +1.9%, 14 unchanged), for under 0.1 s of
  each seed's run. Pricing the same delays in detailed placement's costs
  gained 0.3% more by the estimate, within its noise, and is not done.
- **Routing.** Negotiated congestion (PathFinder) with A*. The estimate is
  a lookahead: the least remaining cost and delay to a sink by wire class
  and distance, found by searching backwards from sample sinks. It bounds
  sinks like those sampled only: sinks are told apart by the classes of
  the wires entering them, and each kind of at least eight sinks that the
  samples spread over all sinks miss gets one of its own; toward rarer
  kinds the searches take a cost per tile of distance (searched with no
  estimate at all, Euler 31's few such sinks took most of its routing).
  Before, built from data inputs alone, the table knew from the clock tree
  only long routes into the fabric, never the two hops into a clock pin,
  and each of a clock net's sinks
  expanded almost everything else first (on Euler 10, 12 s every time its
  clock net of 1,019 sinks was routed). This made routing 39% faster on
  the Euler designs at the same Fmax. Nets in
  disjoint regions (quadrants, then halves) are routed concurrently, and
  each connection searches only around its own source and sink; congested
  nets keep the branches of their trees clear of overused nodes and reroute
  only the sinks they lose. Routing is timing-driven: sink criticalities
  come from the placement's predicted delays, then from the routes every
  other iteration; a connection's criticality (at most 0.99) weighs its
  delay against congestion. Once routing is legal, the critical
  connections are routed again three times, keeping the best timed
  routing; each net keeps the branches of its tree to its other sinks, as
  connection-based routers do (rerouting whole nets gave the same Fmax).
  The router also picks each LUT input pin, so a
  critical connection takes a fast one. Address cascades between block RAM
  tiles and buffer pass-throughs are not general routing.
- **LUT input pins.** A lane's six pins differ by up to 515 ps into the
  LUT (A1 slowest, A6 fastest), but with criticality capped the router
  cannot tell the connections at the worst slack apart, and on Euler 14 and
  58 the lanes at the worst slack could have kept 60-370 ps more slack with
  their inputs on other pins, through paths the interconnect has free.
  After routing, the lanes within 100 ps of the worst slack are routed
  again: each input's branch into the lane is freed, the fastest free path
  from its tree to each of the lane's pins is searched, the assignment of
  inputs to pins that leaves the lane's least slack the most is kept, and
  the paths are committed most critical first. Four rounds, timed between
  them, and the routes are kept only if the worst period improves.
  This gained 2.0% by Vivado's timer over six seeds of the eight Euler
  designs (137.8 to 140.5 MHz geometric mean; Euler 43 +3.0%, 25 +2.9%,
  58 +2.8%, 57 unchanged) and 1.5% by the estimate, for 4-8 s of each
  seed's run; lanes within 300 ps and ten rounds gained no more.
- **Timing estimate.** Static timing over the packed netlist using X-Ray's
  SDF cell delays (slow-corner maxima, per CARRY4 arc and per DSP register
  configuration) and nextpnr-xilinx's routing delay model of X-Ray wire RC,
  calibrated against Vivado 2025.2: X-Ray's pip delays scale by 1.144 to
  Vivado's slow corner, and the pips X-Ray leaves untimed take fitted
  delays, mainly the interconnect-to-slice inputs (515 ps into A1 down to
  none into A6), the slice output muxes and the IOB input path. Fitted on
  Euler 14's 17,000 routed connections and checked on Euler 31's, the
  error per connection against Vivado's net delays fell from 430 ps RMS to
  under 100. Flip-flops are timed as Vivado times them, read from its
  timing arcs: clock to Q 456 ps (419 from the second bank) where X-Ray has
  340, data setup -31 ps from the lane's own LUT or carry and 67 through
  the bypass input, enable 205 and set or reset 429 (X-Ray has -45 and 109
  and no reset check). A pip's RC delay counts 35% of the input load of
  the route tree's other branches leaving its source wire, as Vivado's net
  delays grow with fanout: on Euler 10's routed connections the error went
  from 34 ps optimistic at fanout 1 and 87 ps pessimistic above 30 to under
  40 ps either way. A block RAM's setups are by input class from the
  SDF (data inputs 241 ps read-first, as synth's memories are). Wire
  delays are per input pin, and before routing a carry's CI from the slice
  below costs nothing and an output through a lane's output multiplexer
  185 ps. Vivado's arcs have equal rise and fall delays, so one delay per
  arc loses nothing. The estimate of a whole design is within 4% of
  Vivado's timing of the same routing on the Euler designs (1% on
  average). Before, with X-Ray's flip-flop delays and a carry reading one
  net on S inside its slice and on DI through routing taking the routed
  delay on both, it was up to 6% off, mostly optimistic; placement also
  predicted each carry chain crossing slices at a routed connection's
  delay. It reports each clock's worst period, the critical path, and the
  estimate without block RAM and DSP paths. Clock skew follows Vivado's
  clock tree by region: each row of clock regions farther from the global
  buffers takes the clock 165 ps later (150 ps at the minimum delays the
  capture uses), a block RAM 40 ps later, and Vivado removes 80 ps more
  pessimism when launch and capture share a region (36 ps across the spine
  in one row), fitted to Vivado's clock delays on Euler 10, 31 and 58
  (positions within a region change latency by about 1 ps per tile). A
  path launched in a later region and captured in an earlier one loses
  about 260 ps: small Euler designs straddle the region boundary at row
  150 near the clock pin, which had cost Euler 31 about 8% by Vivado.
  Timing it lets placement and the choice of seed avoid it (Euler 14 +3%,
  geomean +0.5% over six seeds), and brings the estimate to within 1% of
  Vivado on most runs. Hold is checked at Vivado's fast corner, timed as
  the slow delays times 0.30 (its routes measured 0.31-0.35 of synth's
  estimates), against a capture clock 50 ps late within a region, 265 ps
  across the spine and 350 ps one row of regions up, at flip-flops, at
  block RAM inputs (Vivado's hold times: 183 ps for addresses, 155 for
  data, 89 for write enables, on a clock 40 ps later still), and at shift
  register data and LUT RAM write inputs (Vivado's largest: 183 and 240
  ps). After
  routing, the last routed connection of each path short of the
  requirement by less than 50 ps is rerouted through unused wires near
  its sink: a detour pairs a search forward from where its branch leaves
  the net with one backward from the sink, turning where the two meet
  slow enough, and where the sink's branch is too short it reroutes the
  branch above. A pin reached through several site pins (a block RAM
  write enable bit drives two) is timed and slowed on all of them. Carry
  chains stay within one clock region, whose dedicated links could not
  be slowed. Synth's bitstreams had failed Vivado's hold check on 14 of
  48 runs (by up to 60 ps, all across the region boundary at row 150);
  the first repair, of flip-flop inputs only, left 4 of 24 failing, and
  with block RAM inputs 1 of 48 (a write enable timed on one of its two
  site pins), at no cost in Fmax. With the LUT input pass above, 1 of 48
  failed at a shift register's data input across the row 150 boundary,
  unchecked until then; it passes with those inputs checked. There is no I/O timing.
- **Constraints.** An explicit XDC subset: `set_property` (including
  `-dict`), `get_ports` and `create_clock`; every port needs PACKAGE_PIN and
  LVCMOS33. Unknown commands or properties are errors.
- **Bitstreams.** Native FASM assembly with alias and conflict checks,
  frames with ECC, CRC, startup packets and row padding; `unpack-xc7`
  checks part ID, frame boundaries, ECC and CRC.

The 35T uses X-Ray's shared 50T fabric, as the reference tools do; site
legality at device capacity is not independently validated. RAMB36, DSP
cascades, MMCMs, advanced I/O and incremental checkpoints are not
implemented.

- **PLLs.** The family-neutral `synth_pll<IN_HZ, OUT_HZ>` (Weft standard
  library, or a Verilog black box) becomes a `PLLE2_ADV` in internal
  feedback, on the PLL site nearest its reference clock's pad (a
  clock-capable pin then reaches it through `HCLK_CMT_CCIO`), its output on
  a BUFG and its control inputs tied as nextpnr-xilinx ties them. The
  feedback multiplier is always 8: the lock and loop filter settings
  (`LKTABLE`, `TABLE`) depend on it, and the only values known here are
  nextpnr-xilinx's, for 8. From a 100 MHz board clock the VCO runs at
  800 MHz and CLKOUT0 divides it (100, 80, 66.7, 57.1, 50, 40, 33.3, 25 MHz,
  ...). The PLL's output frequency is its clock's timing target. Its
  features match nextpnr-xilinx's for the same design bit for bit, apart
  from the BUFG chosen; it has not run on hardware yet. `--timing-allow-fail` is an explicit bring-up override: without
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
figures are the median of three placement seeds, at revision `b806661`
(each the best of synth's four parallel placement seeds); the netlist
column of Euler 14 and 31 is at `e3d3441` too, the others at `3bec21a`,
whose netlists are the same.
Slices are occupied slices after routing.
Results: [ARTIX7_VIVADO.json](ARTIX7_VIVADO.json).

| Design | Slices: Vivado / netlist / synth | LUTs: Vivado / netlist | FFs: Vivado / netlist / synth | MHz, Vivado's timer: Vivado / netlist / synth | synth's estimate |
|---|---:|---:|---:|---:|---:|
| Euler 10 | 1446 / 1222 / 1224 | 3760 / 3963 | 1369 / 1859 / 1859 | 112.4 / 118.3 / 112.3 | 111.3 |
| Euler 14 | 1224 / 1087 / 1153 | 3234 / 2961 | 2200 / 2212 / 2214 | 147.3 / 151.3 / 141.0 | 149.9 |
| Euler 25 | 167 / 183 / 150 | 665 / 515 | 187 / 187 / 187 | 144.1 / 143.8 / 135.0 | 137.8 |
| Euler 31 | 83 / 86 / 91 | 223 / 196 | 300 / 300 / 300 | 327.1 / 323.4 / 287.7 | 289.3 |
| Euler 43 | 1329 / 1104 / 1228 | 5032 / 3637 | 1523 / 1501 / 1497 | 72.3 / 96.0 / 89.7 | 90.2 |
| Euler 48 | 1244 / 1206 / 1254 | 4781 / 3229 | 2449 / 2400 / 2412 | 134.8 / 132.6 / - | 129.0 |
| Euler 57 | 390 / 374 / 352 | 1562 / 1149 | 857 / 784 / 601 | 130.3 / 137.9 / 120.4 | 119.5 |
| Euler 58 | 1029 / 1097 / 1220 | 3890 / 3743 | 2023 / 2028 / 2028 | 138.8 / 144.9 / 132.5 | 135.6 |
| Euler 92 | 823 / 536 / 577 | 1427 / 879 | 1647 / 2616 / 2616 | 158.0 / 153.8 / 145.1 | 144.7 |
| Euler 97 | 211 / 211 / 223 | 792 / 545 | 443 / 443 / 445 | 85.6 / 83.7 / - | 80.5 |

- **Area.** synth uses fewer slices than Vivado on six of ten designs (by
  7-30%), and up to a fifth more on Euler 31 and 58, whose critical paths
  its placement-driven packing spreads out (see Packing).
- **Synthesis.** Vivado implementing synth's netlist matches or beats its
  own synthesis on Euler 10, 14, 43, 57 and 58 (Euler 43 by 33%, 14 by 3%),
  and is within 3% on 25, 31, 48, 92 and 97.
- **Placement and routing.** synth's own placement and routing are 1-12%
  slower than Vivado's under a tight constraint, 3% on average (Euler 43,
  where synth's netlist is much better, is 23% faster). Since revision
  `ae1dd6c` the flow gained 9.1% (geometric mean over six seeds of eight
  designs): 4.1% at `3909187`, 2.1% at `e3d3441` and 0.5% from a fourth
  placement seed. Controlled experiments
  located the gap in packing: Vivado placing synth's slices (the fasm2bels
  netlist, each slice and carry chain a macro) reached no better than
  synth, while Vivado packing the same netlist itself was 9-20% faster.
  Placement-driven packing closed about half of that; on Euler 31, Vivado
  placing synth's new slices is 4% faster than synth and its own packing 7%
  faster still. Euler 57 shows another part: Vivado stacks carry chains
  that feed each other in one column, where synth's analytical placement
  leaves them a few columns apart. Routing is within a few percent: Vivado
  routing synth's placements of Euler 14 and 58 was no faster than synth's
  own routing.
- **Runtime.** End to end, synth takes 10-169 s per design, 72 s on
  average, and Vivado 44-215 s, 86 s on average (synth measured alone on
  the machine, Vivado with other jobs). synth is faster on seven of ten;
  Euler 10 takes 169 s against 86 (four placement seeds and three timing
  rounds of routing), 58 121 s against 86, and 48 88 s against 96.

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

- Packing and placement: on Euler 31, Vivado placing synth's slices is 4%
  faster than synth, and Vivado's own packing of the same netlist 7%
  faster still. Starting the final placement from the packing's global
  placement (or legalizing that placement directly), capping the LUTs per
  slice, and a wider clustering radius were tried and gained nothing; nor
  did weighing delay two or four times more in routing, or an admissible
  lookahead, though a third of the critical LUT inputs take one of the
  slow pins A1-A3 (the interconnect reaches the faster ones only through
  another hop). Before annealing, the analytical placement converges for
  its cost: its timing weight and criticality exponents, more moves and a
  place-route iteration were tried as well. Carry chains feeding each
  other end up a few columns apart (Euler 57's critical path crosses eight),
  where Vivado stacks them; legalization moves chains only half a tile from
  the analytical placement, so the model, not the legalizer, leaves them
  there. Copying the drivers of critical nets with many sinks among them
  (as Vivado's `phys_opt_design` does; Euler 25's worst path crossed a
  LUT with 199 sinks) gained 0.4% on the five designs with such nets, in
  the noise. Weighting slices' shared nets by criticality in clustering, and
  comparators of two bits per carry lane (as Vivado maps them: half the
  CARRY4s), gained nothing measurable.
- Timing: flip-flops in SLICEM sites, which Vivado times 60 ps slower to
  Q and 30-50 ps faster to set up than in SLICEL, and the carry DI arcs
  from O5, which Vivado times about 40 ps faster than X-Ray's, are not
  modelled.
- Time DSP designs with Vivado: fasm2bels has no DSP48 model, so Euler 48
  and 97 have only synth's estimate.
- Extend timing to I/O, and characterize the BRAM pips
  missing from the upstream data.
- Reduce routing time further. With a lookahead sample for each kind of
  sink, Euler 48 routes in 69 s rather than 113 s and Euler 10 in 99 s
  rather than 234 s (four seeds at once; the whole suite's flow takes a
  third less time at the same Fmax), but the lookahead's thirty-odd
  backward searches cost seconds on any design: Euler 31, of 54 LUTs,
  takes 6 s longer than before.
- Close the remaining synthesis gaps: Euler 43's wide multiplexing, and
  carry-lane LUT replication that costs 2–5% slices on Euler 43, 48 and 97.
- Validate on hardware with a SoC and realistic clock and I/O constraints;
  extend primitive coverage (RAMB36, DSP cascades, clocking) and verify
  legality at device capacity.

The [Phase 2 roadmap](ROADMAP.md#phase-2-artix-7) specifies the remaining
scaling, SoC and hardware exit criteria.
